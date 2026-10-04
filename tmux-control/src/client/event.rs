//! What a [`crate::Connection`] delivers on its event channel: everything the
//! server says that is not the reply to a command, plus the one thing the
//! reader itself has to say — that there will be nothing more.

use super::connection_state::CloseReason;
use crate::protocol::{PaneId, ServerMessage};

/// One delivery from the reader thread. `Closed` is always the last one: the
/// reader sends it as its final act, so a caller that has seen it knows the
/// channel is spent without probing it.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Any notification other than pane output (SPEC §7).
    Notification(ServerMessage),
    /// `%output`/`%extended-output` bytes for one pane (SPEC §7.1).
    PaneOutput(PaneId, Vec<u8>),
    /// The transport's read side ended, and why.
    Closed(CloseReason),
}
