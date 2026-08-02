//! Captured pane text (DESIGN.md §5). A `Pane`'s content is `Option<PaneContent>`
//! — `None` when capture-pane failed for that pane (unresponsive pane,
//! degrades that pane alone rather than the whole snapshot). The internal
//! shape here is deliberately minimal: dirty-tracking and incremental replay
//! (DESIGN.md's M3 milestone) build on top of this later.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneContent {
    pub lines: Vec<String>,
}

impl PaneContent {
    pub fn new(lines: Vec<String>) -> Self {
        Self { lines }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_captured_lines() {
        let c = PaneContent::new(vec!["$ ls".to_string(), "DESIGN.md".to_string()]);
        assert_eq!(c.lines.len(), 2);
    }

    #[test]
    fn blank_pane_is_representable_as_empty_lines() {
        let c = PaneContent::new(vec![]);
        assert!(c.lines.is_empty());
    }
}
