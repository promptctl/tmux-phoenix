//! The effect boundary (DESIGN.md §5, `[LAW:effects-at-boundaries]`): one
//! `list-panes -a -F` round-trip over an existing `tmux-control` connection,
//! one system-wide `ps` pass, optionally one content-capture pass
//! (dirty-tracked — see [`crate::content`]), then the pure fold. Everything
//! past parsing the `list-panes` reply and running `ps`/`capture-pane` is
//! pure and lives in [`crate::fold`].

use std::collections::HashMap;
use std::time::SystemTime;

use phoenix_core::{FormatVersion, OffsetDateTime, PaneContent, Snapshot, TmuxVersion};
use tmux_control::{Client, CommandLine, TmuxError, Transport};

use crate::argv::recover_argv;
use crate::content::{capture_content, PreviousPaneContent};
use crate::fold::{fold, FoldError};
use crate::row::{format_string, parse_row, PaneRow, RowParseError};

/// Whether [`capture`] should also pull pane text (DESIGN.md §5: "plus one
/// `capture-pane` per pane when content capture is on"). `On` carries
/// whatever [`PreviousPaneContent`] the caller has for each pane (keyed by
/// tmux's `%N` pane id) — usually every pane's content from the last
/// persisted `Snapshot` — so unchanged panes can skip a full re-capture.
/// This crate has no persistence dependency of its own
/// (`[LAW:one-way-deps]`), so building that map from a loaded `Snapshot` is
/// the caller's job.
pub enum ContentCapture {
    Off,
    On {
        previous: HashMap<u32, PreviousPaneContent>,
    },
}

#[derive(Debug)]
pub enum CaptureError {
    ListPanes(TmuxError),
    VersionProbe(TmuxError),
    Content(TmuxError),
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
            CaptureError::Content(e) => write!(f, "content capture's indicator query failed: {e}"),
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
/// torn tree. Argv recovery (via `ps`) and content capture (via
/// `capture-pane`, when `content` is [`ContentCapture::On`]) are separately
/// best-effort per pane — a failure there degrades that one pane (empty
/// `argv`, absent `content`) rather than failing the capture (DESIGN.md §5).
pub fn capture<T: Transport>(
    client: &mut Client<T>,
    content: ContentCapture,
) -> Result<Snapshot, CaptureError> {
    let list_panes = CommandLine::new("list-panes", ["-a", "-F", format_string().as_str()])
        .map_err(|err| CaptureError::ListPanes(err.into()))?;
    let output = client
        .execute(&list_panes)
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

    let content_by_pane_id: HashMap<u32, PaneContent> = match content {
        ContentCapture::Off => HashMap::new(),
        ContentCapture::On { previous } => {
            capture_content(client, &previous).map_err(CaptureError::Content)?
        }
    };
    let content_of = |pane_id: u32| content_by_pane_id.get(&pane_id).cloned();

    let sessions = fold(rows, &argv_of, &content_of).map_err(CaptureError::Fold)?;

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
