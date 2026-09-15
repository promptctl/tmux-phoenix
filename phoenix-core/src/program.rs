//! What tmux reports is running in a pane's foreground (DESIGN.md §5), and
//! what that amounts to for anything that acts on it.

use crate::ids::ProgramName;
use crate::nonempty::NonEmpty;

/// Basenames (tmux's own `pane_current_command`) of interactive shells.
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "ash", "elvish", "nu", "xonsh",
];

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

/// What a pane's foreground amounts to. The three stay apart because they
/// mean different things to different callers: restore relaunches only a
/// `Program`, while deciding that a pane is safe to replace needs proof of
/// `IdleShell` — an `Unknown` pane may be running anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Foreground<'a> {
    /// Best-effort `ps` recovery failed for this pane.
    Unknown,
    /// The pane's own interactive shell at a prompt: tmux reports the shell
    /// as the foreground whenever the pane is idle. Only a shell invoked with
    /// no non-flag argument counts — `bash deploy.sh` is a script the user
    /// was running.
    IdleShell,
    /// A program the user was running, with its recovered command line.
    Program(&'a NonEmpty<String>),
}

impl CapturedProgram {
    pub fn foreground(&self) -> Foreground<'_> {
        match &self.argv {
            None => Foreground::Unknown,
            Some(argv)
                if SHELLS.contains(&self.command.as_str())
                    && argv.iter().skip(1).all(|arg| arg.starts_with('-')) =>
            {
                Foreground::IdleShell
            }
            Some(argv) => Foreground::Program(argv),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(command: &str, argv: &[&str]) -> CapturedProgram {
        CapturedProgram {
            command: ProgramName::parse(command).unwrap(),
            argv: NonEmpty::from_vec(argv.iter().map(|a| a.to_string()).collect()),
        }
    }

    #[test]
    fn holds_command_and_argv() {
        let p = program("vim", &["vim", "DESIGN.md"]);
        assert_eq!(p.command.as_str(), "vim");
        assert_eq!(p.argv.unwrap().len(), 2);
    }

    #[test]
    fn failed_recovery_is_unknown_not_idle() {
        assert_eq!(program("zsh", &[]).foreground(), Foreground::Unknown);
    }

    #[test]
    fn a_shell_with_only_flags_is_idle() {
        for (command, argv) in [
            ("zsh", vec!["-zsh"]),
            ("zsh", vec!["/bin/zsh", "-l"]),
            ("bash", vec!["bash"]),
            ("fish", vec!["fish"]),
        ] {
            assert_eq!(
                program(command, &argv).foreground(),
                Foreground::IdleShell,
                "{command} {argv:?}"
            );
        }
    }

    #[test]
    fn a_script_or_any_other_program_is_a_program() {
        for (command, argv) in [
            ("bash", vec!["bash", "deploy.sh"]),
            ("vim", vec!["vim", "notes.md"]),
            ("htop", vec!["htop"]),
        ] {
            let p = program(command, &argv);
            assert!(
                matches!(p.foreground(), Foreground::Program(a) if a.len() == argv.len()),
                "{command} {argv:?}"
            );
        }
    }
}
