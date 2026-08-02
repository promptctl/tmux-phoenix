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
    Closed {
        reason: CloseReason,
    },
}

/// Why a [`ConnectionState::Closed`] transition happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The transport reached a clean EOF (tmux's own `%exit`, or the
    /// process simply exited) with no transport-level error.
    Exit,
    /// A send or read on the transport itself failed.
    TransportError,
    /// The caller explicitly called `Client::close`.
    Disposed,
}
