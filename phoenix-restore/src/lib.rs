//! `phoenix-restore` — pure `Snapshot -> RestorePlan` planning (DESIGN.md §6),
//! plus [`apply`], the one place this crate touches a live `tmux-control`
//! connection to actually run a plan.

mod apply;
mod command;
mod plan;
mod policy;

pub use apply::{apply, ApplyError, ApplyOutcome};
pub use command::TmuxCommand;
pub use plan::{plan, RestorePlan};
pub use policy::RestorePolicy;
