//! Captured pane text (ARCHITECTURE.md §5). Content is captured on every
//! capture — it is not a mode — so a pane without it holds the reason
//! (`[LAW:types-are-the-program]`: absence with a cause, never a bare
//! `None`).

use std::fmt;

/// tmux's own scrollback change indicator (`#{history_size}` /
/// `#{history_bytes}`): stable while a pane is idle, moves whenever it
/// produces output — verified live against a real tmux server. Persisted
/// alongside the captured text so a *later* capture can recognize "this
/// pane hasn't changed since last time" and reuse its scrollback instead of
/// re-pulling it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HistoryIndicator {
    pub history_size: u64,
    pub history_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// `scrollback` is the full history (`capture-pane -S -`), re-pulled only
    /// when the indicator moved and otherwise carried forward from the
    /// previous capture; `visible` is the on-screen lines, always fresh,
    /// since an alt-screen TUI redraws its screen without touching scrollback.
    Captured {
        indicator: HistoryIndicator,
        scrollback: Vec<String>,
        visible: Vec<String>,
    },
    NotCaptured {
        reason: ContentFailure,
    },
}

/// Why a pane's content is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentFailure {
    /// tmux refused `capture-pane` for this pane; `message` is its `%error`.
    CapturePane { message: String },
    /// The generation predates content being captured on every save.
    NotRecorded,
}

impl fmt::Display for ContentFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContentFailure::CapturePane { message } => write!(f, "capture-pane failed: {message}"),
            ContentFailure::NotRecorded => {
                f.write_str("content was not recorded by this generation")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_pane_is_captured_with_empty_lines_not_absent() {
        let c = Content::Captured {
            indicator: HistoryIndicator {
                history_size: 0,
                history_bytes: 0,
            },
            scrollback: vec![],
            visible: vec![],
        };
        assert!(matches!(c, Content::Captured { .. }));
    }
}
