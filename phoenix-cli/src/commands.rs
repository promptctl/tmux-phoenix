//! The effectful side of each subcommand (DESIGN.md §9): connect to tmux,
//! capture, persist. stdout carries only machine-parseable output; every
//! diagnostic goes to stderr, prefixed with the subcommand so `phoenix save`
//! and `phoenix list` output can be told apart in a combined log.

use std::path::Path;

use phoenix_core::Snapshot;
use phoenix_store::Store;
use tmux_control::{Client, SpawnOptions, SpawnTransport};

/// DESIGN.md §9's contract: `0` ok, `3` degraded, `1` fail.
pub const EXIT_OK: i32 = 0;
pub const EXIT_DEGRADED: i32 = 3;
pub const EXIT_FAIL: i32 = 1;

/// The `save` connector: bare `attach-session` (no `-t`, so it attaches to
/// the server's most-recently-used session), which needs *some* session to
/// already exist — always true for `save`, since it's reading a live server
/// the user is looking at. `restore` deliberately does *not* use this: it can
/// run against an empty or not-yet-running server, so it goes through
/// `phoenix_restore::connect_and_apply`, which bootstraps a throwaway session
/// when there's nothing to attach to (tmux-parity-ure.1).
fn connect(socket: Option<String>) -> Result<Client<SpawnTransport>, String> {
    let transport = SpawnTransport::spawn(
        &["attach-session"],
        &SpawnOptions {
            socket,
            ..Default::default()
        },
    )
    .map_err(|e| format!("failed to spawn tmux: {e}"))?;
    Client::connect(transport).map_err(|e| format!("failed to connect to tmux: {e}"))
}

fn open_store() -> Result<Store, String> {
    Store::xdg_default().map_err(|e| format!("could not determine save directory: {e}"))
}

/// A pane's `argv` is documented (`phoenix_core::CapturedProgram`) to be
/// empty exactly when best-effort `ps` recovery failed for that pane — so
/// this is a precise, not approximate, signal for DESIGN.md §5/§9's
/// "degraded" save.
fn is_degraded(snapshot: &Snapshot) -> bool {
    snapshot
        .sessions
        .iter()
        .flat_map(|s| s.windows().iter())
        .flat_map(|w| w.panes().iter())
        .any(|p| p.program.argv.is_empty())
}

pub fn run_save(keep: usize, socket: Option<String>) -> i32 {
    let mut client = match connect(socket) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("phoenix save: {msg}");
            return EXIT_FAIL;
        }
    };

    // Content capture (tmux-content-dos.1) isn't wired into `save` yet —
    // that needs a way to load the previous save's per-pane content for
    // dirty-tracking, which belongs with whichever ticket does the
    // content-addressed persistence side (tmux-content-dos.2).
    let snapshot = match phoenix_capture::capture(&mut client, phoenix_capture::ContentCapture::Off)
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("phoenix save: capture failed: {e}");
            client.close();
            return EXIT_FAIL;
        }
    };
    client.close();

    let store = match open_store() {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("phoenix save: {msg}");
            return EXIT_FAIL;
        }
    };

    let degraded = is_degraded(&snapshot);

    let outcome = match store.save(&snapshot, keep) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("phoenix save: failed to write snapshot: {e}");
            return EXIT_FAIL;
        }
    };

    println!("{}", outcome.path.display());
    for (path, err) in &outcome.prune_errors {
        eprintln!(
            "phoenix save: warning: failed to prune {}: {err}",
            path.display()
        );
    }

    if degraded {
        eprintln!("phoenix save: warning: argv recovery degraded for one or more panes");
        EXIT_DEGRADED
    } else {
        EXIT_OK
    }
}

pub fn run_list() -> i32 {
    let store = match open_store() {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("phoenix list: {msg}");
            return EXIT_FAIL;
        }
    };

    match store.list() {
        Ok(generations) => {
            for g in &generations {
                println!(
                    "{}\t{}\t{}",
                    g.captured_at_unix,
                    g.format_version,
                    g.path.display()
                );
            }
            EXIT_OK
        }
        Err(e) => {
            eprintln!("phoenix list: {e}");
            EXIT_FAIL
        }
    }
}

/// `file`: load that snapshot file directly (DESIGN.md §9's `restore
/// --file`); otherwise load the store's `latest`.
fn load_snapshot(file: Option<&str>) -> Result<Snapshot, String> {
    let store = open_store()?;
    match file {
        Some(path) => store
            .load_file(Path::new(path))
            .map_err(|e| format!("failed to load {path:?}: {e}")),
        None => store
            .load_latest()
            .map_err(|e| format!("failed to load the latest snapshot: {e}")),
    }
}

