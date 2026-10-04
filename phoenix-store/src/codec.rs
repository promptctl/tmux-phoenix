//! `Snapshot <-> bytes` for the current format: the §5 graph, windows once
//! and winlinks by reference. Built entirely on `phoenix_core`'s public
//! constructors, so decoding a corrupt file can produce a [`StoreError`]
//! but never an invalid `Snapshot` (`[LAW:types-are-the-program]`).
//!
//! `origin` and `captured_at` live in the file header (see
//! [`crate::header`]), not here — the header is their one home
//! (`[LAW:one-source-of-truth]`); this encodes only the body.
//!
//! A pane's captured text is stored as a [`BlobStore`] hash, not inline —
//! that's why encoding is fallible: a blob write can fail with a real I/O
//! error.

use phoenix_core::{
    Client, ClientName, Content, ContentFailure, Cwd, Foreground, GenerationId, GroupName,
    HistoryIndicator, Layout, Made, NonEmpty, OffsetDateTime, Origin, Pane, PaneId, PaneIndex,
    RecoveryFailure, Session, SessionName, Snapshot, TmuxVersion, Touched, Utf8PathBuf, WinLink,
    Window, WindowId, WindowIndex, WindowName,
};

use crate::binary::{Reader, Writer};
use crate::blob_store::BlobStore;
use crate::error::StoreError;

pub fn encode_body(snapshot: &Snapshot, blobs: &BlobStore) -> Result<Vec<u8>, StoreError> {
    let mut w = Writer::new();
    w.write_u32(snapshot.tmux_version.major);
    w.write_u32(snapshot.tmux_version.minor);
    match snapshot.touched {
        Touched::Never => w.write_u8(0),
        Touched::By(generation) => {
            w.write_u8(1);
            w.write_i64(generation.0);
        }
    }
    let windows: Vec<&Window> = snapshot.windows().iter().collect();
    w.write_u32(windows.len() as u32);
    for window in windows {
        encode_window(&mut w, window, blobs)?;
    }
    let sessions: Vec<&Session> = snapshot.sessions().iter().collect();
    w.write_vec(&sessions, |w, session| encode_session(w, session));
    w.write_vec(snapshot.clients(), |w, client| {
        w.write_str(client.name.as_str());
        w.write_str(client.session.as_str());
    });
    Ok(w.into_bytes())
}

fn encode_session(w: &mut Writer, session: &Session) {
    w.write_str(session.name().as_str());
    write_option(w, session.group(), |w, group| w.write_str(group.as_str()));
    let links: Vec<&WinLink> = session.windows().iter().collect();
    w.write_vec(&links, |w, link| {
        w.write_u32(link.index.0);
        w.write_u32(link.window.0);
    });
    w.write_u32(session.active().0);
    write_option(w, session.last().as_ref(), |w, last| w.write_u32(last.0));
}

fn encode_window(w: &mut Writer, window: &Window, blobs: &BlobStore) -> Result<(), StoreError> {
    w.write_u32(window.id().0);
    match window.made() {
        Made::NotByPhoenix => w.write_u8(0),
        Made::ByPhoenix { generation, saved } => {
            w.write_u8(1);
            w.write_i64(generation.0);
            w.write_u32(saved.0);
        }
    }
    w.write_str(window.name().as_str());
    w.write_str(window.layout().as_str());
    w.write_u8(u8::from(window.zoomed()));
    w.write_u32(window.panes().len() as u32);
    for pane in window.panes().iter() {
        encode_pane(w, pane, blobs)?;
    }
    w.write_u32(window.active().0);
    Ok(())
}

/// The one tag-byte convention for every optional field in the body, so a
/// reader and writer cannot disagree per field (`[LAW:single-enforcer]`).
fn write_option<T>(w: &mut Writer, value: Option<&T>, write: impl FnOnce(&mut Writer, &T)) {
    match value {
        None => w.write_u8(0),
        Some(v) => {
            w.write_u8(1);
            write(w, v);
        }
    }
}

