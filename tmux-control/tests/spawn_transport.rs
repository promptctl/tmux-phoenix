//! Tests for `SpawnTransport`. Two tiers:
//!
//! - Process-mechanics tests spawn `sh -C -c cat` (an echo-back process, no
//!   `tmux` dependency) to verify send/read/close semantics without needing
//!   tmux installed on the test machine.
//! - Live-tmux tests actually spawn `tmux -C` against an isolated,
//!   throw-away socket and drive it through real guard-block exchanges —
//!   confirming this transport produces exactly the byte stream the codec
//!   expects from a real server, and that tmux reads every encoded argument
//!   back byte for byte.
//!
//! Live tests use a unique socket path per test (never the user's default
//! tmux socket) and always tear down via `kill-session` on their own socket.

use std::io::ErrorKind;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tmux_control::{CommandLine, SpawnOptions, SpawnTransport, Transport};

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

/// Read on a thread, at most `budget(collected)` bytes at a time, until the
/// budget is 0 or the pipe reaches EOF, so a stalled child fails the test at a
/// deadline instead of hanging `cargo test`. After a timeout the thread keeps
/// the transport; in a live test the `IsolatedTmux` drop runs `kill-session`,
/// the attached client exits, and that EOF ends the thread, which drops and so
/// reaps the transport.
fn read_until(
    transport: SpawnTransport,
    budget: impl Fn(&[u8]) -> usize + Send + 'static,
) -> (SpawnTransport, Vec<u8>) {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut transport = transport;
        let mut collected = Vec::new();
        let mut buf = [0u8; 4096];
        let outcome = loop {
            let limit = budget(&collected).min(buf.len());
            if limit == 0 {
                break Ok(());
            }
            match transport.read(&mut buf[..limit]) {
                Ok(0) => break Ok(()),
                Ok(n) => collected.extend_from_slice(&buf[..n]),
                Err(e) => break Err(e),
            }
        };
        // A send error means the deadline already failed the test.
        let _ = tx.send(outcome.map(|()| (transport, collected)));
    });
    rx.recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|e| panic!("read did not finish within 10s: {e}"))
        .unwrap_or_else(|e| panic!("read failed: {e}"))
}

/// Up to exactly `want` bytes, never past them, so later bytes stay in the
/// pipe; shorter only at EOF.
fn read_available(transport: SpawnTransport, want: usize) -> (SpawnTransport, Vec<u8>) {
    read_until(transport, move |collected| want - collected.len())
}

/// Whether `collected` holds a complete `%end` or `%error` line, either of
/// which closes a guard block.
fn closes_a_block(collected: &[u8]) -> bool {
    collected.split_inclusive(|&b| b == b'\n').any(|line| {
        line.ends_with(b"\n") && (line.starts_with(b"%end ") || line.starts_with(b"%error "))
    })
}

/// Everything through the line that closes the next guard block.
fn read_through_end(transport: SpawnTransport) -> (SpawnTransport, String) {
    let (transport, bytes) = read_until(transport, |collected| {
        if closes_a_block(collected) {
            0
        } else {
            usize::MAX
        }
    });
    let text = String::from_utf8_lossy(&bytes).into_owned();
    assert!(
        closes_a_block(&bytes),
        "tmux closed the connection before a %end or %error line: {text}"
    );
    (transport, text)
}

#[test]
fn send_writes_the_line_and_read_gets_it_back() {
    let mut transport = spawn_echo();
    transport.send(&line("display-message", ["hello"])).unwrap();
    let expected = b"display-message hello\n";
    let (mut transport, echoed) = read_available(transport, expected.len());
    assert_eq!(echoed, expected);
    transport.close();
}

#[test]
fn a_newline_in_an_argument_is_written_escaped_on_one_line() {
    let mut transport = spawn_echo();
    transport.send(&line("display-message", ["a\nb"])).unwrap();
    let expected = b"display-message a\\nb\n";
    let (mut transport, echoed) = read_available(transport, expected.len());
    assert_eq!(echoed, expected);
    transport.close();
}

#[test]
fn detach_writes_a_bare_newline() {
    // SPEC §4.1: an empty command line is the wire-level detach signal.
    let mut transport = spawn_echo();
    transport.send(&CommandLine::detach()).unwrap();
    let (mut transport, echoed) = read_available(transport, 1);
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
    let err = transport.send(&CommandLine::detach()).unwrap_err();
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
    transport.send(&line("hi", NO_ARGS)).unwrap();
    assert_eq!(transport.read(&mut []).unwrap(), 0);
    let (mut transport, echoed) = read_available(transport, 3);
    assert_eq!(echoed, b"hi\n");
    transport.close();
}

