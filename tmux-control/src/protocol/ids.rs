//! Typed identifier newtypes for the tmux control-mode wire protocol (SPEC §3).
//!
//! tmux prefixes each identifier kind with a fixed byte (`$`/`@`/`%`) and the
//! prefix is stripped exactly once here, at the parse boundary — no code
//! downstream re-parses a raw id string (`[LAW:parse-dont-validate]`).

/// A tmux session identifier (`$N` on the wire). SPEC §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(pub u32);

/// A tmux window identifier (`@N` on the wire). SPEC §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(pub u32);

/// A tmux pane identifier (`%N` on the wire). SPEC §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneId(pub u32);

/// Strip `prefix` and parse the remainder as an unsigned integer. Shared by
/// the three id newtypes below (`[LAW:one-source-of-truth]`): the prefix
/// byte is the only thing that varies between them.
fn parse_prefixed(bytes: &[u8], prefix: u8) -> Option<u32> {
    if bytes.first().copied()? != prefix {
        return None;
    }
    super::fields::parse_decimal(&bytes[1..])
}

impl SessionId {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        parse_prefixed(bytes, b'$').map(SessionId)
    }
}

impl WindowId {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        parse_prefixed(bytes, b'@').map(WindowId)
    }
}

impl PaneId {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        parse_prefixed(bytes, b'%').map(PaneId)
    }
}
