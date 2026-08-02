//! Tests for `Client::execute`'s FIFO guard-block correlation, isolated
//! from a live tmux via a `MockTransport` that hands back pre-scripted
//! byte chunks — exactly the testability DESIGN.md §3.2 calls out
//! (`Transport` exists so codec and client are testable without a live
//! tmux). Uses `Client::new` throughout, which starts `Ready` with no
//! greeting handshake — the right choice here since these tests are about
//! FIFO correlation, not the handshake itself. `Client::connect`'s greeting
//! consumption and the `ConnectionState` gate it enforces are covered
//! separately in `connection_state.rs`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;
use tmux_control::{Client, ServerMessage, TmuxError};

/// Shared with a `MockTransport` after it's moved into a `Client`, so tests
/// can still assert on what was sent and control what's closed — `Client`
/// owns its transport outright and exposes no way to get it back.
#[derive(Clone, Default)]
struct MockState {
    sent: Rc<RefCell<Vec<String>>>,
    closed: Rc<RefCell<bool>>,
}

struct MockTransport {
    /// Each `read()` call pops and returns one chunk. Exhausted means EOF.
    chunks: VecDeque<Vec<u8>>,
    state: MockState,
    fail_next_send: bool,
}

impl MockTransport {
    fn new(chunks: Vec<&str>) -> (Self, MockState) {
        let state = MockState::default();
        let transport = Self {
            chunks: chunks.into_iter().map(|c| c.as_bytes().to_vec()).collect(),
            state: state.clone(),
            fail_next_send: false,
        };
        (transport, state)
    }

    fn empty() -> (Self, MockState) {
        Self::new(vec![])
    }
}

