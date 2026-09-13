//! The correlation client (DESIGN.md §3.3): `execute()` is the single
//! command-dispatch path every typed operation (a later ticket) delegates
//! to. Correlation is by FIFO — tmux processes commands serially and emits
//! exactly one guard block per command, in order — not by the guard's
//! command-number, which is informational only.
//!
//! [`ConnectionState`] is owned and explicit (DESIGN.md §3.3, "no timing
//! folklore"): on attach tmux emits an unsolicited `%begin…%end`/`%error`
//! greeting block that is not a reply to any command. Correlating a
//! caller's first command against it is a classic off-by-one that corrupts
//! every subsequent reply, so it must be consumed during `Connecting`
//! before `execute()` will run at all.
//!
//! Two ways to build a `Client`, for two different needs:
//! - [`Client::connect`] is what real usage against a live tmux should call
//!   — it performs the greeting handshake synchronously and only returns a
//!   `Ready` client (or an error if the handshake itself failed).
//! - [`Client::new`] skips the handshake and starts `Ready` immediately. It
//!   exists for tests and any other case where the byte stream is already
//!   known to be positioned past a greeting (e.g. a transport that isn't
//!   actually tmux at all). Using it against a real, freshly-attached tmux
//!   transport reintroduces the exact off-by-one this module exists to
//!   prevent — `connect()` is the safe default.
//!
//! Notifications dispatch as typed [`ServerMessage`] events to the
//! notification sink. Pane output (`%output`/`%extended-output`) never goes
//! through that path — it is high-volume and mixing it into the
//! notification stream is how you get head-of-line blocking — it routes
//! through a separate byte sink instead. Both sinks are supplied at
//! construction and neither is optional, so a parsed message with nowhere
//! to go is not a state this client can be in.
//!
//! That requirement is load-bearing rather than ceremonial: [`Client::connect`]
//! dispatches whatever tmux wrote behind the greeting terminator, which is
//! strictly before any caller could have registered a sink afterwards. The
//! crate used to cover that one-handshake window with unbounded queues that
//! then retained for the entire connection — paying a connection-lifetime
//! price for a startup guarantee. Requiring the sinks removes the window
//! instead of paying for it, and leaves this crate holding no queue at all.
//! A caller wanting a stream discarded says so with a discarding closure; a
//! caller wanting it buffered owns that buffer, which is where this module
//! already places the polling policy it declines to own.
//!
//! A sink only fires while some blocking call
//! (`execute`/`connect`/`reconnect`) is actively reading — this client has
//! no background thread of its own, so "dispatch" here means "synchronously
//! invoked the moment a message is parsed during one of those calls," not
//! "delivered independently of any call in progress." A caller that wants
//! to react to notifications while otherwise idle needs to poll (e.g. call
//! `execute` on some interval, or a later layer that owns a dedicated
//! read thread) — that policy belongs above this crate, not in it.

mod connection_state;
mod error;

pub use connection_state::{CloseReason, ConnectionState};
pub use error::TmuxError;

use crate::protocol::{Codec, CommandLine, Guard, PaneId, ServerMessage};
use crate::transport::Transport;

/// A completed command's guard-framed output.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandOutput {
    pub guard: Guard,
    /// Each line of output between `%begin` and `%end`, in order.
    pub lines: Vec<Vec<u8>>,
}

type NotificationSink = Box<dyn FnMut(ServerMessage)>;
type PaneOutputSink = Box<dyn FnMut(PaneId, Vec<u8>)>;

/// Correlates commands to replies over a [`Transport`] + [`Codec`] pair.
pub struct Client<T: Transport> {
    transport: T,
    codec: Codec,
    /// Every dispatched message's destination. Not `Option`
    /// (`[LAW:types-are-the-program]`): "a message with nowhere to go" was
    /// the illegal state whose only cover was an unbounded queue per sink.
    notification_sink: NotificationSink,
    pane_output_sink: PaneOutputSink,
    state: ConnectionState,
}

const READ_CHUNK: usize = 8192;

impl<T: Transport> Client<T> {
    /// The one place a fresh `Client`'s fields are named
    /// (`[LAW:one-source-of-truth]`), so a field added later cannot be
    /// initialized in one entry point and forgotten in the other. The
    /// starting state is all the two differ by.
    fn with_state(
        transport: T,
        state: ConnectionState,
        on_notification: impl FnMut(ServerMessage) + 'static,
        on_pane_output: impl FnMut(PaneId, Vec<u8>) + 'static,
    ) -> Self {
        Self {
            transport,
            codec: Codec::new(),
            notification_sink: Box::new(on_notification),
            pane_output_sink: Box::new(on_pane_output),
            state,
        }
    }

