//! The effect transport (DESIGN.md §3.2): the only place a process is
//! spawned or a byte is read from the OS. Defined behind the [`Transport`]
//! trait so the codec and client stay testable without a live tmux, and so
//! an alternate transport (PTY, remote socket) can swap in without touching
//! the layers above (`[LAW:locality-or-seam]`).
//!
//! Deliberately protocol-agnostic: `Transport` knows nothing about guard
//! blocks or `ServerMessage` — it writes already-encoded [`CommandLine`]s and
//! reads raw bytes. Wiring what it reads into [`crate::Codec::feed`] is the
//! client layer's job (a later ticket), so dependencies still flow one way
//! (`[LAW:one-way-deps]`): transport uses the protocol's command encoding, and
//! the protocol layer knows nothing of transports.

mod spawn;

pub use spawn::{socket_args, KillHandle, SpawnOptions, SpawnTransport};

use crate::protocol::CommandLine;
use std::io;

/// Minimal contract for anything that can carry the tmux control-mode wire
/// protocol: send a command line, read whatever bytes are available, tear
/// down. No protocol knowledge lives here — `send`/`read` move bytes, full
/// stop.
pub trait Transport {
    /// Write one command to tmux. A [`CommandLine`] is exactly one wire line,
    /// newline included, so the transport writes its bytes as they are and has
    /// nothing to frame, escape, or refuse; [`CommandLine::detach`] is the
    /// wire-level detach signal (SPEC §4.1).
    fn send(&mut self, line: &CommandLine) -> io::Result<()>;

    /// Block until at least one byte is available and copy as many as fit
    /// into `buf`, returning the count. As with `std::io::Read`, `Ok(0)` for a
    /// non-empty `buf` means clean EOF: the process exited or the pipe closed.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;

    /// Tear down the transport. Idempotent — calling it more than once, or
    /// after the transport already closed on its own, is a no-op.
    fn close(&mut self);
}
