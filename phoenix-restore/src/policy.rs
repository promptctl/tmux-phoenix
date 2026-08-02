//! DESIGN.md §6: "Program relaunch defaults to cwd + shell only; it never
//! blind-replays captured argv... Which programs may relaunch is a learned,
//! consent-gated ruleset." `RestorePolicy` is where that ruleset's decisions
//! land before [`crate::plan::plan`] runs — `plan` stays pure, so *deciding*
//! (which may mean prompting a user, or consulting a persisted ruleset) is
//! entirely done ahead of time by `tmux-permissions-16s`'s
//! [`crate::permission`] module; `plan` just renders whatever ended up here.

use std::collections::HashMap;

use phoenix_core::{PaneIndex, SessionName, WindowIndex};

/// A pane's identity *within the snapshot being restored* — stable for the
/// lifetime of one `plan` call even though [`PaneIndex`] isn't stable on the
/// live target server (see [`crate::plan::panes_active_last`]'s doc
/// comment). Decisions are made by walking this same snapshot before `plan`
/// runs, so keying by the snapshot's own captured coordinates is exactly the
/// stable identity available at decision time — no dependency on the
/// content-capture-only `PaneId` a structure-only capture wouldn't have.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PaneLocation {
    pub session: SessionName,
    pub window: WindowIndex,
    pub pane: PaneIndex,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RestorePolicy {
    /// Panes whose captured program should actually be relaunched (its real
    /// argv sent into the pane) rather than left as a fresh idle shell.
    /// Absent = don't relaunch — the "cwd + shell only" default, and what an
    /// empty `RestorePolicy::default()` still means, unchanged from before
    /// this field existed.
    pub relaunch: HashMap<PaneLocation, Vec<String>>,
}
