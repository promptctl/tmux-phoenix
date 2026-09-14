//! `tmux-control` — a standalone Rust implementation of the tmux
//! control-mode wire protocol (DESIGN.md §3). Depends on nothing
//! phoenix-specific.
//!
//! Three layers, each a clean seam: a pure codec (no I/O), an effect
//! transport, and a correlation client.

// A guard created in a `match`/`if let` scrutinee lives to the end of the
// whole arm, so a lock's extent is set by scoping rules rather than by the
// author. This lint makes the compiler redraw that map on every build instead
// of leaving it to review (`[LAW:no-ambient-temporal-coupling]`).
#![warn(clippy::significant_drop_in_scrutinee)]

pub mod client;
pub mod commands;
pub mod protocol;
pub mod transport;
pub mod version;

pub use client::{Client, CloseReason, CommandOutput, ConnectionState, TmuxError};
pub use commands::{
    ClientFlag, ColonInSubscriptionName, PaneAction, SubscriptionName, SubscriptionScope,
};
pub use protocol::{
    decode_octal, Codec, CommandLine, Guard, Layout, NulInArgument, PaneId, ServerMessage,
    SessionId, WindowId,
};
pub use transport::{socket_args, KillHandle, SpawnOptions, SpawnTransport, Transport};
pub use version::{TmuxVersion, MIN_TMUX_VERSION};
