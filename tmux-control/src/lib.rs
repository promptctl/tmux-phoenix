//! `tmux-control` — a standalone Rust implementation of the tmux
//! control-mode wire protocol (DESIGN.md §3). Depends on nothing
//! phoenix-specific.
//!
//! Three layers, each a clean seam: a pure codec (this module tree, no I/O),
//! an effect transport, and a correlation client. Only the codec exists so
//! far.

pub mod protocol;

pub use protocol::{
    decode_octal, Codec, Guard, Layout, PaneId, ServerMessage, SessionId, WindowId,
};
