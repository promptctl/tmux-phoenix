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

#[test]
fn restore_dry_run_prints_commands_and_touches_nothing() {
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

    harness.wait_until_settled();
    let save = Command::new(phoenix_bin())
        .args(["save", "--socket", &harness.socket])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix save");
    assert!(save.status.success());

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
    // Built out too: a lone idle session is a bootstrap session, which
    // restore replaces instead of keeping alongside.
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

    let _ = Command::new("tmux")
        .args(["-S", &harness.socket, "kill-session", "-t", "keepalive"])
        .status();
}

/// tmux-parity-ure.1 criteria 1, 2, 4 & 5: restoring onto a server with no
/// sessions used to die with "transport closed before the command's reply
/// arrived" because bare `attach-session` had nothing to attach to. It now
/// bootstraps a throwaway session, restores, and tears the bootstrap down.
///
/// Criteria 1 ("no server process running") and 2 ("running server, zero
/// sessions") are one and the same to `restore`: tmux tears a server down the
/// instant it has no sessions (a running-but-empty server is not a state that
/// persists long enough to invoke a separate command against), and the shared
/// connection strategy keys off `count_sessions == 0`, which is identical for
/// both. So the reachable "no server running" case exercises both.
#[test]
fn restore_bootstraps_an_empty_server_and_leaves_no_scaffolding() {
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

    // A separate socket with no tmux server running at all — the exact
    // scenario the old CLI could not handle.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let target_socket = format!(
        "/tmp/phoenix-cli-test-empty-target-{}-{nanos}",
        std::process::id()
    );
    let _ = std::fs::remove_file(&target_socket);

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

    // Only the restored session remains — the phoenix-boot scaffolding is
    // gone (criterion 4).
    let sessions = Command::new("tmux")
        .args([
            "-S",
            &target_socket,
            "list-sessions",
            "-F",
            "#{session_name}",
        ])
        .output()
        .expect("failed to list sessions on the target");
    let session_names: Vec<String> = String::from_utf8_lossy(&sessions.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        session_names,
        vec![source.session.clone()],
        "the restored session should be the only one, with no bootstrap residue"
    );

    // The full pane tree was recreated (criterion 1).
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
        saved.sessions.first().active_window().panes().len(),
        2,
        "the save should reflect the split that triggered it"
    );
}

