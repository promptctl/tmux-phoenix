//! The pure codec layer (DESIGN.md §3.1): no I/O, no panics. Parses tmux
//! control-mode server output into typed `ServerMessage` values.

mod codec;
mod decode;
mod fields;
mod guard;
mod ids;
mod layout;
mod message;

pub use codec::Codec;
pub use decode::decode_octal;
pub use guard::Guard;
pub use ids::{PaneId, SessionId, WindowId};
pub use layout::Layout;
pub use message::ServerMessage;
