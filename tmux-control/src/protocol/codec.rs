//! The pure, no-I/O guard-block state machine (SPEC §5-6).
//!
//! Exactly two states, because the protocol has exactly two:
//!
//! - **Outside a block:** a `%begin` line opens one; every other `%`-line is
//!   a notification; anything else is dropped (SPEC §4: an empty line
//!   detaches the *client's own* connection — it is never something a
//!   server sends back, so a non-`%` line here is simply not a protocol
//!   line).
//! - **Inside a block:** every line is command output until `%end`/`%error`
//!   closes it — routed by *position*, not by content. A command's output
//!   happening to start with `%` (e.g. `list-panes -F '#{pane_id}'` printing
//!   a bare `%5`) is still output, never mistaken for a notification. This
//!   is SPEC §6's central invariant: a notification never appears inside a
//!   response block, so the state machine needs no lookahead to tell output
//!   from notifications — only position.

use super::message::{parse_notification, ServerMessage};

/// Streaming guard-block codec. Stateful (it buffers a partial trailing line
/// and tracks whether a response block is open across `feed()` calls) but
/// pure: no I/O, no panics, every byte sequence maps to zero or more
/// messages.
#[derive(Debug, Default)]
pub struct Codec {
    buffer: Vec<u8>,
    /// `Some(command_number)` while a `%begin ... %end/%error` block is open.
    active_command: Option<u32>,
}

impl Codec {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of bytes (which may contain zero, one, or many complete
    /// lines, and may end mid-line) and return every message it completes.
    ///
    /// CRLF tolerance: a trailing `\r` immediately before `\n` is stripped
    /// before line processing, so a transport that introduces CRLF parses
    /// identically to an LF-only one. This is safe because tmux always
    /// octal-escapes literal control bytes in pane output (SPEC §10), so an
    /// unescaped `\r` adjacent to `\n` can only be transport noise, never
    /// data — library-side defensive behavior, not a protocol rule.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<ServerMessage> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();

        // Scan forward with a cursor and drain the consumed prefix once at
        // the end, rather than draining after every line: draining inside
        // the loop shifts the remaining buffer tail on every line, which is
        // O(lines × remaining bytes) for a chunk containing many lines (a
        // burst of `%output` lines, or a `list-panes` block with hundreds of
        // rows delivered in one read).
        let mut start = 0;
        while let Some(rel_newline) = self.buffer[start..].iter().position(|&b| b == b'\n') {
            let newline_idx = start + rel_newline;
            let end = if newline_idx > start && self.buffer[newline_idx - 1] == b'\r' {
                newline_idx - 1
            } else {
                newline_idx
            };
            let line = self.buffer[start..end].to_vec();
            self.process_line(&line, &mut out);
            start = newline_idx + 1;
        }
        self.buffer.drain(..start);

        out
    }

    /// Clear buffered partial line and any open-block state.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.active_command = None;
    }

    fn process_line(&mut self, line: &[u8], out: &mut Vec<ServerMessage>) {
        let is_notification = line.first() == Some(&b'%');

        let (type_str, args): (&[u8], &[u8]) = if is_notification {
            match line[1..].iter().position(|&b| b == b' ') {
                Some(rel_idx) => (&line[1..1 + rel_idx], &line[1 + rel_idx + 1..]),
                None => (&line[1..], &[]),
            }
        } else {
            (&[], &[])
        };
        let is_terminator = type_str == b"end" || type_str == b"error";

        // Position-based routing (SPEC §6): inside an open block, every line
        // that isn't the terminator is output, regardless of what it looks
        // like — including one that starts with `%`.
        if let Some(command_number) = self.active_command {
            if !is_terminator {
                out.push(ServerMessage::CommandOutput {
                    command_number,
                    line: line.to_vec(),
                });
                return;
            }
        }

        if !is_notification {
            // A non-`%` line outside any block is not a protocol line.
            return;
        }

        match parse_notification(type_str, args) {
            Some(ServerMessage::GuardBegin(guard)) => {
                self.active_command = Some(guard.command_number);
                out.push(ServerMessage::GuardBegin(guard));
            }
            Some(msg @ (ServerMessage::GuardEnd(_) | ServerMessage::GuardError(_))) => {
                self.active_command = None;
                out.push(msg);
            }
            Some(msg) => out.push(msg),
            None => {
                if let Some(command_number) = self.active_command.take() {
                    // This was positionally the open block's terminator (the
                    // `is_terminator` check above let it reach this point)
                    // but its fields failed to parse. Force-close the block
                    // rather than leaving `active_command` set forever —
                    // otherwise every subsequent line, notifications
                    // included, would misroute as output for a command that
                    // will never settle.
                    out.push(ServerMessage::ProtocolError {
                        command_number,
                        line: line.to_vec(),
                    });
                } else {
                    out.push(ServerMessage::Unknown(
                        String::from_utf8_lossy(line).into_owned(),
                    ));
                }
            }
        }
    }
}