/// Runs `tmux -S socket args…`, which must succeed, and returns its stdout.
fn tmux(socket: &str, args: &[&str]) -> String {
    let out = Command::new("tmux")
        .args(["-S", socket])
        .args(args)
        .output()
        .expect("failed to run tmux");
    assert!(
        out.status.success(),
        "tmux {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Configures the `name` hook (`pre-save`, `post-restore`, …) on a server.
fn set_hook(socket: &str, name: &str, command: &str) {
    tmux(
        socket,
        &[
            "set-option",
            "-g",
            &format!("@phoenix-hook-{name}"),
            command,
        ],
    );
}

/// A directory the hooks under test append their observations to.
fn hook_log_dir(name: &str) -> TestDataDir {
    let dir = TestDataDir::new(name);
    std::fs::create_dir_all(&dir.0).unwrap();
    dir
}

fn save(socket: &str, data_dir: &TestDataDir) -> std::process::Output {
    Command::new(phoenix_bin())
        .args(["save", "--socket", socket])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix save")
}

fn restore(socket: &str, data_dir: &TestDataDir) -> std::process::Output {
    Command::new(phoenix_bin())
        .args(["restore", "--socket", socket])
        .env("XDG_DATA_HOME", &data_dir.0)
        .output()
        .expect("failed to run phoenix restore")
}

/// A server a terminal reached first: one session, `0`, idle at `sh -i` (a
/// developer's zsh prompt runs `git`, which reads as a program). Torn down one
/// session at a time — never a server-wide kill.
struct LoginServer {
    socket: String,
}

impl LoginServer {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let server = Self {
            socket: format!(
                "/tmp/phoenix-cli-test-{name}-{}-{nanos}",
                std::process::id()
            ),
        };
        tmux(&server.socket, &["new-session", "-d", "-s", "0", "sh -i"]);
        for _ in 0..50 {
            if let Ok(phoenix_restore::ServerState::BootstrapOnly(_)) =
                phoenix_restore::probe(Some(&server.socket))
            {
                return server;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!(
            "{} never settled into a bootstrap-only server",
            server.socket
        );
    }

    fn session_names(&self) -> Vec<String> {
        let out = Command::new("tmux")
            .args(["-S", &self.socket, "list-sessions", "-F", "#{session_name}"])
            .output()
            .expect("failed to list sessions");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Drop for LoginServer {
    fn drop(&mut self) {
        for name in self.session_names() {
            let _ = Command::new("tmux")
                .args(["-S", &self.socket, "kill-session", "-t", &name])
                .status();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// tmux-parity-ure.9 criterion 1, save: pre-save runs before anything is
/// written, and post-save after, handed the generation it wrote.
#[test]
fn save_runs_its_pre_and_post_save_hooks_at_their_points() {
    let harness = IsolatedTmux::new("cli-save-hooks");
    harness.build();
    let data_dir = TestDataDir::new("save-hooks");
    let logs = hook_log_dir("save-hooks-log");
    let log = logs.0.join("hooks.log").display().to_string();
    let latest = data_dir.0.join("tmux-phoenix/latest").display().to_string();
    set_hook(
        &harness.socket,
        "pre-save",
        &format!(
            "if [ -e '{latest}' ]; then echo 'pre-save after the save'; \
             else echo 'pre-save before the save'; fi >> '{log}'"
        ),
    );
    set_hook(
        &harness.socket,
        "post-save",
        &format!("printf 'post-save %s\\n' \"$1\" >> '{log}'"),
    );

    harness.wait_until_settled();
    let save = save(&harness.socket, &data_dir);

    assert_eq!(
        save.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&save.stderr)
    );
    let saved_path = String::from_utf8(save.stdout).unwrap().trim().to_string();
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!("pre-save before the save\npost-save {saved_path}\n")
    );
}

/// tmux-parity-ure.9 criterion 2: a failing pre-save hook aborts the save with
/// a message naming the hook, and nothing is saved.
#[test]
fn a_failing_pre_save_hook_aborts_the_save_naming_the_hook() {
    let harness = IsolatedTmux::new("cli-pre-save-fails");
    harness.build();
    let data_dir = TestDataDir::new("pre-save-fails");
    set_hook(&harness.socket, "pre-save", "echo not now >&2; exit 5");

    let save = save(&harness.socket, &data_dir);

    let stderr = String::from_utf8_lossy(&save.stderr);
    assert_eq!(save.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("@phoenix-hook-pre-save") && stderr.contains("not now"),
        "the message should name the hook and carry its output: {stderr}"
    );
    assert!(
        save.stdout.is_empty(),
        "no generation path: nothing was saved"
    );
    let store = phoenix_store::Store::new(data_dir.0.join("tmux-phoenix"));
    assert!(store.list().unwrap().is_empty(), "nothing may be saved");
}

/// tmux-parity-ure.9 criterion 3: a failing post-save hook leaves the completed
/// save intact, and still reports the failure, as a degraded exit.
#[test]
fn a_failing_post_save_hook_leaves_the_save_intact_and_reports_it() {
    let harness = IsolatedTmux::new("cli-post-save-fails");
    harness.build();
    let data_dir = TestDataDir::new("post-save-fails");
    set_hook(&harness.socket, "post-save", "exit 9");

    harness.wait_until_settled();
    let save = save(&harness.socket, &data_dir);

    let stderr = String::from_utf8_lossy(&save.stderr);
    assert_eq!(save.status.code(), Some(3), "stderr: {stderr}");
    assert!(
        stderr.contains("@phoenix-hook-post-save") && stderr.contains("returned 9"),
        "{stderr}"
    );
    let saved_path = String::from_utf8(save.stdout).unwrap().trim().to_string();
    let store = phoenix_store::Store::new(data_dir.0.join("tmux-phoenix"));
    let generations = store.list().unwrap();
    assert_eq!(generations.len(), 1);
    assert_eq!(generations[0].path.display().to_string(), saved_path);
    store
        .load_latest()
        .expect("the save the post-save hook followed should load");
}

/// tmux-parity-ure.9 criteria 1 and 4, CLI restore: pre-restore runs before any
/// restored session exists, post-restore once they do, and only after it does
/// the restore mark itself finished where tmux can see it.
#[test]
fn restore_runs_its_hooks_at_their_points_then_marks_itself_finished() {
    let harness = IsolatedTmux::new("cli-restore-hooks");
    harness.build();
    let data_dir = TestDataDir::new("restore-hooks");
    let logs = hook_log_dir("restore-hooks-log");
    let log = logs.0.join("hooks.log").display().to_string();

    harness.wait_until_settled();
    let saved = save(&harness.socket, &data_dir);
    assert!(
        saved.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&saved.stderr)
    );

    // A built session keeps the server up once the captured one is gone.
    tmux(&harness.socket, &["new-session", "-d", "-s", "keepalive"]);
    tmux(&harness.socket, &["split-window", "-t", "keepalive"]);
    tmux(&harness.socket, &["kill-session", "-t", &harness.session]);

    let session = &harness.session;
    let observe = |point: &str| {
        format!(
            "if tmux has-session -t '={session}'; then echo '{point} with the session'; \
             else echo '{point} without the session'; fi >> '{log}'; \
             printf '{point} marker=%s\\n' \"$(tmux show-options -gqv @phoenix-restored)\" >> '{log}'"
        )
    };
    set_hook(&harness.socket, "pre-restore", &observe("pre-restore"));
    set_hook(&harness.socket, "post-restore", &observe("post-restore"));

    let restore = restore(&harness.socket, &data_dir);

    assert_eq!(
        restore.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&restore.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "pre-restore without the session\npre-restore marker=\n\
         post-restore with the session\npost-restore marker=\n"
    );
    let captured_at = phoenix_store::Store::new(data_dir.0.join("tmux-phoenix"))
        .load_latest()
        .unwrap()
        .captured_at
        .unix_timestamp();
    assert_eq!(
        tmux(
            &harness.socket,
            &["show-options", "-gqv", "@phoenix-restored"]
        ),
        format!("{captured_at}\n")
    );

    let _ = Command::new("tmux")
        .args(["-S", &harness.socket, "kill-session", "-t", "keepalive"])
        .status();
}

/// tmux-parity-ure.9 criterion 2, restore: a failing pre-restore hook aborts
/// the restore naming the hook. The login session restore had set aside gets
/// its name back, nothing is restored, and the restore is never marked finished.
#[test]
fn a_failing_pre_restore_hook_aborts_the_restore_and_puts_the_login_session_back() {
    let source = IsolatedTmux::new("cli-pre-restore-fails");
    source.build();
    let data_dir = TestDataDir::new("pre-restore-fails");
    source.wait_until_settled();
    let saved = save(&source.socket, &data_dir);
    assert!(
        saved.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&saved.stderr)
    );

    let login = LoginServer::new("pre-restore-fails-login");
    set_hook(
        &login.socket,
        "pre-restore",
        "echo not over my session >&2; exit 4",
    );

    let restore = restore(&login.socket, &data_dir);

    let stderr = String::from_utf8_lossy(&restore.stderr);
    assert_eq!(restore.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("@phoenix-hook-pre-restore") && stderr.contains("not over my session"),
        "{stderr}"
    );
    assert_eq!(login.session_names(), vec!["0".to_string()]);
    assert_eq!(
        tmux(
            &login.socket,
            &["show-options", "-gqv", "@phoenix-restored"]
        ),
        ""
    );
}
