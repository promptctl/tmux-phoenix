//! What tmux reports is running in a pane's foreground (DESIGN.md §5).

use crate::ids::ProgramName;
use crate::nonempty::NonEmpty;

/// `command` is tmux's `pane_current_command`: the program *name* only.
/// `argv` is the full command line, recovered separately via a `ps` pass;
/// recovery is best-effort per pane, so `None` means recovery failed for
/// *this* pane. A recovered argv always carries at least `argv[0]`, which is
/// why "failed" and "launched with no arguments" cannot be confused
/// (`[LAW:parse-dont-validate]` — a typed absence, not an empty list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedProgram {
    pub command: ProgramName,
    pub argv: Option<NonEmpty<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_command_and_argv() {
        let p = CapturedProgram {
            command: ProgramName::parse("vim").unwrap(),
            argv: Some(NonEmpty::new(
                "vim".to_string(),
                vec!["DESIGN.md".to_string()],
            )),
        };
        assert_eq!(p.command.as_str(), "vim");
        assert_eq!(p.argv.unwrap().len(), 2);
    }

    #[test]
    fn failed_recovery_is_a_typed_absence() {
        let p = CapturedProgram {
            command: ProgramName::parse("zsh").unwrap(),
            argv: None,
        };
        assert!(p.argv.is_none());
    }
}
