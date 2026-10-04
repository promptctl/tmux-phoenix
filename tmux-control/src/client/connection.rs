//! The connection with an owned reader (ARCHITECTURE.md §4): a thread owns
//! the transport's read side and delivers [`Event`]s on a channel, while
//! [`Connection::execute`] correlates replies off the same stream. The one
//! owner of "when does a notification arrive" is that reader
//! (`[LAW:no-ambient-temporal-coupling]`): a caller that wants to react while
//! idle waits on the channel, with or without a deadline, and no heartbeat
//! command exists anywhere.
//!
//! [`Connection::open`] is also the one place a `tmux` process is started in
//! order to reach a server. `attach-session` is always tried first; when it
//! reports no sessions — which is how tmux presents both "no server on this
//! socket" and "a server holding none" — the caller's [`Attach`] says whether
//! that is the answer or the cue to run `new-session -s <name>`, which opens
//! control mode and creates the session in one step (verified live, tmux
//! 3.7b). The failed attach *is* the signal: there is no prior "does this
//! server have sessions?" probe to race against, and no plain `tmux` is run.

use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use super::connection_state::{CloseReason, ConnectionState};
use super::demux::{Demux, Routed};
use super::error::TmuxError;
use super::event::Event;
use super::{CommandOutput, Execute, READ_CHUNK};
use crate::commands::SessionName;
use crate::protocol::{Codec, CommandLine};
use crate::transport::{spawn_halves, SpawnOptions};

/// What the caller permits [`Connection::open`] to do when `attach-session`
/// finds no session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attach {
    /// Fail with [`TmuxError::NoSessions`]. For a caller whose decision to
    /// start a server is still open — the daemon's idle loop.
    Existing,
    /// Run `new-session -s <name>`, creating the session and attaching in one
    /// step. For a caller that has already decided — a restore.
    OrCreate { name: SessionName },
}

/// What [`Connection::open`] did. `Created` carries the name so that the one
/// session phoenix made in order to attach travels as a value to whoever
/// will remove it, rather than being guessed at later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    Attached,
    Created(SessionName),
}

/// What [`Connection::wait`] returns.
#[derive(Debug, Clone, PartialEq)]
pub enum Wake {
    Event(Event),
    /// The deadline passed with nothing delivered.
    Deadline,
}

/// A settled block, or the reader's report that no more will come. The
/// terminal item is sent on this channel as well as [`Event::Closed`] on the
/// event channel, so whichever call is blocked learns the ending.
enum Reply {
    Settled(Result<CommandOutput, TmuxError>),
    Ended(ReadEnd),
}

/// How a read side ended. The two endings differ for every caller — a clean
/// EOF is tmux's own `%exit` or exit, a failed read is a broken transport —
/// so the distinction is carried, and the pairing of each with its
/// [`CloseReason`] and its [`TmuxError`] is written once
/// (`[LAW:one-source-of-truth]`).
pub(crate) enum ReadEnd {
    Eof,
    Failed(io::Error),
}

impl ReadEnd {
    pub(crate) fn reason(&self) -> CloseReason {
        match self {
            ReadEnd::Eof => CloseReason::Exit,
            ReadEnd::Failed(_) => CloseReason::TransportError,
        }
    }

    pub(crate) fn error(self) -> TmuxError {
        match self {
            ReadEnd::Eof => TmuxError::TransportClosed,
            ReadEnd::Failed(err) => TmuxError::Read(err),
        }
    }
}

/// The two halves while the connection is up. Dropped as a pair by
/// [`Connection::close`], so "closed, but still holding a writer" cannot be
/// written down (`[LAW:types-are-the-program]`).
enum Side {
    Open {
        commands: Box<dyn Write + Send>,
        reader: JoinHandle<()>,
    },
    Closed,
}

/// A control-mode connection whose reader runs on its own thread.
///
/// Only ever observed `Ready` or `Closed`: [`Connection::open`] and
/// [`Connection::over`] consume tmux's unsolicited greeting block before
/// returning, so the off-by-one that correlates a caller's first command
/// against the greeting has no window to occur in.
pub struct Connection {
    side: Side,
    replies: Receiver<Reply>,
    events: Receiver<Event>,
    state: ConnectionState,
}

