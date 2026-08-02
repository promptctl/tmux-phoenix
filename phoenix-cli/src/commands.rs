//! The effectful side of each subcommand (DESIGN.md §9): connect to tmux,
//! capture, persist. stdout carries only machine-parseable output; every
//! diagnostic goes to stderr, prefixed with the subcommand so `phoenix save`
//! and `phoenix list` output can be told apart in a combined log.

use phoenix_core::Snapshot;
use phoenix_store::Store;
use tmux_control::{Client, SpawnOptions, SpawnTransport};

/// DESIGN.md §9's contract: `0` ok, `3` degraded, `1` fail.
pub const EXIT_OK: i32 = 0;
pub const EXIT_DEGRADED: i32 = 3;
pub const EXIT_FAIL: i32 = 1;

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

    let snapshot = match phoenix_capture::capture(&mut client) {
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
