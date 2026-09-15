//! The daemon's run loop (DESIGN.md §8): one connection, `no-output` +
//! structure subscriptions, debounced save. The only place in this crate
//! that touches a live connection — [`crate::debounce`] is pure.

use std::cell::Cell;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::rc::Rc;
use std::time::{Duration, Instant};

use phoenix_capture::{ContentCapture, PreviousPaneContent};
use phoenix_core::Snapshot;
use phoenix_store::{Store, StoreError};
use tmux_control::{
    Client, CommandLine, ServerMessage, SubscriptionName, SubscriptionScope, TmuxError, Transport,
};

use crate::boot::{connect_and_boot, Boot};
use crate::debounce::{DebouncePolicy, DebounceState};

/// The structure-change indicator subscribed to (DESIGN.md §8's "structure
/// subscriptions"): `#{window_layout}` for every window, which changes on
/// pane split/close/resize as well as window add/remove — verified live
/// against a real tmux server. Scoped to the attached session: a change in a
/// session the daemon is not attached to reaches it only through the
/// max-interval backstop.
const SUBSCRIPTION_NAME: &str = "phoenix-structure";
const SUBSCRIPTION_FORMAT: &str = "#{window_layout}";

/// Whether the daemon's structure subscription has fired since the run loop
/// last looked. A client's notification sink is fixed when the client is
/// built, so this flag exists first and the client is built around
/// [`StructureActivity::sink`]; the loop reads it with no extra round-trip.
#[derive(Debug, Clone, Default)]
pub struct StructureActivity(Rc<Cell<bool>>);

impl StructureActivity {
    pub fn new() -> Self {
        Self::default()
    }

    /// The notification sink to build the daemon's client with: marks
    /// activity on this daemon's own `%subscription-changed` and ignores
    /// every other notification.
    pub fn sink(&self) -> impl FnMut(ServerMessage) + 'static {
        let seen = Rc::clone(&self.0);
        move |message| {
            if let ServerMessage::SubscriptionChanged { name, .. } = message {
                if name == SUBSCRIPTION_NAME {
                    seen.set(true);
                }
            }
        }
    }

    /// Whether activity arrived since the last call; clears it.
    fn take(&self) -> bool {
        self.0.replace(false)
    }
}

#[derive(Debug)]
pub enum DaemonError {
    Tmux(TmuxError),
    Capture(phoenix_capture::CaptureError),
    Store(StoreError),
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
                p.id.0,
                PreviousPaneContent {
                    history_size: content.history_size,
                    history_bytes: content.history_bytes,
                    scrollback: content.scrollback.clone(),
                },
            ))
        })
        .collect()
}

/// The daemon never waits on another save: a contended save fails this
/// cycle without recording a save, so [`run`]'s next poll tries again.
const SAVE_WAIT: Duration = Duration::ZERO;

fn capture_and_save<T: Transport>(
    client: &mut Client<T>,
    store: &Store,
    previous: &HashMap<u32, PreviousPaneContent>,
    keep_generations: NonZeroUsize,
) -> Result<Snapshot, DaemonError> {
    let snapshot = phoenix_capture::capture(
        client,
        ContentCapture::On {
            previous: previous.clone(),
        },
    )
    .map_err(DaemonError::Capture)?;
    store
        .save(&snapshot, keep_generations, SAVE_WAIT)
        .map_err(DaemonError::Store)?;
    Ok(snapshot)
}

/// Whether `e` means the connection itself is gone (tmux exited, the pipe
/// broke) rather than one command failing — so [`run`] hands a dead
/// transport back to [`run_resilient`] instead of reporting the same error
/// every poll cycle. Exhaustive on purpose: a new `TmuxError` variant has to
/// be classified here before this compiles.
fn is_connection_dead(e: &TmuxError) -> bool {
    match e {
        TmuxError::Send(_)
        | TmuxError::Read(_)
        | TmuxError::TransportClosed
        | TmuxError::NotReady(_) => true,
        TmuxError::Encode(_)
        | TmuxError::Command { .. }
        | TmuxError::Protocol { .. }
        | TmuxError::UnsupportedTmuxVersion { .. }
        | TmuxError::VersionProbeFailed { .. } => false,
    }
}

