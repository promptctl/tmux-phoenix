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
use tmux_control::{Codec, CommandLine, ServerMessage, SpawnOptions, SpawnTransport, Transport};

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

/// How long a read may block before it has failed. Far past any exchange
/// these tests provoke, and short enough that a wedged run still ends.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Read on a thread, at most `budget(collected)` bytes at a time, until the
/// budget is 0 or the pipe reaches EOF, so a stalled child fails the test at
/// `timeout` instead of hanging `cargo test`.
///
/// The thread *borrows* the transport rather than owning it, which is what
/// leaves the deadline branch something to act on: a
/// [`SpawnTransport::kill_handle`] taken before the scope opens. Killing the
/// child is not merely tidy — it is the only thing that returns a reader
/// blocked in `read()`, so it is also the reason the scope can be joined at
/// all.
fn read_until(
    transport: &mut SpawnTransport,
    timeout: Duration,
    budget: impl Fn(&[u8]) -> usize + Send,
) -> Vec<u8> {
    let killer = transport.kill_handle();
    let (done_tx, done_rx) = mpsc::channel();
    thread::scope(|scope| {
        let reader = scope.spawn(move || {
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
            // A send error means the deadline already fired; the value below
            // is recovered from the join either way.
            let _ = done_tx.send(());
            outcome.map(|()| collected)
        });
        let finished = done_rx.recv_timeout(timeout).is_ok();
        // Kill before the assertion below, never after: `scope` joins its
        // threads while unwinding, and a reader still blocked in `read()`
        // would hold that unwind open forever — turning a failed test into a
        // hung suite. So the deadline's outcome, not the thread's incidental
        // lifetime, is what decides teardown
        // (`[LAW:no-ambient-temporal-coupling]`).
        if !finished {
            killer.kill();
        }
        let collected = reader.join().expect("reader thread panicked");
        assert!(finished, "read did not finish within {timeout:?}");
        collected.unwrap_or_else(|e| panic!("read failed: {e}"))
    })
}

/// Up to exactly `want` bytes, never past them, so later bytes stay in the
/// pipe; shorter only at EOF.
fn read_available(transport: &mut SpawnTransport, want: usize) -> Vec<u8> {
    read_until(transport, READ_TIMEOUT, move |collected| {
        want - collected.len()
    })
}

/// Whether `collected` holds a line that closes a guard block — asked of the
/// real [`Codec`] rather than re-derived from the bytes here, so the tests
/// stop reading on exactly the lines the client treats as terminators
/// (`[LAW:one-source-of-truth]`). A byte-prefix scan would drift: a bare
/// `%end` carrying no fields has no trailing space, yet still force-closes
/// the block.
fn closes_a_block(collected: &[u8]) -> bool {
    Codec::new().feed(collected).iter().any(|message| {
        matches!(
            message,
            ServerMessage::GuardEnd(_)
                | ServerMessage::GuardError(_)
                | ServerMessage::ProtocolError { .. }
        )
    })
}

