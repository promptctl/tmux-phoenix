//! Explicit connection lifecycle (DESIGN.md §3.3): no timing folklore — a
//! caller asks [`crate::Client::state`] rather than inferring liveness from
//! whether the last call happened to succeed.

/// The client's connection lifecycle. `Closed` is terminal — nothing
/// transitions out of it automatically; a caller that wants back in calls
/// [`crate::Client::reconnect`] with a fresh transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Consuming tmux's unsolicited startup greeting block (SPEC §5, an
    /// unsolicited `%begin…%end`/`%error` pair that is not a reply to any
    /// command). No caller command may be correlated yet — see
    /// `Client::execute`'s state gate.
    Connecting,
    /// The greeting settled; safe to correlate caller commands.
    Ready,
    /// Re-attaching after a prior transport died. `attempt` is the caller's
    /// own retry count — this crate does not own retry timing or backoff
    /// policy, only the mechanics of swapping in a fresh transport and
    /// re-consuming its greeting (`Client::reconnect`).
    Reconnecting {
        attempt: u32,
    },
    /// The connection is over. Terminal, and `reason` records the ending
    /// that got here first.
    Closed {
        reason: CloseReason,
    },
}

impl ConnectionState {
    /// The one transition into `Closed` (`[LAW:single-enforcer]`), which is
    /// what makes the terminality asserted above true of the type rather
    /// than true of whichever call sites remembered it. Terminal means the
    /// first close wins: a later best-effort call — `Client::detach` on an
    /// already-`Client::close`d client, whose transport refuses the send —
    /// cannot rewrite why the connection ended.
    ///
    /// Only transitions *into* `Closed` are absorbed. `Client::reconnect`
    /// still assigns `Reconnecting` directly, which is the documented way
    /// back out.
    pub(crate) fn closed(self, reason: CloseReason) -> Self {
        match self {
            Self::Closed { .. } => self,
            _ => Self::Closed { reason },
        }
    }
}

/// Why the connection ended. Records the first ending only: once a
/// connection is closed a later one cannot relabel it, so each variant
/// below says what got there first, not everything that has since been
/// attempted. A best-effort [`crate::Client::detach`] on a client already
/// torn down by [`crate::Client::close`] therefore still reports
/// `Disposed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The transport reached a clean EOF (tmux's own `%exit`, or the
    /// process simply exited) with no transport-level error.
    Exit,
    /// A send or read on the transport itself failed.
    TransportError,
    /// `Client::close` tore the connection down locally, before anything
    /// else had ended it.
    Disposed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_live_state_closes_with_the_given_reason() {
        for live in [
            ConnectionState::Connecting,
            ConnectionState::Ready,
            ConnectionState::Reconnecting { attempt: 2 },
        ] {
            assert_eq!(
                live.closed(CloseReason::TransportError),
                ConnectionState::Closed {
                    reason: CloseReason::TransportError
                }
            );
        }
    }

    #[test]
    fn the_first_close_wins_so_a_later_one_cannot_rewrite_the_reason() {
        let disposed = ConnectionState::Closed {
            reason: CloseReason::Disposed,
        };
        // The sequence that matters in practice: close() then a
        // best-effort detach() whose send is refused by the dead
        // transport. The deliberate teardown is why the connection
        // ended, and a later failure does not get to relabel it.
        assert_eq!(disposed.closed(CloseReason::TransportError), disposed);
        assert_eq!(disposed.closed(CloseReason::Exit), disposed);
    }
}
