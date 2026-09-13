//! Free functions over [`Client::execute`] (DESIGN.md §3.3):
//! `[LAW:single-enforcer]` — these are thin [`CommandLine`] builders, not an
//! alternate dispatch path. Scoped to what ticket `tmux-control-mode-1ju.6`
//! asks for: subscriptions (SPEC §14), pane flow control (SPEC §13), client
//! flags (SPEC §9), and version gating (IMPL.md §2.2). `list-panes`,
//! `send-keys`, and friends are a later ticket's job.

use crate::client::{Client, CommandOutput, TmuxError};
use crate::protocol::{CommandLine, PaneId, WindowId};
use crate::transport::Transport;
use crate::version::{parse_tmux_version, TmuxVersion};
use std::fmt;

/// `refresh-client -A <pane>:<action>` actions (SPEC §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneAction {
    On,
    Off,
    Pause,
    Continue,
}

impl PaneAction {
    fn as_str(self) -> &'static str {
        match self {
            PaneAction::On => "on",
            PaneAction::Off => "off",
            PaneAction::Pause => "pause",
            PaneAction::Continue => "continue",
        }
    }
}

/// The client flags `refresh-client -f` accepts (SPEC §9; tmux documents
/// the vocabulary under `attach-session`). A closed set rather than
/// strings, because `-f` takes them comma-joined and tmux splits on those
/// commas itself: a `&str` flag could carry a comma and silently become
/// two flags, or carry a leading `!` and clear what the caller meant to
/// set (`[LAW:types-are-the-program]` — the enum makes both unsayable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientFlag {
    /// The client has an independent active pane.
    ActivePane,
    /// The client does not affect the size of other clients.
    IgnoreSize,
    /// Do not detach when the attached session is destroyed, if other
    /// sessions remain.
    NoDetachOnDestroy,
    /// Suppresses all pane output notifications, so `%output` never needs
    /// to be read, decoded, or discarded — the flag DESIGN.md §3.4's
    /// efficiency thesis is built on.
    NoOutput,
    /// Pause output once the pane is `seconds` behind in control mode.
    PauseAfter { seconds: u32 },
    /// Only keys bound to `detach-client`/`switch-client` have any effect.
    ReadOnly,
    /// Wait for an empty input line before exiting in control mode.
    WaitExit,
}

impl ClientFlag {
    /// The flag as tmux spells it. `pause-after` is the one that carries a
    /// value, which is why this returns an owned `String` rather than
    /// `&'static str`.
    fn to_field(self) -> String {
        match self {
            ClientFlag::ActivePane => "active-pane".to_owned(),
            ClientFlag::IgnoreSize => "ignore-size".to_owned(),
            ClientFlag::NoDetachOnDestroy => "no-detach-on-destroy".to_owned(),
            ClientFlag::NoOutput => "no-output".to_owned(),
            ClientFlag::PauseAfter { seconds } => format!("pause-after={seconds}"),
            ClientFlag::ReadOnly => "read-only".to_owned(),
            ClientFlag::WaitExit => "wait-exit".to_owned(),
        }
    }
}

/// A subscription name that cannot shift tmux's field boundaries: the
/// first colon-delimited field of `refresh-client -B` (SPEC §14).
///
/// Parsed once, here, so nothing downstream re-checks it
/// (`[LAW:parse-dont-validate]`). A colon in this field is not a malformed
/// string tmux would reject — it is a *different command*: tmux splits the
/// `-B` argument on its first two colons, and dispatches on whether the
/// argument contains one at all. So `"phase:1"` passed to `unsubscribe`
/// would silently subscribe a subscription named `phase`, and passed to
/// `subscribe` would shift `what` and `format` one field left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionName(String);

/// A subscription name held a `:`, so no `refresh-client -B` argument
/// exists that means what the caller asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColonInSubscriptionName {
    pub name: String,
}

impl fmt::Display for ColonInSubscriptionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "subscription name {:?} contains a ':', which tmux would read as a field separator",
            self.name
        )
    }
}

impl std::error::Error for ColonInSubscriptionName {}

