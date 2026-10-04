//! `phoenix-capture` — reads a live tmux server into a `phoenix-core`
//! `Snapshot` (ARCHITECTURE.md §6), over any `tmux_control::Execute`.

mod capture;
mod content;
mod fold;
mod process;
mod row;

pub use capture::{capture, CaptureError};
pub use content::{Previous, PreviousContent};
pub use fold::{FoldError, PaneReads};
pub use row::{
    client_format, pane_format, parse_client_row, parse_pane_row, PaneRow, RowParseError,
};
