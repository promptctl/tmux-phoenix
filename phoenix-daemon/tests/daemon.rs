//! Live: `run()` driven against a real, isolated tmux server.

mod support;
use support::IsolatedTmux;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use phoenix_daemon::{Boot, DebouncePolicy, RunConfig, StructureActivity};
use phoenix_store::Store;
use tmux_control::{Client, SpawnOptions, SpawnTransport};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDataDir(PathBuf);

impl TestDataDir {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "phoenix-daemon-test-{name}-{}-{nanos}-{n}",
            std::process::id()
        ));
        Self(path)
    }
}

impl Drop for TestDataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn connect(harness: &IsolatedTmux, activity: &StructureActivity) -> Client<SpawnTransport> {
    let transport = SpawnTransport::spawn(
        &["attach-session", "-t", &harness.session],
        &SpawnOptions {
            socket: Some(harness.socket.clone()),
            ..Default::default()
        },
    )
    .expect("failed to spawn tmux -C");
    Client::connect(transport, activity.sink(), |_, _| {})
        .expect("handshake failed against real tmux")
}

fn bounded_continue(max_iterations: u32) -> impl FnMut() -> bool {
    let mut count = 0;
    move || {
        count += 1;
        count <= max_iterations
    }
}

#[test]
fn daemon_saves_after_a_structural_change_settles() {
    let harness = IsolatedTmux::new("daemon-debounce");
    let activity = StructureActivity::new();
    let mut client = connect(&harness, &activity);
    let data_dir = TestDataDir::new("debounce");
    let store = Store::new(&data_dir.0);

    let config = RunConfig {
        policy: DebouncePolicy {
            debounce: Duration::from_millis(300),
            max_interval: Duration::from_secs(60),
        },
        poll_interval: Duration::from_millis(100),
        reconnect_interval: Duration::from_millis(100),
        keep_generations: std::num::NonZeroUsize::new(5).unwrap(),
    };

    let socket = harness.socket.clone();
    let session = harness.session.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        std::process::Command::new("tmux")
            .args(["-S", &socket, "split-window", "-t", &session])
            .status()
            .expect("failed to split-window");
    });

    let errors = Rc::new(RefCell::new(Vec::new()));
    let errors_clone = errors.clone();

    phoenix_daemon::run(
        &mut client,
        &activity,
        Boot::Settled,
        &store,
        &config,
        move |e| errors_clone.borrow_mut().push(e.to_string()),
        bounded_continue(30), // 30 * 100ms = 3s, comfortably past debounce+split
    )
    .expect("run() itself failed");

    assert!(
        errors.borrow().is_empty(),
        "unexpected daemon errors: {:?}",
        errors.borrow()
    );

    let saved = store
        .load_latest()
        .expect("expected the debounced save to have happened");
    assert_eq!(
        saved.sessions.first().active_window().panes().len(),
        2,
        "the save should reflect the split that triggered it"
    );

    client.close();
}

#[test]
fn daemon_max_interval_backstop_saves_with_zero_structural_activity() {
    let harness = IsolatedTmux::new("daemon-backstop");
    harness.build();
    let activity = StructureActivity::new();
    let mut client = connect(&harness, &activity);
    let data_dir = TestDataDir::new("backstop");
    let store = Store::new(&data_dir.0);

    let config = RunConfig {
        policy: DebouncePolicy {
            debounce: Duration::from_secs(60), // never fires on its own here
            max_interval: Duration::from_millis(300),
        },
        poll_interval: Duration::from_millis(100),
        reconnect_interval: Duration::from_millis(100),
        keep_generations: std::num::NonZeroUsize::new(5).unwrap(),
    };

    let errors = Rc::new(RefCell::new(Vec::new()));
    let errors_clone = errors.clone();

    phoenix_daemon::run(
        &mut client,
        &activity,
        Boot::Settled,
        &store,
        &config,
        move |e| errors_clone.borrow_mut().push(e.to_string()),
        bounded_continue(10), // 10 * 100ms = 1s, comfortably past max_interval
    )
    .expect("run() itself failed");

    assert!(
        errors.borrow().is_empty(),
        "unexpected daemon errors: {:?}",
        errors.borrow()
    );

    store
        .load_latest()
        .expect("the max-interval backstop should have saved even with no activity");

    client.close();
}

