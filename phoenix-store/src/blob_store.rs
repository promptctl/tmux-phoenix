//! Content-addressed blob storage (DESIGN.md §7, tmux-content-dos.2):
//! "unchanged scrollback stored once across saves, unchanged panes become
//! pointer copies." A blob's name *is* its hash, so two saves whose pane
//! content is byte-identical (the common case for an unchanged pane, since
//! `phoenix-capture`'s dirty-tracking carries the same scrollback bytes
//! forward verbatim) write the same file twice and the second write is a
//! no-op — deduplication falls out of content-addressing for free, no
//! separate dedup pass needed.
//!
//! Concurrent writers racing to store the *same* blob are safe without any
//! locking: same hash implies same content (barring a hash collision, which
//! [`crate::checksum::content_hash`]'s doc comment addresses), so whichever
//! writer's `rename` lands last still leaves the correct bytes at that path.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use crate::checksum::content_hash;
use crate::error::StoreError;

pub struct BlobStore {
    dir: PathBuf,
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl BlobStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn blob_path(&self, hash: &[u8; 16]) -> PathBuf {
        self.dir.join(hex_encode(hash))
    }

    /// Stores `content` under its hash unless a blob with that hash already
    /// exists (dedup), and returns the hash either way.
    pub fn put(&self, content: &[u8]) -> Result<[u8; 16], StoreError> {
        let hash = content_hash(content);
        let path = self.blob_path(&hash);
        if path.exists() {
            return Ok(hash);
        }

        fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!(
            ".tmp-blob-{}-{}",
            std::process::id(),
            hex_encode(&hash)
        ));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(content)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(hash)
    }

    pub fn get(&self, hash: &[u8; 16]) -> Result<Vec<u8>, StoreError> {
        fs::read(self.blob_path(hash)).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                StoreError::BlobNotFound
            } else {
                StoreError::Io(e)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "phoenix-blob-store-test-{name}-{}-{nanos}-{n}",
                std::process::id()
            ));
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn put_then_get_round_trips() {
        let dir = TestDir::new("round-trip");
        let store = BlobStore::new(&dir.0);
        let hash = store.put(b"pane scrollback text").unwrap();
        assert_eq!(store.get(&hash).unwrap(), b"pane scrollback text");
    }

    #[test]
    fn identical_content_deduplicates_to_one_file() {
        let dir = TestDir::new("dedup");
        let store = BlobStore::new(&dir.0);
        let hash_a = store.put(b"same content").unwrap();
        let hash_b = store.put(b"same content").unwrap();
        assert_eq!(hash_a, hash_b);

        let entries: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| !e.file_name().to_string_lossy().starts_with(".tmp"))
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "identical content should produce one blob file"
        );
    }

    #[test]
    fn different_content_gets_different_hashes() {
        let dir = TestDir::new("distinct");
        let store = BlobStore::new(&dir.0);
        let hash_a = store.put(b"content one").unwrap();
        let hash_b = store.put(b"content two").unwrap();
        assert_ne!(hash_a, hash_b);
    }

    #[test]
    fn get_of_an_unknown_hash_is_not_found() {
        let dir = TestDir::new("missing");
        let store = BlobStore::new(&dir.0);
        assert!(matches!(
            store.get(&[0u8; 16]),
            Err(StoreError::BlobNotFound)
        ));
    }
}
