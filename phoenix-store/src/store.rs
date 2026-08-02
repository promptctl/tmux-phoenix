//! Atomic, versioned, generational persistence (DESIGN.md §7): "temp +
//! fsync + rename; the `latest` pointer only ever names a complete
//! snapshot, so a crash mid-save leaves the last good one intact."
//!
//! `latest` is a symlink (a relative one, to the generation's own file
//! name, so the whole save directory stays portable if moved) — repointing
//! it is itself a `rename(2)` over the symlink's temp name, so the pointer
//! flips atomically the same way the generation file's own write does.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use phoenix_core::Snapshot;

use crate::blob_store::BlobStore;
use crate::error::StoreError;
use crate::header::{decode_file, decode_header, encode_file};

const GENERATION_PREFIX: &str = "snapshot-";
const GENERATION_SUFFIX: &str = ".phnx";
const LATEST_NAME: &str = "latest";
const BLOBS_DIR: &str = "blobs";

/// Deletions [`Store::prune`] made, and ones it tried and failed to make.
type PruneResult = (Vec<PathBuf>, Vec<(PathBuf, io::Error)>);

pub struct Store {
    dir: PathBuf,
}

#[derive(Debug)]
pub struct SaveOutcome {
    pub path: PathBuf,
    /// Older generations successfully deleted this save (beyond the
    /// retention count).
    pub pruned: Vec<PathBuf>,
    /// Older generations this save *tried* to delete but couldn't — the
    /// save itself already succeeded (`latest` is already repointed) by the
    /// time pruning runs, so these are reported rather than failing the
    /// whole operation (DESIGN.md §7's crash-safety story is about the
    /// snapshot itself, not disk cleanup).
    pub prune_errors: Vec<(PathBuf, io::Error)>,
}

