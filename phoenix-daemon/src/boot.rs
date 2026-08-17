//! Boot restore (DESIGN.md §8, tmux-daemon-b0h.2): "On start, if the server
//! has only the default empty session, apply `latest`; if sessions already
//! exist, log and stay in save mode — never clobber a live server."
//!
//! **Verified live, and both surprising enough to be worth recording:**
//! - Bare `tmux -C` (no session target) does *not* attach to an existing
//!   session when one is already there — it unconditionally creates a brand
//!   new one, every time. So "does the server already have sessions" has to
//!   be checked with a plain, non-control-mode `list-sessions` *before*
//!   opening any control-mode connection at all; a bare `tmux -C` can't be
//!   used as that check without side effects.
//! - Killing the session a control-mode client is currently attached to
//!   ends that client's connection (`%exit`) — there's no way to "stay
//!   connected" through your own session's death. So the throwaway
//!   bootstrap session this module creates (only when the server had zero
//!   sessions) can't be torn down from the same connection that's attached
//!   to it; this reconnects to a real, restored session first (closing the
//!   old transport, which merely *detaches* — the bootstrap session
//!   survives that, unattended), then kills the now-unattended bootstrap
//!   session from the outside.
//! - A structure subscription (`tmux-daemon-b0h.1`'s `@*` scope) only
//!   observes the *attached* session's windows, not the whole server —
//!   verified live: a change in a second, non-attached session never fired
//!   the subscription. This is why reconnecting to a real restored session
//!   (rather than leaving the client parked on the dead-end bootstrap one)
//!   matters for the ongoing daemon loop, not just for cleanliness.

use std::process::Command;