fn read_option<T>(
    r: &mut Reader,
    read: impl FnOnce(&mut Reader) -> Result<T, StoreError>,
) -> Result<Option<T>, StoreError> {
    match r.read_u8()? {
        0 => Ok(None),
        1 => read(r).map(Some),
        value => Err(StoreError::InvalidTag {
            what: "option",
            value,
        }),
    }
}

fn invalid(what: &'static str, value: u8) -> StoreError {
    StoreError::InvalidTag { what, value }
}

fn encode_pane(w: &mut Writer, pane: &Pane, blobs: &BlobStore) -> Result<(), StoreError> {
    w.write_u32(pane.id.0);
    w.write_u32(pane.index.0);
    write_option(w, pane.cwd.known(), |w, cwd| w.write_str(cwd.as_str()));
    match &pane.foreground {
        Foreground::Shell => w.write_u8(0),
        Foreground::Program { argv } => {
            w.write_u8(1);
            let args: Vec<&String> = argv.iter().collect();
            w.write_vec(&args, |w, arg| w.write_str(arg));
        }
        Foreground::Unrecovered { reason } => {
            w.write_u8(2);
            encode_recovery_failure(w, reason);
        }
    }
    match &pane.content {
        Content::NotCaptured { reason } => {
            w.write_u8(0);
            encode_content_failure(w, reason);
        }
        Content::Captured {
            indicator,
            scrollback,
            visible,
        } => {
            w.write_u8(1);
            w.write_u64(indicator.history_size);
            w.write_u64(indicator.history_bytes);
            w.write_bytes(&blobs.put(&encode_lines_blob(scrollback))?);
            w.write_bytes(&blobs.put(&encode_lines_blob(visible))?);
        }
    }
    Ok(())
}

fn encode_recovery_failure(w: &mut Writer, reason: &RecoveryFailure) {
    match reason {
        RecoveryFailure::ShellGone => w.write_u8(0),
        RecoveryFailure::NoTerminal => w.write_u8(1),
        RecoveryFailure::LeaderGone => w.write_u8(2),
        RecoveryFailure::Os { message } => {
            w.write_u8(3);
            w.write_str(message);
        }
        RecoveryFailure::NotRecorded => w.write_u8(4),
    }
}

fn decode_recovery_failure(r: &mut Reader) -> Result<RecoveryFailure, StoreError> {
    Ok(match r.read_u8()? {
        0 => RecoveryFailure::ShellGone,
        1 => RecoveryFailure::NoTerminal,
        2 => RecoveryFailure::LeaderGone,
        3 => RecoveryFailure::Os {
            message: r.read_str()?,
        },
        4 => RecoveryFailure::NotRecorded,
        value => return Err(invalid("RecoveryFailure", value)),
    })
}

fn encode_content_failure(w: &mut Writer, reason: &ContentFailure) {
    match reason {
        ContentFailure::CapturePane { message } => {
            w.write_u8(0);
            w.write_str(message);
        }
        ContentFailure::NotRecorded => w.write_u8(1),
    }
}

fn decode_content_failure(r: &mut Reader) -> Result<ContentFailure, StoreError> {
    Ok(match r.read_u8()? {
        0 => ContentFailure::CapturePane {
            message: r.read_str()?,
        },
        1 => ContentFailure::NotRecorded,
        value => return Err(invalid("ContentFailure", value)),
    })
}

pub(crate) fn encode_lines_blob(lines: &[String]) -> Vec<u8> {
    let mut w = Writer::new();
    w.write_vec(lines, |w, line| w.write_str(line));
    w.into_bytes()
}

pub(crate) fn decode_lines_blob(bytes: &[u8]) -> Result<Vec<String>, StoreError> {
    let mut r = Reader::new(bytes);
    let lines = r.read_vec(Reader::read_str)?;
    if !r.remaining().is_empty() {
        return Err(StoreError::Truncated);
    }
    Ok(lines)
}