/// Runs until `should_continue` returns `false` (checked once per poll
/// cycle — a test-friendly hook for bounded runs; real callers pass
/// `|| true` and rely on the process being killed to stop). `client` must
/// have been built with `activity`'s sink. Sets `no-output` and subscribes
/// once at startup, then loops: a cheap heartbeat command makes the client
/// read and dispatch whatever notifications arrived since the last cycle
/// (`execute` dispatches every notification it reads on the way to its
/// reply), any structure change counts as activity, then the
/// debounce/max-interval decision runs.
///
/// A single capture-or-save failure is reported (via `on_error`) and the
/// loop continues — a transient hiccup must not kill a long-running daemon.
/// Only a failure setting up `no-output`/the subscription is fatal, since
/// without those the daemon can't do its job at all. The store declining to
/// save a server that holds only bootstrap sessions ends the run instead,
/// returning that error, while `boot` is still [`Boot::Declined`]: the boot
/// probe may have read a login shell's prompt mid-`git` as a program the user
/// was running, and whether to restore is boot restore's question. Once a save
/// succeeds, or when boot settled, the refusal is reported like any other.
pub fn run<T: Transport>(
    client: &mut Client<T>,
    activity: &StructureActivity,
    mut boot: Boot,
    store: &Store,
    config: &RunConfig,
    mut on_error: impl FnMut(&DaemonError),
    mut should_continue: impl FnMut() -> bool,
) -> Result<(), DaemonError> {
    let name = SubscriptionName::new(SUBSCRIPTION_NAME)
        .expect("the subscription name constant holds no colon");
    tmux_control::commands::set_no_output(client).map_err(DaemonError::Tmux)?;
    tmux_control::commands::subscribe(
        client,
        &name,
        SubscriptionScope::AllWindows,
        SUBSCRIPTION_FORMAT,
    )
    .map_err(DaemonError::Tmux)?;

    // Built once: the heartbeat never varies, so encoding it per cycle would
    // re-ask a question already answered (`[LAW:dataflow-not-control-flow]` —
    // a loop-invariant value, not a per-iteration decision).
    let heartbeat =
        CommandLine::new("display-message", ["-p", ""]).map_err(|e| DaemonError::Tmux(e.into()))?;

    // Nothing saved yet is the normal first run; any other failure is
    // reported, and the first save then recaptures every pane in full.
    let mut previous_content = match store.load_latest() {
        Ok(snapshot) => previous_content_from_snapshot(&snapshot),
        Err(StoreError::NoLatest) => HashMap::new(),
        Err(e) => {
            on_error(&DaemonError::Store(e));
            HashMap::new()
        }
    };

    let mut state = DebounceState::new(Instant::now());

    while should_continue() {
        if let Err(e) = client.execute(&heartbeat) {
            let fatal = is_connection_dead(&e);
            let err = DaemonError::Tmux(e);
            on_error(&err);
            if fatal {
                // DESIGN.md §8: "if tmux isn't running the connection sits in
                // Closed/Reconnecting" — reconnecting is `run_resilient`'s
                // job, not this loop's.
                return Err(err);
            }
        }
        if activity.take() {
            state.record_activity(Instant::now());
        }

        if state.should_save(&config.policy, Instant::now()) {
            match capture_and_save(client, store, &previous_content, config.keep_generations) {
                Ok(snapshot) => {
                    previous_content = previous_content_from_snapshot(&snapshot);
                    state.record_save(Instant::now());
                    boot = Boot::Settled;
                }
                // A declining boot met by a server that holds only bootstrap
                // sessions was wrong about it: end the run so `run_resilient`
                // boots again, where that decision lives. Only a declined boot
                // is reopened, so a restore can never repeat itself.
                Err(declined @ DaemonError::Store(StoreError::BootstrapOnly))
                    if boot == Boot::Declined =>
                {
                    return Err(declined)
                }
                Err(e) => on_error(&e),
            }
        }

        std::thread::sleep(config.poll_interval);
    }

    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    /// When [`run`] saves.
    pub policy: DebouncePolicy,
    /// How often [`run`] wakes to drain notifications and decide.
    pub poll_interval: Duration,
    /// How long [`run_resilient`] waits between connection attempts.
    pub reconnect_interval: Duration,
    pub keep_generations: NonZeroUsize,
}

