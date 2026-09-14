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
use std::sync::{Arc, Mutex, MutexGuard};

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
    child: ChildSlot,
    pipes: Pipes,
}

/// The spawned child, shared with every [`KillHandle`] this transport hands
/// out. Emptied exactly once, by whichever of `close()`/`Drop` runs first,
/// which is what makes a handle safe to outlive its transport: a pid can only
/// be signalled while this slot still holds the `Child` that owns it, so
/// "signal a pid the OS has already recycled" is unrepresentable rather than
/// merely unlikely (`[LAW:types-are-the-program]`).
type ChildSlot = Arc<Mutex<Option<Child>>>;

/// The stdio pipes, dropped as a pair by `close()` — so a closed transport
/// holds neither, and `send`/`read` have nothing left to lie about
/// (`[LAW:types-are-the-program]`: "closed, but still holding a pipe" cannot
/// be written down).
enum Pipes {
    Open {
        stdin: ChildStdin,
        stdout: ChildStdout,
    },
    Closed,
}

/// Terminates a [`SpawnTransport`]'s child from outside the thread that holds
/// the transport. `Send + Sync + Clone`, so it can be parked wherever a
/// shutdown is decided.
///
/// This is the only shutdown path available to a caller that reads on its own
/// thread — a daemon supervising a `tmux -C` child being the motivating one.
/// [`Transport::read`] blocks, and interrupting it takes `&mut self`, which
/// the blocked reader is already holding; `close()` is therefore reachable
/// only by the one thread that cannot call it. Killing the child closes the
/// stdout pipe, which is what returns that reader from `read()` with EOF.
#[derive(Clone)]
pub struct KillHandle {
    child: ChildSlot,
}

impl KillHandle {
    /// Signal the child to die, unblocking any read in flight. Reaping stays
    /// the transport's job, so this returns as soon as the signal is sent
    /// rather than waiting for the child to go. A child that has already
    /// exited, or that its transport has already reaped, is a no-op: the
    /// caller asked for it to be dead and it is.
    pub fn kill(&self) {
        if let Some(child) = lock(&self.child).as_mut() {
            // Fails harmlessly on a child that already exited.
            let _ = child.kill();
        }
    }
}

/// A poisoned slot means a thread panicked mid-kill. Recovering the guard
/// rather than propagating is the choice that keeps the child reapable — and
/// `[LAW:no-silent-failure]` is not bent by it, because the panic that
/// poisoned the lock has already been raised somewhere louder than here.
fn lock(slot: &ChildSlot) -> MutexGuard<'_, Option<Child>> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The tmux argv that selects `socket`: `-S <path>` for a path (anything
/// holding a `/`), `-L <name>` for a bare name, nothing for the default.
/// Public because callers that run plain `tmux` helpers beside the
/// control-mode child must select the same server the same way
/// (`[LAW:one-source-of-truth]`).
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
            child: Arc::new(Mutex::new(Some(child))),
            pipes: Pipes::Open { stdin, stdout },
        })
    }

    /// A handle that can terminate this transport's child from another thread.
    /// Handing one out on a transport whose child is already reaped is not an
    /// error — the handle's `kill` is simply a no-op, since the state it would
    /// establish already holds.
    pub fn kill_handle(&self) -> KillHandle {
        KillHandle {
            child: self.child.clone(),
        }
    }
}

fn closed_err() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "transport closed")
}

impl Transport for SpawnTransport {
    fn send(&mut self, line: &CommandLine) -> io::Result<()> {
        match &mut self.pipes {
            Pipes::Open { stdin, .. } => stdin.write_all(line.wire()),
            Pipes::Closed => Err(closed_err()),
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.pipes {
            Pipes::Open { stdout, .. } => stdout.read(buf),
            Pipes::Closed => Err(closed_err()),
        }
    }

    fn close(&mut self) {
        self.pipes = Pipes::Closed;
        // Taking the child is what makes this idempotent, and what a
        // [`KillHandle`] racing us observes: one of us empties the slot, and
        // the loser finds nothing to signal (`[LAW:single-enforcer]`).
        //
        // The take is its own statement so the guard dies at its semicolon:
        // the lock covers the handoff and nothing else. Written as the
        // scrutinee of the `if let` below, the guard would live to the closing
        // brace and a racing handle would block through `wait()` — an extent
        // chosen by the language's temporary-scope rule rather than by us
        // (`[LAW:no-ambient-temporal-coupling]`).
        let taken = lock(&self.child).take();
        if let Some(mut child) = taken {
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
    fn socket_args_is_empty_for_the_default_server() {
        assert_eq!(socket_args(None), Vec::<String>::new());
    }

    #[test]
    fn socket_args_selects_a_bare_name_with_dash_l_and_a_path_with_dash_s() {
        assert_eq!(
            socket_args(Some("phoenix-test")),
            vec!["-L", "phoenix-test"]
        );
        assert_eq!(socket_args(Some("/tmp/sock")), vec!["-S", "/tmp/sock"]);
    }

    #[test]
    fn build_argv_puts_control_mode_first_then_socket_then_the_command() {
        assert_eq!(
            build_argv(Some("/tmp/sock"), &["attach-session", "-t", "x"]),
            vec!["-C", "-S", "/tmp/sock", "attach-session", "-t", "x"]
        );
    }
}