use phoenix_restore::{
    connect_and_apply, count_sessions, default_rules_path, load_rules_file, plan,
    resolve_non_interactive, ConnectApplyError, BOOTSTRAP_SESSION,
};
use phoenix_store::{Store, StoreError};
use tmux_control::{socket_args, Client, SpawnOptions, SpawnTransport, TmuxError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootDecision {
    /// No sessions existed; a snapshot exists to restore into the
    /// bootstrap session's place.
    RestoreLatest,
    /// No sessions existed, and there's nothing saved to restore either.
    NothingToRestore,
    /// Sessions already existed — DESIGN.md's "never clobber a live
    /// server": no restore attempted at all.
    StayInSaveMode { existing_session_count: usize },
}

/// Pure: the actual session-listing and snapshot-loading are effectful
/// (`boot`, below), but the *decision* they feed into is a plain function of
/// two facts, directly testable without tmux or a store.
pub fn decide(existing_session_count: usize, has_snapshot: bool) -> BootDecision {
    if existing_session_count > 0 {
        BootDecision::StayInSaveMode {
            existing_session_count,
        }
    } else if has_snapshot {
        BootDecision::RestoreLatest
    } else {
        BootDecision::NothingToRestore
    }
}

#[derive(Debug)]
pub enum BootError {
    Io(std::io::Error),
    Tmux(TmuxError),
    Store(StoreError),
    /// The shared restore connection strategy (`connect_and_apply`) failed —
    /// carries its own already-specific message (spawn/connect/apply/bootstrap).
    ConnectApply(ConnectApplyError),
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootError::Io(e) => write!(f, "{e}"),
            BootError::Tmux(e) => write!(f, "{e}"),
            BootError::Store(e) => write!(f, "{e}"),
            BootError::ConnectApply(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BootError {}

impl From<ConnectApplyError> for BootError {
    fn from(e: ConnectApplyError) -> Self {
        BootError::ConnectApply(e)
    }
}

fn spawn_options(socket: Option<String>) -> SpawnOptions {
    SpawnOptions {
        socket,
        ..Default::default()
    }
}

fn run_tmux(socket: Option<&str>, args: &[&str]) -> std::io::Result<bool> {
    let mut cmd = Command::new("tmux");
    cmd.args(socket_args(socket));
    cmd.args(args);
    Ok(cmd.status()?.success())
}

/// Connects for the daemon's ongoing run, performing boot restore first —
/// DESIGN.md §8's contract (see the module doc comment for the mechanics
/// that made this trickier than the one-sentence spec text suggests).
/// `on_log` receives one line describing which `BootDecision` was made and
/// what happened.
pub fn connect_and_boot(
    socket: Option<String>,
    store: &Store,
    mut on_log: impl FnMut(&str),
) -> Result<Client<SpawnTransport>, BootError> {
    let existing = count_sessions(socket.as_deref());
    let latest = store.load_latest();
    let has_snapshot = !matches!(latest, Err(StoreError::NoLatest));

    match decide(existing, has_snapshot) {
        BootDecision::StayInSaveMode {
            existing_session_count,
        } => {
            on_log(&format!(
                "server already has {existing_session_count} session(s); staying in save mode"
            ));
            connect(socket)
        }
        BootDecision::NothingToRestore => {
            on_log("no sessions and no saved snapshot; starting fresh");
            run_tmux(
                socket.as_deref(),
                &["new-session", "-d", "-s", BOOTSTRAP_SESSION],
            )
            .map_err(BootError::Io)?;
            connect_to(socket, BOOTSTRAP_SESSION)
        }
        BootDecision::RestoreLatest => {
            let snapshot = latest.map_err(BootError::Store)?;

            // tmux-permissions-16s: a boot restore never prompts (there's
            // nobody to ask) — an already-learned rule is still honored,
            // but anything that would need a real `Ask` falls to the safe
            // "cwd + shell only" default and is logged, never silently
            // escalated. A rules-file load problem (missing HOME, a
            // permissions error) is likewise non-fatal to the restore
            // itself: it just means every relaunch decision falls to that
            // same safe default, same as an empty ruleset would.
            let ruleset = default_rules_path()
                .and_then(|path| load_rules_file(&path))
                .map(|(ruleset, warnings)| {
                    for warning in warnings {
                        on_log(&format!("relaunch rules file: {warning}"));
                    }
                    ruleset
                })
                .unwrap_or_else(|e| {
                    on_log(&format!(
                        "couldn't load relaunch rules ({e}); no program will be relaunched"
                    ));
                    Default::default()
                });
            let policy = resolve_non_interactive(&snapshot, &ruleset, |location, program| {
                on_log(&format!(
                    "{}:{}.{} ({}) needs consent to relaunch; skipping (boot restore never prompts)",
                    location.session, location.window.0, location.pane.0, program.command
                ));
            });

            let restore_plan = plan(&snapshot, &policy);
            // The bootstrap-connect-apply-reconnect-teardown dance lives in
            // `phoenix-restore` now, shared verbatim with the CLI's `restore`
            // (tmux-parity-ure.1) — one connection strategy, one place.
            let (client, _outcome) = connect_and_apply(socket, &snapshot, &restore_plan)?;

            on_log(&format!(
                "restored {} session(s) from the latest snapshot",
                snapshot.sessions.len()
            ));
            Ok(client)
        }
    }
}

fn connect(socket: Option<String>) -> Result<Client<SpawnTransport>, BootError> {
    let transport = SpawnTransport::spawn(&["attach-session"], &spawn_options(socket))
        .map_err(BootError::Io)?;
    Client::connect(transport).map_err(BootError::Tmux)
}

fn connect_to(socket: Option<String>, session: &str) -> Result<Client<SpawnTransport>, BootError> {
    let transport =
        SpawnTransport::spawn(&["attach-session", "-t", session], &spawn_options(socket))
            .map_err(BootError::Io)?;
    Client::connect(transport).map_err(BootError::Tmux)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_sessions_always_means_stay_in_save_mode() {
        assert_eq!(
            decide(1, true),
            BootDecision::StayInSaveMode {
                existing_session_count: 1
            }
        );
        assert_eq!(
            decide(3, false),
            BootDecision::StayInSaveMode {
                existing_session_count: 3
            }
        );
    }

    #[test]
    fn no_sessions_and_a_snapshot_means_restore() {
        assert_eq!(decide(0, true), BootDecision::RestoreLatest);
    }

    #[test]
    fn no_sessions_and_no_snapshot_means_nothing_to_restore() {
        assert_eq!(decide(0, false), BootDecision::NothingToRestore);
    }
}
