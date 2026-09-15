//! The domain tree itself (DESIGN.md §4): `Snapshot -> Session -> Window ->
//! Pane`. `Session`/`Window` keep their `active` field private behind a
//! validating constructor — the invariant is "a single index that must
//! resolve to a member," not a per-child `bool` that could encode
//! two-active-or-none (`[LAW:one-source-of-truth]`). Public fields would let
//! a caller swap in a bad index after construction and silently break that
//! invariant, so encapsulation here isn't style — it's what makes the
//! illegal state actually unrepresentable, not just discouraged.
//!
//! `Pane` and `Snapshot` have no cross-field invariant to protect (their
//! non-emptiness is already guaranteed by [`NonEmpty`] at the type level), so
//! their fields stay public.

use std::fmt;

use crate::content::PaneContent;
use crate::ids::{Layout, PaneId, PaneIndex, SessionName, WindowIndex, WindowName};
use crate::nonempty::NonEmpty;
use crate::path::Utf8PathBuf;
use crate::program::{CapturedProgram, Foreground};
use crate::time::OffsetDateTime;
use crate::version::{FormatVersion, TmuxVersion};

/// A validated snapshot tree failed to construct: an `active` index didn't
/// resolve to exactly one member of its siblings — either none carries it,
/// or two siblings share an index, which would make "the active one"
/// ambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    ActiveWindowNotFound {
        session: SessionName,
        active: WindowIndex,
    },
    ActivePaneNotFound {
        window: WindowIndex,
        active: PaneIndex,
    },
    DuplicateWindowIndex {
        session: SessionName,
        index: WindowIndex,
    },
    DuplicatePaneIndex {
        window: WindowIndex,
        index: PaneIndex,
    },
}

/// The first index that appears twice, if any. Indices are small and few, so
/// the quadratic scan costs nothing and needs no allocation.
fn duplicate<I: Copy + Eq>(indices: impl Iterator<Item = I> + Clone) -> Option<I> {
    indices
        .clone()
        .enumerate()
        .find(|(i, a)| indices.clone().take(*i).any(|b| b == *a))
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
            SnapshotError::ActivePaneNotFound { window, active } => write!(
                f,
                "window {} has no pane at active index {}",
                window.0, active.0
            ),
            SnapshotError::DuplicateWindowIndex { session, index } => {
                write!(f, "session {session} has two windows at index {}", index.0)
            }
            SnapshotError::DuplicatePaneIndex { window, index } => {
                write!(f, "window {} has two panes at index {}", window.0, index.0)
            }
        }
    }
}

impl std::error::Error for SnapshotError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub id: PaneId,
    pub index: PaneIndex,
    /// `None` when tmux could not read the foreground process's cwd.
    pub cwd: Option<Utf8PathBuf>,
    pub program: CapturedProgram,
    pub content: Option<PaneContent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    index: WindowIndex,
    name: WindowName,
    layout: Layout,
    panes: NonEmpty<Pane>,
    active: PaneIndex,
}

impl Window {
    /// Fails unless `active` matches exactly one pane in `panes`: no pane
    /// ([`SnapshotError::ActivePaneNotFound`]) or two panes sharing an index
    /// ([`SnapshotError::DuplicatePaneIndex`]) are both refused, so the only
    /// way to build a `Window` is one where the invariant already holds.
    pub fn new(
        index: WindowIndex,
        name: WindowName,
        layout: Layout,
        panes: NonEmpty<Pane>,
        active: PaneIndex,
    ) -> Result<Self, SnapshotError> {
        if let Some(dup) = duplicate(panes.iter().map(|p| p.index)) {
            return Err(SnapshotError::DuplicatePaneIndex {
                window: index,
                index: dup,
            });
        }
        if !panes.iter().any(|p| p.index == active) {
            return Err(SnapshotError::ActivePaneNotFound {
                window: index,
                active,
            });
        }
        Ok(Self {
            index,
            name,
            layout,
            panes,
            active,
        })
    }

    pub fn index(&self) -> WindowIndex {
        self.index
    }

    pub fn name(&self) -> &WindowName {
        &self.name
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    name: SessionName,
    windows: NonEmpty<Window>,
    active: WindowIndex,
}

impl Session {
    /// Fails unless `active` matches exactly one window in `windows`; see
    /// [`Window::new`] for the two refusals.
    pub fn new(
        name: SessionName,
        windows: NonEmpty<Window>,
        active: WindowIndex,
    ) -> Result<Self, SnapshotError> {
        if let Some(dup) = duplicate(windows.iter().map(|w| w.index())) {
            return Err(SnapshotError::DuplicateWindowIndex {
                session: name,
                index: dup,
            });
        }
        if !windows.iter().any(|w| w.index() == active) {
            return Err(SnapshotError::ActiveWindowNotFound {
                session: name,
                active,
            });
        }
        Ok(Self {
            name,
            windows,
            active,
        })
    }

