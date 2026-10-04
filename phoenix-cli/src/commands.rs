//! The effectful side of each subcommand (DESIGN.md §9): connect to tmux,
//! capture, persist. stdout carries only machine-parseable output; every
//! diagnostic goes to stderr, prefixed with the subcommand so `phoenix save`
//! and `phoenix list` output can be told apart in a combined log.

use std::path::Path;

use phoenix_capture::Previous;
use phoenix_core::{GenerationId, Shells, Snapshot};
use phoenix_restore::{Note, Onto, Plan};
use phoenix_store::{Retention, Store, StoreError};
use tmux_control::{Attach, Client, Connection, Opened, SpawnOptions, SpawnTransport, TmuxError};

/// DESIGN.md §9's contract: `0` ok, `3` degraded, `1` fail.
pub const EXIT_OK: i32 = 0;
pub const EXIT_DEGRADED: i32 = 3;
pub const EXIT_FAIL: i32 = 1;

/// How long `save` waits for a concurrent save (typically the daemon's) to
/// finish before failing: one save is an encode and a few fsyncs, so this is
/// generous for one in flight, and short enough for a person at a prompt.
const SAVE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Bare `attach-session` (no `-t`) attaches to the server's most recently
/// used session, which `save` can rely on existing: it reads a live server
/// the user is looking at.
fn connect(socket: Option<String>) -> Result<Client<SpawnTransport>, String> {
    let transport = SpawnTransport::spawn(
        &["attach-session"],
        &SpawnOptions {
            socket,
            ..Default::default()
        },
    )
    .map_err(|e| format!("failed to spawn tmux: {e}"))?;
    // `save` issues commands and reads replies; nothing it does wants the
    // server's unsolicited notifications or pane output, so both sinks drop.
    Client::connect(transport, drop, |_, _| {})
        .map_err(|e| format!("failed to connect to tmux: {e}"))
}

fn open_store() -> Result<Store, String> {
    Store::xdg_default().map_err(|e| format!("could not determine save directory: {e}"))
}

