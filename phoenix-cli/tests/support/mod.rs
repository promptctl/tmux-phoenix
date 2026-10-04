//! Shared test-only harness for spawning an isolated, throw-away tmux
//! server that live-integration tests can safely drive without touching the
//! developer's real tmux sessions. Duplicated (not shared via a lib crate)
//! from `tmux-control/tests/support/mod.rs` — see that copy's doc comment
//! for why.

pub struct IsolatedTmux {
    pub socket: String,
    pub session: String,
}

impl IsolatedTmux {
    pub fn new(name: &str) -> Self {
        let socket = format!("/tmp/tmux-phoenix-test-{name}-{}", std::process::id());
        let session = format!("phoenix-test-{name}");
        let status = std::process::Command::new("tmux")
            .args(["-S", &socket, "new-session", "-d", "-s", &session])
            .status()
            .expect("failed to run tmux new-session");
        assert!(status.success(), "tmux new-session failed");
        Self { socket, session }
    }

    /// Splits the session's window so the server holds a session the user
    /// built. A lone window with one idle shell is a bootstrap session, which
    /// `save` refuses to publish and restore replaces.
    pub fn build(&self) {
        let status = std::process::Command::new("tmux")
            .args(["-S", &self.socket, "split-window", "-t", &self.session])
            .status()
            .expect("failed to run tmux split-window");
        assert!(status.success(), "tmux split-window failed");
    }

    /// Blocks until a save would come back clean: every pane reports a
    /// working directory, every pane's process holds its terminal's
    /// foreground (`ps` STAT carries `+`), and no pane has produced output
    /// across three consecutive reads. A pane just created is none of
    /// these — its shell is still running its startup files, which briefly
    /// hand the terminal to their own children — and under a loaded test
    /// run that window is wide enough for `save` to exit 3.
    pub fn wait_until_settled(&self) {
        let mut last = None;
        let mut stable_reads = 0;
        for _ in 0..100 {
            let now = self.pane_listing();
            stable_reads = if Some(&now) == last.as_ref() {
                stable_reads + 1
            } else {
                0
            };
            if stable_reads >= 3 && panes_settled(&now) {
                return;
            }
            last = Some(now);
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("panes on {} never settled within 10s", self.socket);
    }

    /// One row per pane: pid, history indicator, cwd.
    fn pane_listing(&self) -> Vec<String> {
        let out = std::process::Command::new("tmux")
            .args([
                "-S",
                &self.socket,
                "list-panes",
                "-a",
                "-F",
                "#{pane_pid} #{history_size} #{history_bytes} #{pane_current_path}",
            ])
            .output()
            .expect("failed to run tmux list-panes");
        assert!(out.status.success(), "list-panes failed on {}", self.socket);
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn panes_settled(listing: &[String]) -> bool {
    !listing.is_empty()
        && listing.iter().all(|row| {
            let mut fields = row.splitn(4, ' ');
            let pid = fields.next().unwrap_or("");
            let cwd = fields.nth(2).unwrap_or("");
            !cwd.is_empty() && in_foreground(pid)
        })
}

fn in_foreground(pid: &str) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains('+'))
        .unwrap_or(false)
}

impl Drop for IsolatedTmux {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux")
            .args(["-S", &self.socket, "kill-session", "-t", &self.session])
            .status();
    }
}