    pub fn name(&self) -> &SessionName {
        &self.name
    }

    pub fn windows(&self) -> &NonEmpty<Window> {
        &self.windows
    }

    pub fn active(&self) -> WindowIndex {
        self.active
    }

    /// The window `active` points at. Always succeeds — `Session::new`
    /// already proved it resolves.
    pub fn active_window(&self) -> &Window {
        self.windows
            .iter()
            .find(|w| w.index() == self.active)
            .expect("Session::new validated that `active` resolves to a member")
    }

    /// A session nobody has built anything in yet: one window holding one
    /// pane idle at its shell — what a terminal that starts `tmux` at login
    /// creates. Restore replaces a server made only of these, and the store
    /// refuses to let a capture made only of these become `latest`.
    pub fn is_bootstrap(&self) -> bool {
        let panes = self.windows.first().panes();
        self.windows.len() == 1
            && panes.len() == 1
            && panes.first().program.foreground() == Foreground::IdleShell
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub format_version: FormatVersion,
    pub tmux_version: TmuxVersion,
    pub captured_at: OffsetDateTime,
    pub sessions: NonEmpty<Session>,
}

impl Snapshot {
    /// Every session is a bootstrap session ([`Session::is_bootstrap`]).
    pub fn is_bootstrap_only(&self) -> bool {
        self.sessions.iter().all(Session::is_bootstrap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ProgramName;

    fn pane(index: u32) -> Pane {
        Pane {
            id: PaneId(index),
            index: PaneIndex(index),
            cwd: Utf8PathBuf::parse("/home/user"),
            program: CapturedProgram {
                command: ProgramName::parse("zsh").unwrap(),
                argv: None,
            },
            content: None,
        }
    }

    fn window(index: u32, panes: NonEmpty<Pane>, active: u32) -> Result<Window, SnapshotError> {
        Window::new(
            WindowIndex(index),
            WindowName::parse("shell").unwrap(),
            Layout::parse("a1b2,80x24,0,0,0").unwrap(),
            panes,
            PaneIndex(active),
        )
    }

    fn running(command: &str, argv: &[&str]) -> Pane {
        Pane {
            program: CapturedProgram {
                command: ProgramName::parse(command).unwrap(),
                argv: NonEmpty::from_vec(argv.iter().map(|a| a.to_string()).collect()),
            },
            ..pane(0)
        }
    }

    fn session_of(name: &str, windows: NonEmpty<Window>) -> Session {
        let active = windows.first().index();
        Session::new(SessionName::parse(name).unwrap(), windows, active).unwrap()
    }

    fn one_window(panes: NonEmpty<Pane>) -> NonEmpty<Window> {
        NonEmpty::singleton(window(0, panes, 0).unwrap())
    }

    fn idle_shell() -> Pane {
        running("zsh", &["-zsh"])
    }

    #[test]
    fn one_window_with_one_pane_idle_at_its_shell_is_a_bootstrap_session() {
        let session = session_of("0", one_window(NonEmpty::singleton(idle_shell())));
        assert!(session.is_bootstrap());
    }

    #[test]
    fn a_session_with_anything_built_or_unknown_in_it_is_not_bootstrap() {
        let second_pane = Pane {
            id: PaneId(1),
            index: PaneIndex(1),
            ..idle_shell()
        };
        let second_window = window(1, NonEmpty::singleton(idle_shell()), 0).unwrap();
        for (what, session) in [
            (
                "two panes",
                session_of(
                    "0",
                    one_window(NonEmpty::new(idle_shell(), vec![second_pane])),
                ),
            ),
            (
                "two windows",
                session_of(
                    "0",
                    NonEmpty::new(
                        window(0, NonEmpty::singleton(idle_shell()), 0).unwrap(),
                        vec![second_window],
                    ),
                ),
            ),
            (
                "a running program",
                session_of(
                    "0",
                    one_window(NonEmpty::singleton(running("vim", &["vim", "notes.md"]))),
                ),
            ),
            (
                "an unrecovered argv",
                session_of("0", one_window(NonEmpty::singleton(pane(0)))),
            ),
        ] {
            assert!(!session.is_bootstrap(), "{what}");
        }
    }

    #[test]
    fn a_snapshot_is_bootstrap_only_exactly_when_every_session_is() {
        let snapshot = |sessions: NonEmpty<Session>| Snapshot {
            format_version: FormatVersion::CURRENT,
            tmux_version: TmuxVersion { major: 3, minor: 6 },
            captured_at: OffsetDateTime::from_unix_timestamp(1_700_000_000),
            sessions,
        };
        let idle = |name: &str| session_of(name, one_window(NonEmpty::singleton(idle_shell())));
        let built = session_of(
            "work",
            one_window(NonEmpty::singleton(running("vim", &["vim"]))),
        );

        assert!(snapshot(NonEmpty::new(idle("0"), vec![idle("1")])).is_bootstrap_only());
        assert!(!snapshot(NonEmpty::new(idle("0"), vec![built])).is_bootstrap_only());
    }

    #[test]
    fn window_new_succeeds_when_active_resolves() {
        let panes = NonEmpty::from_vec(vec![pane(0), pane(1)]).unwrap();
        assert!(window(0, panes, 1).is_ok());
    }

    #[test]
    fn window_new_fails_when_active_does_not_resolve() {
        let panes = NonEmpty::from_vec(vec![pane(0), pane(1)]).unwrap();
        let err = window(0, panes, 99).unwrap_err();
        assert_eq!(
            err,
            SnapshotError::ActivePaneNotFound {
                window: WindowIndex(0),
                active: PaneIndex(99),
            }
        );
    }

    #[test]
    fn window_new_fails_when_two_panes_share_an_index() {
        let panes = NonEmpty::from_vec(vec![pane(0), pane(1), pane(1)]).unwrap();
        let err = window(0, panes, 1).unwrap_err();
        assert_eq!(
            err,
            SnapshotError::DuplicatePaneIndex {
                window: WindowIndex(0),
                index: PaneIndex(1),
            }
        );
    }

    #[test]
    fn session_new_fails_when_two_windows_share_an_index() {
        let panes = NonEmpty::singleton(pane(0));
        let w0 = window(3, panes.clone(), 0).unwrap();
        let w1 = window(3, panes, 0).unwrap();
        let err = Session::new(
            SessionName::parse("main").unwrap(),
            NonEmpty::new(w0, vec![w1]),
            WindowIndex(3),
        )
        .unwrap_err();
        assert_eq!(
            err,
            SnapshotError::DuplicateWindowIndex {
                session: SessionName::parse("main").unwrap(),
                index: WindowIndex(3),
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
    fn session_new_succeeds_when_active_resolves() {
        let panes = NonEmpty::singleton(pane(0));
        let w0 = window(0, panes.clone(), 0).unwrap();
        let w1 = window(1, panes, 0).unwrap();
        let windows = NonEmpty::new(w0, vec![w1]);
        let session = Session::new(SessionName::parse("main").unwrap(), windows, WindowIndex(1));
        assert!(session.is_ok());
    }

    #[test]
    fn session_new_fails_when_active_does_not_resolve() {
        let panes = NonEmpty::singleton(pane(0));
        let w0 = window(0, panes, 0).unwrap();
        let windows = NonEmpty::singleton(w0);
        let err =
            Session::new(SessionName::parse("main").unwrap(), windows, WindowIndex(5)).unwrap_err();
        assert_eq!(
            err,
            SnapshotError::ActiveWindowNotFound {
                session: SessionName::parse("main").unwrap(),
                active: WindowIndex(5),
            }
        );
    }

    #[test]
    fn session_active_window_returns_the_matching_window() {
        let panes = NonEmpty::singleton(pane(0));
        let w0 = window(0, panes.clone(), 0).unwrap();
        let w1 = window(1, panes, 0).unwrap();
        let windows = NonEmpty::new(w0, vec![w1]);
        let session =
            Session::new(SessionName::parse("main").unwrap(), windows, WindowIndex(1)).unwrap();
        assert_eq!(session.active_window().index(), WindowIndex(1));
    }

    #[test]
    fn full_tree_round_trips_through_accessors() {
        let panes = NonEmpty::from_vec(vec![pane(0), pane(1)]).unwrap();
        let win = window(0, panes, 1).unwrap();
        let session = Session::new(
            SessionName::parse("main").unwrap(),
            NonEmpty::singleton(win),
            WindowIndex(0),
        )
        .unwrap();

        let snapshot = Snapshot {
            format_version: FormatVersion::CURRENT,
            tmux_version: TmuxVersion { major: 3, minor: 5 },
            captured_at: OffsetDateTime::from_unix_timestamp(1_700_000_000),
            sessions: NonEmpty::singleton(session),
        };

        assert_eq!(snapshot.sessions.len(), 1);
        let session = snapshot.sessions.first();
        assert_eq!(session.name().as_str(), "main");
        let active_window = session.active_window();
        assert_eq!(active_window.panes().len(), 2);
        assert_eq!(active_window.active_pane().index, PaneIndex(1));
    }
}
