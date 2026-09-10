//! Live: `connect_and_boot` against a real, isolated tmux server — proving
//! the two verified-surprising mechanics in `boot.rs`'s doc comment
//! actually work end to end, not just that the pure decision is right.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use phoenix_core::{
    CapturedProgram, FormatVersion, Layout, NonEmpty, OffsetDateTime, Pane, PaneIndex, Session,
    SessionName, Snapshot, TmuxVersion, Window, WindowIndex, WindowName,
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

fn single_pane_snapshot(session_name: &str) -> Snapshot {
    let pane = Pane {
        index: PaneIndex(0),
        cwd: "/tmp".into(),
        program: CapturedProgram::new("zsh", vec![]),
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

#[test]
fn boots_with_no_sessions_and_no_snapshot_leaves_a_bare_bootstrap_session() {
    let server = EmptyServer::new("nothing-to-restore");
    let data_dir = TestDataDir::new("nothing-to-restore");
    let store = Store::new(&data_dir.0);

    let mut log = Vec::new();
    let mut client =
        phoenix_daemon::connect_and_boot(Some(server.socket.clone()), &store, |line| {
            log.push(line.to_string())
        })
        .expect("connect_and_boot failed");

    assert!(log.iter().any(|l| l.contains("no saved snapshot")));
    assert_eq!(server.session_names().len(), 1);

    client.close();
}

#[test]
fn boots_with_no_sessions_and_a_snapshot_restores_and_removes_the_bootstrap_session() {
    let server = EmptyServer::new("restore");
    let data_dir = TestDataDir::new("restore");
    let store = Store::new(&data_dir.0);

    let session_name = unique_name("restored-session");
    store
        .save(&single_pane_snapshot(&session_name), 5)
        .expect("failed to seed a snapshot to restore");

    let mut log = Vec::new();
    let mut client =
        phoenix_daemon::connect_and_boot(Some(server.socket.clone()), &store, |line| {
            log.push(line.to_string())
        })
        .expect("connect_and_boot failed");

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

    let data_dir = TestDataDir::new("stay-live");
    let store = Store::new(&data_dir.0);
    // Even with a snapshot available, an already-live server must never be
    // restored into (DESIGN.md: "never clobber a live server").
    store
        .save(&single_pane_snapshot(&unique_name("would-be-restored")), 5)
        .unwrap();

    let mut log = Vec::new();
    let mut client =
        phoenix_daemon::connect_and_boot(Some(server.socket.clone()), &store, |line| {
            log.push(line.to_string())
        })
        .expect("connect_and_boot failed");

    assert!(log.iter().any(|l| l.contains("staying in save mode")));
    assert_eq!(
        server.session_names(),
        vec![existing_session],
        "the pre-existing session must be untouched, and no bootstrap/restored session added"
    );

    client.close();
}
