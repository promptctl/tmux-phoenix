//! The effect boundary (ARCHITECTURE.md §6, `[LAW:effects-at-boundaries]`):
//! one fixed sequence of reads over an existing `tmux-control` connection
//! and the OS, always the same operations in the same order
//! (`[LAW:dataflow-not-control-flow]`) — structure including the window
//! mark and the clients, the server mark, then per pane the foreground and
//! content reads, then the server's version and identity, then the clock.
//! What varies is the data: the [`Previous`] indicators the caller passes
//! and the [`Shells`] it names. Everything past parsing is the pure fold
//! in [`crate::fold`].

use std::time::SystemTime;

use phoenix_core::{
    Client, GenerationId, OffsetDateTime, Origin, ServerId, Shells, Snapshot, SnapshotError,
    TmuxVersion, Touched,
};
use tmux_control::{CommandLine, Execute, TmuxError};

use crate::content::{capture_content, Previous};
use crate::fold::{fold, FoldError, PaneReads};
use crate::process;
use crate::row::{client_format, pane_format, parse_client_row, parse_pane_row, RowParseError};

#[derive(Debug)]
pub enum CaptureError {
    ListPanes(TmuxError),
    ListClients(TmuxError),
    ServerMark(TmuxError),
    /// `@phoenix-generation` is set but is not a generation id.
    MalformedServerMark {
        lines: Vec<String>,
    },
    Content(TmuxError),
    VersionProbe(TmuxError),
    ServerIdProbe(TmuxError),
    MalformedServerId {
        line: String,
    },
    MalformedRow {
        line: String,
        reason: RowParseError,
    },
    NotUtf8 {
        line: Vec<u8>,
    },
    Fold(FoldError<TmuxError>),
    Snapshot(SnapshotError),
    Clock(std::time::SystemTimeError),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::ListPanes(e) => write!(f, "list-panes failed: {e}"),
            CaptureError::ListClients(e) => write!(f, "list-clients failed: {e}"),
            CaptureError::ServerMark(e) => write!(f, "reading @phoenix-generation failed: {e}"),
            CaptureError::MalformedServerMark { lines } => {
                write!(
                    f,
                    "@phoenix-generation is set to {lines:?}, not a generation id"
                )
            }
            CaptureError::Content(e) => write!(f, "content capture failed: {e}"),
            CaptureError::VersionProbe(e) => write!(f, "tmux version probe failed: {e}"),
            CaptureError::ServerIdProbe(e) => write!(f, "server identity probe failed: {e}"),
            CaptureError::MalformedServerId { line } => {
                write!(f, "server identity {line:?} is not <pid>:<start_time>")
            }
            CaptureError::MalformedRow { line, reason } => {
                write!(f, "malformed list row {line:?}: {reason}")
            }
            CaptureError::NotUtf8 { line } => write!(f, "list row was not valid UTF-8: {line:?}"),
            CaptureError::Fold(e) => write!(f, "{e}"),
            CaptureError::Snapshot(e) => write!(f, "{e}"),
            CaptureError::Clock(e) => write!(f, "system clock error: {e}"),
        }
    }
}

impl std::error::Error for CaptureError {}

fn utf8_lines(lines: &[Vec<u8>]) -> Result<Vec<String>, CaptureError> {
    lines
        .iter()
        .map(|line| {
            String::from_utf8(line.clone())
                .map_err(|_| CaptureError::NotUtf8 { line: line.clone() })
        })
        .collect()
}

/// `list-panes -a` lists every pane on the server; `list-clients` has no
/// scope flag and already lists every client.
fn list_rows<C: Execute, R>(
    client: &mut C,
    command: &'static str,
    scope: &[&str],
    format: &str,
    parse: impl Fn(&str) -> Result<R, RowParseError>,
    failed: impl Fn(TmuxError) -> CaptureError,
) -> Result<Vec<R>, CaptureError> {
    let args: Vec<&str> = scope.iter().copied().chain(["-F", format]).collect();
    let line = CommandLine::new(command, args).map_err(|e| failed(e.into()))?;
    let output = client.execute(&line).map_err(failed)?;
    utf8_lines(&output.lines)?
        .into_iter()
        .map(|text| {
            parse(&text).map_err(|reason| CaptureError::MalformedRow { line: text, reason })
        })
        .collect()
}

