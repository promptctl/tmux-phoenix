//! The connection with an owned reader (ARCHITECTURE.md §4): a thread owns
//! the transport's read side and delivers [`Event`]s to the caller's sink,
//! while [`Connection::execute`] correlates replies off the same stream. The
//! one owner of "when does a notification arrive" is that reader
//! (`[LAW:no-ambient-temporal-coupling]`): a caller that wants to react while
//! idle gives a sink that forwards to whatever it waits on, and no heartbeat
//! command exists anywhere.
//!
//! The sink is the caller's, not a queue of this crate's: a caller that only
//! ever executes passes a sink that drops, and nothing accumulates on its
//! behalf for the life of the connection — the same shape as
//! [`super::Client`]'s sinks, for the same reason: "events with nowhere to
//! go" is not a state, so there is no buffer to cover it. The sink runs on
//! the reader thread, ahead of any reply that follows in the same read.
//!
//! [`Connection::open`] is also the one place a `tmux` process is started in
//! order to reach a server. `attach-session` is always tried first; when it
//! reports no sessions — which is how tmux presents both "no server on this
//! socket" and "a server holding none" — the caller's [`Attach`] says whether
//! that is the answer or the cue to run `new-session -s <name>`, which opens
//! control mode and creates the session in one step (verified live, tmux
//! 3.7b). The failed attach *is* the signal: there is no prior "does this
//! server have sessions?" probe to race against, and no plain `tmux` is run.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};

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

/// Where the reader delivers everything that is not a reply.
pub type EventSink = Box<dyn FnMut(Event) + Send>;

/// A block's outcome: its output, or the `%error`/malformed-terminator it
/// settled with.
type Settled = Result<CommandOutput, TmuxError>;

/// A settled block, or the reader's report that no more will come. The
/// terminal item is sent here as well as delivered as [`Event::Closed`] to
/// the sink, so both the caller blocked in a call and the caller watching
/// its sink learn the ending.
enum Reply {
    Settled(Settled),
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
/// Only ever observed `Ready` or `Closed`: tmux's unsolicited greeting block
/// is consumed before a `Connection` exists at all (see [`Link::greet`]), so
/// the off-by-one that correlates a caller's first command against the
/// greeting has no window to occur in.
pub struct Connection {
    side: Side,
    replies: Receiver<Reply>,
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
    ///
    /// `events` receives everything the server says that is not a reply, on
    /// the reader thread, from the moment the connection exists; a failed
    /// attempt never reaches it. Pass `drop` to discard events, which is an
    /// intent this crate would otherwise have to infer.
    pub fn open(
        options: &SpawnOptions,
        attach: Attach,
        events: impl FnMut(Event) + Send + 'static,
    ) -> Result<(Self, Opened), TmuxError> {
        let (commands, link, opened) = match Self::spawn(&["attach-session"], options) {
            Ok((commands, link)) => (commands, link, Opened::Attached),
            Err(TmuxError::NoSessions) => match attach {
                Attach::Existing => return Err(TmuxError::NoSessions),
                Attach::OrCreate { name } => {
                    let (commands, link) =
                        Self::spawn(&["new-session", "-s", name.as_str()], options)?;
                    (commands, link, Opened::Created(name))
                }
            },
            Err(err) => return Err(err),
        };
        Ok((Self::start(commands, link, Box::new(events)), opened))
    }

    /// Spawn and read the greeting. A failure here drops the writer, which
    /// ends the child (see [`Connection::over`]).
    fn spawn(
        args: &[&str],
        options: &SpawnOptions,
    ) -> Result<(impl Write + Send + 'static, Link), TmuxError> {
        let (commands, output) = spawn_halves(args, options).map_err(TmuxError::Spawn)?;
        let link = Link::greet(Box::new(output))?;
        Ok((commands, link))
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
        events: impl FnMut(Event) + Send + 'static,
    ) -> Result<Self, TmuxError> {
        let link = Link::greet(Box::new(output))?;
        Ok(Self::start(commands, link, Box::new(events)))
    }

