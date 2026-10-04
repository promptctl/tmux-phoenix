//! The domain graph itself (ARCHITECTURE.md §5). tmux's structure is not a
//! tree: a session holds an ordered set of *winlinks* to windows, a window
//! may be linked into several sessions (grouped sessions, `link-window`),
//! and a window holds panes. The model says exactly that — `Snapshot` holds
//! every window once and every session links them by [`WindowId`] — so a
//! shared window is serialized once and grouped sessions are a data fill,
//! not a redesign.
//!
//! Every cross-field invariant lives behind a validating constructor
//! (`[LAW:types-are-the-program]`): an `active`/`last` index resolves to
//! exactly one winlink; a pane `active` index resolves to exactly one pane;
//! every winlink resolves to a window in the snapshot; every window is
//! linked by at least one session; every client sits on a session the
//! snapshot holds. Fields with an invariant are private; the only way to
//! hold a `Snapshot` is one where the invariants already hold.

use std::fmt;

use crate::content::{Content, ContentFailure};
use crate::ids::{
    ClientName, GroupName, Layout, PaneId, PaneIndex, SessionName, WindowId, WindowIndex,
    WindowName,
};
use crate::nonempty::NonEmpty;
use crate::path::Cwd;
use crate::program::{Foreground, RecoveryFailure};
use crate::provenance::{Made, Origin, Touched};
use crate::time::OffsetDateTime;
use crate::version::TmuxVersion;

/// A validated graph failed to construct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    ActiveWindowNotFound {
        session: SessionName,
        active: WindowIndex,
    },
    LastWindowNotFound {
        session: SessionName,
        last: WindowIndex,
    },
    ActivePaneNotFound {
        window: WindowId,
        active: PaneIndex,
    },
    DuplicateWindowIndex {
        session: SessionName,
        index: WindowIndex,
    },
    DuplicatePaneIndex {
        window: WindowId,
        index: PaneIndex,
    },
    DuplicateWindowId {
        id: WindowId,
    },
    DuplicateSessionName {
        name: SessionName,
    },
    /// A winlink names a window the snapshot does not hold.
    UnlinkedWindowRef {
        session: SessionName,
        window: WindowId,
    },
    /// A window no session links — tmux has no such window.
    OrphanWindow {
        id: WindowId,
    },
    /// A client sits on a session the snapshot does not hold.
    UnknownClientSession {
        client: ClientName,
        session: SessionName,
    },
}

/// The first value that appears twice, if any. Collections here are small,
/// so the quadratic scan costs nothing and needs no allocation.
fn duplicate<I: Clone + PartialEq>(items: impl Iterator<Item = I> + Clone) -> Option<I> {
    items
        .clone()
        .enumerate()
        .find(|(i, a)| items.clone().take(*i).any(|b| b == *a))
        .map(|(_, a)| a)
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::ActiveWindowNotFound { session, active } => write!(
                f,
                "session {session} has no window at active index {}",
                active.0
            ),
            SnapshotError::LastWindowNotFound { session, last } => {
                write!(
                    f,
                    "session {session} has no window at last index {}",
                    last.0
                )
            }
            SnapshotError::ActivePaneNotFound { window, active } => {
                write!(
                    f,
                    "window {window} has no pane at active index {}",
                    active.0
                )
            }
            SnapshotError::DuplicateWindowIndex { session, index } => {
                write!(f, "session {session} has two windows at index {}", index.0)
            }
            SnapshotError::DuplicatePaneIndex { window, index } => {
                write!(f, "window {window} has two panes at index {}", index.0)
            }
            SnapshotError::DuplicateWindowId { id } => write!(f, "window {id} appears twice"),
            SnapshotError::DuplicateSessionName { name } => {
                write!(f, "session {name} appears twice")
            }
            SnapshotError::UnlinkedWindowRef { session, window } => {
                write!(
                    f,
                    "session {session} links window {window}, which the snapshot does not hold"
                )
            }
            SnapshotError::OrphanWindow { id } => write!(f, "window {id} is linked by no session"),
            SnapshotError::UnknownClientSession { client, session } => {
                write!(
                    f,
                    "client {client} is on session {session}, which the snapshot does not hold"
                )
            }
        }
    }
}

impl std::error::Error for SnapshotError {}

