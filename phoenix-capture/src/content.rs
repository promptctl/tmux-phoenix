//! Dirty-tracked pane content capture (DESIGN.md §5, tmux-content-dos.1).
//!
//! One `list-panes -a -F` round-trip for `#{pane_id} #{history_size}
//! #{history_bytes}` — verified live against a real tmux server: this
//! indicator is stable while a pane is idle and moves whenever it produces
//! output. A pane whose indicator matches what `previous` recorded for it
//! reuses that scrollback unchanged (no `capture-pane -S -` round-trip for
//! it); every other pane gets a fresh full-scrollback capture. Every pane,
//! dirty or not, also gets a fresh *visible*-screen capture (no `-S`) —
//! verified live that `capture-pane` targets a bare `%N` pane id directly,
//! no `session:window.pane` needed — since an alt-screen TUI can redraw its
//! screen without ever touching scrollback.
//!
//! Per-pane capture failures are best-effort (DESIGN.md §5: "an
//! unresponsive pane degrades *that* pane's content to `None`"): this
//! module simply omits a pane from its returned map rather than failing the
//! whole capture. Only a failure of the one shared indicator query is
//! treated as fatal, via the outer `Result`.

use std::collections::HashMap;

use phoenix_core::{PaneContent, PaneId};
use tmux_control::{Client, CommandLine, TmuxError, Transport};

const INDICATOR_DELIMITER: char = '\u{1f}';

/// What content-capture needs to know about a pane from the *previous*
/// capture, to decide whether this capture can reuse its scrollback. The
/// caller builds this from whatever `Snapshot` was last persisted — this
/// crate has no persistence dependency of its own (`[LAW:one-way-deps]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousPaneContent {
    pub history_size: u64,
    pub history_bytes: u64,
    pub scrollback: Vec<String>,
}

fn indicator_format_string() -> String {
    ["pane_id", "history_size", "history_bytes"]
        .iter()
        .map(|f| format!("#{{{f}}}"))
        .collect::<Vec<_>>()
        .join(&INDICATOR_DELIMITER.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Indicator {
    pane_id: u32,
    history_size: u64,
    history_bytes: u64,
}

fn parse_indicator_line(line: &str) -> Option<Indicator> {
    let mut fields = line.split(INDICATOR_DELIMITER);
    let pane_id = fields.next()?.strip_prefix('%')?.parse().ok()?;
    let history_size = fields.next()?.parse().ok()?;
    let history_bytes = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    Some(Indicator {
        pane_id,
        history_size,
        history_bytes,
    })
}

/// Program output is not guaranteed valid UTF-8 the way structural fields
/// (names, paths) are — lossily decoding here (replacing bad sequences with
/// U+FFFD) rather than failing matches DESIGN.md §5's best-effort content
/// capture; a garbled character in a scrollback line isn't a reason to
/// degrade the whole pane the way a malformed structural row is.
fn decode_lines(lines: &[Vec<u8>]) -> Vec<String> {
    lines
        .iter()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect()
}

fn capture_pane_lines<T: Transport>(
    client: &mut Client<T>,
    pane_id: u32,
    full_scrollback: bool,
) -> Result<Vec<String>, TmuxError> {
    // `-S -` starts the capture at the oldest scrollback line; without it
    // tmux captures the visible screen only.
    let scrollback: &[&str] = if full_scrollback { &["-S", "-"] } else { &[] };
    let target = format!("%{pane_id}");
    let args: Vec<&str> = ["-p", "-e"]
        .into_iter()
        .chain(scrollback.iter().copied())
        .chain(["-t", target.as_str()])
        .collect();
    let output = client.execute(&CommandLine::new("capture-pane", args)?)?;
    Ok(decode_lines(&output.lines))
}

/// Captures content for every pane currently on the server, reusing
/// scrollback from `previous` where the indicator hasn't moved. The outer
/// `Result` only ever reflects the shared indicator query failing; a
/// per-pane `capture-pane` failure just omits that pane from the returned
/// map (best-effort — see the module doc comment).
pub fn capture_content<T: Transport>(
    client: &mut Client<T>,
    previous: &HashMap<u32, PreviousPaneContent>,
) -> Result<HashMap<u32, PaneContent>, TmuxError> {
    let indicator_cmd = CommandLine::new(
        "list-panes",
        ["-a", "-F", indicator_format_string().as_str()],
    )?;
    let indicator_output = client.execute(&indicator_cmd)?;
    let indicators: Vec<Indicator> = indicator_output
        .lines
        .iter()
        .filter_map(|l| parse_indicator_line(&String::from_utf8_lossy(l)))
        .collect();

    let mut result = HashMap::with_capacity(indicators.len());
    for ind in indicators {
        let prev = previous.get(&ind.pane_id);
        let dirty = match prev {
            Some(p) => p.history_size != ind.history_size || p.history_bytes != ind.history_bytes,
            None => true,
        };

        let scrollback = if dirty {
            match capture_pane_lines(client, ind.pane_id, true) {
                Ok(lines) => lines,
                Err(_) => continue, // this pane degrades to absent, not a fatal capture
            }
        } else {
            prev.map(|p| p.scrollback.clone()).unwrap_or_default()
        };

        let Ok(visible) = capture_pane_lines(client, ind.pane_id, false) else {
            continue;
        };

        result.insert(
            ind.pane_id,
            PaneContent::new(
                PaneId(ind.pane_id),
                ind.history_size,
                ind.history_bytes,
                scrollback,
                visible,
            ),
        );
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_indicator_line() {
        let line = "%3\u{1f}42\u{1f}4096";
        assert_eq!(
            parse_indicator_line(line),
            Some(Indicator {
                pane_id: 3,
                history_size: 42,
                history_bytes: 4096,
            })
        );
    }

    #[test]
    fn rejects_a_pane_id_missing_the_percent_prefix() {
        assert_eq!(parse_indicator_line("3\u{1f}42\u{1f}4096"), None);
    }

    #[test]
    fn rejects_wrong_field_count() {
        assert_eq!(parse_indicator_line("%3\u{1f}42"), None);
        assert_eq!(
            parse_indicator_line("%3\u{1f}42\u{1f}4096\u{1f}extra"),
            None
        );
    }

    #[test]
    fn decode_lines_replaces_invalid_utf8_instead_of_failing() {
        let lines = vec![b"valid".to_vec(), vec![0xff, 0xfe]];
        let decoded = decode_lines(&lines);
        assert_eq!(decoded[0], "valid");
        assert!(decoded[1].contains('\u{fffd}'));
    }
}