impl SubscriptionName {
    pub fn new(name: impl Into<String>) -> Result<Self, ColonInSubscriptionName> {
        let name = name.into();
        if name.contains(':') {
            return Err(ColonInSubscriptionName { name });
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The `what` field of `refresh-client -B` (SPEC §14): which items the
/// subscription's format is evaluated against. tmux documents exactly
/// these five shapes, so they are an enum and not a string — the `%`/`@`
/// sigils stay where the rest of this crate keeps them, at the boundary
/// (`[LAW:one-source-of-truth]` with [`crate::PaneId`]/[`crate::WindowId`]),
/// and no caller can put a colon in this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionScope {
    /// Evaluate the format against the attached session (tmux's empty
    /// `what`).
    AttachedSession,
    /// One pane, by id.
    Pane(PaneId),
    /// Every pane in the attached session.
    AllPanes,
    /// One window, by id.
    Window(WindowId),
    /// Every window in the attached session.
    AllWindows,
}

impl SubscriptionScope {
    fn to_field(self) -> String {
        match self {
            SubscriptionScope::AttachedSession => String::new(),
            SubscriptionScope::Pane(pane) => format!("%{}", pane.0),
            SubscriptionScope::AllPanes => "%*".to_owned(),
            SubscriptionScope::Window(window) => format!("@{}", window.0),
            SubscriptionScope::AllWindows => "@*".to_owned(),
        }
    }
}

/// `refresh-client -B <name>:<what>:<format>` (SPEC §14): subscribe to
/// changes in a tmux format string, reported via `%subscription-changed`.
/// `what` selects scope — empty for the attached session, `%<pane-id>` /
/// `%*` for a pane / all panes, `@<window-id>` / `@*` for a window / all
/// windows (SPEC §14's table).
pub fn subscribe<T: Transport>(
    client: &mut Client<T>,
    name: &SubscriptionName,
    scope: SubscriptionScope,
    format: &str,
) -> Result<CommandOutput, TmuxError> {
    // tmux takes `name:what:format` as one argument and splits it on the
    // first two colons only, so `format` may hold colons freely — tmux 3.6a
    // reports `pre:probe:post` back intact for `nm::pre:#{session_name}:post`
    // — while `name` and `scope` are the fields that must not, and are
    // typed so they cannot.
    let target = format!("{}:{}:{}", name.as_str(), scope.to_field(), format);
    client.execute(&CommandLine::new(
        "refresh-client",
        ["-B", target.as_str()],
    )?)
}

/// `refresh-client -B <name>` (SPEC §14, name-only form): remove a
/// subscription. tmux decides remove-versus-subscribe by whether the
/// argument contains a colon, which is why the name is a
/// [`SubscriptionName`] and not a `&str`.
pub fn unsubscribe<T: Transport>(
    client: &mut Client<T>,
    name: &SubscriptionName,
) -> Result<CommandOutput, TmuxError> {
    client.execute(&CommandLine::new("refresh-client", ["-B", name.as_str()])?)
}

/// `refresh-client -A <pane>:<action>` (SPEC §13).
pub fn set_pane_action<T: Transport>(
    client: &mut Client<T>,
    pane: PaneId,
    action: PaneAction,
) -> Result<CommandOutput, TmuxError> {
    // `pane:action` is one argument, which tmux splits itself.
    let target = format!("%{}:{}", pane.0, action.as_str());
    client.execute(&CommandLine::new(
        "refresh-client",
        ["-A", target.as_str()],
    )?)
}

/// `refresh-client -f <flags>` (SPEC §9), comma-joined.
pub fn set_flags<T: Transport>(
    client: &mut Client<T>,
    flags: &[ClientFlag],
) -> Result<CommandOutput, TmuxError> {
    client.execute(&CommandLine::new(
        "refresh-client",
        ["-f", join_flags(flags, "").as_str()],
    )?)
}

/// `refresh-client -f !<flags>` (SPEC §9): `!`-prefixing a flag clears it.
pub fn clear_flags<T: Transport>(
    client: &mut Client<T>,
    flags: &[ClientFlag],
) -> Result<CommandOutput, TmuxError> {
    client.execute(&CommandLine::new(
        "refresh-client",
        ["-f", join_flags(flags, "!").as_str()],
    )?)
}

/// The one place `-f`'s argument is built (`[LAW:one-source-of-truth]`):
/// set and clear differ only by the prefix each flag carries, so that is
/// the parameter rather than a second join.
fn join_flags(flags: &[ClientFlag], prefix: &str) -> String {
    flags
        .iter()
        .map(|flag| format!("{prefix}{}", flag.to_field()))
        .collect::<Vec<_>>()
        .join(",")
}

/// Convenience wrapper for `set_flags(client, &[ClientFlag::NoOutput])` —
/// DESIGN.md §3.4's efficiency thesis names this the one flag phoenix
/// always sets.
pub fn set_no_output<T: Transport>(client: &mut Client<T>) -> Result<CommandOutput, TmuxError> {
    set_flags(client, &[ClientFlag::NoOutput])
}

/// Probe the connected tmux server's version over the live control-mode
/// connection (`display-message -p "#{version}"`) — works over any
/// [`Transport`], since the probe travels the protocol channel to the real
/// server rather than spawning a local `tmux -V` (IMPL.md §2.2).
pub fn query_tmux_version<T: Transport>(client: &mut Client<T>) -> Result<TmuxVersion, TmuxError> {
    let response = client.execute(&CommandLine::new("display-message", ["-p", "#{version}"])?)?;
    response
        .lines
        .iter()
        .find_map(|line| parse_tmux_version(&String::from_utf8_lossy(line)))
        .ok_or(TmuxError::VersionProbeFailed {
            output: response.lines,
        })
}

/// Gate an operation named `operation` behind `required`, given the
/// server's already-known `have` version — a typed
/// [`TmuxError::UnsupportedTmuxVersion`] instead of letting tmux's raw
/// `%error` for an unrecognized flag leak through (IMPL.md §2.2's
/// `[LAW:no-silent-failure]` stance). None of this ticket's own operations
/// need a floor above the crate's own [`crate::version::MIN_TMUX_VERSION`]
/// — this is the reusable primitive a future higher-floor command gates on.
pub fn require_version(
    operation: &str,
    have: TmuxVersion,
    required: TmuxVersion,
) -> Result<(), TmuxError> {
    if have >= required {
        Ok(())
    } else {
        Err(TmuxError::UnsupportedTmuxVersion {
            operation: operation.to_string(),
            required,
            have,
        })
    }
}
