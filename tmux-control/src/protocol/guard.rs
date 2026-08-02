//! The response-block guard line (SPEC §5.1): `%<begin|end|error> <timestamp>
//! <command-number> <flags>`. `%begin`, `%end`, and `%error` all share this
//! shape, so one struct backs all three `ServerMessage` guard variants
//! (`[LAW:one-type-per-behavior]`).

/// Metadata shared by a `%begin`/`%end`/`%error` line. The command-number is
/// informational only — correlation to a pending command is a FIFO-order
/// concern for the client layer (SPEC §5.1; see DESIGN.md §3.3), not this
/// codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guard {
    /// Seconds since the Unix epoch (`item->time`, `cmd-queue.c:825-833`).
    pub timestamp: i64,
    /// Monotonically increasing per-client sequence number.
    pub command_number: u32,
    /// 1 if `CMDQ_STATE_CONTROL`, 0 otherwise.
    pub flags: u32,
}
