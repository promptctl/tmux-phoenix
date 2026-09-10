//! Free functions over [`Client::execute`] (DESIGN.md §3.3):
//! `[LAW:single-enforcer]` — these are thin [`CommandLine`] builders, not an
//! alternate dispatch path. Scoped to what ticket `tmux-control-mode-1ju.6`
//! asks for: subscriptions (SPEC §14), pane flow control (SPEC §13), client
//! flags (SPEC §9), and version gating (IMPL.md §2.2). `list-panes`,
//! `send-keys`, and friends are a later ticket's job.

use crate::client::{Client, CommandOutput, TmuxError};
use crate::protocol::{CommandLine, PaneId};
use crate::transport::Transport;
use crate::version::{parse_tmux_version, TmuxVersion};

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

/// The flag DESIGN.md §3.4's efficiency thesis is built on: suppresses all
/// pane output notifications so `%output` never needs to be read, decoded,
/// or discarded.
pub const NO_OUTPUT_FLAG: &str = "no-output";

/// `refresh-client -B <name>:<what>:<format>` (SPEC §14): subscribe to
/// changes in a tmux format string, reported via `%subscription-changed`.
/// `what` selects scope — empty for the attached session, `%<pane-id>` /
/// `%*` for a pane / all panes, `@<window-id>` / `@*` for a window / all
/// windows (SPEC §14's table).
pub fn subscribe<T: Transport>(
    client: &mut Client<T>,
    name: &str,
    what: &str,
    format: &str,
) -> Result<CommandOutput, TmuxError> {
    // tmux takes `name:what:format` as one argument and splits it itself.
    let target = format!("{name}:{what}:{format}");
    client.execute(&CommandLine::new(
        "refresh-client",
        ["-B", target.as_str()],
    )?)
}

/// `refresh-client -B <name>` (SPEC §14, name-only form): remove a
/// subscription.
pub fn unsubscribe<T: Transport>(
    client: &mut Client<T>,
    name: &str,
) -> Result<CommandOutput, TmuxError> {
    client.execute(&CommandLine::new("refresh-client", ["-B", name])?)
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
    flags: &[&str],
) -> Result<CommandOutput, TmuxError> {
    client.execute(&CommandLine::new(
        "refresh-client",
        ["-f", flags.join(",").as_str()],
    )?)
}

/// `refresh-client -f !<flags>` (SPEC §9): `!`-prefixing a flag clears it.
pub fn clear_flags<T: Transport>(
    client: &mut Client<T>,
    flags: &[&str],
) -> Result<CommandOutput, TmuxError> {
    let negated: Vec<String> = flags.iter().map(|f| format!("!{f}")).collect();
    client.execute(&CommandLine::new(
        "refresh-client",
        ["-f", negated.join(",").as_str()],
    )?)
}

/// Convenience wrapper for `set_flags(client, &[NO_OUTPUT_FLAG])` — DESIGN.md
/// §3.4's efficiency thesis names this the one flag phoenix always sets.
pub fn set_no_output<T: Transport>(client: &mut Client<T>) -> Result<CommandOutput, TmuxError> {
    set_flags(client, &[NO_OUTPUT_FLAG])
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
