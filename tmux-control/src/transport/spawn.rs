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
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    closed: bool,
    close_reason: Option<String>,
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
    /// stdin/stdout pipes. stderr is discarded: control clients get their
    /// errors through `%error` guard blocks on stdout, not stderr.
    pub fn spawn(args: &[&str], options: &SpawnOptions) -> io::Result<Self> {
        let tmux_path = options.tmux_path.as_deref().unwrap_or("tmux");
        let argv = build_argv(options.socket.as_deref(), args);

        let mut command = Command::new(tmux_path);
        command
            .args(&argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
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
            child,
            stdin,
            stdout,
            closed: false,
            close_reason: None,
        })
    }

    fn closed_err(&self) -> io::Error {
        let msg = match &self.close_reason {
            Some(reason) => format!("transport closed: {reason}"),
            None => "transport closed".to_string(),
        };
        io::Error::new(io::ErrorKind::BrokenPipe, msg)
    }
}

impl Transport for SpawnTransport {
    fn send(&mut self, command: &str) -> io::Result<()> {
        if self.closed {
            return Err(self.closed_err());
        }
        let line = terminate_line(command);
        match self.stdin.write_all(line.as_bytes()) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.closed = true;
                self.close_reason.get_or_insert_with(|| err.to_string());
                Err(err)
            }
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.closed {
            return Err(self.closed_err());
        }
        match self.stdout.read(buf) {
            Ok(0) => {
                self.closed = true;
                self.close_reason.get_or_insert("eof".to_string());
                Ok(0)
            }
            Ok(n) => Ok(n),
            Err(err) => {
                self.closed = true;
                self.close_reason.get_or_insert_with(|| err.to_string());
                Err(err)
            }
        }
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.close_reason.get_or_insert("closed".to_string());
        reap(&mut self.child);
    }
}

/// Best-effort kill-then-wait, tolerant of a child that has already exited
/// (`kill()` on a dead process errors harmlessly; ignored either way).
///
/// Separate from the `closed` flag on purpose: `read()` observing a natural
/// EOF sets `closed = true` without calling `wait()` (EOF only tells us the
/// pipe closed, not that we've reaped the process) — if `close()` then
/// short-circuited on `closed` alone, that path would never reap the child.
/// `Drop` also calls this unconditionally, so a `SpawnTransport` dropped
/// without an explicit `close()` — after a natural EOF, or with no
/// teardown call at all — never leaks a zombie. `std::process::Child`,
/// unlike Node's `child_process`, does not reap on drop by itself.
fn reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

impl Drop for SpawnTransport {
    fn drop(&mut self) {
        reap(&mut self.child);
    }
}
