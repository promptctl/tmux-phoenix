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
}

fn esc(s: &str) -> String {
    tmux_control::commands::tmux_escape(s)
}

fn window_target(session: &SessionName, window: WindowIndex) -> String {
    format!("{}:{}", session.as_str(), window.0)
}

impl TmuxCommand {
    /// The literal tmux command line this represents — this is what
    /// `--dry-run` prints and what gets executed, verbatim, so there's no
    /// way for the two to disagree.
    pub fn to_command_string(&self) -> String {
        match self {
            TmuxCommand::NewSession {
                session,
                first_window_name,
                cwd,
            } => format!(
                "new-session -d -s {} -n {} -c {}",
                esc(session.as_str()),
                esc(first_window_name.as_str()),
                esc(cwd.as_str())
            ),
            TmuxCommand::MoveWindow { session, window } => format!(
                "move-window -s {} -t {}",
                esc(session.as_str()),
                esc(&window_target(session, *window))
            ),
            TmuxCommand::NewWindow {
                session,
                window,
                name,
                cwd,
            } => format!(
                "new-window -t {} -n {} -c {}",
                esc(&window_target(session, *window)),
                esc(name.as_str()),
                esc(cwd.as_str())
            ),
            TmuxCommand::SplitWindow {
                session,
                window,
                cwd,
            } => format!(
                "split-window -t {} -c {}",
                esc(&window_target(session, *window)),
                esc(cwd.as_str())
            ),
            TmuxCommand::SelectLayout {
                session,
                window,
                layout,
            } => format!(
                "select-layout -t {} {}",
                esc(&window_target(session, *window)),
                esc(layout.as_str())
            ),
            TmuxCommand::SelectWindow { session, window } => {
                format!("select-window -t {}", esc(&window_target(session, *window)))
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
            cwd: Utf8PathBuf::new("/home/user"),
        };
        assert_eq!(
            cmd.to_command_string(),
            "new-session -d -s 'main' -n 'shell' -c '/home/user'"
        );
    }

    #[test]
    fn move_window_uses_bare_session_as_source() {
        let cmd = TmuxCommand::MoveWindow {
            session: SessionName::parse("main").unwrap(),
            window: WindowIndex(0),
        };
        assert_eq!(cmd.to_command_string(), "move-window -s 'main' -t 'main:0'");
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
            cmd.to_command_string(),
            "new-window -t 'main:3' -n 'editor' -c '/proj'"
        );
    }

    #[test]
    fn targets_with_single_quotes_are_escaped() {
        let cmd = TmuxCommand::SelectWindow {
            session: SessionName::parse("it's-a-session").unwrap(),
            window: WindowIndex(0),
        };
        assert_eq!(
            cmd.to_command_string(),
            r"select-window -t 'it'\''s-a-session:0'"
        );
    }
}
