//! `tmux-control` — a standalone Rust implementation of the tmux
//! control-mode wire protocol (DESIGN.md §3). Depends on nothing
//! phoenix-specific.
//!
//! Three layers, each a clean seam: a pure codec (no I/O), an effect
//! transport, and a correlation client. The client layer is not built yet.

pub mod protocol;
pub mod transport;

pub use protocol::{
    decode_octal, Codec, CommandLine, Guard, Layout, NulInArgument, PaneId, ServerMessage,
    SessionId, WindowId,
};
pub use transport::{SpawnOptions, SpawnTransport, Transport};
