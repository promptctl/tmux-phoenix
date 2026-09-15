//! `phoenix-restore` — pure `Snapshot -> RestorePlan` planning (DESIGN.md §6),
//! plus [`apply`], the one place this crate touches a live `tmux-control`
//! connection to actually run a plan.

mod apply;
mod command;
mod connect;
mod plan;

pub use apply::{apply, ApplyError, ApplyErrorSource, ApplyOutcome};
pub use command::{PlanStep, TmuxCommand};
pub use connect::{
    connect_and_apply, count_sessions, probe, server_id, ConnectApplyError, Restored, ServerId,
    ServerState, RESTORED_OPTION,
};
pub use plan::{plan, RestorePlan};
