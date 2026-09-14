//! The pure fold from flat `list-panes -a` rows into a `phoenix-core`
//! domain tree (DESIGN.md §5). No I/O — testable against fixture text with
//! no tmux running.
//!
//! Structure capture is all-or-nothing: any row that can't be placed (an
//! empty name, an active flag that points nowhere) fails the whole fold
//! rather than silently dropping a session/window/pane, so a torn tree is
//! never produced.

use phoenix_core::{
    CapturedProgram, Layout, NonEmpty, Pane, PaneContent, PaneId, PaneIndex, ProgramName, Session,
    SessionName, SnapshotError, Utf8PathBuf, Window, WindowIndex, WindowName,
};

use crate::row::PaneRow;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoldError {
    NoSessions,
    EmptySessionName,
    EmptyWindowName {
        session: String,
        window_index: u32,
    },
    EmptyLayout {
        session: String,
        window_index: u32,
    },
    EmptyCommand {
        session: String,
        window_index: u32,
        pane_index: u32,
    },
    NoActiveWindow {
        session: String,
    },
    NoActivePane {
        session: String,
        window_index: u32,
    },
    Snapshot(SnapshotError),
}

impl std::fmt::Display for FoldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FoldError::NoSessions => write!(f, "the server has no sessions to capture"),
            FoldError::EmptySessionName => write!(f, "a list-panes row had an empty session name"),
            FoldError::EmptyWindowName {
                session,
                window_index,
            } => write!(
                f,
                "session {session:?} window {window_index} had an empty name"
            ),
            FoldError::EmptyLayout {
                session,
                window_index,
            } => write!(
                f,
                "session {session:?} window {window_index} had an empty layout"
            ),
            FoldError::EmptyCommand {
                session,
                window_index,
                pane_index,
            } => write!(
                f,
                "session {session:?} window {window_index} pane {pane_index} had an empty command"
            ),
            FoldError::NoActiveWindow { session } => {
                write!(f, "session {session:?} had no window flagged active")
            }
            FoldError::NoActivePane {
                session,
                window_index,
            } => write!(
                f,
                "session {session:?} window {window_index} had no pane flagged active"
            ),
            FoldError::Snapshot(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FoldError {}

/// Groups `items` by a key derived from each, preserving the order each key
/// was first seen — deterministic output regardless of `list-panes`'s
/// emission order, which isn't a documented guarantee.
fn group_by<T, K: PartialEq>(items: Vec<T>, key_of: impl Fn(&T) -> K) -> Vec<(K, Vec<T>)> {
    let mut groups: Vec<(K, Vec<T>)> = Vec::new();
    for item in items {
        let key = key_of(&item);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, group)) => group.push(item),
            None => groups.push((key, vec![item])),
        }
    }
    groups
}

pub fn fold(
    rows: Vec<PaneRow>,
    argv_of: &impl Fn(u32) -> Option<NonEmpty<String>>,
    content_of: &impl Fn(u32) -> Option<PaneContent>,
) -> Result<NonEmpty<Session>, FoldError> {
    let by_session = group_by(rows, |r| r.session.clone());
    let sessions = by_session
        .into_iter()
        .map(|(_, rows)| fold_session(rows, argv_of, content_of))
        .collect::<Result<Vec<_>, _>>()?;
    // [LAW:parse-dont-validate] the one conversion that stamps NonEmpty also rejects the empty server
    NonEmpty::from_vec(sessions).ok_or(FoldError::NoSessions)
}