/// The body of tmux's `%error` greeting when `attach-session` has nothing to
/// attach to. Matched exactly, so a differently worded failure surfaces as
/// the [`TmuxError::Command`] it is rather than being taken for this one
/// (`[LAW:no-silent-failure]`).
const NO_SESSIONS: &[u8] = b"no sessions";

impl Connection {
    /// Open a connection to the server `options` selects: `attach-session`,
    /// then — on exactly the no-sessions failure, and only if `attach`
    /// permits — `new-session -s <name>`. See the module docs.
    pub fn open(options: &SpawnOptions, attach: Attach) -> Result<(Self, Opened), TmuxError> {
        match Self::spawn(&["attach-session"], options) {
            Err(TmuxError::NoSessions) => match attach {
                Attach::Existing => Err(TmuxError::NoSessions),
                Attach::OrCreate { name } => {
                    let connection = Self::spawn(&["new-session", "-s", name.as_str()], options)?;
                    Ok((connection, Opened::Created(name)))
                }
            },
            other => other.map(|connection| (connection, Opened::Attached)),
        }
    }

    fn spawn(args: &[&str], options: &SpawnOptions) -> Result<Self, TmuxError> {
        let (commands, output) = spawn_halves(args, options).map_err(TmuxError::Spawn)?;
        Self::over(commands, output)
    }

    /// Build a connection over an already-open byte link, consuming the
    /// greeting block. This is how a test stands in for tmux; real usage
    /// goes through [`Connection::open`].
    ///
    /// Contract on the halves: dropping `commands` must end `output` — the
    /// reader thread blocks in `read()` and nothing else can return it. The
    /// spawned child honors it by dying; a scripted stand-in honors it by
    /// delivering EOF.
    pub fn over(
        commands: impl Write + Send + 'static,
        output: impl Read + Send + 'static,
    ) -> Result<Self, TmuxError> {
        let (reply_tx, replies) = mpsc::channel();
        let (event_tx, events) = mpsc::channel();
        let reader = thread::spawn(move || pump(Box::new(output), reply_tx, event_tx));
        let mut connection = Self {
            side: Side::Open {
                commands: Box::new(commands),
                reader,
            },
            replies,
            events,
            state: ConnectionState::Ready,
        };
        connection.greeting()?;
        Ok(connection)
    }

    /// The first block on a fresh transport is tmux's reply to the command
    /// it was started with. `%end` means attached (or created); `%error`
    /// means that command failed and tmux is exiting, which is an error to
    /// report rather than a greeting to skip past.
    fn greeting(&mut self) -> Result<(), TmuxError> {
        match self.next_reply() {
            Ok(_) => Ok(()),
            Err(TmuxError::Command { lines, .. }) if lines.as_slice() == [NO_SESSIONS] => {
                Err(TmuxError::NoSessions)
            }
            Err(err) => Err(err),
        }
    }

    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// The single command-dispatch path (`[LAW:single-enforcer]`): send
    /// `command`, then block until the reply that positionally follows
    /// settles. Refuses with [`TmuxError::NotReady`] once the connection has
    /// closed. A `%error` reply is `Err(TmuxError::Command)`.
    pub fn execute(&mut self, command: &CommandLine) -> Result<CommandOutput, TmuxError> {
        if self.state != ConnectionState::Ready {
            return Err(TmuxError::NotReady(self.state));
        }
        self.send(command)?;
        self.next_reply()
    }

    fn send(&mut self, command: &CommandLine) -> Result<(), TmuxError> {
        let Side::Open { commands, .. } = &mut self.side else {
            return Err(TmuxError::NotReady(self.state));
        };
        commands
            .write_all(command.wire())
            .and_then(|()| commands.flush())
            .map_err(|err| {
                self.state = self.state.closed(CloseReason::TransportError);
                TmuxError::Send(err)
            })
    }

