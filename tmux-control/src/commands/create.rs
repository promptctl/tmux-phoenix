//! The commands that create a pane and report what they made: `new-session`,
//! `new-window`, `split-window`, each with `-P -F` so the created ids come
//! back in the reply as typed values (ARCHITECTURE.md §4, verified live on
//! tmux 3.7b). A caller that holds the id never has to know what "the
//! current pane" was when the command ran.
//!
//! Every command here is detached (`-d`): creating never moves what any
//! client is looking at. Selecting is a separate command with its own
//! target, so the two decisions cannot be confused for one another.

use super::target::{SessionName, Target, WindowIndex};
use super::Execute;
use crate::client::{CommandOutput, TmuxError};
use crate::protocol::{CommandLine, PaneId, SessionId, WindowId};

/// What `new-session -P` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewSession {
    pub session: SessionId,
    pub window: WindowId,
    pub pane: PaneId,
}

/// What `new-window -P` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewWindow {
    pub window: WindowId,
    pub pane: PaneId,
}

/// `new-session -d -s <name> [-n <window_name>] [-c <cwd>]`.
pub fn new_session<C: Execute>(
    client: &mut C,
    name: &SessionName,
    window_name: Option<&str>,
    cwd: Option<&str>,
) -> Result<NewSession, TmuxError> {
    const FORMAT: &str = "#{session_id} #{window_id} #{pane_id}";
    let mut args = vec!["-d", "-s", name.as_str()];
    args.extend(flag("-n", window_name));
    args.extend(flag("-c", cwd));
    args.extend(["-P", "-F", FORMAT]);
    let output = client.execute(&CommandLine::new("new-session", args)?)?;
    let [session, window, pane] = ids(output, FORMAT)?;
    Ok(NewSession {
        session: SessionId::parse(&session).ok_or_else(|| unexpected(FORMAT, &session))?,
        window: WindowId::parse(&window).ok_or_else(|| unexpected(FORMAT, &window))?,
        pane: PaneId::parse(&pane).ok_or_else(|| unexpected(FORMAT, &pane))?,
    })
}

/// `new-window -d -t =<session>:<index> [-n <name>] [-c <cwd>]`. An occupied
/// index is tmux's "index N in use" error, returned as the
/// [`TmuxError::Command`] it is.
pub fn new_window<C: Execute>(
    client: &mut C,
    session: &SessionName,
    index: WindowIndex,
    name: Option<&str>,
    cwd: Option<&str>,
) -> Result<NewWindow, TmuxError> {
    const FORMAT: &str = "#{window_id} #{pane_id}";
    let target = Target::Window(session.clone(), index).to_string();
    let mut args = vec!["-d", "-t", target.as_str()];
    args.extend(flag("-n", name));
    args.extend(flag("-c", cwd));
    args.extend(["-P", "-F", FORMAT]);
    let output = client.execute(&CommandLine::new("new-window", args)?)?;
    let [window, pane] = ids(output, FORMAT)?;
    Ok(NewWindow {
        window: WindowId::parse(&window).ok_or_else(|| unexpected(FORMAT, &window))?,
        pane: PaneId::parse(&pane).ok_or_else(|| unexpected(FORMAT, &pane))?,
    })
}

/// `split-window -d -t %<pane> [-c <cwd>]`: the new pane's id. Direction is
/// not a parameter because geometry is `select-layout`'s job once every pane
/// of the window exists.
pub fn split_window<C: Execute>(
    client: &mut C,
    pane: PaneId,
    cwd: Option<&str>,
) -> Result<PaneId, TmuxError> {
    const FORMAT: &str = "#{pane_id}";
    let target = Target::Pane(pane).to_string();
    let mut args = vec!["-d", "-t", target.as_str()];
    args.extend(flag("-c", cwd));
    args.extend(["-P", "-F", FORMAT]);
    let output = client.execute(&CommandLine::new("split-window", args)?)?;
    let [pane] = ids(output, FORMAT)?;
    PaneId::parse(&pane).ok_or_else(|| unexpected(FORMAT, &pane))
}

/// `flag value` when the value is given, nothing when it is not — the one
/// rendering of an optional flag for every command here.
fn flag<'a>(flag: &'a str, value: Option<&'a str>) -> impl Iterator<Item = &'a str> {
    value.into_iter().flat_map(move |value| [flag, value])
}

/// The `-P -F <format>` reply: exactly one line holding exactly `N`
/// space-separated fields. Anything else is [`TmuxError::UnexpectedReply`].
fn ids<const N: usize>(
    output: CommandOutput,
    format: &'static str,
) -> Result<[Vec<u8>; N], TmuxError> {
    let fields: Option<[Vec<u8>; N]> = match output.lines.as_slice() {
        [line] => line
            .split(|b| *b == b' ')
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>()
            .try_into()
            .ok(),
        _ => None,
    };
    fields.ok_or(TmuxError::UnexpectedReply {
        expected: format,
        output: output.lines,
    })
}

fn unexpected(format: &'static str, field: &[u8]) -> TmuxError {
    TmuxError::UnexpectedReply {
        expected: format,
        output: vec![field.to_vec()],
    }
}
