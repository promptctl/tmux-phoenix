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

use phoenix_core::{Layout, SessionName, Utf8PathBuf, WindowIndex, WindowName};
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
        cwd: Utf8PathBuf,
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
        cwd: Utf8PathBuf,
    },
    SplitWindow {
        session: SessionName,
        window: WindowIndex,
        cwd: Utf8PathBuf,
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
    /// Replays a pane's captured content (tmux-content-dos.3: "the
    /// authoritative grid comes from capture-pane, not a re-emulated
    /// stream") so the pane looks as it did when captured. Like
    /// `SplitWindow`, this deliberately targets `session:window` (the
    /// window's *current* pane) rather than a pane index — see
    /// [`crate::plan::panes_active_last`]'s doc comment — so it must be
    /// issued immediately after the pane it targets is created, before any
    /// later split shifts "current" away.
    ///
    /// **Not a literal tmux command**: unlike every other variant,
    /// [`TmuxCommand::to_command_string`] can't render this one verbatim —
    /// replaying arbitrary-length content needs a temp file that only
    /// exists at apply time, which a pure `plan()` can't create. The
    /// executor (tmux-restore-qll.2's `apply`) special-cases this variant;
    /// `to_command_string` returns a human-readable summary for `--dry-run`
    /// instead of runnable syntax.
    ReplayContent {
        session: SessionName,
        window: WindowIndex,
        lines: Vec<String>,
    },
}

/// A window's `session:index` target — one argument, not two tokens.
fn window_target(session: &SessionName, window: WindowIndex) -> String {
    format!("{}:{}", session.as_str(), window.0)
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
                    "-c",
                    cwd.as_str(),
                ],
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
                    ["-t", &target, "-n", name.as_str(), "-c", cwd.as_str()],
                )
            }
            TmuxCommand::SplitWindow {
                session,
                window,
                cwd,
            } => {
                let target = window_target(session, *window);
                CommandLine::new("split-window", ["-t", &target, "-c", cwd.as_str()])
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
            TmuxCommand::ReplayContent {
                session,
                window,
                lines,
            } => format!(
                "# replay {} line(s) of captured content into {}'s active pane",
                lines.len(),
                window_target(session, *window)
            ),
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
            cwd: Utf8PathBuf::new("/home/user"),
        };
        assert_eq!(
            cmd.to_command_line().unwrap().as_str(),
            "new-session -d -s main -n shell -c /home/user"
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
            cwd: Utf8PathBuf::new("/proj"),
        };
        assert_eq!(
            cmd.to_command_line().unwrap().as_str(),
            "new-window -t main:3 -n editor -c /proj"
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
