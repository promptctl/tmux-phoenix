//! `Snapshot <-> bytes`, built entirely on `phoenix_core`'s public
//! constructors and accessors — decoding a corrupt file can produce a
//! [`StoreError`] but can never produce an invalid `Snapshot`, since
//! `phoenix_core` itself won't let one be constructed
//! (`[LAW:types-are-the-program]`, carried through from tmux-capture-4q0.1).
//!
//! `format_version` and `captured_at`/`checksum` live in the file header
//! (see [`crate::header`]), not here — this only encodes the body.
//!
//! A pane's captured text (`scrollback`/`visible`) is stored as a
//! [`crate::blob_store::BlobStore`] hash, not inline (tmux-content-dos.2) —
//! that's why encoding is fallible here (a blob write can fail with a real
//! I/O error) where it wasn't before content capture existed. The tree
//! structure and the `content` option tag are walked/handled with plain
//! loops and manual `u8` tags rather than `Writer`'s old `write_seq`/
//! `write_option` combinators (since removed): those closures couldn't
//! propagate a `Result`, and every call site here now needs one.

use phoenix_core::{
    CapturedProgram, Layout, NonEmpty, Pane, PaneContent, PaneId, PaneIndex, Session, SessionName,
    Snapshot, TmuxVersion, Window, WindowIndex, WindowName,
};

use crate::binary::{Reader, Writer};
use crate::blob_store::BlobStore;
use crate::error::StoreError;

pub fn encode_body(snapshot: &Snapshot, blobs: &BlobStore) -> Result<Vec<u8>, StoreError> {
    let mut w = Writer::new();
    w.write_u32(snapshot.tmux_version.major);
    w.write_u32(snapshot.tmux_version.minor);
    w.write_i64(snapshot.captured_at.unix_timestamp());
    w.write_u32(snapshot.sessions.len() as u32);
    for session in snapshot.sessions.iter() {
        encode_session(&mut w, session, blobs)?;
    }
    Ok(w.into_bytes())
}

fn encode_session(w: &mut Writer, session: &Session, blobs: &BlobStore) -> Result<(), StoreError> {
    w.write_str(session.name().as_str());
    w.write_u32(session.windows().len() as u32);
    for window in session.windows().iter() {
        encode_window(w, window, blobs)?;
    }
    w.write_u32(session.active().0);
    Ok(())
}

fn encode_window(w: &mut Writer, window: &Window, blobs: &BlobStore) -> Result<(), StoreError> {
    w.write_u32(window.index().0);
    w.write_str(window.name().as_str());
    w.write_str(window.layout().as_str());
    w.write_u32(window.panes().len() as u32);
    for pane in window.panes().iter() {
        encode_pane(w, pane, blobs)?;
    }
    w.write_u32(window.active().0);
    Ok(())
}

fn encode_pane(w: &mut Writer, pane: &Pane, blobs: &BlobStore) -> Result<(), StoreError> {
    w.write_u32(pane.index.0);
    w.write_str(pane.cwd.as_str());
    w.write_str(&pane.program.command);
    w.write_vec(&pane.program.argv, |w, arg| w.write_str(arg));
    match &pane.content {
        None => w.write_u8(0),
        Some(content) => {
            w.write_u8(1);
            w.write_u32(content.pane_id.0);
            w.write_u64(content.history_size);
            w.write_u64(content.history_bytes);
            let scrollback_hash = blobs.put(&encode_lines_blob(&content.scrollback))?;
            w.write_bytes(&scrollback_hash);
            let visible_hash = blobs.put(&encode_lines_blob(&content.visible))?;
            w.write_bytes(&visible_hash);
        }
    }
    Ok(())
}

fn encode_lines_blob(lines: &[String]) -> Vec<u8> {
    let mut w = Writer::new();
    w.write_vec(lines, |w, line| w.write_str(line));
    w.into_bytes()
}

