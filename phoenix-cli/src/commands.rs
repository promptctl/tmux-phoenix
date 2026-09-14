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