/// No cross-field invariant to protect, so the fields stay public.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub id: PaneId,
    pub index: PaneIndex,
    pub cwd: Cwd,
    pub foreground: Foreground,
    pub content: Content,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    id: WindowId,
    made: Made,
    name: WindowName,
    layout: Layout,
    /// tmux zooms the active pane and un-zooms when another pane is
    /// selected, so "which pane is zoomed" is always "the active one"; a
    /// flag is the strongest true statement.
    zoomed: bool,
    panes: NonEmpty<Pane>,
    active: PaneIndex,
}

impl Window {
    /// Fails unless `active` matches exactly one pane in `panes`: no pane
    /// ([`SnapshotError::ActivePaneNotFound`]) or two panes sharing an index
    /// ([`SnapshotError::DuplicatePaneIndex`]) are both refused.
    pub fn new(
        id: WindowId,
        made: Made,
        name: WindowName,
        layout: Layout,
        zoomed: bool,
        panes: NonEmpty<Pane>,
        active: PaneIndex,
    ) -> Result<Self, SnapshotError> {
        if let Some(index) = duplicate(panes.iter().map(|p| p.index)) {
            return Err(SnapshotError::DuplicatePaneIndex { window: id, index });
        }
        if !panes.iter().any(|p| p.index == active) {
            return Err(SnapshotError::ActivePaneNotFound { window: id, active });
        }
        Ok(Self {
            id,
            made,
            name,
            layout,
            zoomed,
            panes,
            active,
        })
    }

    pub fn id(&self) -> WindowId {
        self.id
    }

    pub fn made(&self) -> Made {
        self.made
    }

    pub fn name(&self) -> &WindowName {
        &self.name
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn zoomed(&self) -> bool {
        self.zoomed
    }

    pub fn panes(&self) -> &NonEmpty<Pane> {
        &self.panes
    }

    pub fn active(&self) -> PaneIndex {
        self.active
    }

    /// The pane `active` points at. Always succeeds — `Window::new` already
    /// proved it resolves.
    pub fn active_pane(&self) -> &Pane {
        self.panes
            .iter()
            .find(|p| p.index == self.active)
            .expect("Window::new validated that `active` resolves to a member")
    }
}

/// One session's link to a window: the index is per session, the window is
/// shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WinLink {
    pub index: WindowIndex,
    pub window: WindowId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    name: SessionName,
    group: Option<GroupName>,
    windows: NonEmpty<WinLink>,
    active: WindowIndex,
    last: Option<WindowIndex>,
}

impl Session {
    /// Fails unless `active` (and `last`, when there is one) matches exactly
    /// one winlink in `windows`.
    pub fn new(
        name: SessionName,
        group: Option<GroupName>,
        windows: NonEmpty<WinLink>,
        active: WindowIndex,
        last: Option<WindowIndex>,
    ) -> Result<Self, SnapshotError> {
        if let Some(index) = duplicate(windows.iter().map(|w| w.index)) {
            return Err(SnapshotError::DuplicateWindowIndex {
                session: name,
                index,
            });
        }
        if !windows.iter().any(|w| w.index == active) {
            return Err(SnapshotError::ActiveWindowNotFound {
                session: name,
                active,
            });
        }
        if let Some(last) = last.filter(|last| !windows.iter().any(|w| w.index == *last)) {
            return Err(SnapshotError::LastWindowNotFound {
                session: name,
                last,
            });
        }
        Ok(Self {
            name,
            group,
            windows,
            active,
            last,
        })
    }

    pub fn name(&self) -> &SessionName {
        &self.name
    }

    pub fn group(&self) -> Option<&GroupName> {
        self.group.as_ref()
    }

    pub fn windows(&self) -> &NonEmpty<WinLink> {
        &self.windows
    }

    pub fn active(&self) -> WindowIndex {
        self.active
    }

    pub fn last(&self) -> Option<WindowIndex> {
        self.last
    }

    /// The winlink `active` points at. Always succeeds — `Session::new`
    /// already proved it resolves.
    pub fn active_link(&self) -> &WinLink {
        self.windows
            .iter()
            .find(|w| w.index == self.active)
            .expect("Session::new validated that `active` resolves to a member")
    }
}

/// A client attached to the server, from `list-clients` over the same
/// connection: which session each terminal sits on, by name, so a restore
/// moves clients by value and never by implication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub name: ClientName,
    pub session: SessionName,
}