/// `--dry-run` prints exactly the tmux command lines that would run and
/// executes nothing — DESIGN.md §6's safety property for a tool that can
/// `send-keys` into live shells.
pub fn run_restore(dry_run: bool, file: Option<String>, socket: Option<String>) -> i32 {
    let snapshot = match load_snapshot(file.as_deref()) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("phoenix restore: {msg}");
            return EXIT_FAIL;
        }
    };

    let restore_plan = phoenix_restore::plan(&snapshot);

    if dry_run {
        // Render the whole plan before printing any of it: a plan holding an
        // unencodable name is not a plan a human should half-see.
        let lines: Result<Vec<String>, _> = restore_plan
            .commands
            .iter()
            .map(|step| step.describe())
            .collect();
        return match lines {
            Ok(lines) => {
                lines.iter().for_each(|line| println!("{line}"));
                EXIT_OK
            }
            Err(err) => {
                eprintln!("phoenix restore: {err}");
                EXIT_FAIL
            }
        };
    }

    // One connection strategy, shared with the daemon's boot restore
    // (tmux-parity-ure.1): bootstraps a throwaway session when the target
    // server is empty or not yet running, and leaves none behind.
    match phoenix_restore::connect_and_apply(socket, &snapshot, &restore_plan) {
        Ok((mut client, outcome)) => {
            client.close();
            println!(
                "restored {} session(s): {} commands applied, {} redundant move-window(s) skipped",
                snapshot.sessions.len(),
                outcome.executed,
                outcome.skipped_move_window
            );
            EXIT_OK
        }
        Err(e) => {
            eprintln!("phoenix restore: {e}");
            EXIT_FAIL
        }
    }
}

/// Runs foreground (DESIGN.md §8: "`phoenix daemon` runs it foreground for
/// debugging"); a service-supervised background run is `tmux-daemon-b0h.3`'s
/// `install`-generated unit invoking this same subcommand, not a separate
/// code path here. Never returns — `phoenix_daemon::run_resilient` reconnects
/// (including boot restore) whenever the connection is lost or was never
/// established in the first place, so tmux not being up yet (or going away
/// mid-run) doesn't end the process; only `phoenix daemon` itself being
/// killed does. A single save cycle failing is logged to stderr and the
/// daemon keeps running.
pub fn run_daemon(
    keep: usize,
    debounce_secs: u64,
    max_interval_secs: u64,
    socket: Option<String>,
) -> i32 {
    let store = match open_store() {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("phoenix daemon: {msg}");
            return EXIT_FAIL;
        }
    };

    let config = phoenix_daemon::RunConfig {
        policy: phoenix_daemon::DebouncePolicy {
            debounce: std::time::Duration::from_secs(debounce_secs),
            max_interval: std::time::Duration::from_secs(max_interval_secs),
        },
        poll_interval: std::time::Duration::from_secs(1),
        reconnect_interval: std::time::Duration::from_secs(5),
        keep_generations: keep,
    };

    phoenix_daemon::run_resilient(
        socket,
        &store,
        &config,
        |line| eprintln!("phoenix daemon: {line}"),
        || true,
    );

    EXIT_OK
}

/// Writes a launchd/systemd service definition and prints the command to
/// activate it — never runs that command itself (DESIGN.md §8/§9: starting a
/// persistent, reboot-surviving background process is the user's call, not
/// a side effect of writing a config file).
pub fn run_install(keep: usize, debounce_secs: u64, max_interval_secs: u64) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("phoenix install: couldn't determine this binary's path: {e}");
            return EXIT_FAIL;
        }
    };
    let home = match std::env::var_os("HOME") {
        Some(h) => std::path::PathBuf::from(h),
        None => {
            eprintln!("phoenix install: HOME is not set");
            return EXIT_FAIL;
        }
    };

    let plan = match crate::install::plan_for_this_platform(
        &exe,
        &home,
        keep,
        debounce_secs,
        max_interval_secs,
    ) {
        Some(p) => p,
        None => {
            eprintln!(
                    "phoenix install: unsupported platform (only macOS launchd and Linux systemd --user are supported)"
                );
            return EXIT_FAIL;
        }
    };

    match crate::install::write(&plan) {
        Ok(()) => {
            println!("wrote {}", plan.file_path.display());
            println!("to enable now: {}", plan.enable_hint);
            EXIT_OK
        }
        Err(e) => {
            eprintln!(
                "phoenix install: failed to write {}: {e}",
                plan.file_path.display()
            );
            EXIT_FAIL
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::{
        CapturedProgram, FormatVersion, Layout, NonEmpty, OffsetDateTime, Pane, PaneIndex, Session,
        SessionName, TmuxVersion, Window, WindowIndex, WindowName,
    };

    fn snapshot_with_argv(argv: Vec<String>) -> Snapshot {
        let pane = Pane {
            index: PaneIndex(0),
            cwd: "/home/user".into(),
            program: CapturedProgram::new("zsh", argv),
            content: None,
        };
        let window = Window::new(
            WindowIndex(0),
            WindowName::parse("shell").unwrap(),
            Layout::parse("b25d,80x24,0,0,0").unwrap(),
            NonEmpty::singleton(pane),
            PaneIndex(0),
        )
        .unwrap();
        let session = Session::new(
            SessionName::parse("main").unwrap(),
            NonEmpty::singleton(window),
            WindowIndex(0),
        )
        .unwrap();
        Snapshot {
            format_version: FormatVersion::CURRENT,
            tmux_version: TmuxVersion { major: 3, minor: 5 },
            captured_at: OffsetDateTime::from_unix_timestamp(1_700_000_000),
            sessions: NonEmpty::singleton(session),
        }
    }

    #[test]
    fn a_pane_with_empty_argv_is_degraded() {
        assert!(is_degraded(&snapshot_with_argv(vec![])));
    }

    #[test]
    fn a_pane_with_recovered_argv_is_not_degraded() {
        assert!(!is_degraded(&snapshot_with_argv(vec!["zsh".to_string()])));
    }
}
