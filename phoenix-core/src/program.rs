//! What holds a pane's terminal (ARCHITECTURE.md §5), decided once at
//! capture from two OS reads and never re-derived: the pane terminal's
//! foreground process group, then that group leader's exact argv. This
//! module holds the pure rule; the reads live in `phoenix-capture`.

use std::fmt;

use crate::nonempty::NonEmpty;

/// Basenames of interactive shells. A value capture is given, not a
/// constant hidden in the rule, so a caller with an unusual shell passes a
/// different list instead of a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shells(Vec<String>);

impl Default for Shells {
    fn default() -> Self {
        Self(
            [
                "sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "ash", "elvish", "nu",
                "xonsh",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        )
    }
}

impl Shells {
    pub fn new(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(names.into_iter().map(Into::into).collect())
    }

    fn contains(&self, basename: &str) -> bool {
        self.0.iter().any(|s| s == basename)
    }
}

/// What the OS said about the process holding the pane's terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalHolder {
    /// The pane's own process (`pane_pid`, its shell) is the foreground
    /// process group: no job holds the terminal.
    pub at_own_process: bool,
    /// The foreground group leader's exact argv, NUL-split, never
    /// whitespace-split.
    pub argv: NonEmpty<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Foreground {
    /// Idle at the pane's own shell: the pane is at its own process, that
    /// process is a shell, and it was launched with flags only — `bash -l`
    /// is a shell, `bash ./watch.sh` is a program.
    Shell,
    /// A program the user was running, with its exact command line.
    Program {
        argv: NonEmpty<String>,
    },
    Unrecovered {
        reason: RecoveryFailure,
    },
}

/// Why capture could not decide a pane's foreground.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryFailure {
    /// The pane's own process (`pane_pid`) was gone before it was read.
    ShellGone,
    /// The pane's process has no controlling terminal to be the foreground
    /// of, so nothing holds it.
    NoTerminal,
    /// The foreground group's leader exited while its pipeline lives
    /// (`make | less` after `make` finished): there is no one command line
    /// to relaunch, and guessing a survivor is not recovery.
    LeaderGone,
    /// The OS refused a read; `message` is its error.
    Os { message: String },
    /// The generation predates exact argv recording and recorded none.
    NotRecorded,
}

impl fmt::Display for RecoveryFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecoveryFailure::ShellGone => {
                f.write_str("the pane's shell was gone before it was read")
            }
            RecoveryFailure::NoTerminal => {
                f.write_str("the pane's process has no controlling terminal")
            }
            RecoveryFailure::LeaderGone => {
                f.write_str("the foreground process group's leader exited while its pipeline lives")
            }
            RecoveryFailure::Os { message } => write!(f, "process read failed: {message}"),
            RecoveryFailure::NotRecorded => f.write_str("argv was not recorded by this generation"),
        }
    }
}

impl Foreground {
    /// The one rule (`[LAW:single-enforcer]`): a pane at its own process
    /// whose `argv[0]` — a login shell's leading `-` removed — has its
    /// basename in `shells`, with every remaining argument a flag, is
    /// `Shell`. Everything else holding the terminal is a `Program` with
    /// that argv: a job, or a pane whose own process is `vim`, `ssh`, a
    /// script, or a `default-command`.
    pub fn of(holder: TerminalHolder, shells: &Shells) -> Self {
        let TerminalHolder {
            at_own_process,
            argv,
        } = holder;
        let is_shell = at_own_process
            && shells.contains(shell_basename(argv.first()))
            && argv.iter().skip(1).all(|arg| arg.starts_with('-'));
        if is_shell {
            Foreground::Shell
        } else {
            Foreground::Program { argv }
        }
    }
}

/// The program name an `argv[0]` spells: a login shell's leading `-` and any
/// directory dropped, so `-zsh` and `/bin/zsh` both read as `zsh`.
fn shell_basename(arg0: &str) -> &str {
    let name = arg0.trim_start_matches('-');
    name.rsplit('/').next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(at_own_process: bool, argv: &[&str]) -> TerminalHolder {
        TerminalHolder {
            at_own_process,
            argv: NonEmpty::from_vec(argv.iter().map(|a| a.to_string()).collect()).unwrap(),
        }
    }

    #[test]
    fn a_shell_at_its_own_process_with_only_flags_is_idle() {
        let shells = Shells::default();
        for argv in [
            vec!["-zsh"],
            vec!["/bin/zsh", "-l"],
            vec!["bash"],
            vec!["fish"],
        ] {
            assert_eq!(
                Foreground::of(holder(true, &argv), &shells),
                Foreground::Shell,
                "{argv:?}"
            );
        }
    }

    #[test]
    fn a_job_holding_the_terminal_is_a_program_even_when_it_is_a_shell() {
        let shells = Shells::default();
        for argv in [
            vec!["vim", "notes.md"],
            vec!["bash", "-l"],
            vec!["sleep", "300"],
        ] {
            assert!(
                matches!(
                    Foreground::of(holder(false, &argv), &shells),
                    Foreground::Program { argv: a } if a.len() == argv.len()
                ),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn a_shell_running_a_script_or_a_non_shell_own_process_is_a_program() {
        let shells = Shells::default();
        for argv in [vec!["bash", "./watch.sh"], vec!["vim"], vec!["ssh", "host"]] {
            assert!(
                matches!(
                    Foreground::of(holder(true, &argv), &shells),
                    Foreground::Program { .. }
                ),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn the_shell_set_is_a_value() {
        let only_nu = Shells::new(["nu"]);
        assert_eq!(
            Foreground::of(holder(true, &["nu"]), &only_nu),
            Foreground::Shell
        );
        assert!(matches!(
            Foreground::of(holder(true, &["zsh"]), &only_nu),
            Foreground::Program { .. }
        ));
    }
}
