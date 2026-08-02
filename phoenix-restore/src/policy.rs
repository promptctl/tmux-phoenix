//! DESIGN.md §6: "Program relaunch defaults to cwd + shell only; it never
//! blind-replays captured argv... (Full permission model deferred to a
//! later milestone; the default-safe behavior ships first.)"
//!
//! [`plan`](crate::plan) never reads a pane's `program`/`argv` at all — every
//! restored pane just gets its captured `cwd`, and tmux spawns whatever the
//! user's own shell configuration says to spawn there. That *is* the
//! default-safe behavior, so there's nothing for a policy to select yet.
//! `RestorePolicy` exists now, empty, purely so `plan`'s signature won't
//! have to change when the consent-gated relaunch ruleset
//! (tmux-permissions-16s) lands and gives it real fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestorePolicy;
