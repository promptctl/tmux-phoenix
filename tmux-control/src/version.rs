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
    // Tokens are maximal, so a rejected token is skipped whole. A scan that
    // could resume *inside* a rejected digit run would reinterpret a major
    // too large for `u32` as a shorter suffix of itself — reporting a
    // truncated version where this promises `None`.
    s.split(|c: char| !c.is_ascii_digit() && c != '.')
        .find_map(parse_version_token)
}

fn parse_version_token(token: &str) -> Option<TmuxVersion> {
    let mut parts = token.split('.');
    Some(TmuxVersion {
        major: parts.next()?.parse().ok()?,
        minor: parts.next()?.parse().ok()?,
    })
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
    fn rejects_components_too_large_for_u32_rather_than_truncating_them() {
        // Skipping the digit run whole is what makes these `None`: a scan
        // resuming inside the run would find "999999999" and call it 3.x.
        assert_eq!(parse_tmux_version("99999999999.2"), None);
        assert_eq!(parse_tmux_version("3.99999999999"), None);
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
