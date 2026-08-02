//! The correlation client (DESIGN.md §3.3): `execute()` is the single
//! command-dispatch path every typed operation (a later ticket) delegates
//! to. Correlation is by FIFO — tmux processes commands serially and emits
//! exactly one guard block per command, in order — not by the guard's
//! command-number, which is informational only.
//!
//! Scoped narrowly to match ticket tmux-control-mode-1ju.3: this is a
//! synchronous, blocking, single-command-in-flight client with no
//! `ConnectionState` machine and no typed notification events. Both are
//! later tickets (`.4`, `.5`) that layer on top of this primitive rather
//! than being reimplemented here:
//!
//! - **No startup-greeting handling.** tmux emits an unsolicited
//!   `%begin…%end` block on attach before any caller command's reply
//!   (DESIGN.md §3.3's off-by-one warning). `execute()` assumes the byte
//!   stream is already positioned past that block — ticket `.4`'s
//!   `ConnectionState` machine is what actually drains it before allowing a
//!   caller through. Calling `execute()` on a freshly-attached transport
//!   without draining the greeting first misattributes it as the first
//!   command's reply; that is a precondition violation ticket `.4` exists to
//!   prevent, not something this layer defends against.
//! - **No typed notification dispatch.** A `ServerMessage` observed while
//!   waiting for a reply that isn't part of that reply's guard block (a
//!   notification) is buffered verbatim in [`Client::drain_notifications`]
//!   rather than dropped — nothing is silently lost — but there is no
//!   subscriber/event API yet; that shape is ticket `.5`'s job.

mod error;

pub use error::TmuxError;

use crate::protocol::{Codec, Guard, ServerMessage};
use crate::transport::Transport;
use std::collections::VecDeque;

/// A completed command's guard-framed output.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandOutput {
    pub guard: Guard,
    /// Each line of output between `%begin` and `%end`, in order.
    pub lines: Vec<Vec<u8>>,
}

/// Correlates commands to replies over a [`Transport`] + [`Codec`] pair.
pub struct Client<T: Transport> {
    transport: T,
    codec: Codec,
    /// `ServerMessage`s observed while waiting for a reply that were not
    /// part of that reply (notifications) — buffered, not dropped. See
    /// module docs: no typed dispatch yet, this is the interim non-lossy
    /// holding area.
    notifications: VecDeque<ServerMessage>,
}

const READ_CHUNK: usize = 8192;

impl<T: Transport> Client<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            codec: Codec::new(),
            notifications: VecDeque::new(),
        }
    }

    /// The single command-dispatch path (`[LAW:single-enforcer]`). Sends
    /// `command`, then blocks reading from the transport — feeding every
    /// chunk to the codec — until the guard block that positionally
    /// follows (the codec only ever has one block open at a time) settles
    /// with `%end` or `%error`.
    ///
    /// A `%error` reply is `Err(TmuxError::Command)`, not
    /// `Ok(CommandOutput { .. })` with a failure flag — SPEC §5.3's parse
    /// errors and other command failures are error conditions, not data.
    pub fn execute(&mut self, command: &str) -> Result<CommandOutput, TmuxError> {
        self.transport.send(command).map_err(TmuxError::Send)?;

        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut buf = [0u8; READ_CHUNK];
        let mut outcome = None;

        while outcome.is_none() {
            let n = self.transport.read(&mut buf).map_err(TmuxError::Read)?;
            if n == 0 {
                return Err(TmuxError::TransportClosed);
            }
            // A single read() can return a chunk containing our reply's
            // GuardEnd/GuardError *and* trailing bytes after it (tmux wrote
            // them in one burst, e.g. a notification right behind the
            // block). codec.feed() hands back every message in that chunk
            // as one Vec — stopping at the first settling message here
            // would silently drop everything after it in the same batch, so
            // once `outcome` is set the loop keeps draining, routing
            // anything further to `notifications` instead of returning
            // early.
            for msg in self.codec.feed(&buf[..n]) {
                if outcome.is_some() {
                    self.notifications.push_back(msg);
                    continue;
                }
                match msg {
                    // Framing only — GuardEnd/GuardError below carry their
                    // own complete Guard, so nothing needs to be remembered
                    // from GuardBegin.
                    ServerMessage::GuardBegin(_) => {}
                    ServerMessage::CommandOutput { line, .. } => lines.push(line),
                    ServerMessage::GuardEnd(guard) => {
                        outcome = Some(Ok(CommandOutput {
                            guard,
                            lines: std::mem::take(&mut lines),
                        }));
                    }
                    ServerMessage::GuardError(guard) => {
                        outcome = Some(Err(TmuxError::Command {
                            guard,
                            lines: std::mem::take(&mut lines),
                        }));
                    }
                    ServerMessage::ProtocolError {
                        command_number,
                        line,
                    } => {
                        outcome = Some(Err(TmuxError::Protocol {
                            command_number,
                            line,
                        }));
                    }
                    other => self.notifications.push_back(other),
                }
            }
        }

        outcome.expect("loop only exits once outcome is Some")
    }

    /// Drain every `ServerMessage` that arrived while an `execute()` call
    /// was waiting for its own reply but wasn't part of that reply.
    pub fn drain_notifications(&mut self) -> Vec<ServerMessage> {
        self.notifications.drain(..).collect()
    }

    /// The wire-level detach signal: a bare `\n` (SPEC §4.1). Deliberately
    /// bypasses `execute()` — an empty line carries no guard block, so
    /// there is nothing to correlate (IMPL.md §2.4).
    pub fn detach(&mut self) -> Result<(), TmuxError> {
        self.transport.send("").map_err(TmuxError::Send)
    }

    /// Local-side teardown: drops the transport, sends nothing to tmux
    /// (IMPL.md §2.4 — distinct from [`Client::detach`]).
    pub fn close(&mut self) {
        self.transport.close();
    }
}
