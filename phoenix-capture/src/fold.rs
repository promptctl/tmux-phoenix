//! The fold from flat `list-panes -a` rows into the `phoenix-core` graph
//! (ARCHITECTURE.md §6): every window once, every session's winlinks by
//! window id. Pure over its inputs — the per-pane reads (foreground,
//! content) come in as a function the fold calls exactly once per pane, so
//! a test folds fixture rows with a pure one and capture folds live rows
//! with the one that asks the OS and tmux. There is no map to miss a key
//! in: a pane's reads are whatever the function returned for it.
//!
//! Structure capture is all-or-nothing: a row that can't be placed (an
//! active flag that points nowhere, a shared window whose rows disagree)
//! fails the whole fold rather than producing a torn graph.

use phoenix_core::{
    Content, Foreground, NonEmpty, Pane, Session, SnapshotError, WinLink, Window, WindowId,
    WindowIndex,
};

use crate::row::PaneRow;

/// What capture learned about one pane beyond its row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneReads {
    pub foreground: Foreground,
    pub content: Content,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoldError<E> {
    NoSessions,
    NoActiveWindow {
        session: String,
    },
    NoActivePane {
        window: WindowId,
    },
    /// A window linked by several sessions did not list the same panes
    /// under each — the listing changed between rows.
    TornWindow {
        window: WindowId,
    },
    Snapshot(SnapshotError),
    /// The per-pane read function failed in a way that is not one pane's
    /// degradation (the connection itself).
    Read(E),
}

impl<E: std::fmt::Display> std::fmt::Display for FoldError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FoldError::NoSessions => write!(f, "the server has no sessions to capture"),
            FoldError::NoActiveWindow { session } => {
                write!(f, "session {session:?} had no window flagged active")
            }
            FoldError::NoActivePane { window } => {
                write!(f, "window {window} had no pane flagged active")
            }
            FoldError::TornWindow { window } => {
                write!(
                    f,
                    "window {window} listed different panes under different sessions"
                )
            }
            FoldError::Snapshot(e) => write!(f, "{e}"),
            FoldError::Read(e) => write!(f, "{e}"),
        }
    }
}

impl<E: std::fmt::Debug + std::fmt::Display> std::error::Error for FoldError<E> {}

/// The graph's two halves, ready for `Snapshot::new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folded {
    pub windows: NonEmpty<Window>,
    pub sessions: NonEmpty<Session>,
}

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

pub fn fold<E>(
    rows: Vec<PaneRow>,
    read: &mut impl FnMut(&PaneRow) -> Result<PaneReads, E>,
) -> Result<Folded, FoldError<E>> {
    let sessions = group_by(rows.iter().collect(), |r| r.session.clone())
        .into_iter()
        .map(|(_, rows)| fold_session(rows))
        .collect::<Result<Vec<_>, _>>()?;
    // [LAW:parse-dont-validate] the one conversion that stamps NonEmpty also rejects the empty server
    let sessions = NonEmpty::from_vec(sessions).ok_or(FoldError::NoSessions)?;

    let windows = group_by(rows, |r| r.window_id)
        .into_iter()
        .map(|(_, rows)| fold_window(rows, read))
        .collect::<Result<Vec<_>, _>>()?;
    let windows =
        NonEmpty::from_vec(windows).expect("group_by never drops non-empty input to zero groups");

    Ok(Folded { windows, sessions })
}

fn fold_session<E>(rows: Vec<&PaneRow>) -> Result<Session, FoldError<E>> {
    let name = rows[0].session.clone();
    let group = rows[0].group.clone();
    let mut active = None;
    let mut last = None;
    let links: Vec<WinLink> = group_by(rows, |r| r.window_index)
        .into_iter()
        .map(|(index, rows)| {
            if rows[0].window_active {
                active = Some(index);
            }
            if rows[0].window_last {
                last = Some(index);
            }
            WinLink {
                index,
                window: rows[0].window_id,
            }
        })
        .collect();
    let links =
        NonEmpty::from_vec(links).expect("group_by never drops non-empty input to zero groups");
    let active: WindowIndex = active.ok_or_else(|| FoldError::NoActiveWindow {
        session: name.as_str().to_string(),
    })?;
    Session::new(name, group, links, active, last).map_err(FoldError::Snapshot)
}

