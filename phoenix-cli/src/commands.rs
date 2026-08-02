//! The effectful side of each subcommand (DESIGN.md §9): connect to tmux,
//! capture, persist. stdout carries only machine-parseable output; every
//! diagnostic goes to stderr, prefixed with the subcommand so `phoenix save`
//! and `phoenix list` output can be told apart in a combined log.

use std::path::Path;

use phoenix_core::Snapshot;
use phoenix_restore::RestorePolicy;
use phoenix_store::Store;
use tmux_control::{Client, SpawnOptions, SpawnTransport};

/// DESIGN.md §9's contract: `0` ok, `3` degraded, `1` fail.
pub const EXIT_OK: i32 = 0;
pub const EXIT_DEGRADED: i32 = 3;
pub const EXIT_FAIL: i32 = 1;

/// `attach-session` (no `-t`, so it attaches to the server's
/// most-recently-used session) needs *some* session to already exist on
/// the target server. That's always true for `save` (it's reading a live
/// session) and true for `restore` whenever it runs against an
/// already-running server. Bootstrapping a restore onto a completely
/// empty/nonexistent server — where `attach-session` has nothing to attach
/// to at all — is out of scope here; verified live that it needs a
/// different connection strategy (bare `tmux -C` will auto-create its own
/// throwaway session just to have somewhere to attach, which would need
/// cleaning up afterward). That's the daemon's boot-restore job (DESIGN.md
/// §8), a later milestone, not this CLI command's.
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
    match file {
        Some(path) => {
            Store::load_file(Path::new(path)).map_err(|e| format!("failed to load {path:?}: {e}"))
        }
        None => {
            let store = open_store()?;
            store
                .load_latest()
                .map_err(|e| format!("failed to load the latest snapshot: {e}"))
        }
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

    let restore_plan = phoenix_restore::plan(&snapshot, &RestorePolicy);

    if dry_run {
        // Render the whole plan before printing any of it: a plan holding an
        // unencodable name is not a plan a human should half-see.
        let lines: Result<Vec<_>, _> = restore_plan
            .commands
            .iter()
            .map(|cmd| cmd.to_command_line())
            .collect();
        return match lines {
            Ok(lines) => {
                lines.iter().for_each(|line| println!("{}", line.as_str()));
                EXIT_OK
            }
            Err(err) => {
                eprintln!("phoenix restore: {err}");
                EXIT_FAIL
            }
        };
    }

    let mut client = match connect(socket) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("phoenix restore: {msg}");
            return EXIT_FAIL;
        }
    };

    let result = phoenix_restore::apply(&mut client, &restore_plan);
    client.close();

    match result {
        Ok(outcome) => {
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