fn decode_lines_blob(bytes: &[u8]) -> Result<Vec<String>, StoreError> {
    let mut r = Reader::new(bytes);
    let lines = r.read_vec(Reader::read_str)?;
    if !r.remaining().is_empty() {
        return Err(StoreError::Truncated);
    }
    Ok(lines)
}

/// Takes `format_version`/`captured_at` from the already-validated header
/// (see [`crate::header::Header`]) rather than re-decoding them from the
/// body — the header is the single source of truth for both.
pub fn decode_body(
    bytes: &[u8],
    format_version: phoenix_core::FormatVersion,
    captured_at: phoenix_core::OffsetDateTime,
    blobs: &BlobStore,
) -> Result<Snapshot, StoreError> {
    let mut r = Reader::new(bytes);
    let tmux_version = TmuxVersion {
        major: r.read_u32()?,
        minor: r.read_u32()?,
    };
    let _captured_at_in_body = r.read_i64()?; // redundant with the header; kept for a stable body layout
    let session_count = r.read_u32()? as usize;
    let mut sessions = Vec::with_capacity(session_count);
    for _ in 0..session_count {
        sessions.push(decode_session(&mut r, blobs)?);
    }
    let sessions = NonEmpty::from_vec(sessions).ok_or(StoreError::EmptyCollection)?;

    if !r.remaining().is_empty() {
        return Err(StoreError::Truncated);
    }

    Ok(Snapshot {
        format_version,
        tmux_version,
        captured_at,
        sessions,
    })
}

fn decode_session(r: &mut Reader, blobs: &BlobStore) -> Result<Session, StoreError> {
    let name = SessionName::parse(r.read_str()?).ok_or(StoreError::InvalidName)?;
    let window_count = r.read_u32()? as usize;
    let mut windows = Vec::with_capacity(window_count);
    for _ in 0..window_count {
        windows.push(decode_window(r, blobs)?);
    }
    let windows = NonEmpty::from_vec(windows).ok_or(StoreError::EmptyCollection)?;
    let active = WindowIndex(r.read_u32()?);
    Session::new(name, windows, active).map_err(StoreError::from)
}

fn decode_window(r: &mut Reader, blobs: &BlobStore) -> Result<Window, StoreError> {
    let index = WindowIndex(r.read_u32()?);
    let name = WindowName::parse(r.read_str()?).ok_or(StoreError::InvalidName)?;
    let layout = Layout::parse(r.read_str()?).ok_or(StoreError::InvalidName)?;
    let pane_count = r.read_u32()? as usize;
    let mut panes = Vec::with_capacity(pane_count);
    for _ in 0..pane_count {
        panes.push(decode_pane(r, blobs)?);
    }
    let panes = NonEmpty::from_vec(panes).ok_or(StoreError::EmptyCollection)?;
    let active = PaneIndex(r.read_u32()?);
    Window::new(index, name, layout, panes, active).map_err(StoreError::from)
}

fn read_blob_hash(r: &mut Reader) -> Result<[u8; 16], StoreError> {
    r.read_bytes()?
        .try_into()
        .map_err(|_| StoreError::Truncated)
}

