//! Spawn-based transport: wraps `std::process::Command` behind [`Transport`].
//!
//! Emits `tmux -C` (single `-C`) and offers no way to request `-CC`: `-CC`
//! puts the terminal in raw mode, which tmux applies by calling
//! `tcgetattr(stdin)` at startup (IMPL.md §2.1) — so stdin must be a tty.
//! `std::process::Command` with piped stdio is not a tty, so `-CC` would
//! fail at tmux startup. Rather than reach that failure at runtime, this
//! transport simply has no option that produces `-CC`: the incompatible
//! configuration is unrepresentable by construction
//! (`[LAW:types-are-the-program]`). A PTY-backed transport implementing the
//! same trait is where `-CC` would belong, if phoenix ever needed it (it
//! doesn't — DESIGN.md §3.2 chose `-C` deliberately).

use super::Transport;
use crate::protocol::CommandLine;
use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// Options for spawning a tmux child process.
#[derive(Debug, Default, Clone)]
pub struct SpawnOptions {
    /// Path to the tmux binary. Defaults to `"tmux"` (resolved via `PATH`).
    pub tmux_path: Option<String>,
    /// Socket selector: a path containing `/` is passed as `-S <path>`; a
    /// bare name is passed as `-L <name>` (SPEC's socket-selection
    /// convention, mirrored from the reference transport).
    pub socket: Option<String>,
    /// Environment variables set on top of the inherited environment
    /// (additive/override, not a full replacement — `std::process::Command`'s
    /// native semantics).
    pub env: Vec<(String, String)>,
}

/// A tmux control-mode connection backed by a spawned `tmux -C` child
/// process. Owns the child and its stdin/stdout pipes; this is the only
/// place in the crate a process is spawned or an OS byte is read.
pub struct SpawnTransport {
    state: State,
}

/// `close()` moves the child out of `Open` and reaps it, so it is reaped
/// exactly once and a closed transport holds no pipes.
enum State {
    Open {
        child: Child,
        stdin: ChildStdin,
        stdout: ChildStdout,
    },
    Closed,
}

/// The socket-selector arguments alone (no `-C`, no user command): a path
/// containing `/` is `-S <path>`, a bare name is `-L <name>`, `None` is no
/// arguments at all. Public and reused by any caller that needs to talk to
/// the *same* tmux server this transport would via a plain (non-control-mode)
/// `tmux` invocation — e.g. `phoenix-daemon`'s pre-connect `list-sessions`
/// check — so the socket-selection rule has exactly one implementation
/// (`[LAW:one-source-of-truth]`) instead of being duplicated and risking the
/// two copies drifting apart.
pub fn socket_args(socket: Option<&str>) -> Vec<String> {
    match socket {
        None => Vec::new(),
        Some(socket) if socket.contains('/') => vec!["-S".to_string(), socket.to_string()],
        Some(socket) => vec!["-L".to_string(), socket.to_string()],
    }
}

/// `-C` plus the socket selector plus the caller's own tmux command/args, in
/// that order (mirrors the reference transport's `buildArgv`).
fn build_argv(socket: Option<&str>, user_args: &[&str]) -> Vec<String> {
    let mut argv = vec!["-C".to_string()];
    argv.extend(socket_args(socket));
    argv.extend(user_args.iter().map(|s| s.to_string()));
    argv
}

impl SpawnTransport {
    /// Spawn `tmux -C <socket-selector> <args>` and take ownership of its
    /// stdin/stdout pipes. stderr is inherited: control-mode traffic is all on
    /// stdout, and a startup failure (no server, a bad socket) is printed by
    /// tmux before any control connection exists.
    pub fn spawn(args: &[&str], options: &SpawnOptions) -> io::Result<Self> {
        let tmux_path = options.tmux_path.as_deref().unwrap_or("tmux");
        let argv = build_argv(options.socket.as_deref(), args);

        let mut command = Command::new(tmux_path);
        command
            .args(&argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (key, value) in &options.env {
            command.env(key, value);
        }

        let mut child = command.spawn()?;
        // Guaranteed `Some` by `Stdio::piped()` above on a successful spawn
        // — an invariant of `std::process::Command`, not a runtime guess.
        let stdin = child
            .stdin
            .take()
            .expect("child.stdin missing despite Stdio::piped()");
        let stdout = child
            .stdout
            .take()
            .expect("child.stdout missing despite Stdio::piped()");

        Ok(Self {
            state: State::Open {
                child,
                stdin,
                stdout,
            },
        })
    }
}

fn closed_err() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "transport closed")
}

impl Transport for SpawnTransport {
    fn send(&mut self, line: &CommandLine) -> io::Result<()> {
        match &mut self.state {
            State::Open { stdin, .. } => stdin.write_all(line.wire()),
            State::Closed => Err(closed_err()),
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.state {
            State::Open { stdout, .. } => stdout.read(buf),
            State::Closed => Err(closed_err()),
        }
    }

    fn close(&mut self) {
        if let State::Open { mut child, .. } = std::mem::replace(&mut self.state, State::Closed) {
            // `kill()` fails harmlessly on a child that already exited; `wait()`
            // reaps it either way, which `std::process::Child` never does on drop.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for SpawnTransport {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_args_is_empty_for_no_socket() {
        assert_eq!(socket_args(None), Vec::<String>::new());
    }

    #[test]
    fn socket_args_uses_dash_l_for_a_bare_name() {
        assert_eq!(
            socket_args(Some("phoenix-verify")),
            vec!["-L".to_string(), "phoenix-verify".to_string()]
        );
    }

    #[test]
    fn socket_args_uses_dash_s_for_a_path() {
        assert_eq!(
            socket_args(Some("/tmp/some/socket")),
            vec!["-S".to_string(), "/tmp/some/socket".to_string()]
        );
    }

    #[test]
    fn socket_args_uses_dash_s_for_a_relative_path() {
        assert_eq!(
            socket_args(Some("./relative/socket")),
            vec!["-S".to_string(), "./relative/socket".to_string()]
        );
    }

    #[test]
    fn build_argv_has_no_socket_selector_when_socket_is_none() {
        assert_eq!(
            build_argv(None, &["list-sessions"]),
            vec!["-C".to_string(), "list-sessions".to_string()]
        );
    }

    #[test]
    fn build_argv_places_the_socket_selector_between_dash_c_and_user_args() {
        assert_eq!(
            build_argv(Some("phoenix-verify"), &["attach-session"]),
            vec![
                "-C".to_string(),
                "-L".to_string(),
                "phoenix-verify".to_string(),
                "attach-session".to_string(),
            ]
        );
    }
}
