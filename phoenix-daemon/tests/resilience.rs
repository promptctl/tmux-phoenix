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
        socket: Some(socket.clone()),
    };

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let iterations = Arc::new(AtomicU32::new(0));

    let log_clone = log.clone();
    let stop_clone = stop.clone();
    let iterations_clone = iterations.clone();
    let store_for_thread = Store::new(&data_dir);

    let handle = std::thread::spawn(move || {
        phoenix_daemon::run_resilient(
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

/// tmux-parity-ure.j0f: the boot probe reads idleness at one instant, so a
/// lone pane running a program — or a login shell's prompt briefly running
/// `git` — keeps boot restore out. Once that pane is idle, the store refuses
/// to save it, and the daemon boots again and restores in its place.
#[test]
fn run_resilient_restores_once_a_lone_pane_goes_idle() {
    let socket = format!("/tmp/{}", unique("phx-goes-idle"));
    let data_dir = std::env::temp_dir().join(unique("phx-goes-idle-data"));
    let store = Store::new(&data_dir);

    let pane = phoenix_core::Pane {
        id: phoenix_core::PaneId(0),
        index: phoenix_core::PaneIndex(0),
        cwd: None,
        program: phoenix_core::CapturedProgram {
            command: phoenix_core::ProgramName::parse("zsh").unwrap(),
            argv: None,
        },
        content: None,
    };
    let window = phoenix_core::Window::new(
        phoenix_core::WindowIndex(0),
        phoenix_core::WindowName::parse("shell").unwrap(),
        phoenix_core::Layout::parse("b25d,80x24,0,0,0").unwrap(),
        phoenix_core::NonEmpty::singleton(pane),
        phoenix_core::PaneIndex(0),
    )
    .unwrap();
    let work = phoenix_core::Session::new(
        phoenix_core::SessionName::parse("work").unwrap(),
        phoenix_core::NonEmpty::singleton(window),
        phoenix_core::WindowIndex(0),
    )
    .unwrap();
    store
        .save(
            &phoenix_core::Snapshot {
                format_version: phoenix_core::FormatVersion::CURRENT,
                tmux_version: phoenix_core::TmuxVersion { major: 3, minor: 6 },
                captured_at: phoenix_core::OffsetDateTime::from_unix_timestamp(1_700_000_000),
                sessions: phoenix_core::NonEmpty::singleton(work),
            },
            std::num::NonZeroUsize::new(5).unwrap(),
            Duration::ZERO,
        )
        .expect("failed to seed the snapshot to restore");

    // The pane runs a program for three seconds, then becomes an idle
    // interactive shell. Started as the pane's own command rather than typed
    // in, since keys sent while a shell initializes can be swallowed.
    let status = std::process::Command::new("tmux")
        .args([
            "-S",
            &socket,
            "new-session",
            "-d",
            "-s",
            "0",
            "sh -c 'sleep 3; exec sh -i'",
        ])
        .status()
        .expect("failed to start the lone pane's program");
    assert!(status.success());
    let mut running = false;
    for _ in 0..50 {
        if let Ok(phoenix_restore::ServerState::Built(_)) = phoenix_restore::probe(Some(&socket)) {
            running = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(running, "the lone pane never showed its program running");

    let config = phoenix_daemon::RunConfig {
        policy: DebouncePolicy {
            debounce: Duration::from_secs(60),
            // The first save cycle comes after `sleep 3` has exited.
            max_interval: Duration::from_secs(5),
        },
        poll_interval: Duration::from_millis(100),
        reconnect_interval: Duration::from_millis(300),
        keep_generations: std::num::NonZeroUsize::new(5).unwrap(),
        socket: Some(socket.clone()),
    };
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let log_clone = log.clone();
    let stop_clone = stop.clone();
    let store_for_thread = Store::new(&data_dir);
    let handle = std::thread::spawn(move || {
        phoenix_daemon::run_resilient(
            &store_for_thread,
            &config,
            move |line| log_clone.lock().unwrap().push(line.to_string()),
            move || !stop_clone.load(Ordering::Relaxed),
        );
    });

    let mut restored = false;
    for _ in 0..100 {
        if session_names(&socket) == vec!["work".to_string()] {
            restored = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    stop.store(true, Ordering::Relaxed);
    handle.join().expect("daemon thread panicked");
    let log = log.lock().unwrap();
    let names = session_names(&socket);
    for name in &names {
        let _ = std::process::Command::new("tmux")
            .args(["-S", &socket, "kill-session", "-t", name])
            .status();
    }
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&data_dir);

    assert!(
        restored,
        "expected work restored in place of the idle lone pane; sessions: {names:?}; log: {log:#?}"
    );
    assert!(
        log.iter().any(|l| l.contains("not restoring into it")),
        "the first boot should have stayed out while the program ran; log: {log:#?}"
    );
    assert!(
        log.iter().any(|l| l.contains("booting again"))
            && log.iter().any(|l| l.contains("restored 1 session")),
        "the declined save should have led to a restore; log: {log:#?}"
    );
}

fn session_names(socket: &str) -> Vec<String> {
    let out = std::process::Command::new("tmux")
        .args(["-S", socket, "list-sessions", "-F", "#{session_name}"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// tmux-parity-ure.j0f criterion 4, terminal first: the server goes away,
/// and a terminal starts tmux again before the daemon reconnects. The daemon
/// still brings the saved session back in place of the terminal's bootstrap
/// session, and `latest` keeps naming the real state. (Daemon first is
/// `run_resilient_reconnects_and_boot_restores_after_the_server_disappears_and_comes_back`.)
#[test]
fn run_resilient_restores_over_a_terminal_that_reached_tmux_first() {
    let socket = format!("/tmp/{}", unique("phx-terminal-first"));
    let session = unique("phx-terminal-first-session");
    let data_dir = std::env::temp_dir().join(unique("phx-terminal-first-data"));

    for args in [
        vec!["-S", &socket, "new-session", "-d", "-s", &session],
        vec!["-S", &socket, "split-window", "-t", &session],
    ] {
        let status = std::process::Command::new("tmux")
            .args(&args)
            .status()
            .expect("failed to build the session to save");
        assert!(status.success());
    }

    let store = Store::new(&data_dir);
    let config = phoenix_daemon::RunConfig {
        policy: DebouncePolicy {
            debounce: Duration::from_millis(200),
            max_interval: Duration::from_millis(500),
        },
        poll_interval: Duration::from_millis(100),
        // Long enough that the terminal below reaches the restarted server
        // well before the daemon tries again.
        reconnect_interval: Duration::from_secs(2),
        keep_generations: std::num::NonZeroUsize::new(5).unwrap(),
        socket: Some(socket.clone()),
    };

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let log_clone = log.clone();
    let stop_clone = stop.clone();
    let store_for_thread = Store::new(&data_dir);
    let handle = std::thread::spawn(move || {
        phoenix_daemon::run_resilient(
            &store_for_thread,
            &config,
            move |line| log_clone.lock().unwrap().push(line.to_string()),
            move || !stop_clone.load(Ordering::Relaxed),
        );
    });

    let mut saved = false;
    for _ in 0..40 {
        if store
            .load_latest()
            .is_ok_and(|s| s.sessions.first().active_window().panes().len() == 2)
        {
            saved = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        saved,
        "expected a save of the built session before the outage"
    );

    let status = std::process::Command::new("tmux")
        .args(["-S", &socket, "kill-session", "-t", &session])
        .status()
        .expect("failed to kill the session");
    assert!(status.success());
    let status = std::process::Command::new("tmux")
        .args(["-S", &socket, "new-session", "-d", "-s", "0"])
        .status()
        .expect("failed to start the terminal's session");
    assert!(status.success());

    let mut recovered = false;
    for _ in 0..60 {
        if session_names(&socket) == vec![session.clone()] && pane_count(&socket, &session) == 2 {
            recovered = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    stop.store(true, Ordering::Relaxed);
    handle.join().expect("daemon thread panicked");

    let log = log.lock().unwrap();
    let names = session_names(&socket);
    for name in &names {
        let _ = std::process::Command::new("tmux")
            .args(["-S", &socket, "kill-session", "-t", name])
            .status();
    }
    let _ = std::fs::remove_file(&socket);

    assert!(
        recovered,
        "expected the saved session back in place of the terminal's; sessions: {names:?}; log: {log:#?}"
    );
    assert!(
        log.iter().any(|l| l.contains("restored 1 session")),
        "expected a boot-restore log line; log: {log:#?}"
    );
    let latest = store.load_latest().expect("latest should still load");
    assert_eq!(latest.sessions.first().name().as_str(), session.as_str());
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// Reconnecting to the server the daemon was already running on is not a
/// boot. The daemon's connection sits on the session the user built; when the
/// user closes it, the connection ends, and the lone idle session they kept
/// stays theirs rather than having `latest` restored over it.
#[test]
fn run_resilient_never_restores_over_the_server_it_reconnects_to() {
    let socket = format!("/tmp/{}", unique("phx-same-server"));
    let data_dir = std::env::temp_dir().join(unique("phx-same-server-data"));
    let store = Store::new(&data_dir);

    let pane = phoenix_core::Pane {
        id: phoenix_core::PaneId(0),
        index: phoenix_core::PaneIndex(0),
        cwd: None,
        program: phoenix_core::CapturedProgram {
            command: phoenix_core::ProgramName::parse("zsh").unwrap(),
            argv: None,
        },
        content: None,
    };
    let window = phoenix_core::Window::new(
        phoenix_core::WindowIndex(0),
        phoenix_core::WindowName::parse("shell").unwrap(),
        phoenix_core::Layout::parse("b25d,80x24,0,0,0").unwrap(),
        phoenix_core::NonEmpty::singleton(pane),
        phoenix_core::PaneIndex(0),
    )
    .unwrap();
    let saved = phoenix_core::Session::new(
        phoenix_core::SessionName::parse("saved").unwrap(),
        phoenix_core::NonEmpty::singleton(window),
        phoenix_core::WindowIndex(0),
    )
    .unwrap();
    store
        .save(
            &phoenix_core::Snapshot {
                format_version: phoenix_core::FormatVersion::CURRENT,
                tmux_version: phoenix_core::TmuxVersion { major: 3, minor: 6 },
                captured_at: phoenix_core::OffsetDateTime::from_unix_timestamp(1_700_000_000),
                sessions: phoenix_core::NonEmpty::singleton(saved),
            },
            std::num::NonZeroUsize::new(5).unwrap(),
            Duration::ZERO,
        )
        .expect("failed to seed a snapshot that must not come back");

    // The kept session first, a shell that reads no startup files; the built
    // one last, so the daemon's bare attach lands on it.
    for args in [
        vec!["-S", &socket, "new-session", "-d", "-s", "kept", "sh -i"],
        vec!["-S", &socket, "new-session", "-d", "-s", "work"],
        vec!["-S", &socket, "split-window", "-t", "=work:"],
    ] {
        let status = std::process::Command::new("tmux")
            .args(&args)
            .status()
            .expect("failed to build the server");
        assert!(status.success(), "tmux {args:?}");
    }

    let config = phoenix_daemon::RunConfig {
        policy: DebouncePolicy {
            debounce: Duration::from_secs(60),
            max_interval: Duration::from_secs(3600),
        },
        poll_interval: Duration::from_millis(100),
        reconnect_interval: Duration::from_millis(300),
        keep_generations: std::num::NonZeroUsize::new(5).unwrap(),
        socket: Some(socket.clone()),
    };
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let log_clone = log.clone();
    let stop_clone = stop.clone();
    let store_for_thread = Store::new(&data_dir);
    let handle = std::thread::spawn(move || {
        phoenix_daemon::run_resilient(
            &store_for_thread,
            &config,
            move |line| log_clone.lock().unwrap().push(line.to_string()),
            move || !stop_clone.load(Ordering::Relaxed),
        );
    });
    let connections = |log: &Arc<Mutex<Vec<String>>>| {
        log.lock()
            .unwrap()
            .iter()
            .filter(|l| l.as_str() == "connected")
            .count()
    };

    for _ in 0..50 {
        if connections(&log) == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let status = std::process::Command::new("tmux")
        .args(["-S", &socket, "kill-session", "-t", "=work"])
        .status()
        .expect("failed to close the built session");
    assert!(status.success());
    for _ in 0..60 {
        if connections(&log) >= 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    stop.store(true, Ordering::Relaxed);
    handle.join().expect("daemon thread panicked");
    let log = log.lock().unwrap();
    let names = session_names(&socket);
    for name in &names {
        let _ = std::process::Command::new("tmux")
            .args(["-S", &socket, "kill-session", "-t", &format!("={name}")])
            .status();
    }
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&data_dir);

    assert!(
        log.iter().any(|l| l.contains("reconnected to the server")),
        "the daemon should have reconnected to the same server; log: {log:#?}"
    );
    assert!(
        !log.iter().any(|l| l.contains("restored")),
        "nothing may be restored over the kept session; log: {log:#?}"
    );
    assert_eq!(names, vec!["kept".to_string()], "log: {log:#?}");
}

/// A reconnect resumes the boot decision the daemon's last run on that server
/// ended with. The daemon saves, which settles it; the user closes the session
/// the daemon's connection sits on while another session they built still
/// runs, and only after the reconnect empties that one back to one idle shell.
/// The refusal that follows is the user's server, not a misread boot to reopen.
#[test]
fn run_resilient_keeps_the_boot_decision_of_the_server_it_reconnects_to() {
    let socket = format!("/tmp/{}", unique("phx-kept-decision"));
    let data_dir = std::env::temp_dir().join(unique("phx-kept-decision-data"));
    let store = Store::new(&data_dir);

    // `kept` is two shells that read no startup files, built until one closes;
    // `work` is built and comes last, so the daemon's bare attach lands on it.
    for args in [
        vec!["-S", &socket, "new-session", "-d", "-s", "kept", "sh -i"],
        vec!["-S", &socket, "split-window", "-t", "=kept:", "sh -i"],
        vec!["-S", &socket, "new-session", "-d", "-s", "work"],
        vec!["-S", &socket, "split-window", "-t", "=work:"],
    ] {
        let status = std::process::Command::new("tmux")
            .args(&args)
            .status()
            .expect("failed to build the server");
        assert!(status.success(), "tmux {args:?}");
    }

    let config = phoenix_daemon::RunConfig {
        policy: DebouncePolicy {
            debounce: Duration::from_secs(60),
            max_interval: Duration::from_millis(500),
        },
        poll_interval: Duration::from_millis(100),
        reconnect_interval: Duration::from_millis(300),
        keep_generations: std::num::NonZeroUsize::new(5).unwrap(),
        socket: Some(socket.clone()),
    };
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let log_clone = log.clone();
    let stop_clone = stop.clone();
    let store_for_thread = Store::new(&data_dir);
    let handle = std::thread::spawn(move || {
        phoenix_daemon::run_resilient(
            &store_for_thread,
            &config,
            move |line| log_clone.lock().unwrap().push(line.to_string()),
            move || !stop_clone.load(Ordering::Relaxed),
        );
    });
    let tmux = |args: &[&str]| {
        let status = std::process::Command::new("tmux")
            .args(["-S", &socket])
            .args(args)
            .status()
            .expect("failed to run tmux");
        assert!(status.success(), "tmux {args:?}");
    };
    let logged = |pred: &dyn Fn(&str) -> bool| log.lock().unwrap().iter().any(|l| pred(l));
    let eventually = |pred: &dyn Fn() -> bool| {
        (0..60).any(|_| {
            let met = pred();
            if !met {
                std::thread::sleep(Duration::from_millis(100));
            }
            met
        })
    };

    let saved = eventually(&|| {
        store
            .load_latest()
            .is_ok_and(|s| s.sessions.iter().any(|s| s.name().as_str() == "work"))
    });
    tmux(&["kill-session", "-t", "=work"]);
    let reconnected = eventually(&|| {
        log.lock()
            .unwrap()
            .iter()
            .filter(|l| l.as_str() == "connected")
            .count()
            >= 2
    });
    // The active pane is the split, so this leaves `kept` one idle shell.
    tmux(&["kill-pane", "-t", "=kept:"]);
    let refused_as_users =
        |l: &str| l.starts_with("save cycle error") && l.contains("bootstrap session");
    eventually(&|| logged(&|l| refused_as_users(l) || l.contains("restored")));

    stop.store(true, Ordering::Relaxed);
    handle.join().expect("daemon thread panicked");
    let log = log.lock().unwrap();
    let names = session_names(&socket);
    for name in &names {
        let _ = std::process::Command::new("tmux")
            .args(["-S", &socket, "kill-session", "-t", &format!("={name}")])
            .status();
    }
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&data_dir);

    assert!(saved, "the first run should have saved; log: {log:#?}");
    assert!(
        reconnected,
        "the daemon should have reconnected; log: {log:#?}"
    );
    assert!(
        !log.iter().any(|l| l.contains("restored")),
        "nothing may be restored over the kept session; log: {log:#?}"
    );
    assert!(
        log.iter().any(|l| refused_as_users(l)),
        "the refusal should be reported as the user's; log: {log:#?}"
    );
    assert_eq!(names, vec!["kept".to_string()], "log: {log:#?}");
}
