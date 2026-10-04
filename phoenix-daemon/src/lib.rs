//! `phoenix-daemon` — subscription-driven, debounced save, plus boot
//! restore (DESIGN.md §8).

mod boot;
mod daemon;
mod debounce;

pub use boot::{connect_and_boot, probe, Boot, BootError, Booted, Decided, ServerState};
pub use daemon::{run, run_resilient, DaemonError, RunConfig, StructureActivity};
pub use debounce::{DebouncePolicy, DebounceState};
