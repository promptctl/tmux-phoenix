//! `move-window`, with its one legitimate non-move reported as a value
//! (verified live on tmux 3.7b). tmux refuses to move a window onto the
//! index it already sits at — `same index: N` — and for a caller that asked
//! "put this window at N" that refusal is the answer "it is there", not a
//! failure. The wire text is turned into a type here, at the one boundary
//! that reads replies, so no caller matches on error strings
//! (`[LAW:parse-dont-validate]`).

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
