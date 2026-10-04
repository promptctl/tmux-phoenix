//! Shared test-only harness: an isolated, throw-away tmux server that
//! live-integration tests can safely drive without touching the developer's
//! real tmux sessions, the scripted transport that stands in for tmux where
//! no live server is wanted, and the command builder every test sends
//! through.

// Every integration test compiles this module, and none of them uses all of
// it — the alternative to this allowance is a copy of the harness per test
// binary, which is the divergence it exists to prevent.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use tmux_control::{Client, CommandLine, PaneId, ServerMessage, TmuxError, Transport};

/// For a command that takes no arguments.
pub const NO_ARGS: [&str; 0] = [];

/// A command line for tests, whose arguments never hold a NUL.
pub fn line(name: &'static str, args: impl IntoIterator<Item = impl AsRef<str>>) -> CommandLine {
    CommandLine::new(name, args).expect("test arguments hold no NUL")
}

/// Isolated throw-away socket path + a guard that tears the session down via
/// `kill-session` on drop.
pub struct IsolatedTmux {
    pub socket: String,
    pub session: String,
}

impl IsolatedTmux {
    pub fn new(name: &str) -> Self {
        let socket = format!("/tmp/tmux-phoenix-test-{name}-{}", std::process::id());
        let session = format!("phoenix-test-{name}");
        // Pre-create the session out-of-band (not through the transport
        // under test) so `attach-session` has something real to attach to,
        // exactly like a daemon attaching to a live server.
        let status = std::process::Command::new("tmux")
            .args(["-S", &socket, "new-session", "-d", "-s", &session])
            .status()
            .expect("failed to run tmux new-session");
        assert!(status.success(), "tmux new-session failed");
        Self { socket, session }
    }
}

impl Drop for IsolatedTmux {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux")
            .args(["-S", &self.socket, "kill-session", "-t", &self.session])
            .status();
        // Ending the session leaves the socket file itself behind, so every
        // run of these tests used to deposit one more in /tmp permanently.
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// An isolated socket path with *no* server on it, for tests about reaching
/// a server that may not exist. Whatever a test leaves running there is torn
/// down session by session on drop — only ever session-scoped kills, the one
/// form of teardown this repository allows.
pub struct EmptySocket {
    pub socket: String,
}

impl EmptySocket {
    pub fn new(name: &str) -> Self {
        let socket = format!("/tmp/tmux-phoenix-test-{name}-{}", std::process::id());
        let _ = std::fs::remove_file(&socket);
        Self { socket }
    }

