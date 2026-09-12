//! Pure tmux version parsing and comparison (IMPL.md §2.2). No I/O — the
//! effectful probe that turns a live connection into a known
//! [`TmuxVersion`] lives in the commands layer, built on top of this one.

/// A `<major>.<minor>` tmux version pair.
///
/// Field declaration order (`major` then `minor`) makes the derived
/// [`Ord`]/[`PartialOrd`] exactly the major-then-minor comparison IMPL.md
/// §2.2 specifies — no hand-written comparison function needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TmuxVersion {
    pub major: u32,
    pub minor: u32,
}

/// The library-wide floor (IMPL.md §2.2): format subscriptions, pane flow
/// control, and `%client-detached` all arrived in tmux 3.2.
pub const MIN_TMUX_VERSION: TmuxVersion = TmuxVersion { major: 3, minor: 2 };

/// Parse the leading `<major>.<minor>` digit run out of a `tmux -V`-style
/// string (e.g. `"tmux 3.5a"` → `3.5`). A trailing suffix letter (`3.5a`)
/// is ignored — the match is on the digits only, mirroring the reference's
/// regex-based parser. Returns `None` if no such pattern appears anywhere.
pub fn parse_tmux_version(s: &str) -> Option<TmuxVersion> {
    (0..s.len()).find_map(|start| try_parse_version_at(s, start))
}

fn try_parse_version_at(s: &str, start: usize) -> Option<TmuxVersion> {
    let bytes = s.as_bytes();
    if !s.is_char_boundary(start) {
        return None;
    }

    let mut i = start;
    if i >= bytes.len() || !bytes[i].is_ascii_digit() {
        return None;
    }
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let major: u32 = s[start..i].parse().ok()?;

    if i >= bytes.len() || bytes[i] != b'.' {
        return None;
    }
    i += 1;

    let minor_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == minor_start {
        return None;
    }
    let minor: u32 = s[minor_start..i].parse().ok()?;

    Some(TmuxVersion { major, minor })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_version() {
        assert_eq!(
            parse_tmux_version("3.5"),
            Some(TmuxVersion { major: 3, minor: 5 })
        );
    }

    #[test]
    fn ignores_trailing_suffix_letter() {
        assert_eq!(
            parse_tmux_version("3.5a"),
            Some(TmuxVersion { major: 3, minor: 5 })
        );
    }

    #[test]
    fn finds_version_embedded_in_surrounding_text() {
        assert_eq!(
            parse_tmux_version("tmux 3.2"),
            Some(TmuxVersion { major: 3, minor: 2 })
        );
    }

    #[test]
    fn handles_multi_digit_components() {
        assert_eq!(
            parse_tmux_version("12.34"),
            Some(TmuxVersion {
                major: 12,
                minor: 34
            })
        );
    }

    #[test]
    fn returns_none_for_no_version_pattern() {
        assert_eq!(parse_tmux_version("no version here"), None);
        assert_eq!(parse_tmux_version(""), None);
        assert_eq!(parse_tmux_version("3"), None); // no minor component
        assert_eq!(parse_tmux_version("3."), None); // no minor digits
    }

    #[test]
    fn ordering_is_major_then_minor() {
        assert!(TmuxVersion { major: 3, minor: 5 } > TmuxVersion { major: 3, minor: 2 });
        assert!(
            TmuxVersion { major: 4, minor: 0 }
                > TmuxVersion {
                    major: 3,
                    minor: 99
                }
        );
        assert!(MIN_TMUX_VERSION <= TmuxVersion { major: 3, minor: 2 });
    }
}
