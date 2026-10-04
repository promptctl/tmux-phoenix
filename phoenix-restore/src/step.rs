//! The plan's vocabulary (ARCHITECTURE.md §8). A step names the panes and
//! windows it acts on by *reference* — [`PaneRef`], [`WindowRef`] — and the
//! step that creates one says which reference it defines. `apply` binds each
//! reference to the id tmux reports when that step runs, so no step depends
//! on what tmux considers "current" (`[LAW:no-ambient-temporal-coupling]`),
//! and a dry run can print the whole plan before any id exists.

use std::fmt;

use phoenix_core::{Layout, NonEmpty, Utf8PathBuf, WindowId, WindowIndex, WindowName};
use tmux_control::SessionName;

/// A window this plan creates, named before tmux has given it an id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowRef(pub(crate) u32);

/// A pane this plan creates, named before tmux has given it an id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneRef(pub(crate) u32);

impl fmt::Display for WindowRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "w{}", self.0)
    }
}

impl fmt::Display for PaneRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "p{}", self.0)
    }
}

/// The window a [`Step::LinkWindow`] shares: one this plan built, or one the
/// server already held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSource {
    Built(WindowRef),
    Live(WindowId),
}

impl fmt::Display for LinkSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkSource::Built(window) => write!(f, "{window}"),
            LinkSource::Live(window) => write!(f, "{window}"),
        }
    }
}

/// Where a [`Step::SetOption`] writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionScope {
    Server,
    Session(SessionName),
    Window(WindowRef),
}

impl fmt::Display for OptionScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OptionScope::Server => f.write_str("server"),
            OptionScope::Session(session) => write!(f, "session {session}"),
            OptionScope::Window(window) => write!(f, "{window}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// A session, which tmux always creates with one window holding one
    /// pane. `window_name` is the saved window's when that window is one the
    /// snapshot holds, and absent when it exists only so the session can.
    CreateSession {
        name: SessionName,
        window_name: Option<WindowName>,
        /// Absent when capture could not read the pane's cwd: the pane
        /// starts wherever tmux's own default puts it.
        cwd: Option<Utf8PathBuf>,
        window: WindowRef,
        pane: PaneRef,
    },
    /// Puts the one window of a session just created at `to`:
    /// `new-session` cannot be told an index, so the window lands wherever
    /// the server's `base-index` says.
    MoveWindow {
        window: WindowRef,
        session: SessionName,
        to: WindowIndex,
    },
    NewWindow {
        session: SessionName,
        index: WindowIndex,
        name: WindowName,
        cwd: Option<Utf8PathBuf>,
        window: WindowRef,
        pane: PaneRef,
    },
    /// Shares an existing window into another session, so a window several
    /// sessions link is built once. `replacing` is the window a session was
    /// made with only so it could exist, sitting at `index`: the link takes
    /// its place and tmux removes it, in one command, so no index shifts on
    /// a server that renumbers windows when one closes. A link that replaces
    /// differs from one that does not by this value alone
    /// (`[LAW:one-type-per-behavior]`).
    LinkWindow {
        source: LinkSource,
        into: SessionName,
        index: WindowIndex,
        replacing: Option<WindowRef>,
    },
    /// The new pane follows `from` in the window's pane order.
    SplitPane {
        from: PaneRef,
        cwd: Option<Utf8PathBuf>,
        pane: PaneRef,
    },
    /// The saved layout string, applied once every pane of the window
    /// exists: given the same pane count it reproduces the saved geometry.
    SelectLayout {
        window: WindowRef,
        layout: Layout,
    },
    /// The pane's saved scrollback, printed into it so it reads as it did.
    ReplayContent {
        pane: PaneRef,
        lines: Vec<String>,
    },
    /// The pane's saved program, typed into its shell with its exact argv.
    Relaunch {
        pane: PaneRef,
        argv: NonEmpty<String>,
    },
    SelectPane {
        pane: PaneRef,
    },
    SelectWindow {
        session: SessionName,
        index: WindowIndex,
    },
    SetOption {
        scope: OptionScope,
        key: &'static str,
        value: String,
    },
    /// Moves every client attached to `from` onto `to`. Which clients those
    /// are is read when the step runs: a terminal can attach while the plan
    /// applies, and a client whose session is killed is detached.
    SwitchClients {
        from: SessionName,
        to: SessionName,
    },
    KillSession {
        name: SessionName,
    },
}

/// POSIX-shell single-quoting: what tmux types into a pane is parsed by the
/// pane's shell, not by tmux.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The line a [`Step::Relaunch`] types: each saved word quoted, so it reaches
/// the program as the one argument it was. The one rendering, shared by the
/// dry run and `apply` (`[LAW:one-source-of-truth]`).
pub(crate) fn relaunch_line(argv: &NonEmpty<String>) -> String {
    argv.iter()
        .map(|word| shell_quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// ` -c <cwd>` when there is one.
struct InCwd<'a>(&'a Option<Utf8PathBuf>);

impl fmt::Display for InCwd<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
            .iter()
            .try_for_each(|cwd| write!(f, " -c {}", cwd.as_str()))
    }
}

/// What `--dry-run` prints: the step in tmux's own words, with references
/// where the ids will be.
impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::CreateSession {
                name,
                window_name,
                cwd,
                window,
                pane,
            } => {
                write!(f, "{window} {pane} = new-session -s {name}")?;
                window_name
                    .iter()
                    .try_for_each(|window_name| write!(f, " -n {window_name}"))?;
                write!(f, "{}", InCwd(cwd))
            }
            Step::MoveWindow {
                window,
                session,
                to,
            } => write!(f, "move-window {window} to {session}:{}", to.0),
            Step::NewWindow {
                session,
                index,
                name,
                cwd,
                window,
                pane,
            } => write!(
                f,
                "{window} {pane} = new-window {session}:{} -n {name}{}",
                index.0,
                InCwd(cwd)
            ),
            Step::LinkWindow {
                source,
                into,
                index,
                replacing,
            } => {
                write!(f, "link-window {source} into {into}:{}", index.0)?;
                replacing
                    .iter()
                    .try_for_each(|window| write!(f, " replacing {window}"))
            }
            Step::SplitPane { from, cwd, pane } => {
                write!(f, "{pane} = split-window {from}{}", InCwd(cwd))
            }
            Step::SelectLayout { window, layout } => write!(f, "select-layout {window} {layout}"),
            Step::ReplayContent { pane, lines } => {
                write!(
                    f,
                    "replay {} line(s) of saved content into {pane}",
                    lines.len()
                )
            }
            Step::Relaunch { pane, argv } => write!(f, "send-keys {pane} {}", relaunch_line(argv)),
            Step::SelectPane { pane } => write!(f, "select-pane {pane}"),
            Step::SelectWindow { session, index } => {
                write!(f, "select-window {session}:{}", index.0)
            }
            Step::SetOption { scope, key, value } => write!(f, "set-option {scope} {key} {value}"),
            Step::SwitchClients { from, to } => {
                write!(f, "switch-client every client on {from} to {to}")
            }
            Step::KillSession { name } => write!(f, "kill-session {name}"),
        }
    }
}
