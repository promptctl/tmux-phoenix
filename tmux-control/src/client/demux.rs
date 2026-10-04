//! FIFO block assembly over the message stream: the one place that decides
//! whether a parsed message is part of a command's reply, a notification, or
//! pane output (`[LAW:single-enforcer]`). Both drivers in this crate feed it —
//! [`crate::Client`] from inside a blocking call, [`crate::Connection`] from
//! its reader thread — so the two can never disagree about what a message is.
//!
//! Correlation is by position, not by the guard's command-number: tmux
//! processes commands serially and emits exactly one guard block per command,
//! in order (SPEC §5.1), so the block that settles is always the reply to the
//! oldest unanswered command. The command-number is informational only.

use super::{CommandOutput, TmuxError};
use crate::protocol::{PaneId, ServerMessage};

/// What one message turned out to be.
pub(crate) enum Routed {
    /// A guard block settled, with `%end`, `%error`, or a malformed
    /// terminator alike: the reply to the oldest unanswered command (or the
    /// unsolicited greeting, for the first block on a fresh transport).
    Reply(Result<CommandOutput, TmuxError>),
    /// Any notification that is not pane output.
    Notification(ServerMessage),
    /// `%output`/`%extended-output`, kept off the notification path because
    /// it is high-volume and mixing it in is how head-of-line blocking
    /// starts.
    PaneOutput(PaneId, Vec<u8>),
}

/// Assembles the open block's output lines until its terminator arrives.
/// The codec only ever has one block open at a time (SPEC §6's block-purity
/// invariant), so one buffer is all the state there is.
#[derive(Default)]
pub(crate) struct Demux {
    lines: Vec<Vec<u8>>,
}

impl Demux {
    /// Route one message. `None` for framing and body lines, which are
    /// absorbed into the block they belong to.
    pub(crate) fn route(&mut self, msg: ServerMessage) -> Option<Routed> {
        match msg {
            // `GuardEnd`/`GuardError` carry their own complete `Guard`, so
            // nothing needs remembering from `GuardBegin`.
            ServerMessage::GuardBegin(_) => None,
            ServerMessage::CommandOutput { line, .. } => {
                self.lines.push(line);
                None
            }
            ServerMessage::GuardEnd(guard) => Some(Routed::Reply(Ok(CommandOutput {
                guard,
                lines: std::mem::take(&mut self.lines),
            }))),
            // SPEC §5.3: a `%error` reply is an error condition, not data —
            // and the output that arrived ahead of it (a parse error's
            // explanation) travels with it.
            ServerMessage::GuardError(guard) => Some(Routed::Reply(Err(TmuxError::Command {
                guard,
                lines: std::mem::take(&mut self.lines),
            }))),
            // The codec force-closed the block on a malformed terminator;
            // whatever was collected belongs to a block that never properly
            // ended, so it is dropped with it.
            ServerMessage::ProtocolError {
                command_number,
                line,
            } => {
                self.lines.clear();
                Some(Routed::Reply(Err(TmuxError::Protocol {
                    command_number,
                    line,
                })))
            }
            ServerMessage::Output { pane, data }
            | ServerMessage::ExtendedOutput { pane, data, .. } => {
                Some(Routed::PaneOutput(pane, data))
            }
            other => Some(Routed::Notification(other)),
        }
    }
}
