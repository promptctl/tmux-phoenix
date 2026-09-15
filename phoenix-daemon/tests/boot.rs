//! Live: `connect_and_boot` against a real, isolated tmux server, for each
//! branch it can take.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use phoenix_core::{
    CapturedProgram, FormatVersion, Layout, NonEmpty, OffsetDateTime, Pane, PaneId, PaneIndex,
    ProgramName, Session, SessionName, Snapshot, TmuxVersion, Utf8PathBuf, Window, WindowIndex,
    WindowName,
};
use phoenix_store::Store;
use tmux_control::CommandLine;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_name(name: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{name}-{}-{nanos}-{n}", std::process::id())
}

struct EmptyServer {
    socket: String,
}

impl EmptyServer {
    /// Deliberately does *not* create any session — these tests are about
    /// what happens when the server the daemon connects to is completely
    /// fresh, the exact scenario `boot.rs` exists for.
    fn new(name: &str) -> Self {
        let socket = format!("/tmp/{}", unique_name(&format!("phx-boot-{name}")));
        let _ = std::fs::remove_file(&socket);
        Self { socket }
    }

    fn session_names(&self) -> Vec<String> {
        let out = std::process::Command::new("tmux")
            .args(["-S", &self.socket, "list-sessions", "-F", "#{session_name}"])
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
    }
}