pub fn run_save(keep: std::num::NonZeroUsize, socket: Option<String>) -> i32 {
    let mut client = match connect(socket) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("phoenix save: {msg}");
            return EXIT_FAIL;
        }
    };

    let store = match open_store() {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("phoenix save: {msg}");
            return EXIT_FAIL;
        }
    };

    // The previous generation's indicators let an unchanged pane reuse its
    // scrollback; nothing saved yet is the normal first run. Any other
    // failure to read it is reported, and this save then recaptures every
    // pane in full.
    let previous = match store.load_latest() {
        Ok(snapshot) => Previous::from_snapshot(&snapshot),
        Err(StoreError::NoLatest) => Previous::default(),
        Err(e) => {
            eprintln!("phoenix save: warning: failed to load the latest snapshot: {e}");
            Previous::default()
        }
    };
    let snapshot = match phoenix_capture::capture(&mut client, &previous, &Shells::default()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("phoenix save: capture failed: {e}");
            client.close();
            return EXIT_FAIL;
        }
    };
    client.close();

    // Until the recorded provenance replaces it (tmux-laws-a4x.9xh), the
    // bootstrap heuristic keeps a login terminal's fresh server from being
    // published over the generation it is about to be restored from; the
    // daemon makes the same refusal from the same `Snapshot` method.
    if snapshot.is_bootstrap_only() {
        eprintln!(
            "phoenix save: not saved: every session on the server is an untouched bootstrap \
             session (one window, one idle shell), and saving it would replace the latest snapshot"
        );
        return EXIT_FAIL;
    }

    let retention = Retention {
        keep_untagged: keep,
    };
    let outcome = match store.save(&snapshot, None, retention, SAVE_WAIT) {
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

    for unreadable in &outcome.unreadable {
        eprintln!("phoenix save: warning: {unreadable}");
    }

    let degradations = snapshot.degradations();
    for degradation in &degradations {
        eprintln!("phoenix save: warning: {degradation}");
    }
    if degradations.is_empty() {
        EXIT_OK
    } else {
        EXIT_DEGRADED
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
                    "{}\t{}\t{}\t{}\t{}",
                    g.captured_at.unix_timestamp(),
                    g.format_version.0,
                    g.origin,
                    g.tag.as_ref().map(|t| t.as_str()).unwrap_or("-"),
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

/// The snapshot to restore with its generation id, which is what the
/// windows it builds are stamped with: `file` when given (`restore --file`),
/// otherwise the store's newest.
fn load_snapshot(file: Option<&str>) -> Result<(GenerationId, Snapshot), String> {
    let store = open_store()?;
    let Some(path) = file.map(Path::new) else {
        return store
            .latest()
            .map_err(|e| format!("failed to load the latest snapshot: {e}"));
    };
    let generation = phoenix_store::generation_of(path).ok_or_else(|| {
        format!(
            "{} is not named snapshot-<id>.phnx, so the windows restored from it could not be \
             marked as that generation's",
            path.display()
        )
    })?;
    let snapshot = store
        .load_file(path)
        .map_err(|e| format!("failed to load {}: {e}", path.display()))?;
    Ok((generation, snapshot))
}

/// Plans the restore of the latest (or `file`) snapshot onto the server on
/// `socket` and, unless `dry_run`, applies it over the same connection. A
/// dry run may not make a session, so it attaches only to one that exists
/// and otherwise previews the restore that would make one; a real restore
/// has decided to. `tmux-laws-a4x.12k` moves this sequence into
/// `phoenix-ops`, where the daemon shares it.
fn restore(dry_run: bool, file: Option<&str>, socket: Option<String>) -> Result<Plan, String> {
    let (generation, snapshot) = load_snapshot(file)?;
    let scratch = phoenix_restore::scratch_name(&snapshot);
    let options = SpawnOptions {
        socket,
        ..Default::default()
    };
    let connect = |e: TmuxError| format!("failed to connect to tmux: {e}");
    let planned = |connection: &mut Connection, opened: &Opened| {
        let live = phoenix_capture::capture(connection, &Previous::default(), &Shells::default())
            .map_err(|e| format!("failed to read what the server holds: {e}"))?;
        Ok::<Plan, String>(phoenix_restore::plan(
            generation,
            &snapshot,
            Onto::opened(opened, &live),
        ))
    };

    if dry_run {
        return match Connection::open(&options, Attach::Existing, drop) {
            Ok((mut connection, opened)) => planned(&mut connection, &opened),
            Err(TmuxError::NoSessions) => Ok(phoenix_restore::plan(
                generation,
                &snapshot,
                Onto::NoServer { scratch: &scratch },
            )),
            Err(e) => Err(connect(e)),
        };
    }
    let attach = Attach::OrCreate { name: scratch };
    let (mut connection, opened) = Connection::open(&options, attach, drop).map_err(connect)?;
    let plan = planned(&mut connection, &opened)?;
    phoenix_restore::apply(&mut connection, &plan)
        .map_err(|e| format!("{e} ({})", plan.steps()[e.step]))?;
    Ok(plan)
}

/// `--dry-run` prints the plan's steps on stdout and runs none of them —
/// the safety property for a tool that can `send-keys` into live shells.
/// A real restore prints how many it ran. Either way each thing the plan
/// left alone or placed differently goes to stderr, and a saved session
/// that could not be restored makes the exit code 3.
pub fn run_restore(dry_run: bool, file: Option<String>, socket: Option<String>) -> i32 {
    let plan = match restore(dry_run, file.as_deref(), socket) {
        Ok(plan) => plan,
        Err(msg) => {
            eprintln!("phoenix restore: {msg}");
            return EXIT_FAIL;
        }
    };

    if dry_run {
        plan.steps().iter().for_each(|step| println!("{step}"));
    } else {
        println!("restored: {} step(s) applied", plan.steps().len());
    }
    for note in plan.notes() {
        eprintln!("phoenix restore: {note}");
    }
    let degraded = plan
        .notes()
        .iter()
        .any(|note| matches!(note, Note::Unaddressable(_)));
    if degraded {
        EXIT_DEGRADED
    } else {
        EXIT_OK
    }
}

/// Runs in the foreground (DESIGN.md §8: "`phoenix daemon` runs it
/// foreground for debugging"); the service `install` writes runs this same
/// subcommand. Returns only if the store can't be opened:
/// `phoenix_daemon::run_resilient` reconnects, boot restore included, for as
/// long as the process lives, and reports every failure on stderr.
pub fn run_daemon(settings: crate::cli::DaemonSettings, socket: Option<String>) -> i32 {
    let store = match open_store() {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("phoenix daemon: {msg}");
            return EXIT_FAIL;
        }
    };

    let config = phoenix_daemon::RunConfig {
        policy: phoenix_daemon::DebouncePolicy {
            debounce: std::time::Duration::from_secs(settings.debounce_secs),
            max_interval: std::time::Duration::from_secs(settings.max_interval_secs),
        },
        poll_interval: std::time::Duration::from_secs(1),
        reconnect_interval: std::time::Duration::from_secs(5),
        keep_generations: settings.keep,
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
/// activate it — never runs that command itself: starting a persistent,
/// reboot-surviving background process is the user's call, not a side effect
/// of writing a config file.
pub fn run_install(settings: crate::cli::DaemonSettings) -> i32 {
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

    let Some(plan) = crate::install::plan_for_this_platform(&exe, &home, settings) else {
        eprintln!(
            "phoenix install: unsupported platform (only macOS launchd and Linux systemd --user are supported)"
        );
        return EXIT_FAIL;
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
