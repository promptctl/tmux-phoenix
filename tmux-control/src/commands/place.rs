//! The commands that move something — a window to an index, a client to a
//! session — each with its one legitimate non-move reported as a value
//! (verified live on tmux 3.7b). tmux refuses to move a window onto the
//! index it already sits at — `same index: N` — and for a caller that asked
//! "put this window at N" that refusal is the answer "it is there", not a
//! failure; likewise `can't find client: X` for a client that detached
//! before it could be moved. The wire text is turned into a type here, at
//! the one boundary that reads replies, so no caller matches on error
//! strings (`[LAW:parse-dont-validate]`).

use super::target::{SessionName, Target, WindowIndex};
use super::Execute;
use crate::client::TmuxError;
use crate::protocol::{CommandLine, WindowId};

/// What `move-window` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moved {
    Moved,
    /// The window was already at the index asked for.
    AlreadyThere,
}

/// `move-window -d -s @<window> -t =<session>:=<index>`. The window is named
/// by id, so it must be linked into exactly one session for tmux to know
/// which link to move — true of any window just created. An index another
/// window holds is tmux's "index in use" error, returned as the
/// [`TmuxError::Command`] it is.
pub fn move_window<C: Execute>(
    client: &mut C,
    window: WindowId,
    session: &SessionName,
    index: WindowIndex,
) -> Result<Moved, TmuxError> {
    let source = Target::WindowId(window).to_string();
    let target = Target::Window(session.clone(), index).to_string();
    let line = CommandLine::new("move-window", ["-d", "-s", &source, "-t", &target])?;
    let same_index = format!("same index: {}", index.0);
    match client.execute(&line) {
        Ok(_) => Ok(Moved::Moved),
        Err(TmuxError::Command { lines, .. }) if lines.as_slice() == [same_index.as_bytes()] => {
            Ok(Moved::AlreadyThere)
        }
        Err(err) => Err(err),
    }
}

/// What `switch-client` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switched {
    Switched,
    /// The client detached before the move reached it, so nothing is left
    /// to move.
    Gone,
}

/// `switch-client -c <client> -t =<session>:`, where `client` is a
/// `#{client_name}` as `list-clients` reports it. A listing describes a
/// moment, and a terminal can close in the next one: that is
/// [`Switched::Gone`]. A session that does not exist stays the
/// [`TmuxError::Command`] it is.
pub fn switch_client<C: Execute>(
    client: &mut C,
    name: &str,
    to: &SessionName,
) -> Result<Switched, TmuxError> {
    let target = Target::Session(to.clone()).to_string();
    let line = CommandLine::new("switch-client", ["-c", name, "-t", &target])?;
    let gone = format!("can't find client: {name}");
    match client.execute(&line) {
        Ok(_) => Ok(Switched::Switched),
        Err(TmuxError::Command { lines, .. }) if lines.as_slice() == [gone.as_bytes()] => {
            Ok(Switched::Gone)
        }
        Err(err) => Err(err),
    }
}
