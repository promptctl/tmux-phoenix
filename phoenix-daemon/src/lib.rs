//! `phoenix-daemon` — subscription-driven, debounced save, plus boot
//! restore (DESIGN.md §8).

mod boot;
mod daemon;
mod debounce;

pub use boot::{connect_and_boot, decide, BootDecision, BootError};
pub use daemon::{run, DaemonError};
pub use debounce::{DebouncePolicy, DebounceState};
