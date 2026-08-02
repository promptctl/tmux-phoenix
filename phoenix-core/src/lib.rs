//! `phoenix-core` — the pure tmux-phoenix domain model (DESIGN.md §4). No
//! I/O, no dependency on `tmux-control` or any other phoenix crate
//! (`[LAW:one-way-deps]`): this is a foundation crate, same tier as
//! `tmux-control`.

mod content;
mod ids;
mod nonempty;
mod path;
mod program;
mod snapshot;
mod time;
mod version;

pub use content::PaneContent;
pub use ids::{Layout, PaneIndex, SessionName, WindowIndex, WindowName};
pub use nonempty::NonEmpty;
pub use path::Utf8PathBuf;
pub use program::CapturedProgram;
pub use snapshot::{Pane, Session, Snapshot, SnapshotError, Window};
pub use time::OffsetDateTime;
pub use version::{FormatVersion, TmuxVersion};
