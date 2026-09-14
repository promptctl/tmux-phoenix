//! What tmux reports is running in a pane's foreground (DESIGN.md §5).

/// `command` is always present — tmux's `pane_current_command` format var
/// never comes back empty. `argv` is the full command line, recovered
/// separately via a `ps` pass (`pane_current_command` gives only the command
/// *name*); recovery is best-effort per pane, so an empty `argv` means
/// recovery failed for *this* pane, not that the program was launched with
/// no arguments (`[LAW:no-silent-failure]` — degraded, not absent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedProgram {
    pub command: String,
    pub argv: Vec<String>,
}

impl CapturedProgram {
    pub fn new(command: impl Into<String>, argv: Vec<String>) -> Self {
        Self {
            command: command.into(),
            argv,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_command_and_argv() {
        let p = CapturedProgram::new("vim", vec!["vim".to_string(), "DESIGN.md".to_string()]);
        assert_eq!(p.command, "vim");
        assert_eq!(p.argv, vec!["vim", "DESIGN.md"]);
    }

    #[test]
    fn empty_argv_is_representable_for_degraded_recovery() {
        let p = CapturedProgram::new("zsh", vec![]);
        assert!(p.argv.is_empty());
    }
}