pub(crate) fn read_blob_hash(r: &mut Reader) -> Result<[u8; 16], StoreError> {
    r.read_bytes()?
        .try_into()
        .map_err(|_| StoreError::Truncated)
}

pub(crate) fn read_content(
    r: &mut Reader,
    blobs: &BlobStore,
) -> Result<(HistoryIndicator, Vec<String>, Vec<String>), StoreError> {
    let indicator = HistoryIndicator {
        history_size: r.read_u64()?,
        history_bytes: r.read_u64()?,
    };
    let scrollback_hash = read_blob_hash(r)?;
    let scrollback = decode_lines_blob(&blobs.get(&scrollback_hash)?)?;
    let visible_hash = read_blob_hash(r)?;
    let visible = decode_lines_blob(&blobs.get(&visible_hash)?)?;
    Ok((indicator, scrollback, visible))
}

pub(crate) fn read_name<T>(
    r: &mut Reader,
    parse: impl FnOnce(String) -> Option<T>,
) -> Result<T, StoreError> {
    parse(r.read_str()?).ok_or(StoreError::InvalidName)
}

pub(crate) fn read_non_empty<T>(
    r: &mut Reader,
    mut read_item: impl FnMut(&mut Reader) -> Result<T, StoreError>,
) -> Result<NonEmpty<T>, StoreError> {
    let count = r.read_u32()? as usize;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        items.push(read_item(r)?);
    }
    NonEmpty::from_vec(items).ok_or(StoreError::EmptyCollection)
}

/// Takes `origin`/`captured_at` from the already-validated header rather
/// than the body — the header is their single source of truth.
pub fn decode_body(
    bytes: &[u8],
    origin: Origin,
    captured_at: OffsetDateTime,
    blobs: &BlobStore,
) -> Result<Snapshot, StoreError> {
    let mut r = Reader::new(bytes);
    let tmux_version = TmuxVersion {
        major: r.read_u32()?,
        minor: r.read_u32()?,
    };
    let touched = match r.read_u8()? {
        0 => Touched::Never,
        1 => Touched::By(GenerationId(r.read_i64()?)),
        value => return Err(invalid("Touched", value)),
    };
    let windows = read_non_empty(&mut r, |r| decode_window(r, blobs))?;
    let sessions = read_non_empty(&mut r, decode_session)?;
    let clients = r.read_vec(|r| {
        Ok(Client {
            name: read_name(r, ClientName::parse)?,
            session: read_name(r, SessionName::parse)?,
        })
    })?;

    if !r.remaining().is_empty() {
        return Err(StoreError::Truncated);
    }

    Snapshot::new(
        origin,
        touched,
        captured_at,
        tmux_version,
        windows,
        sessions,
        clients,
    )
    .map_err(StoreError::from)
}

fn decode_session(r: &mut Reader) -> Result<Session, StoreError> {
    let name = read_name(r, SessionName::parse)?;
    let group = read_option(r, |r| read_name(r, GroupName::parse))?;
    let links = read_non_empty(r, |r| {
        Ok(WinLink {
            index: WindowIndex(r.read_u32()?),
            window: WindowId(r.read_u32()?),
        })
    })?;
    let active = WindowIndex(r.read_u32()?);
    let last = read_option(r, |r| Ok(WindowIndex(r.read_u32()?)))?;
    Session::new(name, group, links, active, last).map_err(StoreError::from)
}

fn decode_window(r: &mut Reader, blobs: &BlobStore) -> Result<Window, StoreError> {
    let id = WindowId(r.read_u32()?);
    let made = match r.read_u8()? {
        0 => Made::NotByPhoenix,
        1 => Made::ByPhoenix {
            generation: GenerationId(r.read_i64()?),
            saved: WindowId(r.read_u32()?),
        },
        value => return Err(invalid("Made", value)),
    };
    let name = read_name(r, WindowName::parse)?;
    let layout = read_name(r, Layout::parse)?;
    let zoomed = match r.read_u8()? {
        0 => false,
        1 => true,
        value => return Err(invalid("zoomed", value)),
    };
    let panes = read_non_empty(r, |r| decode_pane(r, blobs))?;
    let active = PaneIndex(r.read_u32()?);
    Window::new(id, made, name, layout, zoomed, panes, active).map_err(StoreError::from)
}

