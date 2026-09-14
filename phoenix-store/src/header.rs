//! The on-disk file format: a fixed 32-byte header (magic, format version,
//! capture timestamp, body length, checksum) followed by the codec's body
//! bytes (DESIGN.md §7: "a versioned header with a checksum; an unknown
//! `format_version` is refused loudly, never guessed").
//!
//! `captured_at` lives in the header, not just the body, so a listing can
//! read 32 bytes per snapshot instead of decoding the whole tree.

use phoenix_core::{FormatVersion, OffsetDateTime, Snapshot};

use crate::blob_store::BlobStore;
use crate::checksum::fnv1a_64;
use crate::codec::{decode_body, encode_body};
use crate::error::StoreError;

const MAGIC: [u8; 4] = *b"PHNX";
const HEADER_LEN: usize = 4 + 4 + 8 + 8 + 8;

pub struct Header {
    pub format_version: u32,
    pub captured_at: i64,
    pub body_len: u64,
    pub checksum: u64,
}

pub fn encode_file(snapshot: &Snapshot, blobs: &BlobStore) -> Result<Vec<u8>, StoreError> {
    let body = encode_body(snapshot, blobs)?;
    let checksum = fnv1a_64(&body);

    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&snapshot.format_version.0.to_le_bytes());
    out.extend_from_slice(&snapshot.captured_at.unix_timestamp().to_le_bytes());
    out.extend_from_slice(&(body.len() as u64).to_le_bytes());
    out.extend_from_slice(&checksum.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Header-only decode (for a fast `list`) — does not touch or validate the
/// body's checksum.
pub fn decode_header(bytes: &[u8]) -> Result<Header, StoreError> {
    if bytes.len() < HEADER_LEN {
        return Err(StoreError::Truncated);
    }
    if bytes[0..4] != MAGIC {
        return Err(StoreError::BadMagic);
    }
    let format_version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let captured_at = i64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let body_len = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let checksum = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    Ok(Header {
        format_version,
        captured_at,
        body_len,
        checksum,
    })
}

/// Full decode: validates magic, format version, length, and checksum
/// before handing the body to [`decode_body`].
pub fn decode_file(bytes: &[u8], blobs: &BlobStore) -> Result<Snapshot, StoreError> {
    let header = decode_header(bytes)?;

    if header.format_version != FormatVersion::CURRENT.0 {
        return Err(StoreError::UnsupportedFormatVersion {
            found: header.format_version,
        });
    }

    let body = &bytes[HEADER_LEN..];
    if body.len() as u64 != header.body_len {
        return Err(StoreError::Truncated);
    }
    if fnv1a_64(body) != header.checksum {
        return Err(StoreError::ChecksumMismatch);
    }

    let format_version = FormatVersion(header.format_version);
    let captured_at = OffsetDateTime::from_unix_timestamp(header.captured_at);
    decode_body(body, format_version, captured_at, blobs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::{
        CapturedProgram, Layout, NonEmpty, Pane, PaneId, PaneIndex, ProgramName, Session,
        SessionName, TmuxVersion, Utf8PathBuf, Window, WindowIndex, WindowName,
    };
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
                "phoenix-header-test-blobs-{name}-{}-{nanos}-{n}",
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
        let pane = Pane {
            id: PaneId(0),
            index: PaneIndex(0),
            cwd: Utf8PathBuf::parse("/home/user"),
            program: CapturedProgram {
                command: ProgramName::parse("zsh").unwrap(),
                argv: None,
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

    #[test]
    fn round_trips_through_the_full_file_format() {
        let blob_dir = TestBlobDir::new("round-trip");
        let snapshot = sample_snapshot();
        let bytes = encode_file(&snapshot, &blob_dir.store()).unwrap();
        let decoded = decode_file(&bytes, &blob_dir.store()).unwrap();
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn header_only_decode_reads_captured_at_without_the_body() {
        let blob_dir = TestBlobDir::new("header-only");
        let snapshot = sample_snapshot();
        let bytes = encode_file(&snapshot, &blob_dir.store()).unwrap();
        let header = decode_header(&bytes).unwrap();
        assert_eq!(header.captured_at, 1_700_000_000);
        assert_eq!(header.format_version, FormatVersion::CURRENT.0);
    }

    #[test]
    fn rejects_bad_magic() {
        let blob_dir = TestBlobDir::new("bad-magic");
        let mut bytes = encode_file(&sample_snapshot(), &blob_dir.store()).unwrap();
        bytes[0] = b'X';
        assert!(matches!(
            decode_file(&bytes, &blob_dir.store()),
            Err(StoreError::BadMagic)
        ));
    }

    #[test]
    fn rejects_unsupported_format_version_without_touching_the_body() {
        let blob_dir = TestBlobDir::new("unsupported-version");
        let mut bytes = encode_file(&sample_snapshot(), &blob_dir.store()).unwrap();
        bytes[4..8].copy_from_slice(&999u32.to_le_bytes());
        let err = decode_file(&bytes, &blob_dir.store()).unwrap_err();
        assert!(matches!(
            err,
            StoreError::UnsupportedFormatVersion { found: 999 }
        ));
    }

    #[test]
    fn rejects_corrupted_body_via_checksum_mismatch() {
        let blob_dir = TestBlobDir::new("checksum-mismatch");
        let mut bytes = encode_file(&sample_snapshot(), &blob_dir.store()).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        assert!(matches!(
            decode_file(&bytes, &blob_dir.store()),
            Err(StoreError::ChecksumMismatch)
        ));
    }

    #[test]
    fn rejects_a_truncated_file() {
        let blob_dir = TestBlobDir::new("truncated");
        let bytes = encode_file(&sample_snapshot(), &blob_dir.store()).unwrap();
        let truncated = &bytes[..bytes.len() - 5];
        assert!(matches!(
            decode_file(truncated, &blob_dir.store()),
            Err(StoreError::Truncated)
        ));
    }
}
