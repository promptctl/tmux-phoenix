//! End-to-end tests: the actual compiled `phoenix` binary, run as a
//! subprocess against a real isolated tmux server and a throwaway
//! `XDG_DATA_HOME`, exercising exactly what a real invocation would.

mod support;
use support::IsolatedTmux;

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDataDir(PathBuf);

impl TestDataDir {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "phoenix-cli-test-{name}-{}-{nanos}-{n}",
            std::process::id()
        ));
        Self(path)
    }
}

impl Drop for TestDataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn phoenix_bin() -> &'static str {
    env!("CARGO_BIN_EXE_phoenix")
}

#[test]
fn save_then_list_round_trips_through_the_real_binary() {
    let harness = IsolatedTmux::new("cli-save-list");
    harness.build();
    let data_dir = TestDataDir::new("save-list");

    harness.wait_until_settled();
    let save = Command::new(phoenix_bin())
        .args(["save", "--socket", &harness.socket])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix save");

    assert!(
        save.status.success(),
        "phoenix save failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&save.stdout),
        String::from_utf8_lossy(&save.stderr)
    );
    let saved_path = String::from_utf8(save.stdout).unwrap().trim().to_string();
    assert!(
        std::path::Path::new(&saved_path).exists(),
        "phoenix save printed a path that doesn't exist: {saved_path}"
    );

    let list = Command::new(phoenix_bin())
        .args(["list"])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix list");

    assert!(list.status.success());
    let stdout = String::from_utf8(list.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "expected exactly one generation, got: {stdout:?}"
    );
    let fields: Vec<&str> = lines[0].split('\t').collect();
    assert_eq!(
        fields.len(),
        5,
        "expected captured_at\\tformat_version\\torigin\\ttag\\tpath"
    );
    assert_eq!(fields[1], "2");
    assert_ne!(fields[2], "-", "a fresh save records its server's identity");
    assert_eq!(fields[3], "-");
    assert_eq!(fields[4], saved_path);

    let store = phoenix_store::Store::new(data_dir.0.join("tmux-phoenix"));
    let loaded = store
        .load_latest()
        .expect("saved snapshot should load back");
    assert_eq!(loaded.sessions().first().name().as_str(), harness.session);
}

#[test]
fn save_exit_code_is_zero_when_a_pane_is_idle() {
    let harness = IsolatedTmux::new("cli-exit-code");
    harness.build();
    let data_dir = TestDataDir::new("exit-code");

    harness.wait_until_settled();
    let save = Command::new(phoenix_bin())
        .args(["save", "--socket", &harness.socket])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix save");

    assert_eq!(
        save.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&save.stderr)
    );
}

#[test]
fn save_refuses_a_server_holding_only_a_bootstrap_session_and_writes_nothing() {
    // A fresh `tmux` from a login terminal: one session, one window, one
    // idle shell. Publishing it would replace the generation it is about to
    // be restored from.
    let harness = IsolatedTmux::new("cli-bootstrap-refused");
    let data_dir = TestDataDir::new("bootstrap-refused");

    harness.wait_until_settled();
    let save = Command::new(phoenix_bin())
        .args(["save", "--socket", &harness.socket])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix save");

    assert_eq!(save.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&save.stderr);
    assert!(stderr.contains("not saved"), "stderr: {stderr}");
    let store = phoenix_store::Store::new(data_dir.0.join("tmux-phoenix"));
    assert!(
        store.list().expect("an empty store lists").is_empty(),
        "a refused save must leave no generation behind"
    );
}

#[test]
fn list_before_any_save_succeeds_with_empty_output() {
    let data_dir = TestDataDir::new("empty-list");

    let list = Command::new(phoenix_bin())
        .args(["list"])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix list");

    assert!(list.status.success());
    assert!(String::from_utf8(list.stdout).unwrap().is_empty());
}

#[test]
fn save_respects_the_keep_flag() {
    let harness = IsolatedTmux::new("cli-keep");
    harness.build();
    let data_dir = TestDataDir::new("keep");

    for _ in 0..3 {
        harness.wait_until_settled();
        let save = Command::new(phoenix_bin())
            .args(["save", "--socket", &harness.socket, "--keep", "1"])
            .env("XDG_DATA_HOME", &data_dir.0)
            .output()
            .expect("failed to run phoenix save");
        assert!(save.status.success());
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    let list = Command::new(phoenix_bin())
        .args(["list"])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix list");
    let stdout = String::from_utf8(list.stdout).unwrap();
    assert_eq!(
        stdout.lines().count(),
        1,
        "expected --keep 1 to prune down to one generation"
    );
}

#[test]
fn help_exits_zero_and_prints_usage() {
    let output = Command::new(phoenix_bin())
        .args(["--help"])
        .output()
        .expect("failed to run phoenix --help");
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().contains("USAGE"));
}

