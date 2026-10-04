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
//! the reader thread, always, ahead of any reply that follows in the same
//! read.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
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

/// Ends a connection's link from any thread, which is how a caller blocked in
/// [`Connection::execute`] on a server that stopped answering is released:
/// the read side ends, the reader reports [`CloseReason::Disposed`], and the
/// blocked call returns [`TmuxError::TransportClosed`]. Aborting a link that
/// has already ended is a no-op.
#[derive(Clone)]
pub struct Abort {
    disposed: Arc<AtomicBool>,
    end: Arc<dyn Fn() + Send + Sync>,
}

impl Abort {
    pub fn abort(&self) {
        self.disposed.store(true, Ordering::SeqCst);
        (self.end)();
    }
}

impl std::fmt::Debug for Abort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Abort")
            .field("disposed", &self.disposed.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

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

/// How a read side ended. The endings differ for every caller — a clean
/// EOF is tmux's own `%exit` or exit, a failed read is a broken transport,
/// and an EOF this side caused ([`Connection::close`], [`Abort::abort`]) is
/// neither — so the distinction is carried, and the pairing of each with
/// its [`CloseReason`] and its [`TmuxError`] is written once
/// (`[LAW:one-source-of-truth]`).
pub(crate) enum ReadEnd {
    Eof,
    Failed(io::Error),
    Disposed,
}

impl ReadEnd {
    pub(crate) fn reason(&self) -> CloseReason {
        match self {
            ReadEnd::Eof => CloseReason::Exit,
            ReadEnd::Failed(_) => CloseReason::TransportError,
            ReadEnd::Disposed => CloseReason::Disposed,
        }
    }

    pub(crate) fn error(self) -> TmuxError {
        match self {
            ReadEnd::Eof | ReadEnd::Disposed => TmuxError::TransportClosed,
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
    abort: Abort,
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
        let kill = commands.kill_handle();
        let connection = Self::start(commands, link, Box::new(events), move || kill.kill());
        Ok((connection, opened))
    }

    /// Spawn and read the greeting. A failure here drops the writer, which
    /// ends the child (see [`Connection::over`]).
    fn spawn(
        args: &[&str],
        options: &SpawnOptions,
    ) -> Result<(crate::transport::ChildWriter, Link), TmuxError> {
        let (commands, output) = spawn_halves(args, options).map_err(TmuxError::Spawn)?;
        let link = Link::greet(Box::new(output))?;
        Ok((commands, link))
    }

    /// Build a connection over an already-open byte link, consuming the
    /// greeting block. This is how a test stands in for tmux; real usage
    /// goes through [`Connection::open`].
    ///
    /// Contract on the halves: dropping `commands` must end `output`, as
    /// must calling `end` from any thread, and a write to `commands` that
    /// fails must be followed by `output` ending — the reader thread blocks
    /// in `read()` and nothing else can return it, and the reader is the one
    /// classifier of how a link ended. The spawned child honors all three by
    /// dying; a scripted stand-in honors them by delivering EOF.
    pub fn over(
        commands: impl Write + Send + 'static,
        output: impl Read + Send + 'static,
        events: impl FnMut(Event) + Send + 'static,
        end: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, TmuxError> {
        let link = Link::greet(Box::new(output))?;
        Ok(Self::start(commands, link, Box::new(events), end))
    }

    /// Hand a greeted link its sink and its thread. The reader delivers
    /// whatever arrived behind the greeting terminator first, so the sink is
    /// only ever called from that thread.
    fn start(
        commands: impl Write + Send + 'static,
        link: Link,
        events: EventSink,
        end: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let (reply_tx, replies) = mpsc::channel();
        let abort = Abort {
            disposed: Arc::new(AtomicBool::new(false)),
            end: Arc::new(end),
        };
        let disposed = abort.disposed.clone();
        let reader = thread::spawn(move || pump(link, reply_tx, events, disposed));
        Self {
            side: Side::Open {
                commands: Box::new(commands),
                reader,
            },
            replies,
            abort,
            state: ConnectionState::Ready,
        }
    }

    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// A handle that ends this connection's link from another thread.
    pub fn abort_handle(&self) -> Abort {
        self.abort.clone()
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
    /// command rule, and an ending there is the reader having already
    /// reported a close that no call had yet observed. Both are found here,
    /// before a command is written against them (`[LAW:no-silent-failure]`).
    fn idle(&mut self) -> Result<(), TmuxError> {
        match self.replies.try_recv() {
            Err(TryRecvError::Empty) => Ok(()),
            Ok(Reply::Settled(reply)) => Err(self.unsolicited(reply)),
            Ok(Reply::Ended(end)) => Err(self.ended(end)),
            Err(TryRecvError::Disconnected) => Err(self.reader_vanished()),
        }
    }

    /// A write that fails means the link is ending, and the reader is the
    /// one that says how (see the contract on [`Connection::over`]): wait
    /// for its report rather than guess from this side's error. A block that
    /// settles first is reported, not lost with the write.
    fn send(&mut self, command: &CommandLine) -> Result<(), TmuxError> {
        let Side::Open { commands, .. } = &mut self.side else {
            return Err(TmuxError::NotReady(self.state));
        };
        let written = commands
            .write_all(command.wire())
            .and_then(|()| commands.flush());
        match written {
            Ok(()) => Ok(()),
            Err(_) => Err(match self.replies.recv() {
                Ok(Reply::Settled(reply)) => self.unsolicited(reply),
                Ok(Reply::Ended(end)) => self.ended(end),
                Err(mpsc::RecvError) => self.reader_vanished(),
            }),
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

    /// A block that settled with nothing in flight: positional correlation
    /// is lost for good, so the connection closes on it.
    fn unsolicited(&mut self, reply: Settled) -> TmuxError {
        self.state = self.state.closed(CloseReason::Protocol);
        TmuxError::UnsolicitedReply(Box::new(reply))
    }

    /// The reader always reports its ending before it exits, so a channel
    /// that disconnected without one means the thread died — a panic — and
    /// the read side is gone with it.
    fn reader_vanished(&mut self) -> TmuxError {
        self.state = self.state.closed(CloseReason::TransportError);
        TmuxError::TransportClosed
    }

    /// Local-side teardown: drops the command writer — which ends the read
    /// side, see [`Connection::over`] — and joins the reader. Idempotent. An
    /// ending the reader had already reported stays the reason (the first
    /// close wins); only a link still up becomes `Disposed`.
    pub fn close(&mut self) {
        if let Side::Open { commands, reader } = std::mem::replace(&mut self.side, Side::Closed) {
            // Whatever the reader already reported is the ending; the first
            // one recorded wins, so draining in order is enough.
            for reply in self.replies.try_iter().collect::<Vec<_>>() {
                match reply {
                    Reply::Settled(reply) => self.unsolicited(reply),
                    Reply::Ended(end) => self.ended(end),
                };
            }
            self.abort.disposed.store(true, Ordering::SeqCst);
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

/// A read side past its greeting, with the decoder state that read it and
/// whatever was decoded from the same read as the greeting's terminator, in
/// arrival order. That is held rather than delivered because no sink exists
/// until the greeting has succeeded — a failed attempt's `%exit` is not an
/// event of any connection the caller has.
struct Link {
    output: Box<dyn Read + Send>,
    codec: Codec,
    demux: Demux,
    behind_greeting: VecDeque<Routed>,
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
        Ok(Self {
            output,
            codec,
            demux,
            behind_greeting: routed,
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

/// One routed message to its destination: a reply to the channel, anything
/// else to the sink. The channel's receiver is the [`Connection`], which
/// joins this thread before it can go away, so a reply send cannot fail
/// while there is anything to report to.
fn deliver(routed: Routed, replies: &Sender<Reply>, events: &mut EventSink) {
    match routed {
        Routed::Reply(reply) => {
            let _ = replies.send(Reply::Settled(reply));
        }
        Routed::Notification(msg) => events(Event::Notification(msg)),
        Routed::PaneOutput(pane, data) => events(Event::PaneOutput(pane, data)),
    }
}

/// The reader thread: deliver what arrived behind the greeting, then read,
/// decode, route, until the read side ends; then report the ending — to
/// the reply channel first, so a caller whose sink wakes it finds the
/// ending waiting on its next call rather than a dead pipe — and to the
/// sink, and stop. An EOF this side asked for is `Disposed`, whatever the
/// pipe said.
fn pump(link: Link, replies: Sender<Reply>, mut events: EventSink, disposed: Arc<AtomicBool>) {
    let Link {
        mut output,
        mut codec,
        mut demux,
        behind_greeting,
    } = link;
    for routed in behind_greeting {
        deliver(routed, &replies, &mut events);
    }
    let end = loop {
        let read = read_routed(&mut output, &mut codec, &mut demux, |r| {
            deliver(r, &replies, &mut events)
        });
        if let Err(end) = read {
            break end;
        }
    };
    let end = match disposed.load(Ordering::SeqCst) {
        true => ReadEnd::Disposed,
        false => end,
    };
    let reason = end.reason();
    let _ = replies.send(Reply::Ended(end));
    events(Event::Closed(reason));
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
