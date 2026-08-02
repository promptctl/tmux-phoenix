//! `phoenix-daemon` — subscription-driven, debounced save (DESIGN.md §8).

mod daemon;
mod debounce;

pub use daemon::{run, DaemonError};
pub use debounce::{DebouncePolicy, DebounceState};
