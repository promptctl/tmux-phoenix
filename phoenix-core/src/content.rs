//! Captured pane text (DESIGN.md §5). A `Pane`'s content is `Option<PaneContent>`
//! — `None` when capture-pane failed for that pane (unresponsive pane,
//! degrades that pane alone rather than the whole snapshot).

use crate::ids::PaneId;

/// `history_size`/`history_bytes` are tmux's own scrollback change
/// indicator (`#{history_size}`/`#{history_bytes}`, DESIGN.md §5): stable
/// while a pane is idle, moves whenever it produces output — verified live
/// against a real tmux server. Persisting them alongside the captured text
/// (not just transiently during one capture pass) is what lets a *later*
/// capture recognize "this pane hasn't changed since last time" and skip
/// re-pulling its full scrollback.
///
/// `scrollback` and `visible` are captured differently: `scrollback` (the
/// full history, `capture-pane -S -`) is only re-pulled when the indicator
/// moved — otherwise the previous capture's `scrollback` is carried
/// forward unchanged. `visible` (just the on-screen lines, no `-S`) is
/// *always* re-pulled every capture regardless of the indicator, since an
/// alt-screen TUI (a pager, an editor) can redraw its visible screen
/// without ever touching scrollback at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneContent {
    pub pane_id: PaneId,
    pub history_size: u64,
    pub history_bytes: u64,
    pub scrollback: Vec<String>,
    pub visible: Vec<String>,
}

impl PaneContent {
    pub fn new(
        pane_id: PaneId,
        history_size: u64,
        history_bytes: u64,
        scrollback: Vec<String>,
        visible: Vec<String>,
    ) -> Self {
        Self {
            pane_id,
            history_size,
            history_bytes,
            scrollback,
            visible,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_captured_lines() {
        let c = PaneContent::new(
            PaneId(0),
            10,
            2048,
            vec!["$ ls".to_string(), "DESIGN.md".to_string()],
            vec!["$ ls".to_string()],
        );
        assert_eq!(c.scrollback.len(), 2);
        assert_eq!(c.visible.len(), 1);
    }

    #[test]
    fn blank_pane_is_representable_as_empty_lines() {
        let c = PaneContent::new(PaneId(0), 0, 0, vec![], vec![]);
        assert!(c.scrollback.is_empty());
        assert!(c.visible.is_empty());
    }
}
