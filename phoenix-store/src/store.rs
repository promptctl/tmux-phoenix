//! Atomic, versioned, generational persistence (ARCHITECTURE.md §7): a
//! temp file, an fsync and a rename, so a crash mid-save leaves every
//! complete generation intact.
//!
//! **`latest` is derived, not stored.** The generation with the highest id
//! *is* latest; there is no pointer to keep in step with it
//! (`[LAW:one-source-of-truth]`).
//!
//! **One lock, two modes.** Every save runs under an exclusive `flock` on
//! `.lock` in the store dir, from the temp write through prune, so saves
//! from separate processes (a manual `phoenix save`, the daemon) run one at
//! a time. Every reader takes the same lock shared, so a listing is a
//! consistent view and [`StoreError::NoLatest`] can only mean "no
//! generation exists" — never "a prune ran between two reads".

use std::fs;
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use phoenix_core::{GenerationId, OffsetDateTime, Origin, Snapshot};

use crate::blob_store::BlobStore;
use crate::error::StoreError;
use crate::header::{decode_file, decode_header, encode_file, Header};
use crate::tag::Tag;
use crate::version::FormatVersion;

const GENERATION_PREFIX: &str = "snapshot-";
const GENERATION_SUFFIX: &str = ".phnx";
/// The pointer earlier formats kept beside the generations; removed on the
/// first save, since nothing reads it any more.
const RETIRED_LATEST_NAME: &str = "latest";
const BLOBS_DIR: &str = "blobs";
const LOCK_NAME: &str = ".lock";
/// How often a waiting save retries the lock. Granularity of the caller's
/// `wait` only — correctness rests on the lock, never on this interval.
const LOCK_RETRY: Duration = Duration::from_millis(10);

/// Deletions [`Store::prune`] made, and ones it tried and failed to make.
type PruneResult = (Vec<PathBuf>, Vec<(PathBuf, io::Error)>);

/// How many generations a save leaves behind. Tagged generations are
/// exempt: a name is the user saying "keep this".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub keep_untagged: NonZeroUsize,
}

pub struct Store {
    dir: PathBuf,
}

#[derive(Debug)]
pub struct SaveOutcome {
    pub generation: GenerationId,
    pub path: PathBuf,
    /// Older generations successfully deleted this save (beyond the
    /// retention count).
    pub pruned: Vec<PathBuf>,
    /// Older generations this save *tried* to delete but couldn't — the
    /// save itself already succeeded by the time pruning runs, so these
    /// are reported rather than failing the whole operation (crash-safety
    /// is about the snapshot itself, not disk cleanup).
    pub prune_errors: Vec<(PathBuf, io::Error)>,
}

/// One generation as its header describes it — no body decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationInfo {
    pub id: GenerationId,
    pub path: PathBuf,
    pub format_version: FormatVersion,
    pub origin: Origin,
    pub captured_at: OffsetDateTime,
    pub tag: Option<Tag>,
}

#[derive(Debug, Clone, Copy)]
enum LockMode {
    Shared,
    Exclusive,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `${XDG_DATA_HOME}/tmux-phoenix`, falling back to
    /// `~/.local/share/tmux-phoenix`.
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

    fn generation_path(&self, generation: GenerationId) -> PathBuf {
        self.dir.join(format!(
            "{GENERATION_PREFIX}{}{GENERATION_SUFFIX}",
            generation.0
        ))
    }

    /// Content blobs live in their own subdirectory, shared across every
    /// generation in this store — that sharing is the whole point of
    /// content-addressing (an unchanged pane's scrollback blob is written
    /// once and referenced by every generation that has it).
    fn blob_store(&self) -> BlobStore {
        BlobStore::new(self.dir.join(BLOBS_DIR))
    }

    /// Save `snapshot` as the next generation, then prune down to
    /// `retention` — always keeping this one, since ids rise in save order,
    /// and every tagged one.
    ///
    /// `wait` is how long to wait for another save into this store to
    /// finish before failing with [`StoreError::Contended`];
    /// `Duration::ZERO` tries once.
    pub fn save(
        &self,
        snapshot: &Snapshot,
        tag: Option<&Tag>,
        retention: Retention,
        wait: Duration,
    ) -> Result<SaveOutcome, StoreError> {
        fs::create_dir_all(&self.dir)?;
        // [LAW:single-enforcer] the one place saves are serialized; held until
        // this function returns, so id choice, publish and prune are atomic
        // with respect to every other save and every reader.
        let _lock = self.lock(LockMode::Exclusive, wait)?;

        let generation = self.next_generation_id(snapshot.captured_at.unix_timestamp())?;
        let final_path = self.generation_path(generation);
        let tmp_path = self
            .dir
            .join(format!(".tmp-save-{}-{}", std::process::id(), generation.0));

        let bytes = encode_file(snapshot, tag, &self.blob_store())?;
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
        self.retire_latest_pointer()?;

        let (pruned, prune_errors) = self.prune(retention)?;

        Ok(SaveOutcome {
            generation,
            path: final_path,
            pruned,
            prune_errors,
        })
    }

