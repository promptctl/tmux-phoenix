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
        Self::spawn(name, &[])
    }

    /// A session whose one pane runs `command` instead of the developer's
    /// login shell, whose startup hooks run programs of their own at times no
    /// test controls.
    pub fn with_command(name: &str, command: &str) -> Self {
        Self::spawn(name, &[command])
    }

    fn spawn(name: &str, command: &[&str]) -> Self {
        let socket = format!("/tmp/tmux-phoenix-test-{name}-{}", std::process::id());
        let session = format!("phoenix-test-{name}");
        let status = std::process::Command::new("tmux")
            .args(["-S", &socket, "new-session", "-d", "-s", &session])
            .args(command)
            .status()
            .expect("failed to run tmux new-session");
        assert!(status.success(), "tmux new-session failed");
        Self { socket, session }
    }

    /// Splits the session's window so the server holds a session the user
    /// built. A lone window with one idle shell is a bootstrap session, which
    /// the store refuses to publish as `latest`.
    pub fn build(&self) {
        let status = std::process::Command::new("tmux")
            .args(["-S", &self.socket, "split-window", "-t", &self.session])
            .status()
            .expect("failed to run tmux split-window");
        assert!(status.success(), "tmux split-window failed");
    }
}

impl Drop for IsolatedTmux {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux")
            .args(["-S", &self.socket, "kill-session", "-t", &self.session])
            .status();
    }
}
