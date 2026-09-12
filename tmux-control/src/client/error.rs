use crate::client::ConnectionState;
use crate::protocol::{Guard, NulInArgument};
use crate::version::TmuxVersion;
use std::fmt;
use std::io;

/// Everything `Client::execute`/`connect`/`reconnect`, and the free
/// functions in the commands layer, can fail with.
#[derive(Debug)]
pub enum TmuxError {
    /// `execute()` was called while `Client::state()` wasn't `Ready` — a
    /// command sent during `Connecting`/`Reconnecting` would correlate
    /// against the wrong guard block; `Closed` has no transport to send on
    /// at all.
    NotReady(ConnectionState),
    /// An argument held a NUL, so no command line exists to send: tmux
    /// arguments are C strings.
    Encode(NulInArgument),
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
    /// An operation requires a tmux version newer than the connected
    /// server's (IMPL.md §2.2) — a typed precondition failure instead of
    /// tmux's raw `%error` for an unrecognized flag.
    UnsupportedTmuxVersion {
        operation: String,
        required: TmuxVersion,
        have: TmuxVersion,
    },
    /// The commands layer's version probe replied without a recognizable
    /// `<major>.<minor>` version string in it.
    VersionProbeFailed { output: Vec<Vec<u8>> },
}

impl fmt::Display for TmuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TmuxError::NotReady(state) => write!(f, "client is not ready (state: {state:?})"),
            TmuxError::Encode(err) => write!(f, "cannot build the command line: {err}"),
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
            TmuxError::UnsupportedTmuxVersion {
                operation,
                required,
                have,
            } => {
                write!(
                    f,
                    "{operation} requires tmux {}.{}+, connected server is {}.{}",
                    required.major, required.minor, have.major, have.minor
                )
            }
            TmuxError::VersionProbeFailed { output } => {
                write!(
                    f,
                    "could not determine tmux version from reply: {:?}",
                    output
                        .iter()
                        .map(|l| String::from_utf8_lossy(l))
                        .collect::<Vec<_>>()
                )
            }
        }
    }
}

impl From<NulInArgument> for TmuxError {
    fn from(err: NulInArgument) -> Self {
        TmuxError::Encode(err)
    }
}

impl std::error::Error for TmuxError {}