impl Drop for EmptyServer {
    fn drop(&mut self) {
        // Kill every session this test's server ended up with, one at a
        // time -- never a server-wide kill.
        for name in self.session_names() {
            let _ = std::process::Command::new("tmux")
                .args(["-S", &self.socket, "kill-session", "-t", &name])
                .status();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

struct TestDataDir(PathBuf);

impl TestDataDir {
    fn new(name: &str) -> Self {
        Self(std::env::temp_dir().join(unique_name(&format!("phx-boot-data-{name}"))))
    }
}

impl Drop for TestDataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn snapshot_with_program(session_name: &str, program: CapturedProgram) -> Snapshot {
    let pane = Pane {
        id: PaneId(0),
        index: PaneIndex(0),
        cwd: Utf8PathBuf::parse("/tmp"),
        program,
        content: None,
    };
    let window = Window::new(
        WindowIndex(0),
        WindowName::parse("shell").unwrap(),
        Layout::parse("b25d,80x24,0,0,0").unwrap(),
        NonEmpty::singleton(pane),
        PaneIndex(0),
    )
    .unwrap();
    let session = Session::new(
        SessionName::parse(session_name).unwrap(),
        NonEmpty::singleton(window),
        WindowIndex(0),
    )
    .unwrap();
    Snapshot {
        format_version: FormatVersion::CURRENT,
        tmux_version: TmuxVersion { major: 3, minor: 5 },
        captured_at: OffsetDateTime::from_unix_timestamp(1_700_000_000),
        sessions: NonEmpty::singleton(session),
    }
}

fn single_pane_snapshot(session_name: &str) -> Snapshot {
    snapshot_with_program(
        session_name,
        CapturedProgram {
            command: ProgramName::parse("zsh").unwrap(),
            argv: None,
        },
    )
}

/// Blocks until the server holds only bootstrap sessions as `probe` sees
/// them: a session just created is still starting its shell.
fn wait_until_bootstrap_only(socket: &str) {
    for _ in 0..50 {
        if let Ok(phoenix_restore::ServerState::BootstrapOnly(_)) =
            phoenix_restore::probe(Some(socket))
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{socket} never settled into a bootstrap-only server within 5s");
}

/// tmux-parity-ure.j0f criterion 1: a terminal that reaches tmux before the
/// daemon leaves a lone bootstrap session, named `0` exactly like the
/// snapshot's own session. Boot restore puts the snapshot in its place.
#[test]
fn boots_over_a_lone_bootstrap_session_by_restoring_in_its_place() {
    let server = EmptyServer::new("login-race");
    let status = std::process::Command::new("tmux")
        .args(["-S", &server.socket, "new-session", "-d", "-s", "0"])
        .status()
        .expect("failed to start the login terminal's session");
    assert!(status.success());
    wait_until_bootstrap_only(&server.socket);

    let data_dir = TestDataDir::new("login-race");
    let store = Store::new(&data_dir.0);
    store
        .save(
            &single_pane_snapshot("0"),
            std::num::NonZeroUsize::new(5).unwrap(),
            Duration::ZERO,
        )
        .expect("failed to seed a snapshot to restore");

    let mut log = Vec::new();
    let mut client = phoenix_daemon::connect_and_boot(
        Some(server.socket.clone()),
        &store,
        |line| log.push(line.to_string()),
        drop,
    )
    .expect("connect_and_boot failed")
    .expect("a bootstrap-only server with a snapshot yields a client");

    assert!(
        log.iter().any(|l| l.contains("restored 1 session")),
        "{log:?}"
    );
    assert_eq!(
        server.session_names(),
        vec!["0".to_string()],
        "the login session should be replaced, with no scaffolding left"
    );
    let windows = std::process::Command::new("tmux")
        .args([
            "-S",
            &server.socket,
            "list-windows",
            "-t",
            "=0",
            "-F",
            "#{window_name}",
        ])
        .output()
        .expect("failed to list the restored session's windows");
    assert_eq!(
        String::from_utf8_lossy(&windows.stdout).trim(),
        "shell",
        "session 0 should be the snapshot's, not the login terminal's"
    );

    client.close();
}

#[test]
fn boots_with_no_sessions_and_no_snapshot_waits_without_starting_a_server() {
    let server = EmptyServer::new("nothing-to-restore");
    let data_dir = TestDataDir::new("nothing-to-restore");
    let store = Store::new(&data_dir.0);

    let mut log = Vec::new();
    let booted = phoenix_daemon::connect_and_boot(
        Some(server.socket.clone()),
        &store,
        |line| log.push(line.to_string()),
        drop,
    )
    .expect("connect_and_boot failed");

    assert!(booted.is_none(), "there is nothing to attach to");
    assert!(log.iter().any(|l| l.contains("no saved snapshot")));
    assert!(
        server.session_names().is_empty(),
        "the daemon must not start a server nobody asked for"
    );
}

#[test]
fn boots_with_no_sessions_and_a_snapshot_restores_and_removes_the_bootstrap_session() {
    let server = EmptyServer::new("restore");
    let data_dir = TestDataDir::new("restore");
    let store = Store::new(&data_dir.0);

    let session_name = unique_name("restored-session");
    store
        .save(
            &single_pane_snapshot(&session_name),
            std::num::NonZeroUsize::new(5).unwrap(),
            Duration::ZERO,
        )
        .expect("failed to seed a snapshot to restore");

    let mut log = Vec::new();
    let mut client = phoenix_daemon::connect_and_boot(
        Some(server.socket.clone()),
        &store,
        |line| log.push(line.to_string()),
        drop,
    )
    .expect("connect_and_boot failed")
    .expect("a server with sessions, or a snapshot to restore, yields a client");

    assert!(log.iter().any(|l| l.contains("restored 1 session")));

    let sessions = server.session_names();
    assert_eq!(
        sessions,
        vec![session_name.clone()],
        "the throwaway bootstrap session should be gone, leaving only the restored one"
    );

    // The client should have reconnected onto the restored session, not be
    // left dangling on the now-destroyed bootstrap one.
    let out = client
        .execute(
            &CommandLine::new("display-message", ["-p", "#{session_name}"])
                .expect("no NUL in a literal format"),
        )
        .expect("client should still be usable after boot");
    assert_eq!(
        String::from_utf8_lossy(&out.lines[0]),
        session_name.as_str()
    );

    client.close();
}

#[test]
fn boots_with_an_existing_session_never_touches_it() {
    let server = EmptyServer::new("stay-live");
    let existing_session = unique_name("pre-existing");
    let status = std::process::Command::new("tmux")
        .args([
            "-S",
            &server.socket,
            "new-session",
            "-d",
            "-s",
            &existing_session,
        ])
        .status()
        .expect("failed to pre-create a session");
    assert!(status.success());
    // Built out, so this is a session the user made, not a bootstrap one.
    let status = std::process::Command::new("tmux")
        .args([
            "-S",
            &server.socket,
            "split-window",
            "-t",
            &existing_session,
        ])
        .status()
        .expect("failed to split the pre-existing session");
    assert!(status.success());

    let data_dir = TestDataDir::new("stay-live");
    let store = Store::new(&data_dir.0);
    // Even with a snapshot available, an already-live server must never be
    // restored into (DESIGN.md: "never clobber a live server").
    store
        .save(
            &single_pane_snapshot(&unique_name("would-be-restored")),
            std::num::NonZeroUsize::new(5).unwrap(),
            Duration::ZERO,
        )
        .unwrap();

    let mut log = Vec::new();
    let mut client = phoenix_daemon::connect_and_boot(
        Some(server.socket.clone()),
        &store,
        |line| log.push(line.to_string()),
        drop,
    )
    .expect("connect_and_boot failed")
    .expect("a server with sessions, or a snapshot to restore, yields a client");

    assert!(
        log.iter()
            .any(|l| l.contains("staying in save mode") && l.contains(&existing_session)),
        "the daemon should say which session kept it from restoring: {log:?}"
    );
    assert_eq!(
        server.session_names(),
        vec![existing_session],
        "the pre-existing session must be untouched, and no bootstrap/restored session added"
    );

    client.close();
}

/// An unattended boot restore brings each pane's captured program back —
/// proving that end to end, including that the program actually runs on the
/// restored server, not just that the plan contains the right command
/// (that's `phoenix-restore`'s own unit/live tests).
#[test]
fn a_panes_captured_program_is_relaunched_on_boot() {
    let server = EmptyServer::new("relaunch");
    let data_dir = TestDataDir::new("relaunch");
    let store = Store::new(&data_dir.0);

    let session_name = unique_name("relaunch-session");
    let program = CapturedProgram {
        command: ProgramName::parse("echo").unwrap(),
        argv: Some(NonEmpty::new(
            "echo".to_string(),
            vec!["DISTINCTIVE-BOOT-RELAUNCH-MARKER".to_string()],
        )),
    };
    store
        .save(
            &snapshot_with_program(&session_name, program),
            std::num::NonZeroUsize::new(5).unwrap(),
            Duration::ZERO,
        )
        .expect("failed to seed a snapshot to restore");

    let mut client =
        phoenix_daemon::connect_and_boot(Some(server.socket.clone()), &store, |_| {}, drop)
            .expect("connect_and_boot failed")
            .expect("a snapshot to restore yields a client");

    // Never assume the target-side pane index matches the snapshot's
    // captured one (`phoenix-restore` never targets panes by index at all —
    // see its `panes_active_last` doc comment); enumerate live indices.
    let window_target = format!("{session_name}:0");
    let mut found_marker = false;
    for _ in 0..30 {
        let panes_out = client
            .execute(
                &CommandLine::new(
                    "list-panes",
                    ["-t", window_target.as_str(), "-F", "#{pane_index}"],
                )
                .expect("no NUL in a test target"),
            )
            .unwrap();
        let mut all_text = String::new();
        for line in &panes_out.lines {
            let idx = String::from_utf8_lossy(line);
            let pane_target = format!("{session_name}:0.{idx}");
            let out = client
                .execute(
                    &CommandLine::new("capture-pane", ["-p", "-t", pane_target.as_str()])
                        .expect("no NUL in a test target"),
                )
                .unwrap();
            for line in &out.lines {
                all_text.push_str(&String::from_utf8_lossy(line));
                all_text.push('\n');
            }
        }
        if all_text.contains("DISTINCTIVE-BOOT-RELAUNCH-MARKER") {
            found_marker = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        found_marker,
        "boot restore should have relaunched the pane's captured program"
    );

    client.close();
}
