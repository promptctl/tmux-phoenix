//! Live: `connect_and_boot` against a real, isolated tmux server — proving
//! the two verified-surprising mechanics in `boot.rs`'s doc comment
//! actually work end to end, not just that the pure decision is right.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use phoenix_core::{
    CapturedProgram, FormatVersion, Layout, NonEmpty, OffsetDateTime, Pane, PaneIndex, Session,
    SessionName, Snapshot, TmuxVersion, Window, WindowIndex, WindowName,
};
use phoenix_store::Store;
use tmux_control::CommandLine;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// `connect_and_boot`'s `RestoreLatest` path reads `XDG_CONFIG_HOME`
/// (`phoenix_restore::default_rules_path`) from the process's ambient
/// environment — every test in this binary that reaches that path (whether
/// or not it itself overrides the var) must hold this lock, since env vars
/// are process-global and `cargo test` runs this file's tests concurrently
/// on threads within one process.
static ENV_LOCK: Mutex<()> = Mutex::new(());

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
    let _guard = ENV_LOCK.lock().unwrap();
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
    let _guard = ENV_LOCK.lock().unwrap();
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
    let _guard = ENV_LOCK.lock().unwrap();
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

fn snapshot_with_program(session_name: &str, program: CapturedProgram) -> Snapshot {
    let pane = Pane {
        index: PaneIndex(0),
        cwd: "/tmp".into(),
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

/// tmux-permissions-16s: boot restore never prompts, but it still *honors*
/// a rule the user already granted in an earlier interactive session — this
/// proves that end to end, including that the program actually runs on the
/// restored server, not just that a `RestorePolicy` gets built correctly
/// (that's `phoenix-restore`'s own unit/live tests).
#[test]
fn a_pane_whose_program_already_has_a_restore_rule_gets_relaunched_on_boot() {
    let _guard = ENV_LOCK.lock().unwrap();
    let server = EmptyServer::new("relaunch-granted");
    let data_dir = TestDataDir::new("relaunch-granted");
    let store = Store::new(&data_dir.0);
    let config_dir = std::env::temp_dir().join(unique_name("phx-boot-config"));

    let session_name = unique_name("relaunch-session");
    let program = CapturedProgram::new(
        "echo",
        vec![
            "echo".to_string(),
            "DISTINCTIVE-BOOT-RELAUNCH-MARKER".to_string(),
        ],
    );
    store
        .save(&snapshot_with_program(&session_name, program), 5)
        .expect("failed to seed a snapshot to restore");

    let ruleset = phoenix_restore::RuleSet {
        rules: vec![phoenix_restore::Rule {
            matcher: phoenix_restore::Matcher::Basename("echo".to_string()),
            verdict: phoenix_restore::Verdict::Restore,
        }],
    };
    phoenix_restore::save_rules_file(&config_dir.join("tmux-phoenix/relaunch.rules"), &ruleset)
        .expect("failed to seed the relaunch rules file");

    // SAFETY: serialized against every other test in this binary that
    // reaches `connect_and_boot`'s `RestoreLatest` path via `ENV_LOCK`.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", &config_dir);
    }
    let result = phoenix_daemon::connect_and_boot(Some(server.socket.clone()), &store, |_| {});
    unsafe {
        std::env::remove_var("XDG_CONFIG_HOME");
    }
    let mut client = result.expect("connect_and_boot failed");

    // Never assume the target-side pane index matches the snapshot's
    // captured one (`phoenix-restore` never targets panes by index at all —
    // see its `panes_active_last` doc comment); enumerate live indices.
    let mut found_marker = false;
    for _ in 0..30 {
        let panes_out = client
            .execute(&format!(
                "list-panes -t {session_name}:0 -F '#{{pane_index}}'"
            ))
            .unwrap();
        let mut all_text = String::new();
        for line in &panes_out.lines {
            let idx = String::from_utf8_lossy(line);
            let out = client
                .execute(&format!("capture-pane -p -t '{session_name}:0.{idx}'"))
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
        "boot restore should have relaunched the already-granted program"
    );

    client.close();
    let _ = std::fs::remove_dir_all(&config_dir);
}