fn decode_pane(r: &mut Reader, blobs: &BlobStore) -> Result<Pane, StoreError> {
    let id = PaneId(r.read_u32()?);
    let index = PaneIndex(r.read_u32()?);
    let cwd = match read_option(r, |r| read_name(r, Utf8PathBuf::parse))? {
        Some(path) => Cwd::Known(path),
        None => Cwd::Unreadable,
    };
    let foreground = match r.read_u8()? {
        0 => Foreground::Shell,
        1 => Foreground::Program {
            argv: read_non_empty(r, |r| r.read_str())?,
        },
        2 => Foreground::Unrecovered {
            reason: decode_recovery_failure(r)?,
        },
        value => return Err(invalid("Foreground", value)),
    };
    let content = match r.read_u8()? {
        0 => Content::NotCaptured {
            reason: decode_content_failure(r)?,
        },
        1 => {
            let (indicator, scrollback, visible) = read_content(r, blobs)?;
            Content::Captured {
                indicator,
                scrollback,
                visible,
            }
        }
        value => return Err(invalid("Content", value)),
    };
    Ok(Pane {
        id,
        index,
        cwd,
        foreground,
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{sample_snapshot, TestDir};

    #[test]
    fn round_trips_the_full_graph() {
        let dir = TestDir::new("codec-round-trip");
        let snapshot = sample_snapshot(1_700_000_000);
        let body = encode_body(&snapshot, &dir.blobs()).unwrap();
        let decoded =
            decode_body(&body, snapshot.origin, snapshot.captured_at, &dir.blobs()).unwrap();
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn rejects_truncated_bodies_and_trailing_garbage() {
        let dir = TestDir::new("codec-truncated");
        let snapshot = sample_snapshot(1_700_000_000);
        let body = encode_body(&snapshot, &dir.blobs()).unwrap();

        let mut short = body.clone();
        short.truncate(body.len() - 3);
        assert!(matches!(
            decode_body(&short, snapshot.origin, snapshot.captured_at, &dir.blobs()).unwrap_err(),
            StoreError::Truncated
        ));

        let mut long = body;
        long.push(0xff);
        assert!(matches!(
            decode_body(&long, snapshot.origin, snapshot.captured_at, &dir.blobs()).unwrap_err(),
            StoreError::Truncated
        ));
    }

    #[test]
    fn decode_fails_loudly_when_a_referenced_blob_is_missing() {
        let dir = TestDir::new("codec-missing-blob");
        let snapshot = sample_snapshot(1_700_000_000);
        let body = encode_body(&snapshot, &dir.blobs()).unwrap();
        let _ = std::fs::remove_dir_all(&dir.0);
        assert!(matches!(
            decode_body(&body, snapshot.origin, snapshot.captured_at, &dir.blobs()).unwrap_err(),
            StoreError::BlobNotFound
        ));
    }

    #[test]
    fn identical_pane_content_across_two_encodes_shares_one_blob() {
        let dir = TestDir::new("codec-shared-blob");
        let snapshot = sample_snapshot(1_700_000_000);
        encode_body(&snapshot, &dir.blobs()).unwrap();
        encode_body(&snapshot, &dir.blobs()).unwrap();
        // The sample holds two captured panes with identical text (shared
        // window) plus one with distinct text: scrollback and visible blobs
        // for each distinct body. Encoding twice must not double that.
        let before = dir.blob_count();
        encode_body(&snapshot, &dir.blobs()).unwrap();
        assert_eq!(dir.blob_count(), before);
    }
}