/// `show-options -s -q -v @phoenix-generation`: the server-scope read, so
/// no pane, window or session option of the same name can shadow it.
/// `-q` answers an unset option with no lines at all, while a set one is
/// exactly one line — an empty one if it was set to the empty string — so
/// "unset" and "set to something unparseable" stay apart.
fn server_mark<C: Execute>(client: &mut C) -> Result<Touched, CaptureError> {
    let line = CommandLine::new("show-options", ["-s", "-q", "-v", "@phoenix-generation"])
        .map_err(|e| CaptureError::ServerMark(e.into()))?;
    let output = client.execute(&line).map_err(CaptureError::ServerMark)?;
    let lines = utf8_lines(&output.lines)?;
    match lines.as_slice() {
        [] => Ok(Touched::Never),
        [value] => GenerationId::parse(value)
            .map(Touched::By)
            .ok_or(CaptureError::MalformedServerMark { lines }),
        _ => Err(CaptureError::MalformedServerMark { lines }),
    }
}

fn server_id<C: Execute>(client: &mut C) -> Result<ServerId, CaptureError> {
    let line = CommandLine::new("display-message", ["-p", "#{pid}:#{start_time}"])
        .map_err(|e| CaptureError::ServerIdProbe(e.into()))?;
    let output = client.execute(&line).map_err(CaptureError::ServerIdProbe)?;
    let line = utf8_lines(&output.lines)?
        .into_iter()
        .next()
        .unwrap_or_default();
    ServerId::parse(&line).ok_or(CaptureError::MalformedServerId { line })
}

/// Interrogate the tmux server on the other end of `client` into a
/// [`Snapshot`]. Structure is all-or-nothing: any malformed row or
/// unplaceable active index fails the whole capture rather than persisting a
/// torn graph. The per-pane reads are each a value with a reason — a pane
/// whose process is gone or whose `capture-pane` tmux refused is that one
/// pane's `Unrecovered`/`NotCaptured`, read off `Snapshot::degradations`
/// — while a failure of the connection fails the capture.
pub fn capture<C: Execute>(
    client: &mut C,
    previous: &Previous,
    shells: &Shells,
) -> Result<Snapshot, CaptureError> {
    let rows = list_rows(
        client,
        "list-panes",
        &["-a"],
        &pane_format(),
        parse_pane_row,
        CaptureError::ListPanes,
    )?;
    let clients: Vec<Client> = list_rows(
        client,
        "list-clients",
        &[],
        &client_format(),
        parse_client_row,
        CaptureError::ListClients,
    )?;
    let touched = server_mark(client)?;
    // Read before the panes: content reuse from `previous` is keyed on it.
    let server = server_id(client)?;

    let folded = fold(rows, &mut |row| {
        let foreground = process::foreground(row.pane_pid, shells);
        let content = capture_content(client, server, row.pane_id, row.indicator, previous)?;
        Ok(PaneReads {
            foreground,
            content,
        })
    })
    .map_err(CaptureError::Fold)?;

    let version =
        tmux_control::commands::query_tmux_version(client).map_err(CaptureError::VersionProbe)?;
    let tmux_version = TmuxVersion {
        major: version.major,
        minor: version.minor,
    };
    let origin = Origin::Recorded(server);
    let captured_at = OffsetDateTime::try_from(SystemTime::now()).map_err(CaptureError::Clock)?;

    Snapshot::new(
        origin,
        touched,
        captured_at,
        tmux_version,
        folded.windows,
        folded.sessions,
        clients,
    )
    .map_err(CaptureError::Snapshot)
}
