//! The effect transport (DESIGN.md §3.2): the only place a process is
//! spawned or a byte is read from the OS. Defined behind the [`Transport`]
//! trait so the codec and client stay testable without a live tmux, and so
//! an alternate transport (PTY, remote socket) can swap in without touching
//! the layers above (`[LAW:locality-or-seam]`).
//!
//! Deliberately protocol-agnostic: `Transport` knows nothing about guard
//! blocks or `ServerMessage` — it moves bytes. Wiring transport output into
//! [`crate::Codec::feed`] is the client layer's job (a later ticket), which
//! keeps dependencies flowing one way (`[LAW:one-way-deps]`): transport does
//! not depend on the codec.

mod spawn;

pub use spawn::{SpawnOptions, SpawnTransport};

use std::io;

/// Minimal contract for anything that can carry the tmux control-mode wire
/// protocol: send a command line, read whatever bytes are available, tear
/// down. No protocol knowledge lives here — `send`/`read` move bytes, full
/// stop.
pub trait Transport {
    /// Send one command to tmux. `command` is LF-terminated if it doesn't
    /// already end in `\n` — callers pass a bare command string, not a wire
    /// line.
    ///
    /// Sending an empty command writes a bare `\n`, which is the wire-level
    /// detach signal (SPEC §4.1) — this is a real, valid use of `send`, not
    /// a misuse to guard against.
    fn send(&mut self, command: &str) -> io::Result<()>;

    /// Block until at least one byte is available and copy as many as fit
    /// into `buf`, returning the count. `Ok(0)` means clean EOF: the
    /// transport has closed (the process exited, the pipe closed).
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;

    /// Tear down the transport. Idempotent — calling it more than once, or
    /// after the transport already closed on its own, is a no-op.
    fn close(&mut self);
}
