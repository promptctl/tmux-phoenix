//! Shared test-only harness for spawning an isolated, throw-away tmux
//! server that live-integration tests can safely drive without touching the
//! developer's real tmux sessions, plus the command builder every test sends
//! through. Duplicated (not shared via a lib crate)
//! from `tmux-control/tests/support/mod.rs` — each integration test binary
//! compiles its own copy either way, and Rust's per-binary dead-code lint
//! flags an unused shared module's items if only some binaries use them.

use tmux_control::CommandLine;

/// For a command that takes no arguments. `allow(dead_code)` because only
/// some of this crate's test binaries take the no-argument path, and each
/// compiles its own copy of this module — see the module doc.
#[allow(dead_code)]
pub const NO_ARGS: [&str; 0] = [];

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
}

impl Drop for IsolatedTmux {
    fn drop(&mut self) {
        // exit-empty back on first, so a test that turned it off and removed
        // every session still lets its server exit instead of lingering.
        let _ = std::process::Command::new("tmux")
            .args(["-S", &self.socket, "set-option", "-g", "exit-empty", "on"])
            .status();
        let _ = std::process::Command::new("tmux")
            .args(["-S", &self.socket, "kill-session", "-t", &self.session])
            .status();
    }
}
