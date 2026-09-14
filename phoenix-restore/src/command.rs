//! The plan's vocabulary (DESIGN.md §6: new-session/new-window/
//! split-window/select-layout/select-window/select-pane), typed rather than
//! raw strings, plus the one place each is rendered to the literal tmux
//! command line ticket .2's `--dry-run` prints and then executes — so "what
//! would run" and "what does run" can never drift apart.
//!
//! No `select-pane` variant: see [`crate::plan::panes_active_last`] for why
//! this crate never needs one, and `MoveWindow` below for a command DESIGN.md
//! §6's list doesn't mention but that turned out to be necessary — both
//! divergences were found by running the other five against a real tmux
//! server, not assumed from the spec text.

use phoenix_core::{Layout, NonEmpty, SessionName, Utf8PathBuf, WindowIndex, WindowName};
use tmux_control::{CommandLine, NulInArgument};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxCommand {
    /// Every session restore starts here — tmux always creates exactly one
    /// window when a session is created, so that window's name and first
    /// pane's cwd are set directly at creation (`-n`/`-c`) rather than via a
    /// separate rename.
    NewSession {
        session: SessionName,
        first_window_name: WindowName,
        /// `None` when capture could not read the pane's cwd: the pane
        /// starts wherever tmux's own default puts it, and no `-c` is sent
        /// (tmux-capture-zrz).
        cwd: Option<Utf8PathBuf>,
    },
    /// Relocates the window `NewSession` just created to its captured
    /// index. `new-session` has no flag to request a specific window
    /// index — unlike `new-window` (see below) — so the window lands
    /// wherever the target server's `base-index` puts it, which the plan
    /// can't know (that's live server state, not `Snapshot` data). Verified
    /// live: `move-window -s <session> -t <session>:<index>` (bare session
    /// name as source — right after `new-session` it's the session's only,
    /// and therefore current, window) relocates it correctly. **This
    /// command can legitimately fail** with tmux's "same index" error when
    /// the window already happened to land on the captured index (e.g. the
    /// server's `base-index` already matches) — the executor
    /// (tmux-restore-qll.2) must treat exactly that failure as success, not
    /// a real error.
    MoveWindow {
        session: SessionName,
        window: WindowIndex,
    },
    /// Targets `session:window` explicitly (verified live: tmux honors an
    /// explicit index in `new-window -t`) so restored windows keep their
    /// captured indices rather than whatever tmux would auto-assign.
    NewWindow {
        session: SessionName,
        window: WindowIndex,
        name: WindowName,
        cwd: Option<Utf8PathBuf>,
    },
    SplitWindow {
        session: SessionName,
        window: WindowIndex,
        cwd: Option<Utf8PathBuf>,
    },
    /// Replays the captured layout string verbatim (verified live: given the
    /// same pane count, this reproduces the exact original geometry) —
    /// issued once per window after all of that window's panes exist.
    SelectLayout {
        session: SessionName,
        window: WindowIndex,
        layout: Layout,
    },
    SelectWindow {
        session: SessionName,
        window: WindowIndex,
    },
    /// Relaunches a pane's captured program — the argv the pane had in its
    /// foreground when the snapshot was taken, typed into the restored
    /// pane's shell. Same current-pane targeting as `SplitWindow` and
    /// `PlanStep::ReplayContent`, and the same reason: no addressable pane
    /// index, so this must be issued immediately after the pane's creation,
    /// right after any replay for it (captured scrollback is shown first,
    /// then the program that produced it is actually restarted on top —
    /// same ordering tmux-resurrect used).
    RelaunchProgram {
        session: SessionName,
        window: WindowIndex,
        argv: NonEmpty<String>,
    },
}

