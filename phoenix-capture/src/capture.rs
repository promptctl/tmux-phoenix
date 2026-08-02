//! The effect boundary (DESIGN.md §5, `[LAW:effects-at-boundaries]`): one
//! `list-panes -a -F` round-trip over an existing `tmux-control` connection,
//! one system-wide `ps` pass, then the pure fold. Everything past parsing the
//! `list-panes` reply and running `ps` is pure and lives in [`crate::fold`].

use std::time::SystemTime;

use phoenix_core::{FormatVersion, OffsetDateTime, Snapshot, TmuxVersion};
use tmux_control::{Client, TmuxError, Transport};

use crate::argv::recover_argv;
use crate::fold::{fold, FoldError};
use crate::row::{format_string, parse_row, PaneRow, RowParseError};

#[derive(Debug)]
pub enum CaptureError {
    ListPanes(TmuxError),
    VersionProbe(TmuxError),
    MalformedRow { line: String, reason: RowParseError },
    NotUtf8 { line: Vec<u8> },
    Fold(FoldError),
    Clock(std::time::SystemTimeError),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::ListPanes(e) => write!(f, "list-panes failed: {e}"),
            CaptureError::VersionProbe(e) => write!(f, "tmux version probe failed: {e}"),
            CaptureError::MalformedRow { line, reason } => {
                write!(f, "malformed list-panes row {line:?}: {reason}")
            }
            CaptureError::NotUtf8 { line } => {
                write!(f, "list-panes row was not valid UTF-8: {line:?}")
            }
            CaptureError::Fold(e) => write!(f, "{e}"),
            CaptureError::Clock(e) => write!(f, "system clock error: {e}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Interrogate the tmux server on the other end of `client` into a
/// [`Snapshot`]. Structure capture is all-or-nothing: any malformed row or
/// unplaceable active index fails the whole capture rather than persisting a
/// torn tree. Argv recovery (via `ps`) is separately best-effort — a `ps`
/// failure degrades every pane's `argv` to empty rather than failing the
/// capture (DESIGN.md §5).
pub fn capture<T: Transport>(client: &mut Client<T>) -> Result<Snapshot, CaptureError> {
    let output = client
        .execute(&format!(
            "list-panes -a -F {}",
            tmux_control::commands::tmux_escape(&format_string())
        ))
        .map_err(CaptureError::ListPanes)?;

    let rows: Vec<PaneRow> = output
        .lines
        .iter()
        .map(|line| {
            let text = String::from_utf8(line.clone())
                .map_err(|_| CaptureError::NotUtf8 { line: line.clone() })?;
            parse_row(&text).map_err(|reason| CaptureError::MalformedRow { line: text, reason })
        })
        .collect::<Result<_, _>>()?;

    let pane_pids: Vec<u32> = rows.iter().map(|r| r.pane_pid).collect();
    let argv_by_pid = recover_argv(&pane_pids);
    let argv_of = |pid: u32| argv_by_pid.get(&pid).cloned().unwrap_or_default();

    let sessions = fold(rows, &argv_of).map_err(CaptureError::Fold)?;

    let server_version =
        tmux_control::commands::query_tmux_version(client).map_err(CaptureError::VersionProbe)?;
    let tmux_version = TmuxVersion {
        major: server_version.major,
        minor: server_version.minor,
    };

    let captured_at = OffsetDateTime::try_from(SystemTime::now()).map_err(CaptureError::Clock)?;

    Ok(Snapshot {
        format_version: FormatVersion::CURRENT,
        tmux_version,
        captured_at,
        sessions,
    })
}
