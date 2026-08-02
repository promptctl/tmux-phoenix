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
use crate::ids::{Layout, PaneIndex, SessionName, WindowIndex, WindowName};
use crate::nonempty::NonEmpty;
use crate::path::Utf8PathBuf;
use crate::program::CapturedProgram;
use crate::time::OffsetDateTime;
use crate::version::{FormatVersion, TmuxVersion};

/// A validated snapshot tree failed to construct: an `active` index didn't
/// resolve to any member of its siblings.
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
        }
    }
}

impl std::error::Error for SnapshotError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub index: PaneIndex,
    pub cwd: Utf8PathBuf,
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
    /// Fails with [`SnapshotError::ActivePaneNotFound`] unless `active`
    /// matches some pane in `panes` — the only way to build a `Window` is
    /// one where the invariant already holds.
    pub fn new(
        index: WindowIndex,
        name: WindowName,
        layout: Layout,
        panes: NonEmpty<Pane>,
        active: PaneIndex,
    ) -> Result<Self, SnapshotError> {
        if panes.iter().any(|p| p.index == active) {
            Ok(Self {
                index,
                name,
                layout,
                panes,
                active,
            })
        } else {
            Err(SnapshotError::ActivePaneNotFound {
                window: index,
                active,
            })
        }
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
    /// Fails with [`SnapshotError::ActiveWindowNotFound`] unless `active`
    /// matches some window in `windows`.
    pub fn new(
        name: SessionName,
        windows: NonEmpty<Window>,
        active: WindowIndex,
    ) -> Result<Self, SnapshotError> {
        if windows.iter().any(|w| w.index() == active) {
            Ok(Self {
                name,
                windows,
                active,
            })
        } else {
            Err(SnapshotError::ActiveWindowNotFound {
                session: name,
                active,
            })
        }
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub format_version: FormatVersion,
    pub tmux_version: TmuxVersion,
    pub captured_at: OffsetDateTime,
    pub sessions: NonEmpty<Session>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(index: u32) -> Pane {
        Pane {
            index: PaneIndex(index),
            cwd: Utf8PathBuf::new("/home/user"),
            program: CapturedProgram::new("zsh", vec![]),
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