    /// Build a client that starts `Ready` immediately, with no greeting
    /// handshake. See the module docs for when this is (and isn't) safe.
    ///
    /// `on_notification` receives every non-pane-output [`ServerMessage`];
    /// `on_pane_output` receives `%output`/`%extended-output` bytes. Pass a
    /// closure that drops its argument to discard a stream — an intent this
    /// crate would otherwise have to infer from a missing registration.
    pub fn new(
        transport: T,
        on_notification: impl FnMut(ServerMessage) + 'static,
        on_pane_output: impl FnMut(PaneId, Vec<u8>) + 'static,
    ) -> Self {
        Self::with_state(
            transport,
            ConnectionState::Ready,
            on_notification,
            on_pane_output,
        )
    }

    /// Build a client against a freshly-attached transport: synchronously
    /// consumes tmux's unsolicited startup greeting block before returning,
    /// so the result is guaranteed `Ready` (or the handshake's own failure
    /// is returned instead of a half-initialized client). This is the
    /// entry point real usage against a live tmux should call.
    ///
    /// The sinks are taken here, rather than registered on the returned
    /// client, because the handshake itself dispatches: anything tmux wrote
    /// behind the greeting terminator reaches them during this call.
    pub fn connect(
        transport: T,
        on_notification: impl FnMut(ServerMessage) + 'static,
        on_pane_output: impl FnMut(PaneId, Vec<u8>) + 'static,
    ) -> Result<Self, TmuxError> {
        let mut client = Self::with_state(
            transport,
            ConnectionState::Connecting,
            on_notification,
            on_pane_output,
        );
        client.consume_greeting()?;
        Ok(client)
    }