    /// Every session name on this socket, as plain `tmux` sees it; empty
    /// when no server runs.
    pub fn sessions(&self) -> Vec<String> {
        let output = std::process::Command::new("tmux")
            .args(["-S", &self.socket, "list-sessions", "-F", "#{session_name}"])
            .output()
            .expect("failed to run tmux list-sessions");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for EmptySocket {
    fn drop(&mut self) {
        for session in self.sessions() {
            let _ = std::process::Command::new("tmux")
                .args([
                    "-S",
                    &self.socket,
                    "kill-session",
                    "-t",
                    &format!("={session}"),
                ])
                .status();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// What the scripted output half delivers next. `Hangup` is EOF as a value,
/// so a test can end the stream at any moment — and so the writer's drop can
/// end it too, which is the contract `Connection::over` relies on, without
/// the test having to drop anything in a particular order.
enum Feed {
    Bytes(Vec<u8>),
    Hangup,
}

/// The test's handle on a scripted link: feed tmux's side of the
/// conversation, or end it.
pub struct Script {
    feed: Sender<Feed>,
}

impl Script {
    pub fn send(&self, chunk: &str) {
        self.feed
            .send(Feed::Bytes(chunk.as_bytes().to_vec()))
            .expect("the reader is still running");
    }

    /// The server went away: the next read returns EOF.
    pub fn hangup(&self) {
        let _ = self.feed.send(Feed::Hangup);
    }
}

/// Every command line the `Connection` wrote, readable after the writer has
/// been moved into it. Shared across threads because the type must be `Send`
/// for `Connection::over`.
#[derive(Clone, Default)]
pub struct Sent(Arc<Mutex<Vec<String>>>);

impl Sent {
    pub fn lines(&self) -> Vec<String> {
        self.0.lock().expect("sent lines poisoned").clone()
    }
}

/// The command half of a scripted link. Holds one feeder so that dropping
/// it hangs the output half up, exactly as killing the child does for a
/// spawned `tmux`.
pub struct ScriptWriter {
    sent: Sent,
    feed: Sender<Feed>,
}

impl io::Write for ScriptWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let line = String::from_utf8_lossy(buf);
        self.sent
            .0
            .lock()
            .expect("sent lines poisoned")
            .push(line.trim_end_matches('\n').to_owned());
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for ScriptWriter {
    fn drop(&mut self) {
        let _ = self.feed.send(Feed::Hangup);
    }
}

/// The output half of a scripted link: blocks until fed, like a pipe.
pub struct ScriptReader {
    feed: Receiver<Feed>,
}

impl io::Read for ScriptReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.feed.recv() {
            Ok(Feed::Bytes(chunk)) => {
                assert!(
                    chunk.len() <= buf.len(),
                    "test chunk larger than read buffer"
                );
                buf[..chunk.len()].copy_from_slice(&chunk);
                Ok(chunk.len())
            }
            Ok(Feed::Hangup) | Err(mpsc::RecvError) => Ok(0),
        }
    }
}

/// A scripted link for `Connection::over`, with `chunks` already queued as
/// tmux's opening bytes (a greeting, typically). More can be fed through the
/// returned [`Script`] at any time.
pub fn scripted(chunks: Vec<&str>) -> (ScriptWriter, ScriptReader, Script, Sent) {
    let (feed, output) = mpsc::channel();
    let script = Script { feed: feed.clone() };
    for chunk in chunks {
        script.send(chunk);
    }
    let sent = Sent::default();
    let writer = ScriptWriter {
        sent: sent.clone(),
        feed,
    };
    (writer, ScriptReader { feed: output }, script, sent)
}

/// One dispatched `%output`/`%extended-output` delivery: which pane, what
/// bytes — the pane-output sink's two arguments, paired for collection.
pub type PaneOutputChunk = (PaneId, Vec<u8>);

/// What a `Client`'s two required sinks delivered, readable after the
/// closures have been moved into the client — the same shared-handle shape
/// as [`MockState`], for the same reason.
///
/// This is the caller-owned buffer that replaced the client's own: the crate
/// dispatches and holds nothing, so a test that wants to assert over a window
/// of the connection keeps the window here.
#[derive(Clone, Default)]
pub struct Collected {
    notifications: Rc<RefCell<Vec<ServerMessage>>>,
    pane_output: Rc<RefCell<Vec<PaneOutputChunk>>>,
}

impl Collected {
    /// Everything delivered since the last take, removed as it is read, so
    /// consecutive calls scope to disjoint windows rather than re-reporting.
    pub fn take_notifications(&self) -> Vec<ServerMessage> {
        std::mem::take(&mut *self.notifications.borrow_mut())
    }

    pub fn take_pane_output(&self) -> Vec<PaneOutputChunk> {
        std::mem::take(&mut *self.pane_output.borrow_mut())
    }

    fn notification_sink(&self) -> impl FnMut(ServerMessage) + 'static {
        let into = self.notifications.clone();
        move |msg| into.borrow_mut().push(msg)
    }

    fn pane_output_sink(&self) -> impl FnMut(PaneId, Vec<u8>) + 'static {
        let into = self.pane_output.clone();
        move |pane, data| into.borrow_mut().push((pane, data))
    }
}

/// A `Client` that starts `Ready` with no handshake ([`Client::new`]), with
/// both required sinks collecting into the returned [`Collected`]. One
/// builder for every test whether or not it reads the collector, so "does
/// this test care about notifications" stays a fact about the assertions
/// rather than about which constructor was called.
pub fn collecting_client<T: Transport>(transport: T) -> (Client<T>, Collected) {
    let collected = Collected::default();
    let client = Client::new(
        transport,
        collected.notification_sink(),
        collected.pane_output_sink(),
    );
    (client, collected)
}

/// The same, through the real greeting handshake ([`Client::connect`]).
/// Wiring the collector before the handshake is the point rather than an
/// accident of ordering: `connect()` dispatches whatever tmux wrote behind
/// the greeting terminator, so it is the only way a test can observe it.
pub fn collecting_connect<T: Transport>(transport: T) -> Result<(Client<T>, Collected), TmuxError> {
    let collected = Collected::default();
    let client = Client::connect(
        transport,
        collected.notification_sink(),
        collected.pane_output_sink(),
    )?;
    Ok((client, collected))
}

/// Shared with a [`MockTransport`] after it has been moved into a `Client`,
/// so tests can still assert on what was sent and control what is closed —
/// `Client` owns its transport outright and exposes no way to get it back.
#[derive(Clone, Default)]
pub struct MockState {
    pub sent: Rc<RefCell<Vec<String>>>,
    pub closed: Rc<RefCell<bool>>,
}

/// Hands back pre-scripted byte chunks in place of a live tmux — the
/// testability `Transport` exists for (DESIGN.md §3.2). One transport with
/// its refusal behavior as a field, rather than a variant per test file:
/// three near-copies of this had already drifted apart before it was moved
/// here.
pub struct MockTransport {
    /// Each `read()` call pops and returns one chunk. Exhausted means EOF.
    chunks: VecDeque<Vec<u8>>,
    state: MockState,
    /// Set before the send under test to exercise a refusing transport.
    pub fail_next_send: bool,
}

impl MockTransport {
    pub fn new(chunks: Vec<&str>) -> (Self, MockState) {
        let state = MockState::default();
        let transport = Self {
            chunks: chunks.into_iter().map(|c| c.as_bytes().to_vec()).collect(),
            state: state.clone(),
            fail_next_send: false,
        };
        (transport, state)
    }

    pub fn empty() -> (Self, MockState) {
        Self::new(vec![])
    }
}

impl Transport for MockTransport {
    fn send(&mut self, command: &CommandLine) -> io::Result<()> {
        if *self.state.closed.borrow() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"));
        }
        if self.fail_next_send {
            // One-shot, as the name says: a test that wants the refusal to
            // repeat sets it again. Leaving it set would make every later
            // send fail too, so a test asserting "this send fails and a
            // subsequent one succeeds" would pass while testing neither.
            self.fail_next_send = false;
            return Err(io::Error::other("send refused"));
        }
        self.state
            .sent
            .borrow_mut()
            .push(command.as_str().to_string());
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if *self.state.closed.borrow() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"));
        }
        match self.chunks.pop_front() {
            Some(chunk) => {
                assert!(
                    chunk.len() <= buf.len(),
                    "test chunk larger than read buffer"
                );
                buf[..chunk.len()].copy_from_slice(&chunk);
                Ok(chunk.len())
            }
            None => Ok(0), // simulated EOF once scripted chunks are exhausted
        }
    }

    fn close(&mut self) {
        *self.state.closed.borrow_mut() = true;
    }
}
