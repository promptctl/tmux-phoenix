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

/// Bare `attach-session` (no `-t`) attaches to the server's most recently
/// used session, which `save` can rely on existing: it reads a live server
/// the user is looking at. `restore` cannot rely on that, so it goes through
/// `phoenix_restore::connect_and_apply`, which bootstraps an empty server.
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

/// DESIGN.md §5/§9's "degraded" save: some pane's best-effort recovery came
/// back absent — `argv` when `ps` could not resolve its foreground program,
/// `cwd` when tmux could not read its working directory.
fn is_degraded(snapshot: &Snapshot) -> bool {
    snapshot
        .sessions
        .iter()
        .flat_map(|s| s.windows().iter())
        .flat_map(|w| w.panes().iter())
        .any(|p| p.program.argv.is_none() || p.cwd.is_none())
}

pub fn run_save(keep: usize, socket: Option<String>) -> i32 {
    let mut client = match connect(socket) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("phoenix save: {msg}");
            return EXIT_FAIL;
        }
    };

    // One-shot save captures structure only; content capture needs the
    // previous generation's per-pane content for dirty-tracking, which is
    // tmux-parity-ure.3's work.
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

    if is_degraded(&snapshot) {
        eprintln!("phoenix save: warning: argv or cwd recovery degraded for one or more panes");
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

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::{
        CapturedProgram, FormatVersion, Layout, NonEmpty, OffsetDateTime, Pane, PaneId, PaneIndex,
        ProgramName, Session, SessionName, TmuxVersion, Utf8PathBuf, Window, WindowIndex,
        WindowName,
    };

    fn snapshot_with(argv: Option<NonEmpty<String>>, cwd: Option<Utf8PathBuf>) -> Snapshot {
        let pane = Pane {
            id: PaneId(0),
            index: PaneIndex(0),
            cwd,
            program: CapturedProgram {
                command: ProgramName::parse("zsh").unwrap(),
                argv,
            },
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

    fn zsh() -> Option<NonEmpty<String>> {
        Some(NonEmpty::singleton("zsh".to_string()))
    }

    #[test]
    fn absent_argv_is_degraded() {
        assert!(is_degraded(&snapshot_with(
            None,
            Utf8PathBuf::parse("/home")
        )));
    }

    #[test]
    fn absent_cwd_is_degraded() {
        assert!(is_degraded(&snapshot_with(zsh(), None)));
    }

    #[test]
    fn fully_recovered_pane_is_not_degraded() {
        assert!(!is_degraded(&snapshot_with(
            zsh(),
            Utf8PathBuf::parse("/home")
        )));
    }
}
