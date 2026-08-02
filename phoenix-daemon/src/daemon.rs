//! The daemon's run loop (DESIGN.md §8): one connection, `no-output` +
//! structure subscriptions, debounced save. The only place in this crate
//! that touches a live connection — [`crate::debounce`] is pure.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use phoenix_capture::{ContentCapture, PreviousPaneContent};
use phoenix_core::Snapshot;
use phoenix_store::{SaveOutcome, Store};
use tmux_control::{Client, ServerMessage, TmuxError, Transport};

use crate::debounce::{DebouncePolicy, DebounceState};

/// The structure-change indicator subscribed to (DESIGN.md §8's "structure
/// subscriptions"): `#{window_layout}` per window (`@*`), which changes on
/// pane split/close/resize as well as window add/remove — verified live
/// against a real tmux server. Chosen for daemon-core scope; a later ticket
/// can add more subscriptions if a gap in coverage shows up in practice.
const SUBSCRIPTION_NAME: &str = "phoenix-structure";
const SUBSCRIPTION_WHAT: &str = "@*";
const SUBSCRIPTION_FORMAT: &str = "#{window_layout}";

#[derive(Debug)]
pub enum DaemonError {
    Tmux(TmuxError),
    Capture(phoenix_capture::CaptureError),
    Store(phoenix_store::StoreError),
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonError::Tmux(e) => write!(f, "{e}"),
            DaemonError::Capture(e) => write!(f, "{e}"),
            DaemonError::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DaemonError {}

/// Walks a previously-captured `Snapshot` into the `previous` map
/// `phoenix_capture::ContentCapture::On` needs, keyed by tmux pane id — the
/// bridge `phoenix-capture`'s own doc comment says the caller owns, since
/// that crate has no persistence dependency of its own.
fn previous_content_from_snapshot(snapshot: &Snapshot) -> HashMap<u32, PreviousPaneContent> {
    snapshot
        .sessions
        .iter()
        .flat_map(|s| s.windows().iter())
        .flat_map(|w| w.panes().iter())
        .filter_map(|p| {
            let content = p.content.as_ref()?;
            Some((
                content.pane_id.0,
                PreviousPaneContent {
                    history_size: content.history_size,
                    history_bytes: content.history_bytes,
                    scrollback: content.scrollback.clone(),
                },
            ))
        })
        .collect()
}

fn capture_and_save<T: Transport>(
    client: &mut Client<T>,
    store: &Store,
    previous: &HashMap<u32, PreviousPaneContent>,
    keep_generations: usize,
) -> Result<(Snapshot, SaveOutcome), DaemonError> {
    let snapshot = phoenix_capture::capture(
        client,
        ContentCapture::On {
            previous: previous.clone(),
        },
    )
    .map_err(DaemonError::Capture)?;
    let outcome = store
        .save(&snapshot, keep_generations)
        .map_err(DaemonError::Store)?;
    Ok((snapshot, outcome))
}

/// Runs until `should_continue` returns `false` (checked once per poll
/// cycle — a test-friendly hook for bounded runs; real callers pass
/// `|| true` and rely on the process being killed to stop). Sets
/// `no-output` and subscribes once at startup, then loops: a cheap
/// heartbeat command drains whatever notifications arrived since the last
/// cycle (tmux-control's `execute` already reads and dispatches a full
/// batch before returning — no separate polling primitive needed, verified
/// against the existing `Client` design from tmux-control-mode-1ju), any
/// `SubscriptionChanged` counts as activity, then the debounce/max-interval
/// decision runs.
///
/// A single capture-or-save failure is logged (via `on_error`) and the loop
/// continues — a transient hiccup must not kill a long-running daemon. Only
/// a failure setting up `no-output`/the subscription is fatal, since
/// without those the daemon can't do its job at all.
pub fn run<T: Transport>(
    client: &mut Client<T>,
    store: &Store,
    policy: &DebouncePolicy,
    poll_interval: Duration,
    keep_generations: usize,
    mut on_error: impl FnMut(&DaemonError),
    mut should_continue: impl FnMut() -> bool,
) -> Result<(), DaemonError> {
    tmux_control::commands::set_no_output(client).map_err(DaemonError::Tmux)?;
    tmux_control::commands::subscribe(
        client,
        SUBSCRIPTION_NAME,
        SUBSCRIPTION_WHAT,
        SUBSCRIPTION_FORMAT,
    )
    .map_err(DaemonError::Tmux)?;

    let mut previous_content = store
        .load_latest()
        .map(|s| previous_content_from_snapshot(&s))
        .unwrap_or_default();

    let mut state = DebounceState::new(Instant::now());

    while should_continue() {
        if let Err(e) = client.execute("display-message -p \"\"") {
            on_error(&DaemonError::Tmux(e));
        } else {
            let activity = client
                .drain_notifications()
                .into_iter()
                .any(|m| matches!(m, ServerMessage::SubscriptionChanged { .. }));
            if activity {
                state.record_activity(Instant::now());
            }
        }

        if state.should_save(policy, Instant::now()) {
            match capture_and_save(client, store, &previous_content, keep_generations) {
                Ok((snapshot, _outcome)) => {
                    previous_content = previous_content_from_snapshot(&snapshot);
                    state.record_save(Instant::now());
                }
                Err(e) => on_error(&e),
            }
        }

        std::thread::sleep(poll_interval);
    }

    Ok(())
}
