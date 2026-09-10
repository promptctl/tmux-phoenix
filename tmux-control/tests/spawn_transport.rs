//! Tests for `SpawnTransport`. Two tiers:
//!
//! - Process-mechanics tests spawn `sh -C -c cat` (an echo-back process, no
//!   `tmux` dependency) to verify send/read/close semantics without needing
//!   tmux installed on the test machine.
//! - Live-tmux tests actually spawn `tmux -C` against an isolated,
//!   throw-away socket and drive it through a real guard-block exchange —
//!   confirming this transport produces exactly the byte stream the codec
//!   expects from a real server, not just an assumed shape.
//!
//! Live tests use a unique socket path per test (never the user's default
//! tmux socket) and always tear down via `kill-session` on their own socket.

use std::io::ErrorKind;
use tmux_control::{SpawnOptions, SpawnTransport, Transport};

/// Spawn `sh -C -c cat` — an echo-back process with no `tmux` dependency,
/// for testing send/read/close mechanics in isolation from the real
/// protocol. `SpawnTransport::spawn` always prepends `-C` (it is
/// deliberately tmux-only, per IMPL.md §2.1: no configuration produces
/// `-CC`), so a stand-in binary must tolerate that flag; `sh -C` (noclobber)
/// does, and `-c cat` then runs `cat` as the command — mirrors the same
/// trick the reference transport's own tests use (`spawn-transport.test.ts`).
fn spawn_echo() -> SpawnTransport {
    SpawnTransport::spawn(
        &["-c", "cat"],
        &SpawnOptions {
            tmux_path: Some("sh".to_string()),
            ..Default::default()
        },
    )
    .expect("failed to spawn sh -C -c cat")
}

/// Read up to exactly `want` bytes, never past them, so later bytes stay in
/// the pipe; shorter only at EOF.
fn read_available(transport: &mut SpawnTransport, want: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 256];
    while out.len() < want {
        let limit = (want - out.len()).min(buf.len());
        let n = transport.read(&mut buf[..limit]).expect("read failed");
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

#[test]
fn send_appends_lf_and_read_gets_it_back() {
    let mut transport = spawn_echo();
    transport.send("hello").unwrap();
    let echoed = read_available(&mut transport, 6);
    assert_eq!(echoed, b"hello\n");
    transport.close();
}

#[test]
fn send_does_not_double_terminate_a_line_that_already_ends_in_lf() {
    let mut transport = spawn_echo();
    transport.send("hello\n").unwrap();
    let echoed = read_available(&mut transport, 6);
    assert_eq!(echoed, b"hello\n");
    transport.close();
}

#[test]
fn empty_send_writes_a_bare_newline() {
    // SPEC §4.1: an empty command line is the wire-level detach signal.
    // `send("")` must produce exactly `\n`, not nothing.
    let mut transport = spawn_echo();
    transport.send("").unwrap();
    let echoed = read_available(&mut transport, 1);
    assert_eq!(echoed, b"\n");
    transport.close();
}

#[test]
fn read_returns_eof_when_child_exits_on_its_own() {
    // Distinct from `operations_after_close_return_broken_pipe_error`: here
    // nothing calls our own `close()` — the child exits unprompted (as tmux
    // would on `%exit`), and `read()` must observe that as a natural `Ok(0)`
    // EOF, not an error.
    let mut transport = SpawnTransport::spawn(
        &["-c", "exit 0"],
        &SpawnOptions {
            tmux_path: Some("sh".to_string()),
            ..Default::default()
        },
    )
    .expect("failed to spawn sh -C -c 'exit 0'");
    let mut buf = [0u8; 16];
    let n = transport.read(&mut buf).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn operations_after_close_return_broken_pipe_error() {
    let mut transport = spawn_echo();
    transport.close();
    let err = transport.send("anything").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::BrokenPipe);
}

#[test]
fn close_is_idempotent() {
    let mut transport = spawn_echo();
    transport.close();
    transport.close(); // must not panic or hang
}

#[test]
fn empty_buffer_read_leaves_the_transport_readable() {
    let mut transport = spawn_echo();
    transport.send("hi").unwrap();
    assert_eq!(transport.read(&mut []).unwrap(), 0);
    assert_eq!(read_available(&mut transport, 3), b"hi\n");
    transport.close();
}

#[test]
fn failed_send_leaves_buffered_output_readable() {
    // The child closes its stdin before printing, so once `ready` is read the
    // send must fail while `trailing` is still waiting in the stdout pipe.
    let mut transport = SpawnTransport::spawn(
        &["-c", "exec 0<&-; printf 'ready\\ntrailing\\n'"],
        &SpawnOptions {
            tmux_path: Some("sh".to_string()),
            ..Default::default()
        },
    )
    .expect("failed to spawn sh");
    assert_eq!(read_available(&mut transport, 6), b"ready\n");
    let err = transport.send("anything").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::BrokenPipe);
    assert_eq!(read_available(&mut transport, 9), b"trailing\n");
    assert_eq!(read_available(&mut transport, 1), b"");
}

