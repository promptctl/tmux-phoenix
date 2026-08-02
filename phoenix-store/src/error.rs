use std::fmt;
use std::io;

use phoenix_core::SnapshotError;

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    /// The file didn't start with the expected magic bytes — not a
    /// phoenix-store file at all.
    BadMagic,
    /// DESIGN.md §7: "an unknown `format_version` is refused loudly, never
    /// guessed."
    UnsupportedFormatVersion {
        found: u32,
    },
    ChecksumMismatch,
    /// The body ended before the decoder expected — a truncated or
    /// otherwise corrupt file.
    Truncated,
    InvalidUtf8,
    InvalidOptionTag {
        value: u8,
    },
    /// A `NonEmpty` collection decoded to zero elements — a valid snapshot
    /// never produces one, so this means the file is corrupt.
    EmptyCollection,
    /// A `SessionName`/`WindowName`/`Layout` field decoded to the empty
    /// string — those newtypes never accept one.
    InvalidName,
    /// The body decoded structurally but violated a domain invariant (e.g.
    /// an `active` index resolving to nothing) — corruption, not a bug in
    /// this crate, since [`phoenix_core`] can't construct such a tree itself.
    Snapshot(SnapshotError),
    /// No snapshot exists yet at this store's `latest` pointer.
    NoLatest,
    /// A generation file referenced a content blob (tmux-content-dos.2) that
    /// isn't in the blob store — a torn save (blob write succeeded but the
    /// generation file referencing it didn't, or vice versa) or the blob
    /// store was pruned/damaged independently of the generation files.
    BlobNotFound,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "I/O error: {e}"),
            StoreError::BadMagic => write!(f, "not a phoenix-store snapshot file"),
            StoreError::UnsupportedFormatVersion { found } => write!(
                f,
                "snapshot format_version {found} is not supported by this build"
            ),
            StoreError::ChecksumMismatch => write!(f, "snapshot body failed its checksum"),
            StoreError::Truncated => write!(f, "snapshot file is truncated"),
            StoreError::InvalidUtf8 => write!(f, "snapshot body contains invalid UTF-8"),
            StoreError::InvalidOptionTag { value } => {
                write!(f, "expected an option tag byte (0 or 1), found {value}")
            }
            StoreError::EmptyCollection => {
                write!(
                    f,
                    "snapshot body had an empty collection where tmux guarantees >=1"
                )
            }
            StoreError::InvalidName => write!(f, "snapshot body had an empty name/layout field"),
            StoreError::Snapshot(e) => write!(f, "{e}"),
            StoreError::NoLatest => write!(f, "no snapshot has been saved yet"),
            StoreError::BlobNotFound => write!(f, "referenced content blob is missing"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<SnapshotError> for StoreError {
    fn from(e: SnapshotError) -> Self {
        StoreError::Snapshot(e)
    }
}