#[derive(Debug, Clone)]
pub struct GenerationInfo {
    pub path: PathBuf,
    pub format_version: u32,
    pub captured_at_unix: i64,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `${XDG_DATA_HOME}/tmux-phoenix`, falling back to
    /// `~/.local/share/tmux-phoenix` (DESIGN.md §7).
    pub fn xdg_default() -> io::Result<Self> {
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            if !xdg.is_empty() {
                return Ok(Self::new(PathBuf::from(xdg).join("tmux-phoenix")));
            }
        }
        let home = std::env::var("HOME")
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        Ok(Self::new(
            PathBuf::from(home).join(".local/share/tmux-phoenix"),
        ))
    }

    fn generation_path(&self, generation: i64) -> PathBuf {
        self.dir.join(format!(
            "{GENERATION_PREFIX}{generation}{GENERATION_SUFFIX}"
        ))
    }

    fn latest_path(&self) -> PathBuf {
        self.dir.join(LATEST_NAME)
    }

    /// Content blobs (tmux-content-dos.2) live in their own subdirectory,
    /// shared across every generation in this store — that sharing is the
    /// whole point of content-addressing (an unchanged pane's scrollback
    /// blob is written once and referenced by every generation that has it).
    fn blob_store(&self) -> BlobStore {
        BlobStore::new(self.dir.join(BLOBS_DIR))
    }

    /// Save `snapshot`, then prune down to `keep_generations` (the newest
    /// `keep_generations` files survive; `0` means "keep only this one").
    pub fn save(
        &self,
        snapshot: &Snapshot,
        keep_generations: usize,
    ) -> Result<SaveOutcome, StoreError> {
        fs::create_dir_all(&self.dir)?;

        let generation = self.free_generation_id(snapshot.captured_at.unix_timestamp())?;
        let final_path = self.generation_path(generation);
        let tmp_path = self
            .dir
            .join(format!(".tmp-save-{}-{generation}", std::process::id()));

        let bytes = encode_file(snapshot, &self.blob_store())?;
        {
            let mut f = fs::File::create(&tmp_path)?;
            io::Write::write_all(&mut f, &bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp_path, &final_path)?;
        // Best-effort: durability of the rename itself beyond what the file's
        // own fsync already gives us. Not fatal if the platform disallows
        // opening a directory as a file.
        if let Ok(dir_handle) = fs::File::open(&self.dir) {
            let _ = dir_handle.sync_all();
        }

        self.repoint_latest(&final_path)?;

        let (pruned, prune_errors) = self.prune(keep_generations)?;

        Ok(SaveOutcome {
            path: final_path,
            pruned,
            prune_errors,
        })
    }

    /// `captured_at`'s Unix timestamp is the natural generation id — saves
    /// are seconds apart in practice — but bump past any collision (two
    /// saves landing in the same second) rather than overwrite.
    fn free_generation_id(&self, start: i64) -> io::Result<i64> {
        let mut generation = start;
        while self.generation_path(generation).exists() {
            generation += 1;
        }
        Ok(generation)
    }

    fn repoint_latest(&self, target: &Path) -> io::Result<()> {
        let file_name = target
            .file_name()
            .expect("a generation path always has a file name");
        let tmp_link = self.dir.join(format!(".tmp-latest-{}", std::process::id()));
        let _ = fs::remove_file(&tmp_link);
        std::os::unix::fs::symlink(file_name, &tmp_link)?;
        fs::rename(&tmp_link, self.latest_path())?;
        Ok(())
    }

    pub fn load_latest(&self) -> Result<Snapshot, StoreError> {
        let bytes = fs::read(self.latest_path()).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                StoreError::NoLatest
            } else {
                StoreError::Io(e)
            }
        })?;
        decode_file(&bytes, &self.blob_store())
    }

    /// Loads a specific generation file directly by path — for `restore
    /// --file <path>` (DESIGN.md §9). `path` need not be this store's own
    /// `latest` (e.g. an older, already-pruned generation), but its content
    /// blobs (tmux-content-dos.2) are still resolved against *this* store's
    /// blob directory, since a bare generation file has no blobs of its own.
    pub fn load_file(&self, path: &Path) -> Result<Snapshot, StoreError> {
        let bytes = fs::read(path)?;
        decode_file(&bytes, &self.blob_store())
    }

    /// Every generation's id, newest first.
    fn generation_ids_desc(&self) -> io::Result<Vec<i64>> {
        let mut ids: Vec<i64> = match fs::read_dir(&self.dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .filter_map(|e| generation_id_from_file_name(&e.file_name().to_string_lossy()))
                .collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        ids.sort_unstable_by(|a, b| b.cmp(a));
        Ok(ids)
    }

    /// Header-only summary of every generation, newest first — doesn't
    /// decode any body (DESIGN.md §7's `list`, without paying for a full
    /// snapshot decode per entry).
    pub fn list(&self) -> Result<Vec<GenerationInfo>, StoreError> {
        self.generation_ids_desc()?
            .into_iter()
            .map(|generation| {
                let path = self.generation_path(generation);
                let bytes = fs::read(&path)?;
                let header = decode_header(&bytes)?;
                Ok(GenerationInfo {
                    path,
                    format_version: header.format_version,
                    captured_at_unix: header.captured_at,
                })
            })
            .collect()
    }

    fn prune(&self, keep: usize) -> Result<PruneResult, StoreError> {
        let ids = self.generation_ids_desc()?;
        let mut pruned = Vec::new();
        let mut errors = Vec::new();
        for generation in ids.into_iter().skip(keep) {
            let path = self.generation_path(generation);
            match fs::remove_file(&path) {
                Ok(()) => pruned.push(path),
                Err(e) => errors.push((path, e)),
            }
        }
        Ok((pruned, errors))
    }
}

