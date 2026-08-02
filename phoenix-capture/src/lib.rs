//! `phoenix-capture` — drives `tmux-control` to interrogate a live server
//! into a `phoenix-core` `Snapshot` (DESIGN.md §5).

mod argv;
mod capture;
mod content;
mod fold;
mod row;

pub use capture::{capture, CaptureError, ContentCapture};
pub use content::PreviousPaneContent;
pub use fold::FoldError;
pub use row::{format_string, parse_row, PaneRow, RowParseError};
