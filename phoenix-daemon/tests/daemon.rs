//! Live: `run()` driven against a real, isolated tmux server.

mod support;
use support::IsolatedTmux;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use phoenix_daemon::DebouncePolicy;
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

fn connect(harness: &IsolatedTmux) -> Client<SpawnTransport> {
    let transport = SpawnTransport::spawn(
        &["attach-session", "-t", &harness.session],
        &SpawnOptions {
            socket: Some(harness.socket.clone()),
            ..Default::default()
        },
    )
    .expect("failed to spawn tmux -C");
    Client::connect(transport).expect("handshake failed against real tmux")
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
    let mut client = connect(&harness);
    let data_dir = TestDataDir::new("debounce");
    let store = Store::new(&data_dir.0);

    let policy = DebouncePolicy {
        debounce: Duration::from_millis(300),
        max_interval: Duration::from_secs(60),
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
        &store,
        &policy,
        Duration::from_millis(100),
        5,
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
    let mut client = connect(&harness);
    let data_dir = TestDataDir::new("backstop");
    let store = Store::new(&data_dir.0);

    let policy = DebouncePolicy {
        debounce: Duration::from_secs(60), // never fires on its own here
        max_interval: Duration::from_millis(300),
    };

    let errors = Rc::new(RefCell::new(Vec::new()));
    let errors_clone = errors.clone();

    phoenix_daemon::run(
        &mut client,
        &store,
        &policy,
        Duration::from_millis(100),
        5,
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