#[test]
fn unknown_subcommand_exits_nonzero() {
    let output = Command::new(phoenix_bin())
        .args(["bogus"])
        .output()
        .expect("failed to run phoenix bogus");
    assert!(!output.status.success());
}

/// A socket path with no server on it.
fn empty_socket(name: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket = format!(
        "/tmp/phoenix-cli-test-{name}-{}-{nanos}",
        std::process::id()
    );
    let _ = std::fs::remove_file(&socket);
    socket
}

fn sessions_on(socket: &str) -> Vec<String> {
    let out = Command::new("tmux")
        .args(["-S", socket, "list-sessions", "-F", "#{session_name}"])
        .output()
        .expect("failed to run tmux list-sessions");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn restore_dry_run_prints_the_plan_and_touches_nothing() {
    let harness = IsolatedTmux::new("cli-restore-dry-run");
    harness.build();
    let data_dir = TestDataDir::new("restore-dry-run");

    harness.wait_until_settled();
    let save = Command::new(phoenix_bin())
        .args(["save", "--socket", &harness.socket])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix save");
    assert!(save.status.success());
    let dry_run = |socket: &str| {
        let out = Command::new(phoenix_bin())
            .args(["restore", "--dry-run", "--socket", socket])
            .env("XDG_DATA_HOME", &data_dir.0)
            .output()
            .expect("failed to run phoenix restore --dry-run");
        assert!(
            out.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };

    // Onto no server: everything is built in a session made to attach
    // with, which the plan ends by moving its clients off and removing.
    let target = empty_socket("dry-run-target");
    let plan = dry_run(&target);
    let lines: Vec<&str> = plan.lines().collect();
    assert!(
        lines[0].starts_with(&format!("w0 p0 = new-session -s {}", harness.session)),
        "{plan}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("p1 = split-window p0")),
        "{plan}"
    );
    assert_eq!(
        lines[lines.len() - 3..],
        [
            format!(
                "switch-client every client on phoenix-scratch-0 to {}",
                harness.session
            )
            .as_str(),
            "kill-session phoenix-scratch-0",
            lines[lines.len() - 1],
        ],
        "{plan}"
    );
    assert!(
        lines[lines.len() - 1].starts_with("set-option server @phoenix-generation "),
        "{plan}"
    );
    assert!(
        sessions_on(&target).is_empty(),
        "--dry-run must not have started a server"
    );

    // Onto the server it was saved from: every window is already there, no
    // session was made to attach with, and nothing is created or removed.
    let plan = dry_run(&harness.socket);
    assert!(
        plan.lines().all(|l| l.starts_with("set-option ")),
        "only the marks of a finished restore are left to set: {plan}"
    );
    assert_eq!(
        sessions_on(&harness.socket),
        std::slice::from_ref(&harness.session)
    );
    let mark = Command::new("tmux")
        .args(["-S", &harness.socket, "show-options", "-s", "-q", "-v"])
        .arg("@phoenix-generation")
        .output()
        .expect("failed to read the server mark");
    assert!(
        mark.stdout.is_empty(),
        "--dry-run must not have set the mark"
    );
}

#[test]
fn restore_rebuilds_a_killed_session_onto_the_same_server() {
    let harness = IsolatedTmux::new("cli-restore-apply");
    let data_dir = TestDataDir::new("restore-apply");

    // Give the captured session some real structure beyond the default
    // single window/pane.
    let status = Command::new("tmux")
        .args([
            "-S",
            &harness.socket,
            "split-window",
            "-t",
            &harness.session,
        ])
        .status()
        .expect("failed to split-window");
    assert!(status.success());

    harness.wait_until_settled();
    let save = Command::new(phoenix_bin())
        .args(["save", "--socket", &harness.socket])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix save");
    assert!(
        save.status.success(),
        "phoenix save: {:?} stderr={}",
        save.status.code(),
        String::from_utf8_lossy(&save.stderr)
    );

    // This is the populated-server case (tmux-parity-ure.1 criterion 3): a
    // second, unrelated session survives, so restore attaches to the live
    // server with no bootstrap involved -- the empty/not-running cases get
    // their own tests below. Simulate "the captured session is gone, restore
    // it" by killing only the captured one and keeping this one alive.
    let status = Command::new("tmux")
        .args([
            "-S",
            &harness.socket,
            "new-session",
            "-d",
            "-s",
            "keepalive",
        ])
        .status()
        .expect("failed to create the keepalive session");
    assert!(status.success());
    let status = Command::new("tmux")
        .args(["-S", &harness.socket, "split-window", "-t", "keepalive"])
        .status()
        .expect("failed to split the keepalive session");
    assert!(status.success());
    let status = Command::new("tmux")
        .args([
            "-S",
            &harness.socket,
            "kill-session",
            "-t",
            &harness.session,
        ])
        .status()
        .expect("failed to kill-session");
    assert!(status.success());

    let restore = Command::new(phoenix_bin())
        .args(["restore", "--socket", &harness.socket])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix restore");
    assert!(
        restore.status.success(),
        "phoenix restore failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&restore.stdout),
        String::from_utf8_lossy(&restore.stderr)
    );

    let panes = Command::new("tmux")
        .args([
            "-S",
            &harness.socket,
            "list-panes",
            "-t",
            &harness.session,
            "-F",
            "#{pane_index}",
        ])
        .output()
        .expect("failed to list-panes");
    assert!(panes.status.success());
    let pane_count = String::from_utf8(panes.stdout).unwrap().lines().count();
    assert_eq!(
        pane_count, 2,
        "restored session should have the 2 panes that were captured"
    );

    // The same restore again finds everything there.
    let again = Command::new(phoenix_bin())
        .args(["restore", "--socket", &harness.socket])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix restore");
    assert!(again.status.success());
    assert_eq!(
        String::from_utf8_lossy(&again.stdout),
        "restored: 0 step(s) applied\n"
    );

    let _ = Command::new("tmux")
        .args(["-S", &harness.socket, "kill-session", "-t", "keepalive"])
        .status();
}

/// Restoring onto a socket no server runs on: restore makes a session to
/// attach with, builds the snapshot, and removes that session. A server
/// running with no sessions is not a state that lasts long enough to invoke
/// a command against — tmux exits the instant its last session goes — so
/// this is the one "nothing there" case.
#[test]
fn restore_onto_no_server_leaves_only_the_snapshot() {
    let source = IsolatedTmux::new("cli-restore-empty-source");
    let data_dir = TestDataDir::new("restore-empty");

    // Give the captured session real structure (two panes) so the assertion
    // proves the *full* tree was recreated, not just that a session appeared.
    let status = Command::new("tmux")
        .args(["-S", &source.socket, "split-window", "-t", &source.session])
        .status()
        .expect("failed to split-window");
    assert!(status.success());

    source.wait_until_settled();
    let save = Command::new(phoenix_bin())
        .args(["save", "--socket", &source.socket])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix save");
    assert!(save.status.success());

    // A separate socket with no tmux server running at all.
    let target_socket = empty_socket("empty-target");

    let restore = Command::new(phoenix_bin())
        .args(["restore", "--socket", &target_socket])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix restore");
    let restore_stderr = String::from_utf8_lossy(&restore.stderr);
    assert!(
        restore.status.success(),
        "restore onto an empty server should succeed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&restore.stdout),
        restore_stderr
    );
    assert!(
        !restore_stderr.contains("transport closed"),
        "the cryptic transport error must be gone; got stderr={restore_stderr:?}"
    );

    // Only the restored session remains: the one made to attach with is gone.
    let session_names = sessions_on(&target_socket);
    assert_eq!(
        session_names,
        vec![source.session.clone()],
        "the restored session should be the only one"
    );

    // The full pane tree was recreated.
    let panes = Command::new("tmux")
        .args([
            "-S",
            &target_socket,
            "list-panes",
            "-t",
            &source.session,
            "-F",
            "#{pane_index}",
        ])
        .output()
        .expect("failed to list-panes on the target");
    assert_eq!(
        String::from_utf8_lossy(&panes.stdout).lines().count(),
        2,
        "the restored session should have the 2 panes that were captured"
    );

    // Clean up the target server one session at a time — never a server-wide kill.
    for name in &session_names {
        let _ = Command::new("tmux")
            .args(["-S", &target_socket, "kill-session", "-t", name])
            .status();
    }
    let _ = std::fs::remove_file(&target_socket);
}

#[test]
fn daemon_saves_after_a_structural_change_through_the_real_binary() {
    let harness = IsolatedTmux::new("cli-daemon");
    let data_dir = TestDataDir::new("daemon");

    let mut child = Command::new(phoenix_bin())
        .args([
            "daemon",
            "--socket",
            &harness.socket,
            "--debounce",
            "1",
            "--max-interval",
            "3600",
        ])
        .env("XDG_DATA_HOME", &data_dir.0)
        .spawn()
        .expect("failed to spawn phoenix daemon");

    // Give the daemon a moment to connect, set no-output, and subscribe
    // before making a structural change for it to notice.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let status = Command::new("tmux")
        .args([
            "-S",
            &harness.socket,
            "split-window",
            "-t",
            &harness.session,
        ])
        .status()
        .expect("failed to split-window");
    assert!(status.success());

    let store = phoenix_store::Store::new(data_dir.0.join("tmux-phoenix"));
    let mut saved = None;
    for _ in 0..50 {
        if let Ok(s) = store.load_latest() {
            saved = Some(s);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    let _ = child.kill();
    let _ = child.wait();

    let saved = saved.expect("expected the daemon to have saved after the debounce settled");
    assert_eq!(
        saved.active_window(saved.sessions().first()).panes().len(),
        2,
        "the save should reflect the split that triggered it"
    );
}
