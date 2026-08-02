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
    let data_dir = TestDataDir::new("save-list");

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
        3,
        "expected captured_at\\tformat_version\\tpath"
    );
    assert_eq!(fields[2], saved_path);

    let store = phoenix_store::Store::new(data_dir.0.join("tmux-phoenix"));
    let loaded = store
        .load_latest()
        .expect("saved snapshot should load back");
    assert_eq!(loaded.sessions.first().name().as_str(), harness.session);
}

#[test]
fn save_exit_code_is_zero_when_a_pane_is_idle() {
    let harness = IsolatedTmux::new("cli-exit-code");
    let data_dir = TestDataDir::new("exit-code");

    // Give the shell a moment to settle at its prompt so argv recovery
    // reliably finds it as the foreground process.
    std::thread::sleep(std::time::Duration::from_millis(200));

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
    let data_dir = TestDataDir::new("keep");

    for _ in 0..3 {
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

#[test]
fn restore_dry_run_prints_commands_and_touches_nothing() {
    let harness = IsolatedTmux::new("cli-restore-dry-run");
    let data_dir = TestDataDir::new("restore-dry-run");

    let save = Command::new(phoenix_bin())
        .args(["save", "--socket", &harness.socket])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix save");
    assert!(save.status.success());

    let dry_run = Command::new(phoenix_bin())
        .args(["restore", "--dry-run"])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix restore --dry-run");

    assert!(dry_run.status.success());
    let stdout = String::from_utf8(dry_run.stdout).unwrap();
    assert!(
        stdout.contains("new-session"),
        "dry-run output should contain the plan's commands, got: {stdout:?}"
    );

    // Only one session should exist on the server: --dry-run must not have
    // created anything.
    let sessions = Command::new("tmux")
        .args([
            "-S",
            &harness.socket,
            "list-sessions",
            "-F",
            "#{session_name}",
        ])
        .output()
        .expect("failed to list sessions");
    let session_names = String::from_utf8(sessions.stdout).unwrap();
    assert_eq!(session_names.lines().count(), 1);
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

    let save = Command::new(phoenix_bin())
        .args(["save", "--socket", &harness.socket])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix save");
    assert!(save.status.success());

    // A second, unrelated session that survives: `connect()` attaches via
    // bare `attach-session` (see its doc comment), which needs *some*
    // existing session on the server -- bootstrapping a completely
    // empty/nonexistent server is out of scope here (that's the daemon's
    // boot-restore job, a later milestone). Simulate "the captured session
    // is gone, restore it" by killing only that one.
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

    let _ = Command::new("tmux")
        .args(["-S", &harness.socket, "kill-session", "-t", "keepalive"])
        .status();
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
        saved.sessions.first().active_window().panes().len(),
        2,
        "the save should reflect the split that triggered it"
    );
}