/// One step of a [`crate::RestorePlan`]. Two kinds, because they differ in
/// what it takes to render them: a [`PlanStep::Command`] is a tmux command
/// the pure planner can write out in full, while [`PlanStep::ReplayContent`]
/// needs a temp file that only exists at apply time. Splitting them keeps
/// [`TmuxCommand::to_command_line`] total — there is no variant it has to
/// render as something that isn't a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanStep {
    Command(TmuxCommand),
    /// Replays a pane's captured content (tmux-content-dos.3: "the
    /// authoritative grid comes from capture-pane, not a re-emulated
    /// stream") so the pane looks as it did when captured. Like
    /// `SplitWindow`, this deliberately targets `session:window` (the
    /// window's *current* pane) rather than a pane index — see
    /// [`crate::plan::panes_active_last`]'s doc comment — so it must be
    /// issued immediately after the pane it targets is created, before any
    /// later split shifts "current" away.
    ReplayContent {
        session: SessionName,
        window: WindowIndex,
        lines: Vec<String>,
    },
}

impl From<TmuxCommand> for PlanStep {
    fn from(cmd: TmuxCommand) -> Self {
        PlanStep::Command(cmd)
    }
}

impl PlanStep {
    /// What `--dry-run` prints for this step: the exact command line for a
    /// [`PlanStep::Command`], and for a replay — which has no command line
    /// until apply time writes its temp file — a summary of what apply will
    /// do.
    pub fn describe(&self) -> Result<String, NulInArgument> {
        match self {
            PlanStep::Command(cmd) => Ok(cmd.to_command_line()?.as_str().to_owned()),
            PlanStep::ReplayContent {
                session,
                window,
                lines,
            } => Ok(format!(
                "# replay {} line(s) of captured content into {}'s active pane",
                lines.len(),
                window_target(session, *window)
            )),
        }
    }
}

/// POSIX-shell single-quoting. `send-keys` types its text into the pane,
/// where a *shell* — not tmux — parses it, so tmux's own argument escaping
/// (which `CommandLine` applies to the argument as a whole) is the wrong
/// grammar for the text inside it (`[LAW:one-source-of-truth]`: the one
/// shell-quoter, shared with [`crate::apply`]'s content replay).
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A window's `session:index` target — one argument, not two tokens.
fn window_target(session: &SessionName, window: WindowIndex) -> String {
    format!("{}:{}", session.as_str(), window.0)
}

/// `-c <cwd>` when the snapshot has one, nothing when it does not — the one
/// rendering of an absent cwd for every pane-creating command.
fn cwd_args(cwd: &Option<Utf8PathBuf>) -> Vec<&str> {
    cwd.iter().flat_map(|c| ["-c", c.as_str()]).collect()
}

