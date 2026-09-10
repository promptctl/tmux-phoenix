//! Shared test-only harness: an isolated, throw-away tmux server that
//! live-integration tests can safely drive without touching the developer's
//! real tmux sessions, and the command builder every test sends through.

use tmux_control::CommandLine;

/// For a command that takes no arguments.
pub const NO_ARGS: [&str; 0] = [];

/// A command line for tests, whose arguments never hold a NUL.
pub fn line(name: &'static str, args: impl IntoIterator<Item = impl AsRef<str>>) -> CommandLine {
    CommandLine::new(name, args).expect("test arguments hold no NUL")
}

/// Isolated throw-away socket path + a guard that tears the session down via
/// `kill-session` on drop.
pub struct IsolatedTmux {
    pub socket: String,
    pub session: String,
}

impl IsolatedTmux {
    pub fn new(name: &str) -> Self {
        let socket = format!("/tmp/tmux-phoenix-test-{name}-{}", std::process::id());
        let session = format!("phoenix-test-{name}");
        // Pre-create the session out-of-band (not through the transport
        // under test) so `attach-session` has something real to attach to,
        // exactly like a daemon attaching to a live server.
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
