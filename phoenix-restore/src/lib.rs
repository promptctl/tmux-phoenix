//! `phoenix-restore` — pure `Snapshot -> RestorePlan` planning (DESIGN.md §6),
//! plus [`apply`], the one place this crate touches a live `tmux-control`
//! connection to actually run a plan.

mod apply;
mod command;
mod permission;
mod plan;
mod policy;

pub use apply::{apply, ApplyError, ApplyErrorSource, ApplyOutcome};
pub use command::{PlanStep, TmuxCommand};
pub use permission::{
    default_choice_for, default_rules_path, load_rules_file, parse_prompt_choice,
    resolve_interactive, resolve_non_interactive, save_rules_file, Matcher, PromptChoice,
    Resolution, Rule, RuleSet, Verdict,
};
pub use plan::{plan, RestorePlan};
pub use policy::{PaneLocation, RestorePolicy};