/// `rows` are every row for one window id: one per pane per winlink, and a
/// session may link one window more than once (verified live: `link-window
/// -s a:1 -t a:5` lists each pane twice under `a`). The panes are the first
/// winlink's; every other winlink must have listed exactly the same ones.
fn fold_window<E>(
    rows: Vec<PaneRow>,
    read: &mut impl FnMut(&PaneRow) -> Result<PaneReads, E>,
) -> Result<Window, FoldError<E>> {
    let id = rows[0].window_id;
    let winlinks = group_by(rows.iter().collect(), |r| {
        (r.session.clone(), r.window_index)
    })
    .len();
    let by_pane = group_by(rows, |r| r.pane_id);
    if by_pane.iter().any(|(_, rows)| rows.len() != winlinks) {
        return Err(FoldError::TornWindow { window: id });
    }

    let mut active = None;
    let mut panes = Vec::with_capacity(by_pane.len());
    let mut first: Option<PaneRow> = None;
    for (_, rows) in by_pane {
        let row = &rows[0];
        if row.pane_active {
            active = Some(row.pane_index);
        }
        let reads = read(row).map_err(FoldError::Read)?;
        panes.push(Pane {
            id: row.pane_id,
            index: row.pane_index,
            cwd: row.cwd.clone(),
            foreground: reads.foreground,
            content: reads.content,
        });
        first.get_or_insert_with(|| row.clone());
    }
    let first = first.expect("group_by never drops non-empty input to zero groups");
    let panes =
        NonEmpty::from_vec(panes).expect("group_by never drops non-empty input to zero groups");
    let active = active.ok_or(FoldError::NoActivePane { window: id })?;
    Window::new(
        id,
        first.made,
        first.window_name,
        first.window_layout,
        first.window_zoomed,
        panes,
        active,
    )
    .map_err(FoldError::Snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::{
        ContentFailure, Cwd, HistoryIndicator, Layout, Made, PaneId, PaneIndex, SessionName,
        WindowName,
    };
    use std::convert::Infallible;

    struct Spec {
        session: &'static str,
        window_index: u32,
        window_id: u32,
        window_active: bool,
        window_last: bool,
        pane_index: u32,
        pane_id: u32,
        pane_active: bool,
    }

    fn row(spec: Spec) -> PaneRow {
        PaneRow {
            session: SessionName::parse(spec.session).unwrap(),
            group: None,
            window_index: WindowIndex(spec.window_index),
            window_id: WindowId(spec.window_id),
            window_name: WindowName::parse("shell").unwrap(),
            window_layout: Layout::parse("b25d,80x24,0,0,0").unwrap(),
            window_active: spec.window_active,
            window_last: spec.window_last,
            window_zoomed: false,
            made: Made::NotByPhoenix,
            pane_index: PaneIndex(spec.pane_index),
            pane_id: PaneId(spec.pane_id),
            pane_pid: 100 + spec.pane_id,
            cwd: Cwd::parse("/home/user"),
            pane_active: spec.pane_active,
            indicator: HistoryIndicator {
                history_size: 0,
                history_bytes: 0,
            },
        }
    }

    fn simple(
        session: &'static str,
        window_index: u32,
        window_id: u32,
        window_active: bool,
        pane_index: u32,
        pane_id: u32,
        pane_active: bool,
    ) -> PaneRow {
        row(Spec {
            session,
            window_index,
            window_id,
            window_active,
            window_last: false,
            pane_index,
            pane_id,
            pane_active,
        })
    }

    fn shell(_row: &PaneRow) -> Result<PaneReads, Infallible> {
        Ok(PaneReads {
            foreground: Foreground::Shell,
            content: Content::NotCaptured {
                reason: ContentFailure::NotRecorded,
            },
        })
    }

    #[test]
    fn folds_a_single_session_single_window_single_pane() {
        let rows = vec![simple("main", 0, 0, true, 0, 1, true)];
        let folded = fold(rows, &mut shell).unwrap();
        assert_eq!(folded.sessions.len(), 1);
        assert_eq!(folded.windows.len(), 1);
        let session = folded.sessions.first();
        assert_eq!(session.name().as_str(), "main");
        assert_eq!(session.active_link().window, WindowId(0));
        assert_eq!(folded.windows.first().active_pane().index.0, 0);
    }

    #[test]
    fn folding_no_rows_is_a_no_sessions_error() {
        assert_eq!(fold(vec![], &mut shell).err(), Some(FoldError::NoSessions));
    }

    #[test]
    fn groups_multiple_sessions_windows_and_panes() {
        let rows = vec![
            simple("main", 0, 0, false, 0, 1, true),
            simple("main", 1, 1, true, 0, 2, true),
            simple("other", 0, 2, true, 0, 3, false),
            simple("other", 0, 2, true, 1, 4, true),
        ];
        let folded = fold(rows, &mut shell).unwrap();
        assert_eq!(folded.sessions.len(), 2);
        assert_eq!(folded.windows.len(), 3);
        let main = folded.sessions.first();
        assert_eq!(main.windows().len(), 2);
        assert_eq!(main.active().0, 1);
        let other = folded.sessions.last();
        assert_eq!(other.active_link().window, WindowId(2));
        let w2 = folded.windows.last();
        assert_eq!(w2.panes().len(), 2);
        assert_eq!(w2.active_pane().index.0, 1);
    }

    #[test]
    fn a_window_linked_by_two_sessions_is_folded_once_and_read_once_per_pane() {
        // `list-panes -a` lists a shared window once per session that links
        // it (verified live on 3.7b); the fold must not duplicate it.
        let rows = vec![
            simple("alpha", 1, 0, true, 0, 0, true),
            simple("alpha", 2, 1, false, 0, 1, true),
            simple("beta", 1, 0, false, 0, 0, true),
            simple("beta", 2, 1, true, 0, 1, true),
        ];
        let mut reads = 0;
        let folded = fold(rows, &mut |r| {
            reads += 1;
            shell(r)
        })
        .unwrap();
        assert_eq!(folded.windows.len(), 2);
        assert_eq!(reads, 2);
        for session in folded.sessions.iter() {
            assert_eq!(session.windows().len(), 2);
        }
        assert_eq!(folded.sessions.first().active_link().window, WindowId(0));
        assert_eq!(folded.sessions.last().active_link().window, WindowId(1));
    }

    #[test]
    fn a_window_linked_twice_into_one_session_is_two_winlinks_to_one_window() {
        // `link-window -s a:1 -t a:5` is legal and lists each pane of the
        // window once per winlink (verified live on 3.7b).
        let rows = vec![
            simple("alpha", 1, 0, true, 0, 0, true),
            simple("alpha", 5, 0, false, 0, 0, true),
        ];
        let mut reads = 0;
        let folded = fold(rows, &mut |r| {
            reads += 1;
            shell(r)
        })
        .unwrap();
        assert_eq!(folded.windows.len(), 1);
        assert_eq!(reads, 1);
        let links = folded.sessions.first().windows();
        assert_eq!(links.len(), 2);
        assert!(links.iter().all(|l| l.window == WindowId(0)));
    }

    #[test]
    fn a_shared_window_whose_sessions_disagree_on_panes_is_torn() {
        let rows = vec![
            simple("alpha", 1, 0, true, 0, 0, true),
            simple("beta", 1, 0, true, 0, 0, true),
            simple("beta", 1, 0, true, 1, 9, false),
        ];
        assert_eq!(
            fold(rows, &mut shell).unwrap_err(),
            FoldError::TornWindow {
                window: WindowId(0)
            }
        );
    }

    #[test]
    fn last_window_and_group_are_carried() {
        let mut rows = vec![
            simple("main", 0, 0, false, 0, 1, true),
            simple("main", 1, 1, true, 0, 2, true),
        ];
        rows[0].window_last = true;
        rows[0].group = phoenix_core::GroupName::parse("g");
        rows[1].group = phoenix_core::GroupName::parse("g");
        let folded = fold(rows, &mut shell).unwrap();
        let main = folded.sessions.first();
        assert_eq!(main.last(), Some(WindowIndex(0)));
        assert_eq!(main.group().unwrap().as_str(), "g");
    }

    #[test]
    fn fails_when_no_window_or_pane_is_flagged_active() {
        assert_eq!(
            fold(vec![simple("main", 0, 0, false, 0, 1, true)], &mut shell).unwrap_err(),
            FoldError::NoActiveWindow {
                session: "main".to_string()
            }
        );
        assert_eq!(
            fold(vec![simple("main", 0, 0, true, 0, 1, false)], &mut shell).unwrap_err(),
            FoldError::NoActivePane {
                window: WindowId(0)
            }
        );
    }

    #[test]
    fn a_read_failure_that_is_not_a_panes_fails_the_fold() {
        let rows = vec![simple("main", 0, 0, true, 0, 1, true)];
        let err = fold(rows, &mut |_: &PaneRow| Err::<PaneReads, _>("link down")).unwrap_err();
        assert_eq!(err, FoldError::Read("link down"));
    }

    #[test]
    fn the_reads_land_on_the_pane_they_were_made_for() {
        let rows = vec![
            simple("main", 0, 0, true, 0, 7, true),
            simple("main", 0, 0, true, 1, 8, false),
        ];
        let folded = fold(rows, &mut |r: &PaneRow| {
            Ok::<_, Infallible>(PaneReads {
                foreground: Foreground::Program {
                    argv: NonEmpty::singleton(format!("prog-{}", r.pane_id)),
                },
                content: Content::NotCaptured {
                    reason: ContentFailure::NotRecorded,
                },
            })
        })
        .unwrap();
        let panes = folded.windows.first().panes();
        for pane in panes.iter() {
            assert_eq!(
                pane.foreground,
                Foreground::Program {
                    argv: NonEmpty::singleton(format!("prog-{}", pane.id))
                }
            );
        }
    }
}
