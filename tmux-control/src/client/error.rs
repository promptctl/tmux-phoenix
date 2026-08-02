use crate::client::ConnectionState;
use crate::protocol::Guard;
use std::fmt;
use std::io;

/// Everything `Client::execute`/`connect`/`reconnect` can fail with.
#[derive(Debug)]
pub enum TmuxError {
    /// `execute()` was called while `Client::state()` wasn't `Ready` — a
    /// command sent during `Connecting`/`Reconnecting` would correlate
    /// against the wrong guard block; `Closed` has no transport to send on
    /// at all.
    NotReady(ConnectionState),
    /// The transport refused or failed to send the command.
    Send(io::Error),
    /// A read from the transport failed (distinct from a clean close, which
    /// is [`TmuxError::TransportClosed`]).
    Read(io::Error),
    /// The transport reached EOF before the command's guard block settled.
    TransportClosed,
    /// tmux replied with `%error` instead of `%end`. Carries whatever
    /// output arrived before the error (SPEC §5.3: parse errors are
    /// delivered as ordinary block output ahead of the `%error` line).
    Command { guard: Guard, lines: Vec<Vec<u8>> },
    /// The block's terminator line arrived but failed to parse (a malformed
    /// `%end`/`%error`) — the codec force-closed the block rather than
    /// leaving it open forever; this is that failure made observable to
    /// the caller who was waiting on it.
    Protocol { command_number: u32, line: Vec<u8> },
}

impl fmt::Display for TmuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TmuxError::NotReady(state) => write!(f, "client is not ready (state: {state:?})"),
            TmuxError::Send(err) => write!(f, "failed to send command: {err}"),
            TmuxError::Read(err) => write!(f, "failed to read from transport: {err}"),
            TmuxError::TransportClosed => {
                write!(f, "transport closed before the command's reply arrived")
            }
            TmuxError::Command { guard, lines } => {
                write!(
                    f,
                    "command {} failed: {}",
                    guard.command_number,
                    String::from_utf8_lossy(&lines.join(&b'\n'))
                )
            }
            TmuxError::Protocol {
                command_number,
                line,
            } => {
                write!(
                    f,
                    "malformed guard terminator for command {command_number}: {}",
                    String::from_utf8_lossy(line)
                )
            }
        }
    }
}

impl std::error::Error for TmuxError {}
