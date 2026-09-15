//! Atomic, versioned, generational persistence (DESIGN.md §7): "temp +
//! fsync + rename; the `latest` pointer only ever names a complete
//! snapshot, so a crash mid-save leaves the last good one intact."
//!
//! `latest` is a symlink (a relative one, to the generation's own file
//! name, so the whole save directory stays portable if moved) — repointing
//! it is itself a `rename(2)` over the symlink's temp name, so the pointer
//! flips atomically the same way the generation file's own write does.
//!
//! Every save runs under an exclusive `flock` on `.lock` in the store dir,
//! so saves from separate processes (a manual `phoenix save`, the daemon)
//! run one at a time, from the temp write through prune.

use std::fs;
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use phoenix_core::Snapshot;

use crate::blob_store::BlobStore;
use crate::error::StoreError;
use crate::header::{decode_file, decode_header, encode_file};

const GENERATION_PREFIX: &str = "snapshot-";
const GENERATION_SUFFIX: &str = ".phnx";
const LATEST_NAME: &str = "latest";
const BLOBS_DIR: &str = "blobs";
const LOCK_NAME: &str = ".lock";
/// How often a waiting save retries the lock. Granularity of the caller's
/// `wait` only — correctness rests on the lock, never on this interval.
const LOCK_RETRY: Duration = Duration::from_millis(10);

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

    /// Save `snapshot`, then prune down to the newest `keep_generations`
    /// generations — always including this one, since ids rise in save order.
    ///
    /// `wait` is how long to wait for another save into this store to
    /// finish before failing with [`StoreError::Contended`];
    /// `Duration::ZERO` tries once.
    pub fn save(
        &self,
        snapshot: &Snapshot,
        keep_generations: NonZeroUsize,
        wait: Duration,
    ) -> Result<SaveOutcome, StoreError> {
        // [LAW:single-enforcer] the one writer of `latest` is the one place a
        // capture holding nothing the user built is kept from replacing it.
        if snapshot.is_bootstrap_only() {
            return Err(StoreError::BootstrapOnly);
        }
        fs::create_dir_all(&self.dir)?;
        // [LAW:single-enforcer] the one place saves are serialized; held until
        // this function returns, so id choice, publish and prune are atomic
        // with respect to every other save.
        let _lock = self.lock(wait)?;

        let generation = self.next_generation_id(snapshot.captured_at.unix_timestamp())?;
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

    /// Takes this store's exclusive save lock, released when the returned
    /// file closes. The lock file is never deleted: unlinking it while
    /// another save holds or awaits the old inode would let two saves each
    /// lock a different file.
    fn lock(&self, wait: Duration) -> Result<fs::File, StoreError> {
        let path = self.dir.join(LOCK_NAME);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(fs::TryLockError::Error(e)) => return Err(e.into()),
                Err(fs::TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(StoreError::Contended {
                            lock: path,
                            waited: wait,
                        });
                    }
                    std::thread::sleep(remaining.min(LOCK_RETRY));
                }
            }
        }
    }

    /// `captured_at`'s Unix timestamp, raised above the newest existing
    /// generation when it isn't already. Ids must rise in save order:
    /// `latest` names this save and pruning keeps the highest ids, so a lower
    /// id — two captures in one second, an older capture saved after a newer
    /// one, a clock stepped backwards — would be pruned out from under
    /// `latest`.
    fn next_generation_id(&self, captured_at: i64) -> io::Result<i64> {
        match self.generation_ids_desc()?.first() {
            None => Ok(captured_at),
            Some(newest) => newest
                .checked_add(1)
                .map(|above_newest| captured_at.max(above_newest))
                .ok_or_else(|| {
                    io::Error::other(format!("generation id {newest} leaves no id above it"))
                }),
        }
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

    fn prune(&self, keep: NonZeroUsize) -> Result<PruneResult, StoreError> {
        let ids = self.generation_ids_desc()?;
        let mut pruned = Vec::new();
        let mut errors = Vec::new();
        for generation in ids.into_iter().skip(keep.get()) {
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
        CapturedProgram, FormatVersion, Layout, NonEmpty, OffsetDateTime, Pane, PaneId, PaneIndex,
        ProgramName, Session, SessionName, TmuxVersion, Utf8PathBuf, Window, WindowIndex,
        WindowName,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn keep(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

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
        snapshot_with_argv(captured_at, None)
    }

    fn snapshot_with_argv(captured_at: i64, argv: Option<NonEmpty<String>>) -> Snapshot {
        let pane = Pane {
            id: PaneId(0),
            index: PaneIndex(0),
            cwd: Utf8PathBuf::parse("/home/user"),
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
            captured_at: OffsetDateTime::from_unix_timestamp(captured_at),
            sessions: NonEmpty::singleton(session),
        }
    }

    #[test]
    fn a_capture_of_only_bootstrap_sessions_is_refused_and_writes_nothing() {
        let dir = TestDir::new("bootstrap-only");
        let store = Store::new(&dir.0);
        store
            .save(&snapshot_at(1_700_000_000), keep(5), Duration::ZERO)
            .unwrap();

        let login = snapshot_with_argv(1_700_000_100, Some(NonEmpty::singleton("-zsh".into())));
        assert!(matches!(
            store.save(&login, keep(5), Duration::ZERO),
            Err(StoreError::BootstrapOnly)
        ));
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(
            store.load_latest().unwrap().captured_at.unix_timestamp(),
            1_700_000_000
        );
    }

    #[test]
    fn save_then_load_latest_round_trips() {
        let dir = TestDir::new("round-trip");
        let store = Store::new(&dir.0);
        let snapshot = snapshot_at(1_700_000_000);

        store.save(&snapshot, keep(5), Duration::ZERO).unwrap();
        let loaded = store.load_latest().unwrap();
        assert_eq!(loaded, snapshot);
    }

    #[test]
    fn load_file_reads_a_specific_generation_by_path_directly() {
        let dir = TestDir::new("load-file");
        let store = Store::new(&dir.0);
        let snapshot = snapshot_at(1_700_000_000);

        let outcome = store.save(&snapshot, keep(5), Duration::ZERO).unwrap();
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

        store
            .save(&snapshot_at(1_700_000_000), keep(5), Duration::ZERO)
            .unwrap();
        store
            .save(&snapshot_at(1_700_000_100), keep(5), Duration::ZERO)
            .unwrap();

        let loaded = store.load_latest().unwrap();
        assert_eq!(loaded.captured_at.unix_timestamp(), 1_700_000_100);
    }

    #[test]
    fn two_saves_in_the_same_second_get_distinct_generations() {
        let dir = TestDir::new("same-second-collision");
        let store = Store::new(&dir.0);

        store
            .save(&snapshot_at(1_700_000_000), keep(5), Duration::ZERO)
            .unwrap();
        store
            .save(&snapshot_at(1_700_000_000), keep(5), Duration::ZERO)
            .unwrap();

        let generations = store.list().unwrap();
        assert_eq!(generations.len(), 2);
    }

    #[test]
    fn prune_keeps_only_the_newest_n_generations() {
        let dir = TestDir::new("prune");
        let store = Store::new(&dir.0);

        for ts in [1_700_000_000, 1_700_000_100, 1_700_000_200, 1_700_000_300] {
            store
                .save(&snapshot_at(ts), keep(2), Duration::ZERO)
                .unwrap();
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
        store
            .save(&snapshot_at(1_700_000_000), keep(5), Duration::ZERO)
            .unwrap();

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
        let outcome = store
            .save(&snapshot_at(1_700_000_000), keep(5), Duration::ZERO)
            .unwrap();

        let mut bytes = fs::read(&outcome.path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&outcome.path, bytes).unwrap();

        assert!(matches!(
            store.load_latest(),
            Err(StoreError::ChecksumMismatch)
        ));
    }

    /// `WRITERS` threads each saving `ROUNDS` same-second snapshots into one
    /// store dir. Each writer builds its own `Store`, so each opens its own
    /// lock file description — `flock` excludes those from one another
    /// exactly as it excludes separate processes.
    fn race_saves(dir: &Path, keep: NonZeroUsize) -> Vec<PathBuf> {
        const WRITERS: usize = 8;
        const ROUNDS: usize = 10;
        let start = std::sync::Barrier::new(WRITERS);
        std::thread::scope(|scope| {
            let writers: Vec<_> = (0..WRITERS)
                .map(|_| {
                    scope.spawn(|| {
                        let store = Store::new(dir);
                        start.wait();
                        (0..ROUNDS)
                            .map(|_| {
                                store
                                    .save(
                                        &snapshot_at(1_700_000_000),
                                        keep,
                                        Duration::from_secs(30),
                                    )
                                    .unwrap()
                                    .path
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            writers
                .into_iter()
                .flat_map(|w| w.join().unwrap())
                .collect()
        })
    }

    #[test]
    fn concurrent_saves_each_land_their_own_generation() {
        let dir = TestDir::new("concurrent-keep-all");
        let paths = race_saves(&dir.0, NonZeroUsize::MAX);

        let distinct: std::collections::HashSet<_> = paths.iter().collect();
        assert_eq!(distinct.len(), paths.len(), "two saves reported one path");
        assert!(paths.iter().all(|p| p.exists()));
        let store = Store::new(&dir.0);
        assert_eq!(store.list().unwrap().len(), paths.len());
        store.load_latest().unwrap();
    }

    #[test]
    fn concurrent_saves_with_pruning_never_leave_latest_dangling() {
        let dir = TestDir::new("concurrent-prune");
        race_saves(&dir.0, keep(2));

        let store = Store::new(&dir.0);
        assert_eq!(store.list().unwrap().len(), 2);
        store.load_latest().unwrap();
    }

    #[test]
    fn a_save_that_cannot_take_the_lock_in_time_fails_and_writes_nothing() {
        let dir = TestDir::new("contended");
        let store = Store::new(&dir.0);
        store
            .save(&snapshot_at(1_700_000_000), keep(5), Duration::ZERO)
            .unwrap();

        let holder = fs::File::open(dir.0.join(LOCK_NAME)).unwrap();
        holder.lock().unwrap();
        for wait in [Duration::ZERO, Duration::from_millis(50)] {
            let err = store
                .save(&snapshot_at(1_700_000_100), keep(5), wait)
                .unwrap_err();
            assert!(matches!(err, StoreError::Contended { .. }), "{err}");
        }

        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(
            store.load_latest().unwrap().captured_at.unix_timestamp(),
            1_700_000_000
        );
    }

    #[test]
    fn a_save_waits_out_a_lock_released_within_its_wait() {
        let dir = TestDir::new("waits");
        let store = Store::new(&dir.0);
        store
            .save(&snapshot_at(1_700_000_000), keep(5), Duration::ZERO)
            .unwrap();

        let holder = fs::File::open(dir.0.join(LOCK_NAME)).unwrap();
        holder.lock().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(holder);
        });

        store
            .save(
                &snapshot_at(1_700_000_100),
                keep(5),
                Duration::from_secs(30),
            )
            .unwrap();
        release.join().unwrap();
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn a_retention_of_one_keeps_exactly_the_generation_just_written() {
        let dir = TestDir::new("keep-one");
        let store = Store::new(&dir.0);
        for ts in [1_700_000_000, 1_700_000_100] {
            let outcome = store
                .save(&snapshot_at(ts), keep(1), Duration::ZERO)
                .unwrap();
            assert!(outcome.path.exists());
            assert_eq!(store.list().unwrap().len(), 1);
            assert_eq!(
                store.load_latest().unwrap().captured_at.unix_timestamp(),
                ts
            );
        }
    }

    #[test]
    fn a_save_with_no_id_above_the_newest_generation_fails_and_leaves_latest_alone() {
        let dir = TestDir::new("id-overflow");
        let store = Store::new(&dir.0);
        store
            .save(&snapshot_at(1_700_000_000), keep(5), Duration::ZERO)
            .unwrap();
        fs::write(store.generation_path(i64::MAX), b"stray").unwrap();

        assert!(matches!(
            store.save(&snapshot_at(1_700_000_100), keep(5), Duration::ZERO),
            Err(StoreError::Io(_))
        ));
        assert_eq!(
            store.load_latest().unwrap().captured_at.unix_timestamp(),
            1_700_000_000
        );
    }

    #[test]
    fn an_older_capture_saved_after_a_newer_one_is_latest_and_survives_pruning() {
        let dir = TestDir::new("older-capture-last");
        let store = Store::new(&dir.0);

        store
            .save(&snapshot_at(1_700_000_100), keep(1), Duration::ZERO)
            .unwrap();
        let outcome = store
            .save(&snapshot_at(1_700_000_000), keep(1), Duration::ZERO)
            .unwrap();

        assert!(outcome.path.exists());
        assert_eq!(
            store.load_latest().unwrap().captured_at.unix_timestamp(),
            1_700_000_000
        );
    }
}