    /// The oldest reply not yet taken, waiting for the reader if none has
    /// settled. A reader that ended closes the connection with its reason
    /// and returns its error, so [`Connection::state`] says so afterwards
    /// no matter which call observed the ending.
    fn next_reply(&mut self) -> Result<CommandOutput, TmuxError> {
        match self.replies.recv() {
            Ok(Reply::Settled(reply)) => reply,
            Ok(Reply::Ended(end)) => {
                self.state = self.state.closed(end.reason());
                Err(end.error())
            }
            Err(mpsc::RecvError) => Err(self.reader_vanished()),
        }
    }

    /// Wait for the next event, up to `deadline`. `Closed` closes the
    /// connection as it is delivered, so the caller sees it exactly once and
    /// every later call refuses with [`TmuxError::NotReady`].
    pub fn wait(&mut self, deadline: Option<Instant>) -> Result<Wake, TmuxError> {
        if self.state != ConnectionState::Ready {
            return Err(TmuxError::NotReady(self.state));
        }
        let received = match deadline {
            None => self
                .events
                .recv()
                .map_err(|_| RecvTimeoutError::Disconnected),
            Some(deadline) => self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now())),
        };
        match received {
            Ok(Event::Closed(reason)) => {
                self.state = self.state.closed(reason);
                Ok(Wake::Event(Event::Closed(reason)))
            }
            Ok(event) => Ok(Wake::Event(event)),
            Err(RecvTimeoutError::Timeout) => Ok(Wake::Deadline),
            Err(RecvTimeoutError::Disconnected) => Err(self.reader_vanished()),
        }
    }

    /// The reader always reports its ending before it exits, so a channel
    /// that disconnected without one means the thread died — a panic — and
    /// the read side is gone with it.
    fn reader_vanished(&mut self) -> TmuxError {
        self.state = self.state.closed(CloseReason::TransportError);
        TmuxError::TransportClosed
    }

    /// Local-side teardown: drops the command writer — which ends the read
    /// side, see [`Connection::over`] — and joins the reader. Idempotent.
    pub fn close(&mut self) {
        if let Side::Open { commands, reader } = std::mem::replace(&mut self.side, Side::Closed) {
            drop(commands);
            // A panic on the reader thread has already been raised louder
            // than here, and the thread is gone either way.
            let _ = reader.join();
        }
        self.state = self.state.closed(CloseReason::Disposed);
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}

/// The lifecycle state is the one thing about a connection worth printing;
/// the halves and channels are plumbing.
impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Execute for Connection {
    fn execute(&mut self, command: &CommandLine) -> Result<CommandOutput, TmuxError> {
        Connection::execute(self, command)
    }
}

/// The reader thread: read, decode, route, until the read side ends; then
/// report the ending on both channels and stop. A send that fails means the
/// [`Connection`] is gone, and with it anyone to report to.
fn pump(mut output: Box<dyn Read + Send>, replies: Sender<Reply>, events: Sender<Event>) {
    let mut codec = Codec::new();
    let mut demux = Demux::default();
    let mut buf = [0u8; READ_CHUNK];
    let end = loop {
        let n = match output.read(&mut buf) {
            Ok(0) => break ReadEnd::Eof,
            Ok(n) => n,
            Err(err) => break ReadEnd::Failed(err),
        };
        for msg in codec.feed(&buf[..n]) {
            let delivered = match demux.route(msg) {
                None => Ok(()),
                Some(Routed::Reply(reply)) => replies.send(Reply::Settled(reply)).map_err(drop),
                Some(Routed::Notification(msg)) => {
                    events.send(Event::Notification(msg)).map_err(drop)
                }
                Some(Routed::PaneOutput(pane, data)) => {
                    events.send(Event::PaneOutput(pane, data)).map_err(drop)
                }
            };
            if delivered.is_err() {
                return;
            }
        }
    };
    let _ = events.send(Event::Closed(end.reason()));
    let _ = replies.send(Reply::Ended(end));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A daemon moves its connection between threads; the type has to allow
    /// it, and this fails to compile the day a field stops being `Send`.
    #[test]
    fn a_connection_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Connection>();
    }
}