#[test]
fn failed_send_leaves_buffered_output_readable() {
    // The child closes its stdin before printing, so once `ready` is read the
    // send must fail while `trailing` is still waiting in the stdout pipe.
    let transport = SpawnTransport::spawn(
        &["-c", "exec 0<&-; printf 'ready\\ntrailing\\n'"],
        &SpawnOptions {
            tmux_path: Some("sh".to_string()),
            ..Default::default()
        },
    )
    .expect("failed to spawn sh");
    let (mut transport, ready) = read_available(transport, 6);
    assert_eq!(ready, b"ready\n");
    let err = transport.send(&line("anything", NO_ARGS)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::BrokenPipe);
    let (transport, trailing) = read_available(transport, 9);
    assert_eq!(trailing, b"trailing\n");
    let (_, eof) = read_available(transport, 1);
    assert_eq!(eof, b"");
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

mod support;
use support::{line, IsolatedTmux, NO_ARGS};

/// A control client attached to the harness's session.
fn attach(harness: &IsolatedTmux) -> SpawnTransport {
    SpawnTransport::spawn(
        &["attach-session", "-t", &harness.session],
        &SpawnOptions {
            socket: Some(harness.socket.clone()),
            ..Default::default()
        },
    )
    .expect("failed to spawn tmux -C")
}

/// Send `line` and read its reply block, failing the test if tmux rejected it.
fn run(mut transport: SpawnTransport, line: &CommandLine) -> (SpawnTransport, String) {
    transport.send(line).expect("send failed");
    let (transport, text) = read_through_end(transport);
    assert!(
        !text.lines().any(|l| l.starts_with("%error ")),
        "tmux rejected `{}`: {text}",
        line.as_str()
    );
    (transport, text)
}

#[test]
fn live_tmux_startup_greeting_is_an_empty_guard_block() {
    let harness = IsolatedTmux::new("greeting");

    // The unsolicited startup greeting (DESIGN.md §3.3) arrives as a complete
    // guard block before any command of ours is sent.
    let (mut transport, text) = read_through_end(attach(&harness));
    assert!(
        text.starts_with("%begin "),
        "expected greeting to start with %begin, got: {text}"
    );

    transport.close();
}

#[test]
fn live_tmux_command_round_trip_produces_correlated_guard_block() {
    let harness = IsolatedTmux::new("roundtrip");

    // Drain the startup greeting first.
    let (transport, _) = read_through_end(attach(&harness));

    let (mut transport, text) = run(
        transport,
        &line("display-message", ["-p", "PHOENIX-MARKER"]),
    );
    assert!(
        text.contains("PHOENIX-MARKER"),
        "expected our command's output in the reply block, got: {text}"
    );

    transport.close();
}

/// Arguments tmux's lexer would split, expand, or unescape if the encoding were
/// wrong.
const HOSTILE: &[&str] = &[
    " ",
    "~",
    "~/x",
    "a~b",
    "$",
    "$HOME",
    "${x}",
    "$1",
    "a$",
    "$é",
    "\"",
    "'",
    "\\",
    "a b\\c",
    "trailing\\",
    "\n",
    "one\ntwo\n",
    "\r\n",
    "tab\there",
    "\x017\x1b[0m\x7f",
    "#",
    "#{session_name}",
    ";",
    "a;b",
    "{",
    "}",
    "%",
    "%if",
    "-t",
    "--",
    "A=b",
    "héllo wörld ✓",
    "~$HOME \"it's\" \\ ; #{x} % {}\n\r\t\x017é\\",
];

#[test]
fn live_tmux_reads_every_encoded_argument_back_byte_for_byte() {
    let harness = IsolatedTmux::new("encoding");
    let saved =
        std::env::temp_dir().join(format!("tmux-phoenix-test-encoding-{}", std::process::id()));
    let saved_path = saved.to_str().expect("temp path is UTF-8");

    let (mut transport, _) = read_through_end(attach(&harness));
    for &arg in HOSTILE {
        (transport, _) = run(transport, &line("set-buffer", ["-b", "phx", "--", arg]));
        (transport, _) = run(transport, &line("save-buffer", ["-b", "phx", saved_path]));
        let read_back = std::fs::read(&saved).expect("save-buffer wrote no file");
        assert_eq!(
            read_back,
            arg.as_bytes(),
            "argument {arg:?} came back changed"
        );
    }

    // set-buffer refuses empty data, so the empty argument round-trips through
    // a user option instead.
    (transport, _) = run(transport, &line("set-option", ["-g", "@phx", ""]));
    let (mut transport, text) = run(transport, &line("display-message", ["-p", "[#{@phx}]"]));
    assert!(
        text.lines().any(|l| l == "[]"),
        "expected the option to hold the empty string, got: {text}"
    );

    std::fs::remove_file(&saved).expect("failed to remove the saved buffer");
    transport.close();
}