impl tmux_control::Transport for MockTransport {
    fn send(&mut self, command: &str) -> io::Result<()> {
        if *self.state.closed.borrow() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"));
        }
        if self.fail_next_send {
            return Err(io::Error::other("send refused"));
        }
        self.state.sent.borrow_mut().push(command.to_string());
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

#[test]
fn execute_sends_the_command_and_returns_output_on_end() {
    let (transport, state) =
        MockTransport::new(vec!["%begin 1000 1 1\n0: bash* (1 panes)\n%end 1000 1 1\n"]);
    let mut client = Client::new(transport);

    let result = client.execute("list-windows").unwrap();

    assert_eq!(result.guard.command_number, 1);
    assert_eq!(result.lines, vec![b"0: bash* (1 panes)".to_vec()]);
    assert_eq!(*state.sent.borrow(), vec!["list-windows".to_string()]);
}

#[test]
fn execute_sends_exactly_the_command_string_unmodified() {
    // Line-termination is the transport's job (SpawnTransport tests cover
    // that); the client passes the command through as given, untouched.
    let (transport, state) = MockTransport::new(vec!["%begin 1 1 1\n%end 1 1 1\n"]);
    let mut client = Client::new(transport);
    client.execute("display-message -p test").unwrap();
    assert_eq!(
        *state.sent.borrow(),
        vec!["display-message -p test".to_string()]
    );
}

#[test]
fn execute_returns_command_error_on_tmux_error_reply() {
    let (transport, _state) = MockTransport::new(vec![
        "%begin 1000 3 1\nparse error: unknown command\n%error 1000 3 1\n",
    ]);
    let mut client = Client::new(transport);

    let err = client.execute("bad-command").unwrap_err();
    match err {
        TmuxError::Command { guard, lines } => {
            assert_eq!(guard.command_number, 3);
            assert_eq!(lines, vec![b"parse error: unknown command".to_vec()]);
        }
        other => panic!("expected TmuxError::Command, got {other:?}"),
    }
}

#[test]
fn execute_reads_across_multiple_transport_chunks() {
    // The reply arrives split across several read() calls, mirroring a real
    // pipe delivering partial lines.
    let (transport, _state) = MockTransport::new(vec![
        "%begin 1 5 1\n",
        "line one\n",
        "line two\n",
        "%end 1 5 1\n",
    ]);
    let mut client = Client::new(transport);

    let result = client.execute("list-panes").unwrap();
    assert_eq!(
        result.lines,
        vec![b"line one".to_vec(), b"line two".to_vec()]
    );
}

#[test]
fn execute_buffers_a_notification_that_arrives_before_the_reply_block_opens() {
    // A notification genuinely can't appear *inside* a guard block (SPEC
    // §6's block-purity invariant, enforced by the codec) — the realistic
    // "arrives while execute() is waiting" case is a notification on the
    // wire just before tmux starts our command's own block.
    let (transport, _state) = MockTransport::new(vec![
        "%sessions-changed\n", // unrelated background notification
        "%begin 1 7 1\n0: bash* (1 panes)\n%end 1 7 1\n",
    ]);
    let mut client = Client::new(transport);

    let result = client.execute("list-windows").unwrap();
    assert_eq!(result.lines, vec![b"0: bash* (1 panes)".to_vec()]);

    let notifications = client.drain_notifications();
    assert_eq!(notifications, vec![ServerMessage::SessionsChanged]);
}

#[test]
fn execute_buffers_a_notification_trailing_in_the_same_read_chunk_as_the_reply() {
    // Regression: a single read() can return a chunk containing our reply's
    // GuardEnd *and* bytes written right after it in the same burst — e.g.
    // tmux emitting a notification immediately behind our block, delivered
    // in one OS-level read. codec.feed() returns both messages in one Vec;
    // execute() must not stop consuming that Vec the instant it sees
    // GuardEnd, or the trailing notification is lost for good (it was
    // already parsed out of the transport, not left to read again).
    let (transport, _state) =
        MockTransport::new(vec!["%begin 1 1 1\n%end 1 1 1\n%sessions-changed\n"]);
    let mut client = Client::new(transport);

    client.execute("noop").unwrap();

    assert_eq!(
        client.drain_notifications(),
        vec![ServerMessage::SessionsChanged]
    );
}

#[test]
fn drain_notifications_empties_the_buffer() {
    let (transport, _state) =
        MockTransport::new(vec!["%sessions-changed\n%begin 1 1 1\n%end 1 1 1\n"]);
    let mut client = Client::new(transport);
    client.execute("noop").unwrap();

    assert_eq!(client.drain_notifications().len(), 1);
    assert_eq!(client.drain_notifications(), vec![]); // already drained
}

#[test]
fn execute_surfaces_malformed_terminator_as_protocol_error() {
    let (transport, _state) = MockTransport::new(vec![
        "%begin 1699900000 7 0\n%end 1699900000 7\n", // truncated %end
    ]);
    let mut client = Client::new(transport);

    let err = client.execute("anything").unwrap_err();
    match err {
        TmuxError::Protocol {
            command_number,
            line,
        } => {
            assert_eq!(command_number, 7);
            assert_eq!(line, b"%end 1699900000 7");
        }
        other => panic!("expected TmuxError::Protocol, got {other:?}"),
    }
}

#[test]
fn execute_returns_transport_closed_on_eof_before_reply_completes() {
    let (transport, _state) = MockTransport::new(vec!["%begin 1 1 1\n"]); // no %end ever arrives
    let mut client = Client::new(transport);

    let err = client.execute("hangs").unwrap_err();
    assert!(matches!(err, TmuxError::TransportClosed));
}

#[test]
fn execute_propagates_send_failure_without_reading() {
    let (mut transport, _state) = MockTransport::empty();
    transport.fail_next_send = true;
    let mut client = Client::new(transport);

    let err = client.execute("anything").unwrap_err();
    assert!(matches!(err, TmuxError::Send(_)));
}

#[test]
fn sequential_execute_calls_correlate_independently() {
    let (transport, state) = MockTransport::new(vec![
        "%begin 1 1 1\nfirst\n%end 1 1 1\n",
        "%begin 2 2 1\nsecond\n%end 2 2 1\n",
    ]);
    let mut client = Client::new(transport);

    let first = client.execute("cmd-one").unwrap();
    let second = client.execute("cmd-two").unwrap();

    assert_eq!(first.lines, vec![b"first".to_vec()]);
    assert_eq!(first.guard.command_number, 1);
    assert_eq!(second.lines, vec![b"second".to_vec()]);
    assert_eq!(second.guard.command_number, 2);
    assert_eq!(
        *state.sent.borrow(),
        vec!["cmd-one".to_string(), "cmd-two".to_string()]
    );
}

#[test]
fn detach_sends_a_bare_newline_bypassing_execute() {
    let (transport, state) = MockTransport::empty();
    let mut client = Client::new(transport);
    client.detach().unwrap();
    // detach() must succeed against a transport with zero scripted read
    // chunks, proving it never entered execute()'s read loop — and it must
    // send exactly a bare "" (the transport, not the client, appends the
    // wire-level \n; see SpawnTransport's own send() tests for that half).
    assert_eq!(*state.sent.borrow(), vec!["".to_string()]);
}

// ---------------------------------------------------------------------------
// Live tmux integration
// ---------------------------------------------------------------------------

mod support;
use support::IsolatedTmux;
use tmux_control::SpawnTransport;

#[test]
fn live_tmux_execute_round_trips_against_a_real_server() {
    let harness = IsolatedTmux::new("client-execute");
    let mut transport = SpawnTransport::spawn(
        &["attach-session", "-t", &harness.session],
        &tmux_control::SpawnOptions {
            socket: Some(harness.socket.clone()),
            ..Default::default()
        },
    )
    .expect("failed to spawn tmux -C");

    // Drain the unsolicited startup greeting manually — this is exactly
    // ticket `.4`'s job in the real client, done by hand here so this test
    // can exercise `execute()` correctly without depending on unbuilt code.
    let mut collected = Vec::new();
    let mut buf = [0u8; 4096];
    let mut codec = tmux_control::Codec::new();
    loop {
        let n = tmux_control::Transport::read(&mut transport, &mut buf).expect("read failed");
        assert!(n > 0, "transport closed before the greeting arrived");
        for msg in codec.feed(&buf[..n]) {
            collected.push(msg);
        }
        if collected
            .iter()
            .any(|m| matches!(m, tmux_control::ServerMessage::GuardEnd(_)))
        {
            break;
        }
    }

    // Rebuild a fresh Client wired to the same transport, past the
    // greeting. execute() from here on should correlate cleanly.
    let mut client = Client::new(transport);
    let result = client
        .execute("display-message -p PHOENIX-CLIENT-MARKER")
        .unwrap();
    let joined = result.lines.concat();
    let text = String::from_utf8_lossy(&joined);
    assert!(
        text.contains("PHOENIX-CLIENT-MARKER"),
        "unexpected output: {text}"
    );

    let second = client.execute("list-sessions").unwrap();
    let joined = second.lines.concat();
    let listed = String::from_utf8_lossy(&joined);
    assert!(
        listed.contains(&harness.session),
        "expected session name in output: {listed}"
    );

    client.close();
}

#[test]
fn close_delegates_to_transport_close() {
    let (transport, state) = MockTransport::empty();
    let mut client = Client::new(transport);
    client.close();
    assert!(*state.closed.borrow());
    assert_eq!(
        client.state(),
        tmux_control::ConnectionState::Closed {
            reason: tmux_control::CloseReason::Disposed
        }
    );
    // The state gate refuses before ever touching the transport again.
    let err = client.execute("anything").unwrap_err();
    assert!(matches!(err, TmuxError::NotReady(_)));
}