/// One pane's absence with its reason — the one derived view of "degraded"
/// (`[LAW:one-source-of-truth]`): the CLI's exit code and the daemon's log
/// line both read [`Snapshot::degradations`], neither recomputes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Degradation {
    Cwd {
        pane: PaneId,
    },
    Foreground {
        pane: PaneId,
        reason: RecoveryFailure,
    },
    Content {
        pane: PaneId,
        reason: ContentFailure,
    },
}

impl fmt::Display for Degradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Degradation::Cwd { pane } => write!(f, "pane {pane}: working directory unreadable"),
            Degradation::Foreground { pane, reason } => write!(f, "pane {pane}: {reason}"),
            Degradation::Content { pane, reason } => write!(f, "pane {pane}: {reason}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub origin: Origin,
    pub touched: Touched,
    pub captured_at: OffsetDateTime,
    pub tmux_version: TmuxVersion,
    windows: NonEmpty<Window>,
    sessions: NonEmpty<Session>,
    clients: Vec<Client>,
}

impl Snapshot {
    /// Fails unless every winlink resolves to a window in `windows`, every
    /// window is linked by at least one session, window ids and session
    /// names are unique, and every client sits on a session held here.
    pub fn new(
        origin: Origin,
        touched: Touched,
        captured_at: OffsetDateTime,
        tmux_version: TmuxVersion,
        windows: NonEmpty<Window>,
        sessions: NonEmpty<Session>,
        clients: Vec<Client>,
    ) -> Result<Self, SnapshotError> {
        if let Some(id) = duplicate(windows.iter().map(|w| w.id)) {
            return Err(SnapshotError::DuplicateWindowId { id });
        }
        if let Some(name) = duplicate(sessions.iter().map(|s| s.name.clone())) {
            return Err(SnapshotError::DuplicateSessionName { name });
        }
        let held = |id: WindowId| windows.iter().any(|w| w.id == id);
        for session in sessions.iter() {
            if let Some(link) = session.windows.iter().find(|l| !held(l.window)) {
                return Err(SnapshotError::UnlinkedWindowRef {
                    session: session.name.clone(),
                    window: link.window,
                });
            }
        }
        let linked = |id: WindowId| {
            sessions
                .iter()
                .any(|s| s.windows.iter().any(|l| l.window == id))
        };
        if let Some(window) = windows.iter().find(|w| !linked(w.id)) {
            return Err(SnapshotError::OrphanWindow { id: window.id });
        }
        if let Some(client) = clients
            .iter()
            .find(|c| !sessions.iter().any(|s| s.name == c.session))
        {
            return Err(SnapshotError::UnknownClientSession {
                client: client.name.clone(),
                session: client.session.clone(),
            });
        }
        Ok(Self {
            origin,
            touched,
            captured_at,
            tmux_version,
            windows,
            sessions,
            clients,
        })
    }

    pub fn windows(&self) -> &NonEmpty<Window> {
        &self.windows
    }

    pub fn sessions(&self) -> &NonEmpty<Session> {
        &self.sessions
    }

    pub fn clients(&self) -> &[Client] {
        &self.clients
    }

    /// The window a winlink names. Always succeeds — `Snapshot::new` already
    /// proved every link resolves.
    pub fn window(&self, id: WindowId) -> &Window {
        self.windows
            .iter()
            .find(|w| w.id == id)
            .expect("Snapshot::new validated that every winlink resolves")
    }

