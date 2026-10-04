//! The on-disk generation format version (ARCHITECTURE.md §7): an unknown
//! one is refused loudly, never guessed. It is the store's concept — a
//! `Snapshot` carries no format, the file that holds it does.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FormatVersion(pub u32);

impl FormatVersion {
    /// The strict tree with three bare `Option`s per pane and no origin;
    /// still readable, through its own decoder, as a snapshot whose origin
    /// was never recorded.
    pub const BEFORE_ORIGIN: FormatVersion = FormatVersion(1);
    /// The windows-and-winlinks graph with reasoned absences, an origin and
    /// a tag in a self-sized header.
    pub const CURRENT: FormatVersion = FormatVersion(2);
}