/// Blocks until the server holds only bootstrap sessions as `probe` sees
/// them: a session just created is still starting its shell.
fn wait_until_bootstrap_only(socket: &str) {
    for _ in 0..50 {
        if let Ok(phoenix_restore::ServerState::BootstrapOnly(_)) =
            phoenix_restore::probe(Some(socket))
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{socket} never settled into a bootstrap-only server within 5s");
}

/// Runs `run` for 1.2s — well past the first max-interval cycle — against a
/// login terminal's bootstrap-only server, with a built prior state already
/// saved as `latest`. Returns the run's result, the errors it reported, and
/// asserts `latest` still names the prior state.
fn run_over_a_login_server(
    name: &str,
    boot: Boot,
) -> (Result<(), phoenix_daemon::DaemonError>, Vec<String>) {
    let data_dir = TestDataDir::new(name);
    let store = Store::new(&data_dir.0);

    let prior = IsolatedTmux::new(&format!("{name}-prior"));
    prior.build();
    let mut prior_client = connect(&prior, &StructureActivity::new());
    let prior_state =
        phoenix_capture::capture(&mut prior_client, phoenix_capture::ContentCapture::Off)
            .expect("failed to capture the prior state");
    prior_client.close();
    store
        .save(
            &prior_state,
            std::num::NonZeroUsize::new(5).unwrap(),
            Duration::ZERO,
        )
        .expect("failed to seed the prior state");

    // An interactive shell that reads no startup files, so nothing but the
    // shell itself is ever in the pane's foreground while the run saves.
    let login = IsolatedTmux::with_command(&format!("{name}-login"), "sh -i");
    wait_until_bootstrap_only(&login.socket);
    let activity = StructureActivity::new();
    let mut client = connect(&login, &activity);
    let config = RunConfig {
        policy: DebouncePolicy {
            debounce: Duration::from_secs(60),
            max_interval: Duration::from_millis(400),
        },
        poll_interval: Duration::from_millis(100),
        reconnect_interval: Duration::from_millis(100),
        keep_generations: std::num::NonZeroUsize::new(5).unwrap(),
    };

    let errors = Rc::new(RefCell::new(Vec::new()));
    let errors_clone = errors.clone();
    let result = phoenix_daemon::run(
        &mut client,
        &activity,
        boot,
        &store,
        &config,
        move |e| errors_clone.borrow_mut().push(e.to_string()),
        bounded_continue(12),
    );
    client.close();

    assert_eq!(store.load_latest().unwrap(), prior_state);
    assert_eq!(store.list().unwrap().len(), 1);
    let errors = errors.borrow().clone();
    (result, errors)
}

/// tmux-parity-ure.j0f criterion 3: a server holding only a login terminal's
/// bootstrap session never replaces the real prior state as `latest`, and
/// after a boot that stayed out the declined save ends `run`, so
/// `run_resilient` boots again.
#[test]
fn a_bootstrap_only_server_never_replaces_latest_and_reopens_a_declined_boot() {
    let (result, errors) = run_over_a_login_server("bootstrap-declined", Boot::Declined);

    assert!(errors.is_empty(), "unexpected daemon errors: {errors:?}");
    assert!(
        matches!(
            result,
            Err(phoenix_daemon::DaemonError::Store(
                phoenix_store::StoreError::BootstrapOnly
            ))
        ),
        "the declined save should end the run: {result:?}"
    );
}

/// After a boot that restored (or had nothing to restore), a server that
/// reads as bootstrap-only is the user's: the refusal is reported and the run
/// keeps going, so a restore never repeats itself.
#[test]
fn a_settled_boot_reports_the_refusal_and_keeps_running() {
    let (result, errors) = run_over_a_login_server("bootstrap-settled", Boot::Settled);

    assert!(result.is_ok(), "the run should not end: {result:?}");
    assert!(
        errors.iter().any(|e| e.contains("bootstrap session")),
        "the refusal should be reported: {errors:?}"
    );
}
