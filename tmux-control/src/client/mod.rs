//! The correlation layer (DESIGN.md §3.3). Two drivers share one
//! demultiplexer ([`demux`]): [`Connection`], whose reader thread owns the
//! transport's read side and delivers events on a channel, and [`Client`],
//! which reads only from inside a blocking call. Both expose the same
//! `execute` through the [`Execute`] seam, so every typed command in
//! [`crate::commands`] is written once.
//!
//! [`ConnectionState`] is owned and explicit (DESIGN.md §3.3, "no timing
//! folklore"): on attach tmux emits an unsolicited `%begin…%end`/`%error`
//! greeting block that is not a reply to any command. Correlating a
//! caller's first command against it is a classic off-by-one that corrupts
//! every subsequent reply, so it must be consumed during `Connecting`
//! before `execute()` will run at all.
//!
//! Two ways to build a `Client`, for two different needs:
//! - [`Client::connect`] performs the greeting handshake synchronously and
//!   only returns a `Ready` client (or an error if the handshake itself
//!   failed).
//! - [`Client::new`] skips the handshake and starts `Ready` immediately. It
//!   exists for tests and any other case where the byte stream is already
//!   known to be positioned past a greeting. Using it against a real,
//!   freshly-attached tmux transport reintroduces the exact off-by-one this
//!   module exists to prevent — `connect()` is the safe default.
//!
//! A `Client`'s notifications dispatch as typed [`ServerMessage`] events to
//! the notification sink, and pane output (`%output`/`%extended-output`) to a
//! separate byte sink. Both sinks are supplied at construction and neither
//! is optional, so a parsed message with nowhere to go is not a state this
//! client can be in; [`Client::connect`] dispatches whatever tmux wrote
//! behind the greeting terminator, which is strictly before any caller
//! could have registered a sink afterwards.
//!
//! A sink only fires while some blocking call (`execute`/`connect`/
//! `reconnect`) is actively reading — a `Client` has no thread of its own.
//! A caller that wants to react to notifications while otherwise idle wants
//! a [`Connection`], whose reader is exactly that thread.

mod connection;
mod connection_state;
mod demux;
mod error;
mod event;

pub use connection::{Abort, Attach, Connection, EventSink, Opened};
pub use connection_state::{CloseReason, ConnectionState};
pub use error::TmuxError;
pub use event::Event;

use std::collections::VecDeque;

use crate::protocol::{Codec, CommandLine, Guard, PaneId, ServerMessage};
use crate::transport::Transport;
use connection::ReadEnd;
use demux::{Demux, Routed};

/// A completed command's guard-framed output.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandOutput {
    pub guard: Guard,
    /// Each line of output between `%begin` and `%end`, in order.
    pub lines: Vec<Vec<u8>>,
}

/// The seam every typed command is written against (`[LAW:locality-or-seam]`):
/// anything that can send one command line and return the block that
/// answers it.
pub trait Execute {
    fn execute(&mut self, command: &CommandLine) -> Result<CommandOutput, TmuxError>;
}

type NotificationSink = Box<dyn FnMut(ServerMessage)>;
type PaneOutputSink = Box<dyn FnMut(PaneId, Vec<u8>)>;

/// Correlates commands to replies over a [`Transport`] + [`Codec`] pair,
/// reading from inside each blocking call.
pub struct Client<T: Transport> {
    transport: T,
    codec: Codec,
    demux: Demux,
    /// Settled blocks not yet handed to a caller, oldest first. tmux answers
    /// one block per command, so this holds at most the reply of the one
    /// command in flight.
    replies: VecDeque<Result<CommandOutput, TmuxError>>,
    /// Every dispatched message's destination. Not `Option`
    /// (`[LAW:types-are-the-program]`): "a message with nowhere to go" was
    /// the illegal state whose only cover was an unbounded queue per sink.
    notification_sink: NotificationSink,
    pane_output_sink: PaneOutputSink,
    state: ConnectionState,
}

pub(crate) const READ_CHUNK: usize = 8192;

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
            demux: Demux::default(),
            replies: VecDeque::new(),
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
    /// is returned instead of a half-initialized client).
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
        self.demux = Demux::default();
        self.replies.clear();
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

    fn read_or_close(&mut self, buf: &mut [u8]) -> Result<usize, TmuxError> {
        let end = match self.transport.read(buf) {
            Ok(0) => ReadEnd::Eof,
            Ok(n) => return Ok(n),
            Err(err) => ReadEnd::Failed(err),
        };
        self.state = self.state.closed(end.reason());
        Err(end.error())
    }

    /// Block, reading and feeding the codec, until the first guard block
    /// (the unsolicited greeting) settles — on `%end`, `%error`, *or* a
    /// malformed terminator alike: none of those are something a caller
    /// could retry or observe (there is no pending command to fail), so
    /// each equally just closes the phase and moves to `Ready` (mirrors the
    /// reference client's `awaitingGreeting` handling). Only a transport
    /// failure before the terminator arrives is a genuine handshake error.
    fn consume_greeting(&mut self) -> Result<(), TmuxError> {
        let _settled = self.next_reply()?;
        self.state = ConnectionState::Ready;
        Ok(())
    }

    /// The oldest settled block, reading until one has. Everything that is
    /// not a block — notifications, pane output — is dispatched to its sink
    /// on the way, including anything that arrived in the same read as the
    /// block's terminator: the codec hands back every message in a chunk,
    /// and stopping at the terminator would drop the rest for good.
    ///
    /// The outer `Err` is the transport failing; the inner `Result` is the
    /// block's own outcome.
    fn next_reply(&mut self) -> Result<Result<CommandOutput, TmuxError>, TmuxError> {
        let mut buf = [0u8; READ_CHUNK];
        loop {
            if let Some(reply) = self.replies.pop_front() {
                return Ok(reply);
            }
            let n = self.read_or_close(&mut buf)?;
            for msg in self.codec.feed(&buf[..n]) {
                match self.demux.route(msg) {
                    None => {}
                    Some(Routed::Reply(reply)) => self.replies.push_back(reply),
                    Some(Routed::Notification(msg)) => (self.notification_sink)(msg),
                    Some(Routed::PaneOutput(pane, data)) => (self.pane_output_sink)(pane, data),
                }
            }
        }
    }

    /// The single command-dispatch path (`[LAW:single-enforcer]`). Sends
    /// `command`, then blocks reading from the transport until the guard
    /// block that positionally follows settles with `%end` or `%error`.
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
        // A block settled while nothing was in flight is tmux breaking the
        // one-block-per-command rule; reported before a command is written
        // against it, as `Connection::idle` does (`[LAW:no-silent-failure]`).
        if let Some(stray) = self.replies.pop_front() {
            self.state = self.state.closed(CloseReason::Protocol);
            return Err(TmuxError::UnsolicitedReply(Box::new(stray)));
        }
        self.send_or_close(command)?;
        self.next_reply()?
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

impl<T: Transport> Execute for Client<T> {
    fn execute(&mut self, command: &CommandLine) -> Result<CommandOutput, TmuxError> {
        Client::execute(self, command)
    }
}
