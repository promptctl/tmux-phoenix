//! The complete `ServerMessage` union (SPEC §23) and its per-type parsers.
//!
//! Every server→client wire message tmux defines is representable, plus two
//! codec-level additions beyond SPEC §23's notification catalogue that the
//! guard-block state machine (`codec.rs`) needs to stay total:
//!
//! - `CommandOutput` — a line of command-response text between `%begin` and
//!   `%end`/`%error`. SPEC §23 only catalogues the `%`-prefixed protocol
//!   messages; a command's actual output (e.g. a `list-panes` row) has no
//!   wire type of its own, so it needs a home here too, or the codec would
//!   have no way to hand it back to a caller.
//! - `ProtocolError` — a `%end`/`%error` line that arrives positionally as
//!   the open block's terminator but fails to parse (SPEC §5.1's required
//!   three fields). The reference library force-closes the block in this
//!   case rather than leaving `activeCommandNumber` set forever, which would
//!   silently misroute every subsequent line — including real notifications
//!   — as output for a command that will never settle. This is the single
//!   most important stability property this codec has to preserve.
//!
//! `Unknown` is the SPEC §23 catch-all: an unrecognized `%`-type, or a
//! recognized type whose fields fail to parse, degrades to data instead of
//! being dropped (`[LAW:no-silent-failure]`) — a deliberate strengthening
//! over the reference library, which silently skips both cases outside a
//! block. phoenix cares about not losing tmux state changes, so a malformed
//! or unrecognized line must stay observable rather than vanish inside the
//! codec.

use super::decode::decode_octal;
use super::fields::{
    find_colon_sep, find_space, first_token, parse_i64, parse_optional, parse_optional_u32,
    parse_u32, parse_u64, split_ws, to_text,
};
use super::guard::Guard;
use super::ids::{PaneId, SessionId, WindowId};
use super::layout::Layout;

/// Every server→client message the codec can produce from one line.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    // -- Response-block framing (SPEC §5.1) --
    GuardBegin(Guard),
    GuardEnd(Guard),
    GuardError(Guard),
    /// A line of command output between `%begin` and `%end`/`%error`. Not a
    /// SPEC §23 wire type — see module docs.
    CommandOutput {
        command_number: u32,
        line: Vec<u8>,
    },
    /// A malformed block terminator that force-closed an open block. Not a
    /// SPEC §23 wire type — see module docs.
    ProtocolError {
        command_number: u32,
        line: Vec<u8>,
    },

    // -- Pane output (SPEC §7.1) --
    Output {
        pane: PaneId,
        data: Vec<u8>,
    },
    ExtendedOutput {
        pane: PaneId,
        age_ms: u64,
        data: Vec<u8>,
    },

    // -- Pane flow control (SPEC §7.2) --
    Pause {
        pane: PaneId,
    },
    Continue {
        pane: PaneId,
    },

    // -- Pane mode (SPEC §7.3) --
    PaneModeChanged {
        pane: PaneId,
    },

    // -- Window events (SPEC §7.4) --
    WindowAdd {
        window: WindowId,
    },
    WindowClose {
        window: WindowId,
    },
    WindowRenamed {
        window: WindowId,
        name: String,
    },
    WindowPaneChanged {
        window: WindowId,
        pane: PaneId,
    },
    UnlinkedWindowAdd {
        window: WindowId,
    },
    UnlinkedWindowClose {
        window: WindowId,
    },
    UnlinkedWindowRenamed {
        window: WindowId,
        name: String,
    },

    // -- Layout events (SPEC §7.5) --
    LayoutChange {
        window: WindowId,
        layout: Layout,
        visible: Layout,
        flags: String,
    },

    // -- Session events (SPEC §7.6) --
    SessionChanged {
        session: SessionId,
        name: String,
    },
    /// SPEC §25: the man page documents only `<name>`, but the code sends
    /// `<session-id> <name>`.
    SessionRenamed {
        session: SessionId,
        name: String,
    },
    SessionsChanged,
    SessionWindowChanged {
        session: SessionId,
        window: WindowId,
    },

    // -- Client events (SPEC §7.6-7.7) --
    ClientSessionChanged {
        client: String,
        session: SessionId,
        name: String,
    },
    ClientDetached {
        client: String,
    },

    // -- Paste buffer events (SPEC §7.8) --
    PasteBufferChanged {
        name: String,
    },
    PasteBufferDeleted {
        name: String,
    },

    // -- Subscriptions (SPEC §7.9) --
    SubscriptionChanged {
        name: String,
        session: Option<SessionId>,
        window: Option<WindowId>,
        window_index: Option<u32>,
        pane: Option<PaneId>,
        value: String,
    },

    // -- Messages / errors / exit (SPEC §7.10-7.12) --
    Message {
        text: String,
    },
    ConfigError {
        text: String,
    },
    Exit {
        reason: Option<String>,
    },

    /// Catch-all: an unrecognized `%`-type, or a recognized type whose
    /// fields failed to parse. Carries the raw line, lossily decoded.
    Unknown(String),
}

