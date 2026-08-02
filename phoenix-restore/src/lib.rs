//! `phoenix-restore` — pure `Snapshot -> RestorePlan` planning (DESIGN.md §6).
//! `tmux-control` executes the resulting plan; this crate never opens a
//! connection or spawns anything itself.

mod command;
mod plan;
mod policy;

pub use command::TmuxCommand;
pub use plan::{plan, RestorePlan};
pub use policy::RestorePolicy;