fn generation_id_from_file_name(name: &str) -> Option<i64> {
    name.strip_prefix(GENERATION_PREFIX)?
        .strip_suffix(GENERATION_SUFFIX)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::{
        CapturedProgram, FormatVersion, Layout, NonEmpty, OffsetDateTime, Pane, PaneIndex, Session,
        SessionName, TmuxVersion, Window, WindowIndex, WindowName,
    };
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
                "phoenix-store-test-{name}-{}-{nanos}-{n}",
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

    fn snapshot_at(captured_at: i64) -> Snapshot {
        let pane = Pane {
            index: PaneIndex(0),
            cwd: "/home/user".into(),
            program: CapturedProgram::new("zsh", vec![]),
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
            captured_at: OffsetDateTime::from_unix_timestamp(captured_at),
            sessions: NonEmpty::singleton(session),
        }
    }

    #[test]
    fn save_then_load_latest_round_trips() {
        let dir = TestDir::new("round-trip");
        let store = Store::new(&dir.0);
        let snapshot = snapshot_at(1_700_000_000);

        store.save(&snapshot, 5).unwrap();
        let loaded = store.load_latest().unwrap();
        assert_eq!(loaded, snapshot);
    }

    #[test]
    fn load_file_reads_a_specific_generation_by_path_directly() {
        let dir = TestDir::new("load-file");
        let store = Store::new(&dir.0);
        let snapshot = snapshot_at(1_700_000_000);

        let outcome = store.save(&snapshot, 5).unwrap();
        let loaded = store.load_file(&outcome.path).unwrap();
        assert_eq!(loaded, snapshot);
    }

    #[test]
    fn load_latest_before_any_save_is_no_latest() {
        let dir = TestDir::new("no-latest");
        let store = Store::new(&dir.0);
        assert!(matches!(store.load_latest(), Err(StoreError::NoLatest)));
    }

    #[test]
    fn latest_always_points_at_the_most_recent_save() {
        let dir = TestDir::new("latest-tracks-newest");
        let store = Store::new(&dir.0);

        store.save(&snapshot_at(1_700_000_000), 5).unwrap();
        store.save(&snapshot_at(1_700_000_100), 5).unwrap();

        let loaded = store.load_latest().unwrap();
        assert_eq!(loaded.captured_at.unix_timestamp(), 1_700_000_100);
    }

    #[test]
    fn two_saves_in_the_same_second_get_distinct_generations() {
        let dir = TestDir::new("same-second-collision");
        let store = Store::new(&dir.0);

        store.save(&snapshot_at(1_700_000_000), 5).unwrap();
        store.save(&snapshot_at(1_700_000_000), 5).unwrap();

        let generations = store.list().unwrap();
        assert_eq!(generations.len(), 2);
    }

    #[test]
    fn prune_keeps_only_the_newest_n_generations() {
        let dir = TestDir::new("prune");
        let store = Store::new(&dir.0);

        for ts in [1_700_000_000, 1_700_000_100, 1_700_000_200, 1_700_000_300] {
            store.save(&snapshot_at(ts), 2).unwrap();
        }

        let generations = store.list().unwrap();
        assert_eq!(generations.len(), 2);
        assert_eq!(generations[0].captured_at_unix, 1_700_000_300);
        assert_eq!(generations[1].captured_at_unix, 1_700_000_200);

        // latest still resolves after its older siblings were pruned.
        let loaded = store.load_latest().unwrap();
        assert_eq!(loaded.captured_at.unix_timestamp(), 1_700_000_300);
    }

    #[test]
    fn a_leftover_temp_file_from_an_interrupted_save_does_not_affect_latest() {
        let dir = TestDir::new("interrupted-save");
        let store = Store::new(&dir.0);
        store.save(&snapshot_at(1_700_000_000), 5).unwrap();

        // Simulate a save that wrote its temp file but crashed before the
        // rename that would have made it a real generation.
        fs::write(dir.0.join(".tmp-save-99999-1700000999"), b"garbage").unwrap();

        let loaded = store.load_latest().unwrap();
        assert_eq!(loaded.captured_at.unix_timestamp(), 1_700_000_000);
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn load_latest_surfaces_corruption_instead_of_returning_bad_data() {
        let dir = TestDir::new("corrupt-latest");
        let store = Store::new(&dir.0);
        let outcome = store.save(&snapshot_at(1_700_000_000), 5).unwrap();

        let mut bytes = fs::read(&outcome.path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&outcome.path, bytes).unwrap();

        assert!(matches!(
            store.load_latest(),
            Err(StoreError::ChecksumMismatch)
        ));
    }
}