/// Parse one notification's `type` word and `args` bytes (the line with its
/// leading `%` and the type word already stripped by the codec) into a
/// message. Returns `None` if `type_str` is unrecognized or `args` doesn't
/// match the recognized type's shape — the codec decides what to do with
/// that (`Unknown` outside a block, `ProtocolError` for a block terminator).
pub fn parse_notification(type_str: &[u8], args: &[u8]) -> Option<ServerMessage> {
    match type_str {
        b"begin" => parse_guard(args).map(ServerMessage::GuardBegin),
        b"end" => parse_guard(args).map(ServerMessage::GuardEnd),
        b"error" => parse_guard(args).map(ServerMessage::GuardError),
        b"output" => parse_output(args),
        b"extended-output" => parse_extended_output(args),
        b"pause" => parse_pane_only(args).map(|pane| ServerMessage::Pause { pane }),
        b"continue" => parse_pane_only(args).map(|pane| ServerMessage::Continue { pane }),
        b"pane-mode-changed" => {
            parse_pane_only(args).map(|pane| ServerMessage::PaneModeChanged { pane })
        }
        b"window-add" => parse_window_only(args).map(|window| ServerMessage::WindowAdd { window }),
        b"window-close" => {
            parse_window_only(args).map(|window| ServerMessage::WindowClose { window })
        }
        b"window-renamed" => parse_window_renamed(args)
            .map(|(window, name)| ServerMessage::WindowRenamed { window, name }),
        b"window-pane-changed" => parse_window_pane_changed(args),
        b"unlinked-window-add" => {
            parse_window_only(args).map(|window| ServerMessage::UnlinkedWindowAdd { window })
        }
        b"unlinked-window-close" => {
            parse_window_only(args).map(|window| ServerMessage::UnlinkedWindowClose { window })
        }
        b"unlinked-window-renamed" => parse_window_renamed(args)
            .map(|(window, name)| ServerMessage::UnlinkedWindowRenamed { window, name }),
        b"layout-change" => parse_layout_change(args),
        b"session-changed" => parse_session_with_name(args)
            .map(|(session, name)| ServerMessage::SessionChanged { session, name }),
        b"session-renamed" => parse_session_with_name(args)
            .map(|(session, name)| ServerMessage::SessionRenamed { session, name }),
        b"sessions-changed" => Some(ServerMessage::SessionsChanged),
        b"session-window-changed" => parse_session_window_changed(args),
        b"client-session-changed" => parse_client_session_changed(args),
        b"client-detached" => parse_client_detached(args),
        b"paste-buffer-changed" => {
            parse_name_only(args).map(|name| ServerMessage::PasteBufferChanged { name })
        }
        b"paste-buffer-deleted" => {
            parse_name_only(args).map(|name| ServerMessage::PasteBufferDeleted { name })
        }
        b"subscription-changed" => parse_subscription_changed(args),
        b"message" => Some(ServerMessage::Message {
            text: to_text(args),
        }),
        b"config-error" => Some(ServerMessage::ConfigError {
            text: to_text(args),
        }),
        b"exit" => Some(ServerMessage::Exit {
            reason: if args.is_empty() {
                None
            } else {
                Some(to_text(args))
            },
        }),
        _ => None,
    }
}

fn parse_guard(args: &[u8]) -> Option<Guard> {
    let parts = split_ws(args);
    if parts.len() < 3 {
        return None;
    }
    Some(Guard {
        timestamp: parse_i64(parts[0])?,
        command_number: parse_u32(parts[1])?,
        flags: parse_u32(parts[2])?,
    })
}

