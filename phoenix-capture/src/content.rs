//! Dirty-tracked pane content capture (ARCHITECTURE.md §6). Content is not
//! a mode: every capture pulls every pane's content, and what varies is the
//! [`Previous`] the caller passes — empty on a cold start, the last
//! generation's on every other call. A pane whose indicator matches what
//! `previous` recorded reuses that scrollback unchanged (no `capture-pane
//! -S -` round-trip for it); every other pane gets a fresh full-scrollback
//! capture. Every pane, dirty or not, gets a fresh *visible*-screen capture
//! (no `-S`), since an alt-screen TUI can redraw its screen without ever
//! touching scrollback. Verified live that `capture-pane` targets a bare
//! `%N` pane id directly.
//!
//! A pane tmux refuses `capture-pane` for degrades *that* pane to
//! [`Content::NotCaptured`] with tmux's own error as the reason; a failure
//! of the connection itself is the caller's error, not a pane's.

use std::collections::HashMap;

use phoenix_core::{Content, ContentFailure, HistoryIndicator, Origin, PaneId, ServerId, Snapshot};
use tmux_control::{CommandLine, Execute, TmuxError};

/// What content capture needs from the *previous* capture: which server
/// incarnation it was of, and per pane the indicator its scrollback was
/// captured at and that scrollback. The one bridge from a persisted
/// `Snapshot` lives here, so every caller builds it the same way
/// (`[LAW:one-source-of-truth]`); the crate still has no persistence
/// dependency (`[LAW:one-way-deps]`).
///
/// Pane ids restart at `%0` with every tmux server, so an indicator match
/// means "same scrollback" only on the server it was recorded from: reuse
/// is keyed on the origin as well as the pane. A `Previous` of no recorded
/// origin (the default, or a pre-origin generation) reuses nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Previous {
    origin: Origin,
    panes: HashMap<PaneId, PreviousContent>,
}

