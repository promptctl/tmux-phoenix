//! `phoenix-core` — the pure tmux-phoenix domain model (ARCHITECTURE.md
//! §5). No I/O, no dependency on `tmux-control` or any other phoenix crate
//! (`[LAW:one-way-deps]`): this is a foundation crate, same tier as
//! `tmux-control`.

mod content;
mod ids;
mod nonempty;
mod path;
mod program;
mod provenance;
mod snapshot;
mod time;
mod version;

pub use content::{Content, ContentFailure, HistoryIndicator};
pub use ids::{
    ClientName, GenerationId, GroupName, Layout, PaneId, PaneIndex, ServerId, SessionName,
    WindowId, WindowIndex, WindowName,
};
pub use nonempty::NonEmpty;
pub use path::{Cwd, Utf8PathBuf};
pub use program::{Foreground, RecoveryFailure, Shells, TerminalHolder};
pub use provenance::{Made, MalformedWindowMark, Origin, Touched};
pub use snapshot::{Client, Degradation, Pane, Session, Snapshot, SnapshotError, WinLink, Window};
pub use time::OffsetDateTime;
pub use version::TmuxVersion;
