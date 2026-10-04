//! The body decoder for [`FormatVersion::BEFORE_ORIGIN`] generations, kept
//! so every generation on disk stays readable (ARCHITECTURE.md §7). Read
//! only: a save always writes the current format.
//!
//! That shape was a strict `Session -> Window -> Pane` tree with three
//! bare `Option`s per pane and no window ids. Decoding it into the graph:
//! each window gets a fresh id in decode order (identity within the
//! snapshot is all a `WindowId` is for), is `NotByPhoenix`, and is linked
//! once by the session that held it; an absent argv is
//! `Unrecovered { NotRecorded }`, absent content `NotCaptured { NotRecorded }`;
//! a present argv is classified by the same rule capture applies now, with
//! "tmux reported the shell as the pane's command" standing in for "the
//! pane is at its own process". `touched` is `Never` and `origin` is
//! `BeforeOriginWasRecorded`: neither was recorded.

use phoenix_core::{
    Content, ContentFailure, Cwd, Foreground, Layout, Made, NonEmpty, OffsetDateTime, Origin, Pane,
    PaneId, PaneIndex, RecoveryFailure, Session, SessionName, Shells, Snapshot, TerminalHolder,
    TmuxVersion, Touched, Utf8PathBuf, WinLink, Window, WindowId, WindowIndex, WindowName,
};

use crate::binary::Reader;
use crate::blob_store::BlobStore;
use crate::codec::{read_content, read_name, read_non_empty, read_option};
use crate::error::StoreError;

pub fn decode_body(
    bytes: &[u8],
    captured_at: OffsetDateTime,
    blobs: &BlobStore,
) -> Result<Snapshot, StoreError> {
    let mut r = Reader::new(bytes);
    let tmux_version = TmuxVersion {
        major: r.read_u32()?,
        minor: r.read_u32()?,
    };
    let _captured_at_in_body = r.read_i64()?;
    let shells = Shells::default();
    let mut windows = Vec::new();
    let sessions = read_non_empty(&mut r, |r| {
        let name = read_name(r, SessionName::parse)?;
        let links = read_non_empty(r, |r| {
            let id = WindowId(windows.len() as u32);
            let (index, window) = decode_window(r, id, blobs, &shells)?;
            windows.push(window);
            Ok(WinLink { index, window: id })
        })?;
        let active = WindowIndex(r.read_u32()?);
        Session::new(name, None, links, active, None).map_err(StoreError::from)
    })?;
    if !r.remaining().is_empty() {
        return Err(StoreError::Truncated);
    }
    let windows = NonEmpty::from_vec(windows).ok_or(StoreError::EmptyCollection)?;
    Snapshot::new(
        Origin::BeforeOriginWasRecorded,
        Touched::Never,
        captured_at,
        tmux_version,
        windows,
        sessions,
        Vec::new(),
    )
    .map_err(StoreError::from)
}

fn decode_window(
    r: &mut Reader,
    id: WindowId,
    blobs: &BlobStore,
    shells: &Shells,
) -> Result<(WindowIndex, Window), StoreError> {
    let index = WindowIndex(r.read_u32()?);
    let name = read_name(r, WindowName::parse)?;
    let layout = read_name(r, Layout::parse)?;
    let panes = read_non_empty(r, |r| decode_pane(r, blobs, shells))?;
    let active = PaneIndex(r.read_u32()?);
    let window = Window::new(id, Made::NotByPhoenix, name, layout, false, panes, active)?;
    Ok((index, window))
}

fn decode_pane(r: &mut Reader, blobs: &BlobStore, shells: &Shells) -> Result<Pane, StoreError> {
    let id = PaneId(r.read_u32()?);
    let index = PaneIndex(r.read_u32()?);
    let cwd = match read_option(r, |r| read_name(r, Utf8PathBuf::parse))? {
        Some(path) => Cwd::Known(path),
        None => Cwd::Unreadable,
    };
    let command = r.read_str()?;
    let argv = read_option(r, |r| read_non_empty(r, |r| r.read_str()))?;
    let foreground = match argv {
        None => Foreground::Unrecovered {
            reason: RecoveryFailure::NotRecorded,
        },
        Some(argv) => Foreground::of(
            TerminalHolder {
                at_own_process: shell_command(&command, shells),
                argv,
            },
            shells,
        ),
    };
    let content = match read_option(r, |r| read_content(r, blobs))? {
        None => Content::NotCaptured {
            reason: ContentFailure::NotRecorded,
        },
        Some((indicator, scrollback, visible)) => Content::Captured {
            indicator,
            scrollback,
            visible,
        },
    };
    Ok(Pane {
        id,
        index,
        cwd,
        foreground,
        content,
    })
}

/// The old format's `pane_current_command` names a shell — the half of the
/// old rule that `at_own_process` replaces. Asked of the same rule, so the
/// shell list has one reader.
fn shell_command(command: &str, shells: &Shells) -> bool {
    Foreground::of(
        TerminalHolder {
            at_own_process: true,
            argv: NonEmpty::singleton(command.to_string()),
        },
        shells,
    ) == Foreground::Shell
}