impl Default for Previous {
    fn default() -> Self {
        Self {
            origin: Origin::BeforeOriginWasRecorded,
            panes: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousContent {
    pub indicator: HistoryIndicator,
    pub scrollback: Vec<String>,
}

impl Previous {
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            origin: snapshot.origin,
            panes: snapshot
                .panes()
                .filter_map(|pane| match &pane.content {
                    Content::Captured {
                        indicator,
                        scrollback,
                        ..
                    } => Some((
                        pane.id,
                        PreviousContent {
                            indicator: *indicator,
                            scrollback: scrollback.clone(),
                        },
                    )),
                    Content::NotCaptured { .. } => None,
                })
                .collect(),
        }
    }

    /// A `Previous` of `server` holding nothing yet.
    pub fn of(server: ServerId) -> Self {
        Self {
            origin: Origin::Recorded(server),
            panes: HashMap::new(),
        }
    }

    pub fn insert(&mut self, pane: PaneId, content: PreviousContent) {
        self.panes.insert(pane, content);
    }

    /// The scrollback `previous` holds for `pane` on `server` if it was
    /// captured there at exactly `indicator`.
    fn reusable(
        &self,
        server: ServerId,
        pane: PaneId,
        indicator: HistoryIndicator,
    ) -> Option<&[String]> {
        self.panes
            .get(&pane)
            .filter(|p| self.origin == Origin::Recorded(server) && p.indicator == indicator)
            .map(|p| p.scrollback.as_slice())
    }
}

/// Program output is not guaranteed valid UTF-8 the way structural fields
/// (names, paths) are — lossily decoding here (replacing bad sequences with
/// U+FFFD) rather than failing: a garbled character in a scrollback line
/// isn't a reason to degrade the whole pane the way a malformed structural
/// row is.
fn decode_lines(lines: &[Vec<u8>]) -> Vec<String> {
    lines
        .iter()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect()
}

#[derive(Debug, Clone, Copy)]
enum Extent {
    /// `-S -`: from the oldest scrollback line.
    FullScrollback,
    /// The visible screen only.
    Visible,
}

/// A `%error` from tmux for this one pane is the pane's degradation; any
/// other failure is the connection's.
fn capture_pane_lines<C: Execute>(
    client: &mut C,
    pane: PaneId,
    extent: Extent,
) -> Result<Result<Vec<String>, ContentFailure>, TmuxError> {
    let scrollback: &[&str] = match extent {
        Extent::FullScrollback => &["-S", "-"],
        Extent::Visible => &[],
    };
    let target = pane.to_string();
    let args: Vec<&str> = ["-p", "-e"]
        .into_iter()
        .chain(scrollback.iter().copied())
        .chain(["-t", target.as_str()])
        .collect();
    match client.execute(&CommandLine::new("capture-pane", args)?) {
        Ok(output) => Ok(Ok(decode_lines(&output.lines))),
        Err(TmuxError::Command { lines, .. }) => Ok(Err(ContentFailure::CapturePane {
            message: decode_lines(&lines).join("\n"),
        })),
        Err(other) => Err(other),
    }
}

/// `pane`'s content now, at `indicator` (read in the same `list-panes` row
/// as the rest of the pane), reusing `previous`'s scrollback when it is of
/// this `server` and the indicator has not moved.
pub fn capture_content<C: Execute>(
    client: &mut C,
    server: ServerId,
    pane: PaneId,
    indicator: HistoryIndicator,
    previous: &Previous,
) -> Result<Content, TmuxError> {
    let scrollback = match previous.reusable(server, pane, indicator) {
        Some(reused) => Ok(reused.to_vec()),
        None => capture_pane_lines(client, pane, Extent::FullScrollback)?,
    };
    let visible = capture_pane_lines(client, pane, Extent::Visible)?;
    Ok(match (scrollback, visible) {
        (Ok(scrollback), Ok(visible)) => Content::Captured {
            indicator,
            scrollback,
            visible,
        },
        (Err(reason), _) | (_, Err(reason)) => Content::NotCaptured { reason },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_lines_replaces_invalid_utf8_instead_of_failing() {
        let lines = vec![b"valid".to_vec(), vec![0xff, 0xfe]];
        let decoded = decode_lines(&lines);
        assert_eq!(decoded[0], "valid");
        assert!(decoded[1].contains('\u{fffd}'));
    }

    #[test]
    fn previous_scrollback_is_reusable_only_at_the_same_indicator() {
        let at = HistoryIndicator {
            history_size: 3,
            history_bytes: 300,
        };
        let moved = HistoryIndicator {
            history_size: 4,
            history_bytes: 400,
        };
        let server = ServerId::parse("4242:1700000000").unwrap();
        let mut previous = Previous::of(server);
        previous.insert(
            PaneId(1),
            PreviousContent {
                indicator: at,
                scrollback: vec!["old".to_string()],
            },
        );
        assert_eq!(
            previous.reusable(server, PaneId(1), at),
            Some(&["old".to_string()][..])
        );
        assert_eq!(previous.reusable(server, PaneId(1), moved), None);
        assert_eq!(previous.reusable(server, PaneId(2), at), None);
    }

    #[test]
    fn previous_scrollback_is_never_reused_across_server_incarnations() {
        // Pane ids restart at %0 per server: the same %1 at the same
        // indicator on a restarted server is a different pane.
        let at = HistoryIndicator {
            history_size: 0,
            history_bytes: 0,
        };
        let content = PreviousContent {
            indicator: at,
            scrollback: vec!["old".to_string()],
        };
        let first = ServerId::parse("4242:1700000000").unwrap();
        let restarted = ServerId::parse("4243:1700000100").unwrap();
        let mut previous = Previous::of(first);
        previous.insert(PaneId(1), content.clone());
        assert_eq!(previous.reusable(restarted, PaneId(1), at), None);

        let mut unknown = Previous::default();
        unknown.insert(PaneId(1), content);
        assert_eq!(unknown.reusable(first, PaneId(1), at), None);
    }
}