fn fold_session(
    rows: Vec<PaneRow>,
    argv_of: &impl Fn(u32) -> Option<NonEmpty<String>>,
    content_of: &impl Fn(u32) -> Option<PaneContent>,
) -> Result<Session, FoldError> {
    let session_name = rows[0].session.clone();
    let name = SessionName::parse(session_name.clone()).ok_or(FoldError::EmptySessionName)?;

    let by_window = group_by(rows, |r| r.window_index);
    let mut active_window: Option<WindowIndex> = None;
    let windows = by_window
        .into_iter()
        .map(|(index, rows)| {
            if rows.iter().any(|r| r.window_active) {
                active_window = Some(WindowIndex(index));
            }
            fold_window(&session_name, index, rows, argv_of, content_of)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let windows =
        NonEmpty::from_vec(windows).expect("group_by never drops non-empty input to zero groups");
    let active = active_window.ok_or_else(|| FoldError::NoActiveWindow {
        session: session_name.clone(),
    })?;

    Session::new(name, windows, active).map_err(FoldError::Snapshot)
}

fn fold_window(
    session_name: &str,
    window_index: u32,
    rows: Vec<PaneRow>,
    argv_of: &impl Fn(u32) -> Option<NonEmpty<String>>,
    content_of: &impl Fn(u32) -> Option<PaneContent>,
) -> Result<Window, FoldError> {
    let window_name = rows[0].window_name.clone();
    let layout = rows[0].window_layout.clone();

    let name = WindowName::parse(window_name).ok_or_else(|| FoldError::EmptyWindowName {
        session: session_name.to_string(),
        window_index,
    })?;
    let layout = Layout::parse(layout).ok_or_else(|| FoldError::EmptyLayout {
        session: session_name.to_string(),
        window_index,
    })?;

    let mut active_pane: Option<PaneIndex> = None;
    let mut panes = Vec::with_capacity(rows.len());
    for row in rows {
        if row.pane_active {
            active_pane = Some(PaneIndex(row.pane_index));
        }
        let command =
            ProgramName::parse(row.pane_command).ok_or_else(|| FoldError::EmptyCommand {
                session: session_name.to_string(),
                window_index,
                pane_index: row.pane_index,
            })?;
        panes.push(Pane {
            id: PaneId(row.pane_id),
            index: PaneIndex(row.pane_index),
            cwd: Utf8PathBuf::parse(row.pane_cwd),
            program: CapturedProgram {
                command,
                argv: argv_of(row.pane_pid),
            },
            content: content_of(row.pane_id),
        });
    }
    let panes =
        NonEmpty::from_vec(panes).expect("group_by never drops non-empty input to zero groups");
    let active = active_pane.ok_or_else(|| FoldError::NoActivePane {
        session: session_name.to_string(),
        window_index,
    })?;

    Window::new(WindowIndex(window_index), name, layout, panes, active).map_err(FoldError::Snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn row(
        session: &str,
        window_index: u32,
        window_name: &str,
        window_active: bool,
        pane_index: u32,
        pane_active: bool,
        pane_pid: u32,
        pane_id: u32,
    ) -> PaneRow {
        PaneRow {
            session: session.to_string(),
            window_index,
            window_name: window_name.to_string(),
            window_layout: "b25d,80x24,0,0,0".to_string(),
            window_active,
            pane_index,
            pane_cwd: "/home/user".to_string(),
            pane_command: "zsh".to_string(),
            pane_active,
            pane_pid,
            pane_id,
        }
    }

    fn no_argv(_pid: u32) -> Option<NonEmpty<String>> {
        None
    }

    fn no_content(_pane_id: u32) -> Option<PaneContent> {
        None
    }

    #[test]
    fn folds_a_single_session_single_window_single_pane() {
        let rows = vec![row("main", 0, "shell", true, 0, true, 100, 1)];
        let sessions = fold(rows, &no_argv, &no_content).unwrap();
        assert_eq!(sessions.len(), 1);
        let session = sessions.first();
        assert_eq!(session.name().as_str(), "main");
        assert_eq!(session.active_window().active_pane().index.0, 0);
    }

    #[test]
    fn folding_no_rows_is_a_no_sessions_error() {
        assert_eq!(
            fold(vec![], &no_argv, &no_content).err(),
            Some(FoldError::NoSessions)
        );
    }

    #[test]
    fn groups_multiple_sessions_windows_and_panes() {
        let rows = vec![
            row("main", 0, "shell", false, 0, true, 100, 1),
            row("main", 1, "editor", true, 0, true, 101, 2),
            row("other", 0, "shell", true, 0, false, 200, 3),
            row("other", 0, "shell", true, 1, true, 201, 4),
        ];
        let sessions = fold(rows, &no_argv, &no_content).unwrap();
        assert_eq!(sessions.len(), 2);
        let main = sessions.first();
        assert_eq!(main.windows().len(), 2);
        assert_eq!(main.active().0, 1);
        let other = sessions.last();
        assert_eq!(other.active_window().panes().len(), 2);
        assert_eq!(other.active_window().active_pane().index.0, 1);
    }

    #[test]
    fn fails_when_no_window_is_flagged_active() {
        let rows = vec![row("main", 0, "shell", false, 0, true, 100, 1)];
        let err = fold(rows, &no_argv, &no_content).unwrap_err();
        assert_eq!(
            err,
            FoldError::NoActiveWindow {
                session: "main".to_string()
            }
        );
    }

    #[test]
    fn fails_when_no_pane_is_flagged_active() {
        let rows = vec![row("main", 0, "shell", true, 0, false, 100, 1)];
        let err = fold(rows, &no_argv, &no_content).unwrap_err();
        assert_eq!(
            err,
            FoldError::NoActivePane {
                session: "main".to_string(),
                window_index: 0
            }
        );
    }

    #[test]
    fn passes_recovered_argv_through_to_captured_program() {
        let rows = vec![row("main", 0, "shell", true, 0, true, 100, 1)];
        let sessions = fold(
            rows,
            &|pid| {
                assert_eq!(pid, 100);
                Some(NonEmpty::new(
                    "vim".to_string(),
                    vec!["DESIGN.md".to_string()],
                ))
            },
            &no_content,
        )
        .unwrap();
        let pane = sessions.first().active_window().active_pane();
        let argv: Vec<_> = pane.program.argv.clone().unwrap().into_iter().collect();
        assert_eq!(argv, vec!["vim", "DESIGN.md"]);
    }

    #[test]
    fn an_empty_cwd_is_a_typed_absence_not_an_empty_path() {
        let mut r = row("main", 0, "shell", true, 0, true, 100, 1);
        r.pane_cwd = String::new();
        let sessions = fold(vec![r], &no_argv, &no_content).unwrap();
        assert_eq!(sessions.first().active_window().active_pane().cwd, None);
    }

    #[test]
    fn an_empty_command_fails_the_fold() {
        let mut r = row("main", 0, "shell", true, 0, true, 100, 1);
        r.pane_command = String::new();
        let err = fold(vec![r], &no_argv, &no_content).unwrap_err();
        assert_eq!(
            err,
            FoldError::EmptyCommand {
                session: "main".to_string(),
                window_index: 0,
                pane_index: 0,
            }
        );
    }

    #[test]
    fn passes_captured_content_through_keyed_by_pane_id_not_pane_pid() {
        // pane_pid and pane_id are deliberately different values here, so a
        // fold that mixed them up would fail this test.
        let rows = vec![row("main", 0, "shell", true, 0, true, 100, 42)];
        let sessions = fold(rows, &no_argv, &|pane_id| {
            assert_eq!(pane_id, 42);
            Some(PaneContent::new(
                5,
                512,
                vec!["scrollback".to_string()],
                vec!["visible".to_string()],
            ))
        })
        .unwrap();
        let pane = sessions.first().active_window().active_pane();
        let content = pane.content.as_ref().unwrap();
        assert_eq!(content.scrollback, vec!["scrollback"]);
        assert_eq!(content.visible, vec!["visible"]);
    }
}
