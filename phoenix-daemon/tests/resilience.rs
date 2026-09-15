//! Live: `run_resilient` actually recovers when the tmux server it's
//! talking to disappears out from under it and comes back later — not just
//! that a dead transport is *detected* (that's `daemon.rs`'s own unit
//! tests), but that the daemon reconnects, and (since the server it
//! reconnects to has no sessions left after dying) exercises boot restore
//! for real: the session it comes back up with should match the last thing
//! it managed to save before the outage.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoenix_daemon::DebouncePolicy;
use phoenix_store::Store;

fn unique(name: &str) -> String {
    format!(
        "{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn pane_count(socket: &str, session: &str) -> usize {
    let out = std::process::Command::new("tmux")
        .args([
            "-S",
            socket,
            "list-panes",
            "-t",
            session,
            "-F",
            "#{pane_index}",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .count(),
        _ => 0,
    }
}

#[test]
fn run_resilient_reconnects_and_boot_restores_after_the_server_disappears_and_comes_back() {
    let socket = format!("/tmp/{}", unique("phx-resilience"));
    let session = unique("phx-resilience-session");
    let data_dir = std::env::temp_dir().join(unique("phx-resilience-data"));

    let status = std::process::Command::new("tmux")
        .args(["-S", &socket, "new-session", "-d", "-s", &session])
        .status()
        .expect("failed to create the initial session");
    assert!(status.success());

    let store = Store::new(&data_dir);
    let config = phoenix_daemon::RunConfig {
        policy: DebouncePolicy {
            debounce: Duration::from_millis(200),
            max_interval: Duration::from_secs(3600),
        },
        poll_interval: Duration::from_millis(100),
        reconnect_interval: Duration::from_millis(300),
        keep_generations: std::num::NonZeroUsize::new(5).unwrap(),
    };

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let iterations = Arc::new(AtomicU32::new(0));

    let log_clone = log.clone();
    let stop_clone = stop.clone();
    let iterations_clone = iterations.clone();
    let socket_clone = socket.clone();
    let store_for_thread = Store::new(&data_dir);

    let handle = std::thread::spawn(move || {
        phoenix_daemon::run_resilient(
            Some(socket_clone),
            &store_for_thread,
            &config,
            move |line| log_clone.lock().unwrap().push(line.to_string()),
            move || {
                iterations_clone.fetch_add(1, Ordering::Relaxed);
                !stop_clone.load(Ordering::Relaxed)
            },
        );
    });

    // Give it time to connect and subscribe, then make a structural change
    // and wait for a *real* save reflecting it -- so there's something for
    // boot restore to recover once the server goes away.
    std::thread::sleep(Duration::from_millis(300));
    let status = std::process::Command::new("tmux")
        .args(["-S", &socket, "split-window", "-t", &session])
        .status()
        .expect("failed to split-window before the outage");
    assert!(status.success());

    let mut pre_outage_saved = false;
    for _ in 0..40 {
        if let Ok(s) = store.load_latest() {
            if s.sessions.first().active_window().panes().len() == 2 {
                pre_outage_saved = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        pre_outage_saved,
        "expected a pre-outage save reflecting the split before simulating the outage"
    );

    // Pull the rug out: killing the only session makes the whole server
    // exit (verified live elsewhere in this crate's tests), simulating
    // "tmux isn't running" mid-run.
    let status = std::process::Command::new("tmux")
        .args(["-S", &socket, "kill-session", "-t", &session])
        .status()
        .expect("failed to kill the session");
    assert!(status.success());

    // With no sessions left and a snapshot on disk, the next reconnect
    // attempt should boot-restore that snapshot back onto the server.
    let mut recovered = false;
    for _ in 0..60 {
        if pane_count(&socket, &session) == 2 {
            recovered = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    stop.store(true, Ordering::Relaxed);
    handle.join().expect("daemon thread panicked");

    let log = log.lock().unwrap();
    assert!(
        recovered,
        "expected the session to be boot-restored with 2 panes after the outage; log: {log:#?}"
    );
    assert!(
        log.iter().any(|l| l.contains("restored 1 session")),
        "expected a boot-restore log line; log: {log:#?}"
    );
    assert!(
        log.iter()
            .any(|l| l.contains("lost") || l.contains("could not connect")),
        "expected the daemon to log the outage; log: {log:#?}"
    );

    let _ = std::process::Command::new("tmux")
        .args(["-S", &socket, "kill-session", "-t", &session])
        .status();
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&data_dir);
}
