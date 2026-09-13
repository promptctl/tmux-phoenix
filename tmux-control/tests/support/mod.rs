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
