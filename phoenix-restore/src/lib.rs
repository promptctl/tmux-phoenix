//! `phoenix-restore` — turns a saved `Snapshot` and a live one into a
//! [`Plan`], and applies a `Plan` over a connection (ARCHITECTURE.md §8).
//! Connecting is not this crate's job: [`plan`] is pure and [`apply`] is
//! handed whatever executes tmux commands.

mod apply;
mod plan;
mod step;

pub use apply::{apply, ApplyError, ApplyErrorSource};
pub use plan::{plan, scratch_name, Note, Onto, Plan};
pub use step::{LinkSource, OptionScope, PaneRef, Step, WindowRef};
