//! The pure codec layer (DESIGN.md §3.1): no I/O, no panics. Parses tmux
//! control-mode server output into typed `ServerMessage` values, and encodes
//! the other direction — a command as the single wire line tmux reads back as
//! exactly the arguments given.

mod codec;
mod decode;
mod encode;
mod fields;
mod guard;
mod ids;
mod layout;
mod message;

pub use codec::Codec;
pub use decode::decode_octal;
pub use encode::{CommandLine, NulInArgument};
pub use guard::Guard;
pub use ids::{PaneId, SessionId, WindowId};
pub use layout::Layout;
pub use message::ServerMessage;
