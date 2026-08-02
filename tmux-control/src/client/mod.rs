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
//! Notifications dispatch as typed [`ServerMessage`] events to a registered
//! subscriber. Pane output (`%output`/`%extended-output`) never goes
//! through that path — it is high-volume and mixing it into the
//! notification stream is how you get head-of-line blocking — it routes
//! through a separate byte sink instead ([`Client::on_pane_output`]). If no
//! sink is registered for either path, messages are buffered rather than
//! dropped ([`Client::drain_notifications`], [`Client::drain_pane_output`]):
//! nothing is silently lost just because a caller hasn't wired up a
//! subscriber yet.
//!
//! A registered sink only fires while some blocking call
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

use crate::protocol::{Codec, Guard, PaneId, ServerMessage};
use crate::transport::Transport;
use std::collections::VecDeque;

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
    /// Non-pane-output `ServerMessage`s with no registered
    /// [`Client::on_notification`] sink — buffered, not dropped.
    notifications: VecDeque<ServerMessage>,
    /// `%output`/`%extended-output` bytes with no registered
    /// [`Client::on_pane_output`] sink — buffered, not dropped.
    pane_output: VecDeque<(PaneId, Vec<u8>)>,
    notification_sink: Option<NotificationSink>,
    pane_output_sink: Option<PaneOutputSink>,
    state: ConnectionState,
}

const READ_CHUNK: usize = 8192;

impl<T: Transport> Client<T> {
    /// Build a client that starts `Ready` immediately, with no greeting
    /// handshake. See the module docs for when this is (and isn't) safe.
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            codec: Codec::new(),
            notifications: VecDeque::new(),
            pane_output: VecDeque::new(),
            notification_sink: None,
            pane_output_sink: None,
            state: ConnectionState::Ready,
        }
    }

    /// Build a client against a freshly-attached transport: synchronously
    /// consumes tmux's unsolicited startup greeting block before returning,
    /// so the result is guaranteed `Ready` (or the handshake's own failure
    /// is returned instead of a half-initialized client). This is the
    /// entry point real usage against a live tmux should call.
    pub fn connect(transport: T) -> Result<Self, TmuxError> {
        let mut client = Self {
            transport,
            codec: Codec::new(),
            notifications: VecDeque::new(),
            pane_output: VecDeque::new(),
            notification_sink: None,
            pane_output_sink: None,
            state: ConnectionState::Connecting,
        };
        client.consume_greeting()?;
        Ok(client)
    }

    /// Re-attach after a prior transport died: swaps in `transport`, resets
    /// the codec, and re-consumes its greeting exactly as `connect()` does
    /// for the first connection (DESIGN.md §3.3: "the client re-enters
    /// Connecting and re-consumes the greeting on reconnect"). `attempt` is
    /// the caller's own retry counter — this crate owns the reconnect
    /// mechanics, not when or how often to retry.
    pub fn reconnect(&mut self, transport: T, attempt: u32) -> Result<(), TmuxError> {
        self.transport = transport;
        self.codec = Codec::new();
        self.state = ConnectionState::Reconnecting { attempt };
        self.consume_greeting()
    }

    pub fn state(&self) -> ConnectionState {
        self.state
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
            let n = match self.transport.read(&mut buf) {
                Ok(n) => n,
                Err(err) => {
                    self.state = ConnectionState::Closed {
                        reason: CloseReason::TransportError,
                    };
                    return Err(TmuxError::Read(err));
                }
            };
            if n == 0 {
                self.state = ConnectionState::Closed {
                    reason: CloseReason::Exit,
                };
                return Err(TmuxError::TransportClosed);
            }
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
                        // caller is waiting on it, so its output (if any)
                        // is intentionally discarded, not buffered.
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

    /// Route a parsed message to its sink (`[LAW:single-enforcer]` — the
    /// one place that decides notification vs. pane-output vs. buffered
    /// fallback). Only ever called with messages that are neither guard
    /// framing nor command output — those are handled by their own callers
    /// before reaching here.
    fn dispatch(&mut self, msg: ServerMessage) {
        match msg {
            ServerMessage::Output { pane, data }
            | ServerMessage::ExtendedOutput { pane, data, .. } => {
                match &mut self.pane_output_sink {
                    Some(sink) => sink(pane, data),
                    None => self.pane_output.push_back((pane, data)),
                }
            }
            other => match &mut self.notification_sink {
                Some(sink) => sink(other),
                None => self.notifications.push_back(other),
            },
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
    pub fn execute(&mut self, command: &str) -> Result<CommandOutput, TmuxError> {
        if self.state != ConnectionState::Ready {
            return Err(TmuxError::NotReady(self.state));
        }

        if let Err(err) = self.transport.send(command) {
            self.state = ConnectionState::Closed {
                reason: CloseReason::TransportError,
            };
            return Err(TmuxError::Send(err));
        }

        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut buf = [0u8; READ_CHUNK];
        let mut outcome = None;

        while outcome.is_none() {
            let n = match self.transport.read(&mut buf) {
                Ok(n) => n,
                Err(err) => {
                    self.state = ConnectionState::Closed {
                        reason: CloseReason::TransportError,
                    };
                    return Err(TmuxError::Read(err));
                }
            };
            if n == 0 {
                self.state = ConnectionState::Closed {
                    reason: CloseReason::Exit,
                };
                return Err(TmuxError::TransportClosed);
            }
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

    /// Register the sink every non-pane-output `ServerMessage` dispatches
    /// to from now on, replacing any previously registered sink. Messages
    /// already buffered before this call stay buffered — call
    /// [`Client::drain_notifications`] first if you want them too.
    pub fn on_notification(&mut self, sink: impl FnMut(ServerMessage) + 'static) {
        self.notification_sink = Some(Box::new(sink));
    }

    /// Register the sink `%output`/`%extended-output` bytes dispatch to
    /// from now on, replacing any previously registered sink. High-volume
    /// and kept off the notification path entirely (module docs) so it
    /// never head-of-line-blocks other notifications.
    pub fn on_pane_output(&mut self, sink: impl FnMut(PaneId, Vec<u8>) + 'static) {
        self.pane_output_sink = Some(Box::new(sink));
    }

    /// Drain every non-pane-output `ServerMessage` that arrived with no
    /// [`Client::on_notification`] sink registered at the time.
    pub fn drain_notifications(&mut self) -> Vec<ServerMessage> {
        self.notifications.drain(..).collect()
    }

    /// Drain every `%output`/`%extended-output` chunk that arrived with no
    /// [`Client::on_pane_output`] sink registered at the time.
    pub fn drain_pane_output(&mut self) -> Vec<(PaneId, Vec<u8>)> {
        self.pane_output.drain(..).collect()
    }

    /// The wire-level detach signal: a bare `\n` (SPEC §4.1). Deliberately
    /// bypasses `execute()` — an empty line carries no guard block, so
    /// there is nothing to correlate (IMPL.md §2.4). Not gated on
    /// `ConnectionState`: a caller may reasonably try to detach from any
    /// state, best-effort.
    pub fn detach(&mut self) -> Result<(), TmuxError> {
        self.transport.send("").map_err(TmuxError::Send)
    }

    /// Local-side teardown: drops the transport, sends nothing to tmux
    /// (IMPL.md §2.4 — distinct from [`Client::detach`]).
    pub fn close(&mut self) {
        self.transport.close();
        self.state = ConnectionState::Closed {
            reason: CloseReason::Disposed,
        };
    }
}