impl TmuxCommand {
    /// The tmux command line this represents. `--dry-run` prints this very
    /// value (via [`CommandLine::as_str`]) and the executor sends this very
    /// value, so there is no second rendering that could disagree with the
    /// first (`[LAW:one-source-of-truth]`).
    ///
    /// Fallible because a `Snapshot` read back from disk carries names that
    /// were only parsed for non-emptiness — a NUL in one has no tmux
    /// argument to encode it as, and that is a failure to report, not to
    /// paper over (`[LAW:no-silent-failure]`).
    pub fn to_command_line(&self) -> Result<CommandLine, NulInArgument> {
        match self {
            TmuxCommand::NewSession {
                session,
                first_window_name,
                cwd,
            } => CommandLine::new(
                "new-session",
                [
                    "-d",
                    "-s",
                    session.as_str(),
                    "-n",
                    first_window_name.as_str(),
                ]
                .into_iter()
                .chain(cwd_args(cwd)),
            ),
            TmuxCommand::MoveWindow { session, window } => {
                let target = window_target(session, *window);
                CommandLine::new("move-window", ["-s", session.as_str(), "-t", &target])
            }
            TmuxCommand::NewWindow {
                session,
                window,
                name,
                cwd,
            } => {
                let target = window_target(session, *window);
                CommandLine::new(
                    "new-window",
                    ["-t", target.as_str(), "-n", name.as_str()]
                        .into_iter()
                        .chain(cwd_args(cwd)),
                )
            }
            TmuxCommand::SplitWindow {
                session,
                window,
                cwd,
            } => {
                let target = window_target(session, *window);
                CommandLine::new(
                    "split-window",
                    ["-t", target.as_str()].into_iter().chain(cwd_args(cwd)),
                )
            }
            TmuxCommand::SelectLayout {
                session,
                window,
                layout,
            } => {
                let target = window_target(session, *window);
                CommandLine::new("select-layout", ["-t", &target, layout.as_str()])
            }
            TmuxCommand::SelectWindow { session, window } => {
                let target = window_target(session, *window);
                CommandLine::new("select-window", ["-t", &target])
            }
            TmuxCommand::RelaunchProgram {
                session,
                window,
                argv,
            } => {
                let target = window_target(session, *window);
                // What tmux types into the pane is parsed by a shell, not by
                // tmux, so each captured word is shell-quoted inside the one
                // argument tmux itself sees.
                let shell_command = argv
                    .iter()
                    .map(|a| shell_quote(a))
                    .collect::<Vec<_>>()
                    .join(" ");
                CommandLine::new("send-keys", ["-t", &target, &shell_command, "Enter"])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_sets_name_and_first_window_in_one_command() {
        let cmd = TmuxCommand::NewSession {
            session: SessionName::parse("main").unwrap(),
            first_window_name: WindowName::parse("shell").unwrap(),
            cwd: Utf8PathBuf::parse("/home/user"),
        };
        assert_eq!(
            cmd.to_command_line().unwrap().as_str(),
            "new-session -d -s main -n shell -c /home/user"
        );
    }

    #[test]
    fn an_absent_cwd_sends_no_dash_c() {
        let cmd = TmuxCommand::SplitWindow {
            session: SessionName::parse("main").unwrap(),
            window: WindowIndex(0),
            cwd: None,
        };
        assert_eq!(
            cmd.to_command_line().unwrap().as_str(),
            "split-window -t main:0"
        );
    }

    #[test]
    fn move_window_uses_bare_session_as_source() {
        let cmd = TmuxCommand::MoveWindow {
            session: SessionName::parse("main").unwrap(),
            window: WindowIndex(0),
        };
        assert_eq!(
            cmd.to_command_line().unwrap().as_str(),
            "move-window -s main -t main:0"
        );
    }

    #[test]
    fn new_window_targets_an_explicit_index() {
        let cmd = TmuxCommand::NewWindow {
            session: SessionName::parse("main").unwrap(),
            window: WindowIndex(3),
            name: WindowName::parse("editor").unwrap(),
            cwd: Utf8PathBuf::parse("/proj"),
        };
        assert_eq!(
            cmd.to_command_line().unwrap().as_str(),
            "new-window -t main:3 -n editor -c /proj"
        );
    }

    #[test]
    fn relaunch_program_sends_a_send_keys_targeting_the_current_pane() {
        let cmd = TmuxCommand::RelaunchProgram {
            session: SessionName::parse("main").unwrap(),
            window: WindowIndex(1),
            argv: NonEmpty::new("vim".to_string(), vec!["DESIGN.md".to_string()]),
        };
        let rendered = cmd.to_command_line().unwrap();
        assert!(rendered.as_str().starts_with("send-keys -t main:1 "));
        assert!(rendered.as_str().ends_with(" Enter"));
    }

    #[test]
    fn relaunch_program_preserves_spaces_within_a_single_argv_element() {
        // A single captured argument containing a space (e.g. `grep "hello
        // world" file.txt`) must reach the pane's shell as one word, not
        // split into two by either layer of quoting.
        let cmd = TmuxCommand::RelaunchProgram {
            session: SessionName::parse("main").unwrap(),
            window: WindowIndex(0),
            argv: NonEmpty::new(
                "grep".to_string(),
                vec!["hello world".to_string(), "file.txt".to_string()],
            ),
        };
        let rendered = cmd.to_command_line().unwrap();
        assert!(
            rendered.as_str().contains(r"'hello world'"),
            "the argument should reach the shell single-quoted, in {:?}",
            rendered.as_str()
        );
    }

    #[test]
    fn a_target_holding_a_quote_is_encoded_as_one_argument() {
        let cmd = TmuxCommand::SelectWindow {
            session: SessionName::parse("it's-a-session").unwrap(),
            window: WindowIndex(0),
        };
        assert_eq!(
            cmd.to_command_line().unwrap().as_str(),
            r#"select-window -t "it's-a-session:0""#
        );
    }
}
