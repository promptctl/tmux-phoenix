//! The daemon's run loop (DESIGN.md §8): one connection, `no-output` +
//! structure subscriptions, debounced save. The only place in this crate
//! that touches a live connection — [`crate::debounce`] is pure.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use phoenix_capture::{ContentCapture, PreviousPaneContent};
use phoenix_core::Snapshot;
use phoenix_store::{SaveOutcome, Store};
use tmux_control::{Client, CommandLine, ServerMessage, TmuxError, Transport};

use crate::boot::connect_and_boot;
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

/// Whether `e` means the connection itself is gone (tmux exited, the pipe
/// broke) rather than just this one command failing — the distinction
/// `run`'s heartbeat handling needs to stop hammering a dead transport with
/// the same error every poll cycle and hand control back to
/// [`run_resilient`] instead.
fn is_connection_dead(e: &TmuxError) -> bool {
    matches!(
        e,
        TmuxError::Send(_)
            | TmuxError::Read(_)
            | TmuxError::TransportClosed
            | TmuxError::NotReady(_)
    )
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

    // Built once: the heartbeat never varies, so encoding it per cycle would
    // re-ask a question already answered (`[LAW:dataflow-not-control-flow]` —
    // a loop-invariant value, not a per-iteration decision).
    let heartbeat =
        CommandLine::new("display-message", ["-p", ""]).map_err(|e| DaemonError::Tmux(e.into()))?;

    let mut previous_content = store
        .load_latest()
        .map(|s| previous_content_from_snapshot(&s))
        .unwrap_or_default();

    let mut state = DebounceState::new(Instant::now());

    while should_continue() {
        if let Err(e) = client.execute(&heartbeat) {
            let fatal = is_connection_dead(&e);
            let err = DaemonError::Tmux(e);
            on_error(&err);
            if fatal {
                // No point looping on a dead transport, spamming on_error
                // every poll cycle forever — hand it back to the caller
                // (DESIGN.md §8: "if tmux isn't running the connection
                // sits in Closed/Reconnecting" — reconnecting is
                // `run_resilient`'s job, not this loop's).
                return Err(err);
            }
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

/// Runs indefinitely (until `should_continue` returns `false`), reconnecting
/// — including boot restore — whenever the connection is lost or was never
/// established in the first place. DESIGN.md §8: "Independent of the tmux
/// server — if tmux isn't running the connection sits in
/// `Closed`/`Reconnecting` and the daemon idles." A service-supervised
/// daemon can start before the user has ever touched tmux, or keep running
/// across a tmux server restart; it must not just exit and leave recovery
/// entirely to launchd/systemd respawning the whole process.
///
/// Concretely typed to `tmux_control::SpawnTransport` (unlike [`run`], generic over any
/// `Transport` for testability) because reconnecting means spawning a brand
/// new `tmux -C` process, which is [`crate::boot::connect_and_boot`]'s job
/// and that function is itself concrete for the same reason.
#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    pub policy: DebouncePolicy,
    pub poll_interval: Duration,
    pub reconnect_interval: Duration,
    pub keep_generations: usize,
}

pub fn run_resilient(
    socket: Option<String>,
    store: &Store,
    config: &RunConfig,
    mut on_log: impl FnMut(&str),
    mut should_continue: impl FnMut() -> bool,
) {
    while should_continue() {
        match connect_and_boot(socket.clone(), store, &mut on_log) {
            Ok(mut client) => {
                on_log("connected");
                let result = run(
                    &mut client,
                    store,
                    &config.policy,
                    config.poll_interval,
                    config.keep_generations,
                    |e| on_log(&format!("save cycle error: {e}")),
                    &mut should_continue,
                );
                client.close();
                if let Err(e) = result {
                    on_log(&format!("connection lost ({e}); will retry"));
                }
            }
            Err(e) => on_log(&format!("could not connect ({e}); will retry")),
        }

        if should_continue() {
            std::thread::sleep(config.reconnect_interval);
        }
    }
}

#[cfg(test)]
mod resilience_tests {
    use super::*;
    use std::cell::RefCell;
    use std::io;

    struct DeadTransport;

    impl Transport for DeadTransport {
        fn send(&mut self, _line: &CommandLine) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "dead"))
        }
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
        fn close(&mut self) {}
    }

    #[test]
    fn a_dead_transport_makes_run_return_promptly_instead_of_looping_forever() {
        let mut client = Client::new(DeadTransport);
        let policy = DebouncePolicy {
            debounce: Duration::from_secs(3600),
            max_interval: Duration::from_secs(3600),
        };
        let store = {
            let dir = std::env::temp_dir().join(format!(
                "phoenix-daemon-dead-transport-test-{}",
                std::process::id()
            ));
            Store::new(dir)
        };

        let error_count = RefCell::new(0);
        let result = run(
            &mut client,
            &store,
            &policy,
            Duration::from_millis(1),
            5,
            |_e| *error_count.borrow_mut() += 1,
            || true, // would spin forever if `run` didn't return on a dead connection
        );

        assert!(
            result.is_err(),
            "a dead transport should surface as an error, not loop silently"
        );
        assert!(
            *error_count.borrow() <= 1,
            "a dead connection should be reported once and returned, not spammed every poll cycle"
        );
    }

    #[test]
    fn detects_every_connection_dead_variant() {
        assert!(is_connection_dead(&TmuxError::Send(io::Error::other("x"))));
        assert!(is_connection_dead(&TmuxError::Read(io::Error::other("x"))));
        assert!(is_connection_dead(&TmuxError::TransportClosed));
        assert!(is_connection_dead(&TmuxError::NotReady(
            tmux_control::ConnectionState::Closed {
                reason: tmux_control::CloseReason::TransportError
            }
        )));
    }

    #[test]
    fn command_level_failures_are_not_connection_dead() {
        use tmux_control::protocol::Guard;
        let guard = Guard {
            timestamp: 0,
            command_number: 1,
            flags: 0,
        };
        assert!(!is_connection_dead(&TmuxError::Command {
            guard,
            lines: vec![b"nope".to_vec()],
        }));
    }
}