fn parse_output(args: &[u8]) -> Option<ServerMessage> {
    let idx = find_space(args)?;
    let pane = PaneId::parse(&args[..idx])?;
    let data = decode_octal(&args[idx + 1..]);
    Some(ServerMessage::Output { pane, data })
}

fn parse_extended_output(args: &[u8]) -> Option<ServerMessage> {
    let sep = find_colon_sep(args)?;
    let head = &args[..sep];
    let value = &args[sep + 3..];
    let parts = split_ws(head);
    if parts.len() < 2 {
        return None;
    }
    let pane = PaneId::parse(parts[0])?;
    let age_ms = parse_u64(parts[1])?;
    let data = decode_octal(value);
    Some(ServerMessage::ExtendedOutput { pane, age_ms, data })
}

fn parse_pane_only(args: &[u8]) -> Option<PaneId> {
    PaneId::parse(first_token(args))
}

fn parse_window_only(args: &[u8]) -> Option<WindowId> {
    WindowId::parse(first_token(args))
}

fn parse_window_renamed(args: &[u8]) -> Option<(WindowId, String)> {
    let idx = find_space(args)?;
    let window = WindowId::parse(&args[..idx])?;
    Some((window, to_text(&args[idx + 1..])))
}

fn parse_window_pane_changed(args: &[u8]) -> Option<ServerMessage> {
    let parts = split_ws(args);
    if parts.len() < 2 {
        return None;
    }
    let window = WindowId::parse(parts[0])?;
    let pane = PaneId::parse(parts[1])?;
    Some(ServerMessage::WindowPaneChanged { window, pane })
}

fn parse_layout_change(args: &[u8]) -> Option<ServerMessage> {
    let parts = split_ws(args);
    if parts.len() < 4 {
        return None;
    }
    let window = WindowId::parse(parts[0])?;
    Some(ServerMessage::LayoutChange {
        window,
        layout: Layout(to_text(parts[1])),
        visible: Layout(to_text(parts[2])),
        flags: to_text(parts[3]),
    })
}

fn parse_session_with_name(args: &[u8]) -> Option<(SessionId, String)> {
    let idx = find_space(args)?;
    let session = SessionId::parse(&args[..idx])?;
    Some((session, to_text(&args[idx + 1..])))
}

fn parse_session_window_changed(args: &[u8]) -> Option<ServerMessage> {
    let parts = split_ws(args);
    if parts.len() < 2 {
        return None;
    }
    let session = SessionId::parse(parts[0])?;
    let window = WindowId::parse(parts[1])?;
    Some(ServerMessage::SessionWindowChanged { session, window })
}

fn parse_client_session_changed(args: &[u8]) -> Option<ServerMessage> {
    let idx = find_space(args)?;
    let client = to_text(&args[..idx]);
    let rest = &args[idx + 1..];
    let idx2 = find_space(rest)?;
    let session = SessionId::parse(&rest[..idx2])?;
    let name = to_text(&rest[idx2 + 1..]);
    Some(ServerMessage::ClientSessionChanged {
        client,
        session,
        name,
    })
}

fn parse_client_detached(args: &[u8]) -> Option<ServerMessage> {
    let raw = first_token(args);
    if raw.is_empty() {
        return None;
    }
    Some(ServerMessage::ClientDetached {
        client: to_text(raw),
    })
}

fn parse_name_only(args: &[u8]) -> Option<String> {
    let raw = first_token(args);
    if raw.is_empty() {
        return None;
    }
    Some(to_text(raw))
}

fn parse_subscription_changed(args: &[u8]) -> Option<ServerMessage> {
    let sep = find_colon_sep(args)?;
    let head = &args[..sep];
    let value = to_text(&args[sep + 3..]);
    let parts = split_ws(head);
    if parts.len() < 5 {
        return None;
    }
    let name = to_text(parts[0]);
    let session = parse_optional(parts[1], SessionId::parse)?;
    let window = parse_optional(parts[2], WindowId::parse)?;
    let window_index = parse_optional_u32(parts[3])?;
    let pane = parse_optional(parts[4], PaneId::parse)?;
    Some(ServerMessage::SubscriptionChanged {
        name,
        session,
        window,
        window_index,
        pane,
        value,
    })
}