#[test]
fn nonexistent_binary_returns_an_error_not_a_panic() {
    let result = SpawnTransport::spawn(
        &[],
        &SpawnOptions {
            tmux_path: Some("definitely-not-a-real-binary-xyz".to_string()),
            ..Default::default()
        },
    );
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Live tmux integration
// ---------------------------------------------------------------------------

/// Isolated throw-away socket path + a guard that tears the session down via
/// `kill-session` on drop.
struct IsolatedTmux {
    socket: String,
    session: String,
}

impl IsolatedTmux {
    fn new(name: &str) -> Self {
        let socket = format!("/tmp/tmux-phoenix-test-{name}-{}", std::process::id());
        let session = format!("phoenix-test-{name}");
        // Pre-create the session out-of-band (not through the transport
        // under test) so `attach-session` below has something real to
        // attach to, exactly like a daemon attaching to a live server.
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
    }
}

/// Read until a `%end ` arrives, on a thread, so a stalled tmux fails the
/// test at a deadline instead of hanging `cargo test`. A timeout panic drops
/// the `IsolatedTmux` harness, whose `kill-session` makes the attached client
/// exit; that EOF ends the thread's read and it drops, and so reaps, the
/// transport.
fn read_through_end(transport: SpawnTransport) -> (SpawnTransport, String) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut transport = transport;
        let mut collected = Vec::new();
        let mut buf = [0u8; 4096];
        let outcome = loop {
            if collected.windows(5).any(|w| w == b"%end ") {
                break Ok(String::from_utf8_lossy(&collected).into_owned());
            }
            match transport.read(&mut buf) {
                Ok(0) => break Err("transport closed before a %end arrived".to_string()),
                Ok(n) => collected.extend_from_slice(&buf[..n]),
                Err(e) => break Err(format!("read failed: {e}")),
            }
        };
        // A send error means the deadline already failed the test.
        let _ = tx.send(outcome.map(|text| (transport, text)));
    });
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .unwrap_or_else(|e| panic!("no complete %end block from tmux within 10s: {e}"))
        .unwrap_or_else(|reason| panic!("{reason}"))
}

#[test]
fn live_tmux_startup_greeting_is_an_empty_guard_block() {
    let harness = IsolatedTmux::new("greeting");
    let transport = SpawnTransport::spawn(
        &["attach-session", "-t", &harness.session],
        &SpawnOptions {
            socket: Some(harness.socket.clone()),
            ..Default::default()
        },
    )
    .expect("failed to spawn tmux -C");

    // The unsolicited startup greeting (DESIGN.md §3.3) arrives as a complete
    // guard block before any command of ours is sent.
    let (mut transport, text) = read_through_end(transport);
    assert!(
        text.starts_with("%begin "),
        "expected greeting to start with %begin, got: {text}"
    );

    transport.close();
}

#[test]
fn live_tmux_command_round_trip_produces_correlated_guard_block() {
    let harness = IsolatedTmux::new("roundtrip");
    let transport = SpawnTransport::spawn(
        &["attach-session", "-t", &harness.session],
        &SpawnOptions {
            socket: Some(harness.socket.clone()),
            ..Default::default()
        },
    )
    .expect("failed to spawn tmux -C");

    // Drain the startup greeting first.
    let (mut transport, _) = read_through_end(transport);

    transport.send("display-message -p PHOENIX-MARKER").unwrap();

    let (mut transport, text) = read_through_end(transport);
    assert!(
        text.contains("PHOENIX-MARKER"),
        "expected our command's output in the reply block, got: {text}"
    );

    transport.close();
}