    /// `session`'s windows in winlink order, each paired with its link.
    pub fn windows_of<'a>(
        &'a self,
        session: &'a Session,
    ) -> impl Iterator<Item = (&'a WinLink, &'a Window)> + 'a {
        session
            .windows
            .iter()
            .map(move |link| (link, self.window(link.window)))
    }

    pub fn active_window(&self, session: &Session) -> &Window {
        self.window(session.active_link().window)
    }

    /// Every pane once, whichever sessions link its window.
    pub fn panes(&self) -> impl Iterator<Item = &Pane> {
        self.windows.iter().flat_map(|w| w.panes.iter())
    }

    /// Each absence in the snapshot with its reason, one entry per pane and
    /// kind. Empty means nothing was degraded.
    pub fn degradations(&self) -> Vec<Degradation> {
        self.panes()
            .flat_map(|pane| {
                let cwd = match &pane.cwd {
                    Cwd::Unreadable => Some(Degradation::Cwd { pane: pane.id }),
                    Cwd::Known(_) => None,
                };
                let foreground = match &pane.foreground {
                    Foreground::Unrecovered { reason } => Some(Degradation::Foreground {
                        pane: pane.id,
                        reason: reason.clone(),
                    }),
                    Foreground::Shell | Foreground::Program { .. } => None,
                };
                let content = match &pane.content {
                    Content::NotCaptured { reason } => Some(Degradation::Content {
                        pane: pane.id,
                        reason: reason.clone(),
                    }),
                    Content::Captured { .. } => None,
                };
                [cwd, foreground, content].into_iter().flatten()
            })
            .collect()
    }

    /// A session nobody has built anything in yet: one window holding one
    /// pane idle at its shell — what a terminal that starts `tmux` at login
    /// creates. Read only by the daemon's boot heuristic, which
    /// ARCHITECTURE.md §3 replaces with [`Touched`]; the daemon ticket
    /// deletes this with the heuristic.
    pub fn is_bootstrap(&self, session: &Session) -> bool {
        let window = self.window(session.windows.first().window);
        session.windows.len() == 1
            && window.panes.len() == 1
            && window.panes.first().foreground == Foreground::Shell
    }

    /// Every session is a bootstrap session ([`Snapshot::is_bootstrap`]).
    pub fn is_bootstrap_only(&self) -> bool {
        self.sessions.iter().all(|s| self.is_bootstrap(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::HistoryIndicator;
    use crate::ids::ServerId;
    use crate::nonempty::NonEmpty;

    fn pane(index: u32) -> Pane {
        Pane {
            id: PaneId(index),
            index: PaneIndex(index),
            cwd: Cwd::parse("/home/user"),
            foreground: Foreground::Shell,
            content: Content::Captured {
                indicator: HistoryIndicator {
                    history_size: 0,
                    history_bytes: 0,
                },
                scrollback: vec![],
                visible: vec![],
            },
        }
    }

    fn running(argv: &[&str]) -> Pane {
        Pane {
            foreground: Foreground::Program {
                argv: NonEmpty::from_vec(argv.iter().map(|a| a.to_string()).collect()).unwrap(),
            },
            ..pane(0)
        }
    }

    fn window(id: u32, panes: NonEmpty<Pane>, active: u32) -> Result<Window, SnapshotError> {
        Window::new(
            WindowId(id),
            Made::NotByPhoenix,
            WindowName::parse("shell").unwrap(),
            Layout::parse("a1b2,80x24,0,0,0").unwrap(),
            false,
            panes,
            PaneIndex(active),
        )
    }

    fn link(index: u32, window: u32) -> WinLink {
        WinLink {
            index: WindowIndex(index),
            window: WindowId(window),
        }
    }

    fn session(name: &str, links: NonEmpty<WinLink>) -> Session {
        let active = links.first().index;
        Session::new(SessionName::parse(name).unwrap(), None, links, active, None).unwrap()
    }

    fn snapshot(
        windows: NonEmpty<Window>,
        sessions: NonEmpty<Session>,
        clients: Vec<Client>,
    ) -> Result<Snapshot, SnapshotError> {
        Snapshot::new(
            Origin::Recorded(ServerId {
                pid: 1,
                start_time: 2,
            }),
            Touched::Never,
            OffsetDateTime::from_unix_timestamp(1_700_000_000),
            TmuxVersion { major: 3, minor: 7 },
            windows,
            sessions,
            clients,
        )
    }

    #[test]
    fn window_new_fails_when_active_does_not_resolve() {
        let panes = NonEmpty::from_vec(vec![pane(0), pane(1)]).unwrap();
        assert_eq!(
            window(0, panes, 99).unwrap_err(),
            SnapshotError::ActivePaneNotFound {
                window: WindowId(0),
                active: PaneIndex(99),
            }
        );
    }

    #[test]
    fn window_new_fails_when_two_panes_share_an_index() {
        let panes = NonEmpty::from_vec(vec![pane(0), pane(1), pane(1)]).unwrap();
        assert_eq!(
            window(0, panes, 1).unwrap_err(),
            SnapshotError::DuplicatePaneIndex {
                window: WindowId(0),
                index: PaneIndex(1),
            }
        );
    }

    #[test]
    fn window_active_pane_returns_the_matching_pane() {
        let panes = NonEmpty::from_vec(vec![pane(0), pane(1)]).unwrap();
        let w = window(0, panes, 1).unwrap();
        assert_eq!(w.active_pane().index, PaneIndex(1));
    }

    #[test]
    fn session_new_refuses_a_duplicate_index_and_unresolved_active_or_last() {
        let name = SessionName::parse("main").unwrap();
        assert_eq!(
            Session::new(
                name.clone(),
                None,
                NonEmpty::new(link(3, 0), vec![link(3, 1)]),
                WindowIndex(3),
                None
            )
            .unwrap_err(),
            SnapshotError::DuplicateWindowIndex {
                session: name.clone(),
                index: WindowIndex(3),
            }
        );
        assert_eq!(
            Session::new(
                name.clone(),
                None,
                NonEmpty::singleton(link(0, 0)),
                WindowIndex(5),
                None
            )
            .unwrap_err(),
            SnapshotError::ActiveWindowNotFound {
                session: name.clone(),
                active: WindowIndex(5),
            }
        );
        assert_eq!(
            Session::new(
                name.clone(),
                None,
                NonEmpty::singleton(link(0, 0)),
                WindowIndex(0),
                Some(WindowIndex(9))
            )
            .unwrap_err(),
            SnapshotError::LastWindowNotFound {
                session: name,
                last: WindowIndex(9),
            }
        );
    }

    #[test]
    fn a_shared_window_is_held_once_and_reached_from_each_session() {
        let shared = window(7, NonEmpty::singleton(pane(0)), 0).unwrap();
        let alpha = session("alpha", NonEmpty::singleton(link(1, 7)));
        let beta = session("beta", NonEmpty::singleton(link(4, 7)));
        let snapshot = snapshot(
            NonEmpty::singleton(shared),
            NonEmpty::new(alpha, vec![beta]),
            vec![],
        )
        .unwrap();

        assert_eq!(snapshot.windows().len(), 1);
        for s in snapshot.sessions().iter() {
            let (link, window) = snapshot.windows_of(s).next().unwrap();
            assert_eq!(window.id(), WindowId(7));
            assert_eq!(link.window, WindowId(7));
            assert_eq!(snapshot.active_window(s).id(), WindowId(7));
        }
        assert_eq!(snapshot.panes().count(), 1);
    }

    #[test]
    fn snapshot_new_refuses_a_dangling_link_an_orphan_window_and_a_stray_client() {
        let w0 = window(0, NonEmpty::singleton(pane(0)), 0).unwrap();
        let w1 = window(1, NonEmpty::singleton(pane(0)), 0).unwrap();
        let main = session("main", NonEmpty::singleton(link(0, 0)));

        assert_eq!(
            snapshot(
                NonEmpty::singleton(w0.clone()),
                NonEmpty::singleton(session("main", NonEmpty::singleton(link(0, 9)))),
                vec![]
            )
            .unwrap_err(),
            SnapshotError::UnlinkedWindowRef {
                session: SessionName::parse("main").unwrap(),
                window: WindowId(9),
            }
        );
        assert_eq!(
            snapshot(
                NonEmpty::new(w0.clone(), vec![w1]),
                NonEmpty::singleton(main.clone()),
                vec![]
            )
            .unwrap_err(),
            SnapshotError::OrphanWindow { id: WindowId(1) }
        );
        assert_eq!(
            snapshot(
                NonEmpty::singleton(w0),
                NonEmpty::singleton(main),
                vec![Client {
                    name: ClientName::parse("/dev/ttys001").unwrap(),
                    session: SessionName::parse("other").unwrap(),
                }]
            )
            .unwrap_err(),
            SnapshotError::UnknownClientSession {
                client: ClientName::parse("/dev/ttys001").unwrap(),
                session: SessionName::parse("other").unwrap(),
            }
        );
    }

    #[test]
    fn snapshot_new_refuses_duplicate_window_ids_and_session_names() {
        let w = window(0, NonEmpty::singleton(pane(0)), 0).unwrap();
        let main = session("main", NonEmpty::singleton(link(0, 0)));
        assert_eq!(
            snapshot(
                NonEmpty::new(w.clone(), vec![w.clone()]),
                NonEmpty::singleton(main.clone()),
                vec![]
            )
            .unwrap_err(),
            SnapshotError::DuplicateWindowId { id: WindowId(0) }
        );
        assert_eq!(
            snapshot(
                NonEmpty::singleton(w),
                NonEmpty::new(main.clone(), vec![main]),
                vec![]
            )
            .unwrap_err(),
            SnapshotError::DuplicateSessionName {
                name: SessionName::parse("main").unwrap()
            }
        );
    }

    #[test]
    fn degradations_list_every_absence_with_its_reason_and_each_pane_once() {
        let degraded = Pane {
            id: PaneId(5),
            cwd: Cwd::Unreadable,
            foreground: Foreground::Unrecovered {
                reason: RecoveryFailure::LeaderGone,
            },
            content: Content::NotCaptured {
                reason: ContentFailure::NotRecorded,
            },
            ..pane(0)
        };
        let shared = window(0, NonEmpty::singleton(degraded), 0).unwrap();
        let degraded = snapshot(
            NonEmpty::singleton(shared),
            NonEmpty::new(
                session("a", NonEmpty::singleton(link(0, 0))),
                vec![session("b", NonEmpty::singleton(link(0, 0)))],
            ),
            vec![],
        )
        .unwrap();
        assert_eq!(
            degraded.degradations(),
            vec![
                Degradation::Cwd { pane: PaneId(5) },
                Degradation::Foreground {
                    pane: PaneId(5),
                    reason: RecoveryFailure::LeaderGone
                },
                Degradation::Content {
                    pane: PaneId(5),
                    reason: ContentFailure::NotRecorded
                },
            ]
        );

        let clean = snapshot(
            NonEmpty::singleton(window(0, NonEmpty::singleton(pane(0)), 0).unwrap()),
            NonEmpty::singleton(session("a", NonEmpty::singleton(link(0, 0)))),
            vec![],
        )
        .unwrap();
        assert!(clean.degradations().is_empty());
    }

    #[test]
    fn one_window_with_one_pane_idle_at_its_shell_is_a_bootstrap_session() {
        let snapshot = snapshot(
            NonEmpty::singleton(window(0, NonEmpty::singleton(pane(0)), 0).unwrap()),
            NonEmpty::singleton(session("0", NonEmpty::singleton(link(0, 0)))),
            vec![],
        )
        .unwrap();
        assert!(snapshot.is_bootstrap_only());
    }

    #[test]
    fn a_session_with_anything_built_or_unrecovered_in_it_is_not_bootstrap() {
        let second_pane = Pane {
            id: PaneId(1),
            index: PaneIndex(1),
            ..pane(0)
        };
        let cases: Vec<(&str, NonEmpty<Window>, NonEmpty<WinLink>)> = vec![
            (
                "two panes",
                NonEmpty::singleton(
                    window(0, NonEmpty::new(pane(0), vec![second_pane]), 0).unwrap(),
                ),
                NonEmpty::singleton(link(0, 0)),
            ),
            (
                "two windows",
                NonEmpty::new(
                    window(0, NonEmpty::singleton(pane(0)), 0).unwrap(),
                    vec![window(1, NonEmpty::singleton(pane(0)), 0).unwrap()],
                ),
                NonEmpty::new(link(0, 0), vec![link(1, 1)]),
            ),
            (
                "a running program",
                NonEmpty::singleton(
                    window(0, NonEmpty::singleton(running(&["vim", "notes.md"])), 0).unwrap(),
                ),
                NonEmpty::singleton(link(0, 0)),
            ),
            (
                "an unrecovered foreground",
                NonEmpty::singleton(
                    window(
                        0,
                        NonEmpty::singleton(Pane {
                            foreground: Foreground::Unrecovered {
                                reason: RecoveryFailure::ShellGone,
                            },
                            ..pane(0)
                        }),
                        0,
                    )
                    .unwrap(),
                ),
                NonEmpty::singleton(link(0, 0)),
            ),
        ];
        for (what, windows, links) in cases {
            let built =
                snapshot(windows, NonEmpty::singleton(session("0", links)), vec![]).unwrap();
            assert!(!built.is_bootstrap_only(), "{what}");
        }
    }
}