/// Everything through the line that closes the next guard block.
fn read_through_end(transport: &mut SpawnTransport) -> String {
    let bytes = read_until(transport, READ_TIMEOUT, |collected| {
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
    text
}

#[test]
fn send_writes_the_line_and_read_gets_it_back() {
    let mut transport = spawn_echo();
    transport.send(&line("display-message", ["hello"])).unwrap();
    let expected = b"display-message hello\n";
    assert_eq!(read_available(&mut transport, expected.len()), expected);
    transport.close();
}

#[test]
fn a_newline_in_an_argument_is_written_escaped_on_one_line() {
    let mut transport = spawn_echo();
    transport.send(&line("display-message", ["a\nb"])).unwrap();
    let expected = b"display-message a\\nb\n";
    assert_eq!(read_available(&mut transport, expected.len()), expected);
    transport.close();
}

#[test]
fn detach_writes_a_bare_newline() {
    // SPEC §4.1: an empty command line is the wire-level detach signal.
    let mut transport = spawn_echo();
    transport.send(&CommandLine::detach()).unwrap();
    assert_eq!(read_available(&mut transport, 1), b"\n");
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
    let err = transport.send(&line("anything", NO_ARGS)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::BrokenPipe);
    assert_eq!(read_available(&mut transport, 9), b"trailing\n");
    assert_eq!(read_available(&mut transport, 1), b"");
}

#[test]
fn a_read_past_its_deadline_leaves_no_surviving_child() {
    // Before the transport had a kill handle, the reader thread owned the
    // child and stayed blocked in `read()` past the deadline, so this
    // `sh -C -c cat` outlived every timed-out test for the rest of the
    // binary's run (tmux-testing-h1t).
    let mut transport = spawn_echo();

    // `cat` echoes only what it was sent, and it was sent nothing — so this
    // asks for a byte that can never arrive.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read_until(&mut transport, Duration::from_millis(200), |_| 1)
    }));
    assert!(
        outcome.is_err(),
        "the read was supposed to blow its deadline"
    );

    // EOF is the proof of death, and it is exact rather than circumstantial:
    // `sh -C -c cat` execs into `cat`, so the spawned pid is the only process
    // holding the write end of this pipe, and nothing but its exit can close
    // that end. A child that had survived would leave this read blocking into
    // a second timeout instead.
    assert_eq!(
        read_available(&mut transport, 1),
        b"",
        "the child outlived the deadline that was supposed to kill it"
    );
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
fn run(transport: &mut SpawnTransport, line: &CommandLine) -> String {
    transport.send(line).expect("send failed");
    let text = read_through_end(transport);
    assert!(
        !text.lines().any(|l| l.starts_with("%error ")),
        "tmux rejected `{}`: {text}",
        line.as_str()
    );
    text
}

#[test]
fn live_tmux_startup_greeting_is_an_empty_guard_block() {
    let harness = IsolatedTmux::new("greeting");

    // The unsolicited startup greeting (DESIGN.md §3.3) arrives as a complete
    // guard block before any command of ours is sent.
    let mut transport = attach(&harness);
    let text = read_through_end(&mut transport);
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
    let mut transport = attach(&harness);
    read_through_end(&mut transport);

    let text = run(
        &mut transport,
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

/// Removes its file on drop. The cleanup has to ride on `Drop` the way
/// [`IsolatedTmux`]'s does: an assertion inside the hostile-argument loop
/// panics past any cleanup line written at the end of the test, which would
/// strand the buffer file in the temp dir for good.
struct TempFile(std::path::PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn live_tmux_reads_every_encoded_argument_back_byte_for_byte() {
    let harness = IsolatedTmux::new("encoding");
    let saved = TempFile(
        std::env::temp_dir().join(format!("tmux-phoenix-test-encoding-{}", std::process::id())),
    );
    let saved_path = saved.0.to_str().expect("temp path is UTF-8");

    let mut transport = attach(&harness);
    read_through_end(&mut transport);
    for &arg in HOSTILE {
        run(
            &mut transport,
            &line("set-buffer", ["-b", "phx", "--", arg]),
        );
        run(
            &mut transport,
            &line("save-buffer", ["-b", "phx", saved_path]),
        );
        let read_back = std::fs::read(&saved.0).expect("save-buffer wrote no file");
        assert_eq!(
            read_back,
            arg.as_bytes(),
            "argument {arg:?} came back changed"
        );
    }

    // set-buffer refuses empty data, so the empty argument round-trips through
    // a user option instead.
    run(&mut transport, &line("set-option", ["-g", "@phx", ""]));
    let text = run(
        &mut transport,
        &line("display-message", ["-p", "[#{@phx}]"]),
    );
    assert!(
        text.lines().any(|l| l == "[]"),
        "expected the option to hold the empty string, got: {text}"
    );

    transport.close();
}
