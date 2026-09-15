//! Live: `connect_and_apply` against a server a login terminal reached first
//! (tmux-parity-ure.j0f), and against one holding a session the user built.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use phoenix_core::{
    CapturedProgram, FormatVersion, Layout, NonEmpty, OffsetDateTime, Pane, PaneId, PaneIndex,
    ProgramName, Session, SessionName, Snapshot, TmuxVersion, Window, WindowIndex, WindowName,
};
use phoenix_restore::{connect_and_apply, plan, probe, ServerState};

/// A throwaway tmux server on its own socket, torn down one session at a
/// time — never a server-wide kill.
struct Server(String);

impl Server {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(format!(
            "/tmp/phx-restore-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn tmux(&self, args: &[&str]) -> String {
        let out = std::process::Command::new("tmux")
            .args(["-S", &self.0])
            .args(args)
            .output()
            .expect("failed to run tmux");
        assert!(
            out.status.success(),
            "tmux {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn lines(&self, args: &[&str]) -> Vec<String> {
        let mut lines: Vec<String> = self.tmux(args).lines().map(str::to_string).collect();
        lines.sort();
        lines
    }

    fn session_names(&self) -> Vec<String> {
        self.lines(&["list-sessions", "-F", "#{session_name}"])
    }

    /// Blocks until `probe` sees only bootstrap sessions: a session just
    /// created is still starting its shell.
    fn wait_until_bootstrap_only(&self) {
        for _ in 0..50 {
            if let Ok(ServerState::BootstrapOnly(_)) = probe(Some(&self.0)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "{} never settled into a bootstrap-only server within 5s",
            self.0
        );
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let listing = std::process::Command::new("tmux")
            .args(["-S", &self.0, "list-sessions", "-F", "#{session_name}"])
            .output();
        if let Ok(out) = listing {
            for name in String::from_utf8_lossy(&out.stdout).lines() {
                let _ = std::process::Command::new("tmux")
                    .args(["-S", &self.0, "kill-session", "-t", &format!("={name}")])
                    .status();
            }
        }
        let _ = std::fs::remove_file(&self.0);
    }
}

fn session(name: &str, window_names: &[&str]) -> Session {
    let windows = window_names
        .iter()
        .enumerate()
        .map(|(index, window_name)| {
            let pane = Pane {
                id: PaneId(index as u32),
                index: PaneIndex(0),
                cwd: None,
                program: CapturedProgram {
                    command: ProgramName::parse("zsh").unwrap(),
                    argv: None,
                },
                content: None,
            };
            Window::new(
                WindowIndex(index as u32),
                WindowName::parse(*window_name).unwrap(),
                Layout::parse("b25d,80x24,0,0,0").unwrap(),
                NonEmpty::singleton(pane),
                PaneIndex(0),
            )
            .unwrap()
        })
        .collect();
    Session::new(
        SessionName::parse(name).unwrap(),
        NonEmpty::from_vec(windows).unwrap(),
        WindowIndex(0),
    )
    .unwrap()
}

fn snapshot(sessions: Vec<Session>) -> Snapshot {
    Snapshot {
        format_version: FormatVersion::CURRENT,
        tmux_version: TmuxVersion { major: 3, minor: 6 },
        captured_at: OffsetDateTime::from_unix_timestamp(1_700_000_000),
        sessions: NonEmpty::from_vec(sessions).unwrap(),
    }
}

/// Criterion 1 with a real terminal attached: the login session shares the
/// snapshot's session name, and a terminal client (hosted in a pane of a
/// second server, so it has a real pty) is attached to it. Restore replaces
/// the session and moves the terminal onto the restored one instead of
/// detaching it.
#[test]
fn restoring_over_a_login_session_replaces_it_and_keeps_its_terminal() {
    let inner = Server::new("login");
    let host = Server::new("login-host");
    inner.tmux(&["new-session", "-d", "-s", "0", "-x", "80", "-y", "24"]);
    // The pane's command runs through the user's shell, where zsh would read
    // an unquoted `=0` as "the path of a command named 0", so it is quoted.
    host.tmux(&[
        "new-session",
        "-d",
        "-s",
        "terminal",
        &format!("tmux -S {} attach -t '=0'", inner.0),
    ]);
    let mut attached = false;
    for _ in 0..50 {
        if inner.lines(&["list-clients", "-F", "#{session_name}"]) == ["0"] {
            attached = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(attached, "the terminal never attached to the login session");
    inner.wait_until_bootstrap_only();

    let restored = snapshot(vec![
        session("0", &["editor", "logs"]),
        session("work", &["shell"]),
    ]);
    let (mut client, _outcome) =
        connect_and_apply(Some(inner.0.clone()), &restored, &plan(&restored), drop)
            .expect("restore over the login session failed");
    client.close();

    assert_eq!(inner.session_names(), ["0", "work"], "no scaffolding left");
    assert_eq!(
        inner.lines(&["list-windows", "-t", "=0", "-F", "#{window_name}"]),
        ["editor", "logs"],
        "session 0 should be the snapshot's"
    );
    assert_eq!(
        inner.lines(&["list-clients", "-F", "#{session_name}"]),
        ["0"],
        "the terminal should now show the restored session"
    );
    assert_eq!(
        host.tmux(&["list-panes", "-t", "=terminal", "-F", "#{pane_dead}"])
            .trim(),
        "0",
        "the terminal's tmux client should still be running"
    );
}

/// A server holding a session the user built is only added to.
#[test]
fn restoring_into_a_built_server_adds_beside_its_sessions() {
    let server = Server::new("built");
    server.tmux(&["new-session", "-d", "-s", "mine"]);
    server.tmux(&["split-window", "-t", "=mine:"]);

    let restored = snapshot(vec![session("work", &["shell"])]);
    let (mut client, _outcome) =
        connect_and_apply(Some(server.0.clone()), &restored, &plan(&restored), drop)
            .expect("restore into a built server failed");
    client.close();

    assert_eq!(server.session_names(), ["mine", "work"]);
    assert_eq!(
        server
            .lines(&["list-panes", "-t", "=mine", "-F", "#{pane_index}"])
            .len(),
        2,
        "the user's session is untouched"
    );
}