    /// Hand a greeted link its sink and its thread. Whatever arrived behind
    /// the greeting terminator is delivered first — events to the sink on
    /// this thread, a further settled block to the reply channel, where
    /// [`Connection::idle`] will find it for the protocol violation it is.
    fn start(commands: impl Write + Send + 'static, link: Link, mut events: EventSink) -> Self {
        let (reply_tx, replies) = mpsc::channel();
        let Link {
            output,
            codec,
            demux,
            behind_greeting,
        } = link;
        for item in behind_greeting {
            match item {
                Behind::Event(event) => events(event),
                Behind::Settled(settled) => reply_tx
                    .send(Reply::Settled(settled))
                    .expect("the receiver is held right here"),
            }
        }
        let reader = thread::spawn(move || pump(output, codec, demux, reply_tx, events));
        Self {
            side: Side::Open {
                commands: Box::new(commands),
                reader,
            },
            replies,
            state: ConnectionState::Ready,
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
        self.idle()?;
        self.send(command)?;
        self.next_reply()
    }

    /// Nothing may be waiting on the reply channel while no command is in
    /// flight: a settled block there is tmux breaking the one-block-per-
    /// command rule, after which positional correlation is lost, and an
    /// ending there is the reader having already reported a close that no
    /// call had yet observed. Both are found here, before a command is
    /// written against them (`[LAW:no-silent-failure]`).
    fn idle(&mut self) -> Result<(), TmuxError> {
        match self.replies.try_recv() {
            Err(TryRecvError::Empty) => Ok(()),
            Ok(Reply::Settled(reply)) => Err(TmuxError::UnsolicitedReply(Box::new(reply))),
            Ok(Reply::Ended(end)) => Err(self.ended(end)),
            Err(TryRecvError::Disconnected) => Err(self.reader_vanished()),
        }
    }

    /// A write that fails after the reader has reported its ending is that
    /// ending — tmux left, and the dead pipe is how the writer found out —
    /// so the reported reason wins over the write error.
    fn send(&mut self, command: &CommandLine) -> Result<(), TmuxError> {
        let Side::Open { commands, .. } = &mut self.side else {
            return Err(TmuxError::NotReady(self.state));
        };
        let written = commands
            .write_all(command.wire())
            .and_then(|()| commands.flush());
        match written {
            Ok(()) => Ok(()),
            Err(err) => match self.replies.try_recv() {
                Ok(Reply::Ended(end)) => Err(self.ended(end)),
                _ => {
                    self.state = self.state.closed(CloseReason::TransportError);
                    Err(TmuxError::Send(err))
                }
            },
        }
    }

    /// The oldest reply not yet taken, waiting for the reader if none has
    /// settled.
    fn next_reply(&mut self) -> Result<CommandOutput, TmuxError> {
        match self.replies.recv() {
            Ok(Reply::Settled(reply)) => reply,
            Ok(Reply::Ended(end)) => Err(self.ended(end)),
            Err(mpsc::RecvError) => Err(self.reader_vanished()),
        }
    }

    /// The reader's ending, applied: closes with its reason and becomes its
    /// error, so [`Connection::state`] says so afterwards no matter which
    /// call observed it.
    fn ended(&mut self, end: ReadEnd) -> TmuxError {
        self.state = self.state.closed(end.reason());
        end.error()
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

/// Something decoded from the same read as the greeting's terminator, in
/// arrival order. Held rather than delivered because no sink exists until
/// the greeting has succeeded — a failed attempt's `%exit` is not an event
/// of any connection the caller has.
enum Behind {
    Event(Event),
    Settled(Settled),
}

/// A read side past its greeting, with the decoder state that read it.
struct Link {
    output: Box<dyn Read + Send>,
    codec: Codec,
    demux: Demux,
    behind_greeting: Vec<Behind>,
}

impl Link {
    /// Read until the first block settles. `%end` means attached (or
    /// created); `%error` means that command failed and tmux is exiting,
    /// which is an error to report rather than a greeting to skip past; EOF
    /// first is the transport closing.
    fn greet(mut output: Box<dyn Read + Send>) -> Result<Self, TmuxError> {
        let mut codec = Codec::new();
        let mut demux = Demux::default();
        let mut routed = VecDeque::new();
        let greeting = loop {
            read_routed(&mut output, &mut codec, &mut demux, |r| routed.push_back(r))
                .map_err(ReadEnd::error)?;
            let settled_at = routed.iter().position(|r| matches!(r, Routed::Reply(_)));
            if let Some(at) = settled_at {
                let Some(Routed::Reply(greeting)) = routed.remove(at) else {
                    unreachable!("position found a reply")
                };
                break greeting;
            }
        };
        match greeting {
            Ok(_) => {}
            Err(TmuxError::Command { lines, .. }) if lines.as_slice() == [NO_SESSIONS] => {
                return Err(TmuxError::NoSessions)
            }
            Err(err) => return Err(err),
        }
        let behind_greeting = routed
            .into_iter()
            .map(|r| match r {
                Routed::Reply(settled) => Behind::Settled(settled),
                Routed::Notification(msg) => Behind::Event(Event::Notification(msg)),
                Routed::PaneOutput(pane, data) => Behind::Event(Event::PaneOutput(pane, data)),
            })
            .collect();
        Ok(Self {
            output,
            codec,
            demux,
            behind_greeting,
        })
    }
}

/// One read, decoded and routed: every message the chunk held, in order, to
/// `routed`. The one read-and-route for both the greeting and the pump
/// (`[LAW:single-enforcer]`).
fn read_routed(
    output: &mut dyn Read,
    codec: &mut Codec,
    demux: &mut Demux,
    mut routed: impl FnMut(Routed),
) -> Result<(), ReadEnd> {
    let mut buf = [0u8; READ_CHUNK];
    let n = match output.read(&mut buf) {
        Ok(0) => return Err(ReadEnd::Eof),
        Ok(n) => n,
        Err(err) => return Err(ReadEnd::Failed(err)),
    };
    codec
        .feed(&buf[..n])
        .into_iter()
        .filter_map(|msg| demux.route(msg))
        .for_each(&mut routed);
    Ok(())
}

/// The reader thread: read, decode, route, until the read side ends; then
/// report the ending to both the reply channel and the sink and stop. A reply
/// send that fails means the [`Connection`] is gone, and with it anyone to
/// report to.
fn pump(
    mut output: Box<dyn Read + Send>,
    mut codec: Codec,
    mut demux: Demux,
    replies: Sender<Reply>,
    mut events: EventSink,
) {
    let end = loop {
        let mut gone = false;
        let read = read_routed(&mut output, &mut codec, &mut demux, |r| match r {
            Routed::Reply(reply) => gone |= replies.send(Reply::Settled(reply)).is_err(),
            Routed::Notification(msg) => events(Event::Notification(msg)),
            Routed::PaneOutput(pane, data) => events(Event::PaneOutput(pane, data)),
        });
        if gone {
            return;
        }
        if let Err(end) = read {
            break end;
        }
    };
    events(Event::Closed(end.reason()));
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