fn decode_pane(r: &mut Reader, blobs: &BlobStore) -> Result<Pane, StoreError> {
    let index = PaneIndex(r.read_u32()?);
    let cwd = r.read_str()?.into();
    let command = r.read_str()?;
    let argv = r.read_vec(Reader::read_str)?;
    let content = match r.read_u8()? {
        0 => None,
        1 => {
            let pane_id = PaneId(r.read_u32()?);
            let history_size = r.read_u64()?;
            let history_bytes = r.read_u64()?;
            let scrollback_hash = read_blob_hash(r)?;
            let scrollback = decode_lines_blob(&blobs.get(&scrollback_hash)?)?;
            let visible_hash = read_blob_hash(r)?;
            let visible = decode_lines_blob(&blobs.get(&visible_hash)?)?;
            Some(PaneContent::new(
                pane_id,
                history_size,
                history_bytes,
                scrollback,
                visible,
            ))
        }
        value => return Err(StoreError::InvalidOptionTag { value }),
    };
    Ok(Pane {
        index,
        cwd,
        program: CapturedProgram::new(command, argv),
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::{FormatVersion, OffsetDateTime};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestBlobDir(PathBuf);

    impl TestBlobDir {
        fn new(name: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "phoenix-codec-test-blobs-{name}-{}-{nanos}-{n}",
                std::process::id()
            ));
            Self(path)
        }

        fn store(&self) -> BlobStore {
            BlobStore::new(&self.0)
        }
    }

    impl Drop for TestBlobDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sample_snapshot() -> Snapshot {
        let pane0 = Pane {
            index: PaneIndex(0),
            cwd: "/home/user".into(),
            program: CapturedProgram::new("zsh", vec![]),
            content: None,
        };
        let pane1 = Pane {
            index: PaneIndex(1),
            cwd: "/home/user/proj".into(),
            program: CapturedProgram::new("vim", vec!["vim".to_string(), "DESIGN.md".to_string()]),
            content: Some(PaneContent::new(
                PaneId(1),
                42,
                4096,
                vec!["line one".to_string(), "line two".to_string()],
                vec!["line two".to_string()],
            )),
        };
        let window = Window::new(
            WindowIndex(0),
            WindowName::parse("shell").unwrap(),
            Layout::parse("b25d,80x24,0,0,0").unwrap(),
            NonEmpty::new(pane0, vec![pane1]),
            PaneIndex(1),
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
    fn round_trips_a_full_tree() {
        let blob_dir = TestBlobDir::new("round-trip");
        let snapshot = sample_snapshot();
        let body = encode_body(&snapshot, &blob_dir.store()).unwrap();
        let decoded = decode_body(
            &body,
            snapshot.format_version,
            snapshot.captured_at,
            &blob_dir.store(),
        )
        .unwrap();
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn rejects_truncated_bodies() {
        let blob_dir = TestBlobDir::new("truncated");
        let snapshot = sample_snapshot();
        let mut body = encode_body(&snapshot, &blob_dir.store()).unwrap();
        body.truncate(body.len() - 3);
        let err = decode_body(
            &body,
            snapshot.format_version,
            snapshot.captured_at,
            &blob_dir.store(),
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::Truncated));
    }

    #[test]
    fn rejects_trailing_garbage_after_a_valid_body() {
        let blob_dir = TestBlobDir::new("trailing-garbage");
        let snapshot = sample_snapshot();
        let mut body = encode_body(&snapshot, &blob_dir.store()).unwrap();
        body.push(0xff);
        let err = decode_body(
            &body,
            snapshot.format_version,
            snapshot.captured_at,
            &blob_dir.store(),
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::Truncated));
    }

    #[test]
    fn decode_fails_loudly_when_a_referenced_blob_is_missing() {
        let blob_dir = TestBlobDir::new("missing-blob");
        let snapshot = sample_snapshot();
        let body = encode_body(&snapshot, &blob_dir.store()).unwrap();
        // Delete the blob store entirely -- every content blob it referenced
        // is now gone.
        let _ = std::fs::remove_dir_all(&blob_dir.0);
        let err = decode_body(
            &body,
            snapshot.format_version,
            snapshot.captured_at,
            &blob_dir.store(),
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::BlobNotFound));
    }

    #[test]
    fn identical_pane_content_across_two_snapshots_shares_one_blob() {
        let blob_dir = TestBlobDir::new("shared-blob");
        let store = blob_dir.store();
        let snapshot = sample_snapshot();
        encode_body(&snapshot, &store).unwrap();
        encode_body(&snapshot, &store).unwrap();

        let blob_files: Vec<_> = std::fs::read_dir(&blob_dir.0)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| !e.file_name().to_string_lossy().starts_with(".tmp"))
            .collect();
        // One pane has content (two distinct blobs: scrollback + visible);
        // encoding the *same* snapshot twice must not double that count.
        assert_eq!(blob_files.len(), 2);
    }
}