/// Runs until `should_continue` returns `false`, reconnecting — including
/// boot restore — whenever the connection is lost or was never established.
/// DESIGN.md §8: "Independent of the tmux server — if tmux isn't running the
/// connection sits in `Closed`/`Reconnecting` and the daemon idles." A
/// service-supervised daemon can start before the user has ever touched
/// tmux, or keep running across a tmux server restart; it must not exit and
/// leave recovery to launchd/systemd respawning the whole process.
pub fn run_resilient(
    socket: Option<String>,
    store: &Store,
    config: &RunConfig,
    mut on_log: impl FnMut(&str),
    mut should_continue: impl FnMut() -> bool,
) {
    while should_continue() {
        let activity = StructureActivity::new();
        match connect_and_boot(socket.clone(), store, &mut on_log, activity.sink()) {
            Ok(Some((mut client, boot))) => {
                on_log("connected");
                let result = run(
                    &mut client,
                    &activity,
                    boot,
                    store,
                    config,
                    |e| on_log(&format!("save cycle error: {e}")),
                    &mut should_continue,
                );
                client.close();
                match result {
                    Ok(()) => {}
                    Err(DaemonError::Store(StoreError::BootstrapOnly)) => on_log(
                        "the server boot stayed out of holds only bootstrap sessions; booting again",
                    ),
                    Err(e) => on_log(&format!("connection lost ({e}); will retry")),
                }
            }
            Ok(None) => {}
            Err(e) => on_log(&format!("could not connect ({e}); will retry")),
        }

        if should_continue() {
            std::thread::sleep(config.reconnect_interval);
        }
    }
}

#[cfg(test)]
mod tests {
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

    fn changed(name: &str) -> ServerMessage {
        ServerMessage::SubscriptionChanged {
            name: name.to_string(),
            session: None,
            window: None,
            window_index: None,
            pane: None,
            value: String::new(),
        }
    }

    #[test]
    fn the_subscription_name_constant_is_a_valid_subscription_name() {
        assert!(SubscriptionName::new(SUBSCRIPTION_NAME).is_ok());
    }

    #[test]
    fn activity_is_marked_only_by_this_daemons_subscription_and_cleared_on_take() {
        let activity = StructureActivity::new();
        let mut sink = activity.sink();

        sink(changed("someone-elses-subscription"));
        assert!(!activity.take());

        sink(changed(SUBSCRIPTION_NAME));
        assert!(activity.take());
        assert!(!activity.take(), "take must clear the flag");
    }

    #[test]
    fn a_dead_transport_makes_run_return_promptly_instead_of_looping_forever() {
        let activity = StructureActivity::new();
        let mut client = Client::new(DeadTransport, activity.sink(), |_, _| {});
        let config = RunConfig {
            policy: DebouncePolicy {
                debounce: Duration::from_secs(3600),
                max_interval: Duration::from_secs(3600),
            },
            poll_interval: Duration::from_millis(1),
            reconnect_interval: Duration::from_millis(1),
            keep_generations: NonZeroUsize::new(5).unwrap(),
        };
        let store = Store::new(std::env::temp_dir().join(format!(
            "phoenix-daemon-dead-transport-test-{}",
            std::process::id()
        )));

        let error_count = RefCell::new(0);
        let result = run(
            &mut client,
            &activity,
            Boot::Declined,
            &store,
            &config,
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
        let guard = tmux_control::Guard {
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
