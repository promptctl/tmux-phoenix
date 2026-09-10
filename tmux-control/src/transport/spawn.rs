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

/// `-C` plus the socket selector plus the caller's own tmux command/args, in
/// that order (mirrors the reference transport's `buildArgv`).
fn build_argv(socket: Option<&str>, user_args: &[&str]) -> Vec<String> {
    let mut argv = vec!["-C".to_string()];
    if let Some(socket) = socket {
        if socket.contains('/') {
            argv.push("-S".to_string());
        } else {
            argv.push("-L".to_string());
        }
        argv.push(socket.to_string());
    }
    argv.extend(user_args.iter().map(|s| s.to_string()));
    argv
}

/// Append a trailing `\n` only if `command` doesn't already end with one
/// (idempotent line-termination, mirrored from the reference transport).
fn terminate_line(command: &str) -> String {
    if command.ends_with('\n') {
        command.to_string()
    } else {
        format!("{command}\n")
    }
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
    fn send(&mut self, command: &str) -> io::Result<()> {
        match &mut self.state {
            State::Open { stdin, .. } => stdin.write_all(terminate_line(command).as_bytes()),
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