    /// Re-attach after a prior transport died: swaps in `transport`, resets
    /// the codec, and re-consumes its greeting as `connect()` does for the
    /// first connection — but from `Reconnecting { attempt }`, which is the
    /// greeting-consuming phase of a reconnect rather than a step before
    /// one (DESIGN.md §3.3). Passing back through `Connecting` would
    /// overwrite the attempt count to say less than `Reconnecting` already
    /// does, and say it where nothing can read it: this call is
    /// synchronous, and [`Client::execute`] gates on `Ready` alone, so both
    /// states refuse correlation alike.
    ///
    /// `attempt` is the caller's own retry counter — this crate owns the
    /// reconnect mechanics, not when or how often to retry.
    pub fn reconnect(&mut self, transport: T, attempt: u32) -> Result<(), TmuxError> {
        self.transport = transport;
        self.codec = Codec::new();
        self.state = ConnectionState::Reconnecting { attempt };
        self.consume_greeting()
    }

    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// Every transport touch goes through these two
    /// (`[LAW:single-enforcer]`), which is what makes "a dead transport
    /// ends the connection" true of the client rather than true of
    /// whichever call sites remembered to say so. A caller asking
    /// [`Client::state`] after any failure gets the same answer no matter
    /// which operation failed.
    fn send_or_close(&mut self, command: &CommandLine) -> Result<(), TmuxError> {
        match self.transport.send(command) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.state = self.state.closed(CloseReason::TransportError);
                Err(TmuxError::Send(err))
            }
        }
    }

    /// A clean EOF and a failed read are different endings — `Exit` versus
    /// `TransportError` — so the distinction is carried, not collapsed.
    fn read_or_close(&mut self, buf: &mut [u8]) -> Result<usize, TmuxError> {
        match self.transport.read(buf) {
            Ok(0) => {
                self.state = self.state.closed(CloseReason::Exit);
                Err(TmuxError::TransportClosed)
            }
            Ok(n) => Ok(n),
            Err(err) => {
                self.state = self.state.closed(CloseReason::TransportError);
                Err(TmuxError::Read(err))
            }
        }
    }

    /// Block, reading and feeding the codec, until the first guard block
    /// (the unsolicited greeting) settles — on `%end`, `%error`, *or* a
    /// malformed terminator alike: none of those are something a caller
    /// could retry or observe (there is no pending command to fail), so
    /// each equally just closes the phase and moves to `Ready` (mirrors the
    /// reference client's `awaitingGreeting` handling). Only a transport
    /// failure before the terminator arrives is a genuine handshake error.
    fn consume_greeting(&mut self) -> Result<(), TmuxError> {
        let mut buf = [0u8; READ_CHUNK];
        let mut settled = false;

        while !settled {
            let n = self.read_or_close(&mut buf)?;
            // Same reasoning as execute()'s identically-shaped loop: one
            // read() can return a chunk whose codec.feed() batch contains
            // both the greeting's terminator *and* trailing bytes after it
            // (a notification tmux wrote right behind it). Returning the
            // instant the terminator is seen would drop the rest of that
            // batch, so draining continues — dispatching anything past the
            // terminator — until the whole batch is consumed.
            for msg in self.codec.feed(&buf[..n]) {
                if settled {
                    self.dispatch(msg);
                    continue;
                }
                match msg {
                    ServerMessage::GuardBegin(_) | ServerMessage::CommandOutput { .. } => {
                        // Framing and body of the greeting block — no
                        // caller is waiting on it, so its output (if any) is
                        // intentionally dropped rather than dispatched as a
                        // notification the caller would have to recognize
                        // and ignore.
                    }
                    ServerMessage::GuardEnd(_)
                    | ServerMessage::GuardError(_)
                    | ServerMessage::ProtocolError { .. } => {
                        self.state = ConnectionState::Ready;
                        settled = true;
                    }
                    other => self.dispatch(other),
                }
            }
        }

        Ok(())
    }

    /// Route a parsed message to its sink (`[LAW:single-enforcer]` — the one
    /// place that decides notification vs. pane-output). The only branch left
    /// is the domain's own discriminator: with both sinks required, "is there
    /// somewhere to put this" is no longer a question the code can ask
    /// (`[LAW:dataflow-not-control-flow]`). Only ever called with messages
    /// that are neither guard framing nor command output — those are handled
    /// by their own callers before reaching here.
    fn dispatch(&mut self, msg: ServerMessage) {
        match msg {
            ServerMessage::Output { pane, data }
            | ServerMessage::ExtendedOutput { pane, data, .. } => {
                (self.pane_output_sink)(pane, data)
            }
            other => (self.notification_sink)(other),
        }
    }

    /// The single command-dispatch path (`[LAW:single-enforcer]`). Sends
    /// `command`, then blocks reading from the transport — feeding every
    /// chunk to the codec — until the guard block that positionally
    /// follows (the codec only ever has one block open at a time) settles
    /// with `%end` or `%error`.
    ///
    /// Refuses with `TmuxError::NotReady` unless [`Client::state`] is
    /// `Ready` — a command sent while still `Connecting` would correlate
    /// against the unsolicited greeting instead of a real reply.
    ///
    /// A `%error` reply is `Err(TmuxError::Command)`, not
    /// `Ok(CommandOutput { .. })` with a failure flag — SPEC §5.3's parse
    /// errors and other command failures are error conditions, not data.
    pub fn execute(&mut self, command: &CommandLine) -> Result<CommandOutput, TmuxError> {
        if self.state != ConnectionState::Ready {
            return Err(TmuxError::NotReady(self.state));
        }

        self.send_or_close(command)?;

        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut buf = [0u8; READ_CHUNK];
        let mut outcome = None;

        while outcome.is_none() {
            let n = self.read_or_close(&mut buf)?;
            // A single read() can return a chunk containing our reply's
            // GuardEnd/GuardError *and* trailing bytes after it (tmux wrote
            // them in one burst, e.g. a notification right behind the
            // block). codec.feed() hands back every message in that chunk
            // as one Vec — stopping at the first settling message here
            // would silently drop everything after it in the same batch, so
            // once `outcome` is set the loop keeps draining, dispatching
            // anything further instead of returning early.
            for msg in self.codec.feed(&buf[..n]) {
                if outcome.is_some() {
                    self.dispatch(msg);
                    continue;
                }
                match msg {
                    // Framing only — GuardEnd/GuardError below carry their
                    // own complete Guard, so nothing needs to be remembered
                    // from GuardBegin.
                    ServerMessage::GuardBegin(_) => {}
                    ServerMessage::CommandOutput { line, .. } => lines.push(line),
                    ServerMessage::GuardEnd(guard) => {
                        outcome = Some(Ok(CommandOutput {
                            guard,
                            lines: std::mem::take(&mut lines),
                        }));
                    }
                    ServerMessage::GuardError(guard) => {
                        outcome = Some(Err(TmuxError::Command {
                            guard,
                            lines: std::mem::take(&mut lines),
                        }));
                    }
                    ServerMessage::ProtocolError {
                        command_number,
                        line,
                    } => {
                        outcome = Some(Err(TmuxError::Protocol {
                            command_number,
                            line,
                        }));
                    }
                    other => self.dispatch(other),
                }
            }
        }

        outcome.expect("loop only exits once outcome is Some")
    }

    /// The wire-level detach signal: a bare `\n` (SPEC §4.1). Deliberately
    /// bypasses `execute()` — an empty line carries no guard block, so
    /// there is nothing to correlate (IMPL.md §2.4). Not gated on
    /// `ConnectionState`: a caller may reasonably try to detach from any
    /// state, best-effort.
    pub fn detach(&mut self) -> Result<(), TmuxError> {
        self.send_or_close(&CommandLine::detach())
    }

    /// Local-side teardown: drops the transport, sends nothing to tmux
    /// (IMPL.md §2.4 — distinct from [`Client::detach`]).
    pub fn close(&mut self) {
        self.transport.close();
        self.state = self.state.closed(CloseReason::Disposed);
    }
}
