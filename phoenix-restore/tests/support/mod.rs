//! Shared test-only harness for spawning an isolated, throw-away tmux
//! server that live-integration tests can safely drive without touching the
//! developer's real tmux sessions, plus the command builder and connection
//! every test drives it through. Duplicated (not shared via a lib crate)
//! from `tmux-control/tests/support/mod.rs` — see that copy's doc comment
//! for why.

use tmux_control::{Client, CommandLine, SpawnOptions, SpawnTransport};

/// A command line for tests, whose arguments never hold a NUL.
pub fn line(name: &'static str, args: impl IntoIterator<Item = impl AsRef<str>>) -> CommandLine {
    CommandLine::new(name, args).expect("test arguments hold no NUL")
}

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

    /// A control-mode client attached to this server — the same connection
    /// the real `phoenix restore` drives.
    pub fn connect(&self) -> Client<SpawnTransport> {
        let transport = SpawnTransport::spawn(
            &["attach-session", "-t", &self.session],
            &SpawnOptions {
                socket: Some(self.socket.clone()),
                ..Default::default()
            },
        )
        .expect("failed to spawn tmux -C");
        Client::connect(transport, drop, |_, _| {}).expect("handshake failed against real tmux")
    }
}

impl Drop for IsolatedTmux {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux")
            .args(["-S", &self.socket, "kill-session", "-t", &self.session])
            .status();
    }
}
