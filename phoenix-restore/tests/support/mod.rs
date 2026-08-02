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
}

impl Drop for IsolatedTmux {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux")
            .args(["-S", &self.socket, "kill-session", "-t", &self.session])
            .status();
    }
}