    /// A store written by the previous format carries a `latest` symlink;
    /// it would silently go stale, so the first save removes it.
    fn retire_latest_pointer(&self) -> io::Result<()> {
        match fs::remove_file(self.dir.join(RETIRED_LATEST_NAME)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Takes this store's lock in `mode`, released when the returned file
    /// closes. The lock file is never deleted: unlinking it while another
    /// holder has or awaits the old inode would let two holders each lock a
    /// different file.
    fn lock(&self, mode: LockMode, wait: Duration) -> Result<fs::File, StoreError> {
        let path = self.dir.join(LOCK_NAME);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        let deadline = Instant::now() + wait;
        loop {
            let attempt = match mode {
                LockMode::Shared => file.try_lock_shared(),
                LockMode::Exclusive => file.try_lock(),
            };
            match attempt {
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

    /// Readers wait for a save in progress rather than racing it. A save
    /// holds the lock only for its own writes and prune, so this bounds the
    /// wait by the slowest disk, not by anything a reader could choose.
    const READ_WAIT: Duration = Duration::from_secs(30);

    fn read_lock(&self) -> Result<fs::File, StoreError> {
        fs::create_dir_all(&self.dir)?;
        self.lock(LockMode::Shared, Self::READ_WAIT)
    }

    /// `captured_at`'s Unix timestamp, raised above the newest existing
    /// generation when it isn't already. Ids must rise in save order:
    /// the highest id is latest and pruning keeps the highest ids, so a
    /// lower id — two captures in one second, an older capture saved after
    /// a newer one, a clock stepped backwards — would be pruned out from
    /// under the save that made it.
    fn next_generation_id(&self, captured_at: i64) -> io::Result<GenerationId> {
        match self.generation_ids_desc()?.first() {
            None => Ok(GenerationId(captured_at)),
            Some(newest) => newest
                .0
                .checked_add(1)
                .map(|above_newest| GenerationId(captured_at.max(above_newest)))
                .ok_or_else(|| {
                    io::Error::other(format!("generation id {newest} leaves no id above it"))
                }),
        }
    }

    /// The newest generation. Under the shared lock from listing through
    /// reading, so a prune cannot take the file out from under it.
    pub fn load_latest(&self) -> Result<Snapshot, StoreError> {
        let _lock = self.read_lock()?;
        let newest = self
            .generation_ids_desc()?
            .first()
            .copied()
            .ok_or(StoreError::NoLatest)?;
        self.read_generation(newest)
    }

    pub fn load(&self, generation: GenerationId) -> Result<Snapshot, StoreError> {
        let _lock = self.read_lock()?;
        self.read_generation(generation)
    }

    fn read_generation(&self, generation: GenerationId) -> Result<Snapshot, StoreError> {
        let bytes = fs::read(self.generation_path(generation)).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                StoreError::NoSuchGeneration { id: generation }
            } else {
                StoreError::Io(e)
            }
        })?;
        decode_file(&bytes, &self.blob_store())
    }

    /// Loads a generation file directly by path — for `restore --file
    /// <path>`. `path` need not be in this store (e.g. an already-pruned
    /// generation kept elsewhere), so no lock covers it; its content blobs
    /// are still resolved against *this* store's blob directory, since a
    /// bare generation file has no blobs of its own.
    pub fn load_file(&self, path: &Path) -> Result<Snapshot, StoreError> {
        let bytes = fs::read(path)?;
        decode_file(&bytes, &self.blob_store())
    }

    /// Every generation's id, newest first.
    fn generation_ids_desc(&self) -> io::Result<Vec<GenerationId>> {
        let mut ids: Vec<GenerationId> = match fs::read_dir(&self.dir) {
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

    fn read_header(&self, generation: GenerationId) -> Result<(PathBuf, Header), StoreError> {
        let path = self.generation_path(generation);
        let bytes = fs::read(&path)?;
        let header = decode_header(&bytes)?;
        Ok((path, header))
    }

    fn headers_desc(&self) -> Result<Vec<GenerationInfo>, StoreError> {
        self.generation_ids_desc()?
            .into_iter()
            .map(|id| {
                let (path, header) = self.read_header(id)?;
                Ok(GenerationInfo {
                    id,
                    path,
                    format_version: header.format_version,
                    origin: header.origin,
                    captured_at: header.captured_at,
                    tag: header.tag,
                })
            })
            .collect()
    }

    /// Header-only summary of every generation, newest first — doesn't
    /// decode any body. The first entry is latest.
    pub fn list(&self) -> Result<Vec<GenerationInfo>, StoreError> {
        let _lock = self.read_lock()?;
        self.headers_desc()
    }

    /// Deletes every untagged generation beyond the newest
    /// `keep_untagged`. Runs under the exclusive lock the save holds.
    fn prune(&self, retention: Retention) -> Result<PruneResult, StoreError> {
        let mut pruned = Vec::new();
        let mut errors = Vec::new();
        let untagged = self.headers_desc()?.into_iter().filter(|g| g.tag.is_none());
        for generation in untagged.skip(retention.keep_untagged.get()) {
            match fs::remove_file(&generation.path) {
                Ok(()) => pruned.push(generation.path),
                Err(e) => errors.push((generation.path, e)),
            }
        }
        Ok((pruned, errors))
    }
}

fn generation_id_from_file_name(name: &str) -> Option<GenerationId> {
    name.strip_prefix(GENERATION_PREFIX)?
        .strip_suffix(GENERATION_SUFFIX)
        .and_then(GenerationId::parse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{sample_snapshot, TestDir};

    fn keep(n: usize) -> Retention {
        Retention {
            keep_untagged: NonZeroUsize::new(n).unwrap(),
        }
    }

    fn save(store: &Store, captured_at: i64, retention: Retention) -> SaveOutcome {
        store
            .save(
                &sample_snapshot(captured_at),
                None,
                retention,
                Duration::ZERO,
            )
            .unwrap()
    }

    #[test]
    fn save_then_load_latest_round_trips() {
        let dir = TestDir::new("round-trip");
        let store = Store::new(&dir.0);
        let snapshot = sample_snapshot(1_700_000_000);
        let outcome = store
            .save(&snapshot, None, keep(5), Duration::ZERO)
            .unwrap();
        assert_eq!(outcome.generation, GenerationId(1_700_000_000));
        assert_eq!(store.load_latest().unwrap(), snapshot);
        assert_eq!(store.load(outcome.generation).unwrap(), snapshot);
        assert_eq!(store.load_file(&outcome.path).unwrap(), snapshot);
    }

    #[test]
    fn load_before_any_save_is_no_latest_and_an_unknown_id_is_named() {
        let dir = TestDir::new("no-latest");
        let store = Store::new(&dir.0);
        assert!(matches!(store.load_latest(), Err(StoreError::NoLatest)));
        assert!(matches!(
            store.load(GenerationId(7)),
            Err(StoreError::NoSuchGeneration {
                id: GenerationId(7)
            })
        ));
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn latest_is_the_highest_id_and_list_leads_with_it() {
        let dir = TestDir::new("latest-is-highest");
        let store = Store::new(&dir.0);
        save(&store, 1_700_000_000, keep(5));
        save(&store, 1_700_000_100, keep(5));
        assert_eq!(
            store.load_latest().unwrap().captured_at.unix_timestamp(),
            1_700_000_100
        );
        let listed = store.list().unwrap();
        assert_eq!(listed[0].id, GenerationId(1_700_000_100));
        assert_eq!(listed[0].captured_at.unix_timestamp(), 1_700_000_100);
        assert_eq!(listed[0].format_version, FormatVersion::CURRENT);
        assert!(matches!(listed[0].origin, Origin::Recorded(_)));
        assert_eq!(listed[0].tag, None);
    }

    #[test]
    fn two_saves_in_the_same_second_get_distinct_generations() {
        let dir = TestDir::new("same-second-collision");
        let store = Store::new(&dir.0);
        save(&store, 1_700_000_000, keep(5));
        save(&store, 1_700_000_000, keep(5));
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn prune_keeps_only_the_newest_n_untagged_generations() {
        let dir = TestDir::new("prune");
        let store = Store::new(&dir.0);
        for ts in [1_700_000_000, 1_700_000_100, 1_700_000_200, 1_700_000_300] {
            save(&store, ts, keep(2));
        }
        let generations = store.list().unwrap();
        assert_eq!(generations.len(), 2);
        assert_eq!(generations[0].captured_at.unix_timestamp(), 1_700_000_300);
        assert_eq!(generations[1].captured_at.unix_timestamp(), 1_700_000_200);
        assert_eq!(
            store.load_latest().unwrap().captured_at.unix_timestamp(),
            1_700_000_300
        );
    }

    #[test]
    fn a_tagged_generation_survives_a_retention_of_one() {
        let dir = TestDir::new("tagged-survives");
        let store = Store::new(&dir.0);
        let tag = Tag::parse("before-upgrade").unwrap();
        store
            .save(
                &sample_snapshot(1_700_000_000),
                Some(&tag),
                keep(1),
                Duration::ZERO,
            )
            .unwrap();
        save(&store, 1_700_000_100, keep(1));
        save(&store, 1_700_000_200, keep(1));

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, GenerationId(1_700_000_200));
        assert_eq!(listed[1].id, GenerationId(1_700_000_000));
        assert_eq!(listed[1].tag, Some(tag));
    }

    #[test]
    fn a_leftover_temp_file_and_a_retired_latest_pointer_do_not_affect_latest() {
        let dir = TestDir::new("interrupted-save");
        let store = Store::new(&dir.0);
        save(&store, 1_700_000_000, keep(5));
        fs::write(dir.0.join(".tmp-save-99999-1700000999"), b"garbage").unwrap();
        std::os::unix::fs::symlink("snapshot-1700000000.phnx", dir.0.join("latest")).unwrap();

        assert_eq!(
            store.load_latest().unwrap().captured_at.unix_timestamp(),
            1_700_000_000
        );
        assert_eq!(store.list().unwrap().len(), 1);

        save(&store, 1_700_000_100, keep(5));
        assert!(
            !dir.0.join("latest").exists(),
            "the next save retires the pointer"
        );
    }

    #[test]
    fn load_latest_surfaces_corruption_instead_of_returning_bad_data() {
        let dir = TestDir::new("corrupt-latest");
        let store = Store::new(&dir.0);
        let outcome = save(&store, 1_700_000_000, keep(5));
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
    fn race_saves(dir: &Path, retention: Retention) -> Vec<PathBuf> {
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
                                        &sample_snapshot(1_700_000_000),
                                        None,
                                        retention,
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
        let paths = race_saves(
            &dir.0,
            Retention {
                keep_untagged: NonZeroUsize::MAX,
            },
        );
        let distinct: std::collections::HashSet<_> = paths.iter().collect();
        assert_eq!(distinct.len(), paths.len(), "two saves reported one path");
        assert!(paths.iter().all(|p| p.exists()));
        let store = Store::new(&dir.0);
        assert_eq!(store.list().unwrap().len(), paths.len());
        store.load_latest().unwrap();
    }

    #[test]
    fn readers_racing_pruning_saves_never_see_no_latest_while_a_generation_exists() {
        let dir = TestDir::new("readers-vs-prune");
        let store = Store::new(&dir.0);
        save(&store, 1_700_000_000, keep(2));
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let readers: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        let store = Store::new(&dir.0);
                        let mut reads = 0;
                        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                            store.load_latest().expect("a generation always exists");
                            assert!(!store.list().unwrap().is_empty());
                            reads += 1;
                        }
                        reads
                    })
                })
                .collect();
            race_saves(&dir.0, keep(2));
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            for reader in readers {
                assert!(reader.join().unwrap() > 0);
            }
        });
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn a_save_that_cannot_take_the_lock_in_time_fails_and_writes_nothing() {
        let dir = TestDir::new("contended");
        let store = Store::new(&dir.0);
        save(&store, 1_700_000_000, keep(5));

        let holder = fs::File::open(dir.0.join(LOCK_NAME)).unwrap();
        holder.lock().unwrap();
        for wait in [Duration::ZERO, Duration::from_millis(50)] {
            let err = store
                .save(&sample_snapshot(1_700_000_100), None, keep(5), wait)
                .unwrap_err();
            assert!(matches!(err, StoreError::Contended { .. }), "{err}");
        }
        drop(holder);
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn a_save_waits_out_a_lock_released_within_its_wait() {
        let dir = TestDir::new("waits");
        let store = Store::new(&dir.0);
        save(&store, 1_700_000_000, keep(5));

        let holder = fs::File::open(dir.0.join(LOCK_NAME)).unwrap();
        holder.lock().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(holder);
        });
        store
            .save(
                &sample_snapshot(1_700_000_100),
                None,
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
            let outcome = save(&store, ts, keep(1));
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
        save(&store, 1_700_000_000, keep(5));
        fs::write(store.generation_path(GenerationId(i64::MAX)), b"stray").unwrap();

        assert!(matches!(
            store.save(
                &sample_snapshot(1_700_000_100),
                None,
                keep(5),
                Duration::ZERO
            ),
            Err(StoreError::Io(_))
        ));
    }

    #[test]
    fn an_older_capture_saved_after_a_newer_one_is_latest_and_survives_pruning() {
        let dir = TestDir::new("older-capture-last");
        let store = Store::new(&dir.0);
        save(&store, 1_700_000_100, keep(1));
        let outcome = save(&store, 1_700_000_000, keep(1));
        assert!(outcome.path.exists());
        assert_eq!(
            store.load_latest().unwrap().captured_at.unix_timestamp(),
            1_700_000_000
        );
    }
}
