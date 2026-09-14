//! A UTF-8-checked path, standing in for `camino::Utf8PathBuf` (DESIGN.md
//! §4). This environment has no network access to fetch external crates
//! (confirmed: `cargo add` against crates.io times out), so this crate is
//! std-only like `tmux-control` — see [`crate::time::OffsetDateTime`] for the
//! same story. tmux's control-mode wire protocol is line-oriented text, so a
//! checked-UTF-8 path is the right domain type regardless of which crate
//! provides it.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Utf8PathBuf(String);

impl Utf8PathBuf {
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Utf8PathBuf {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Utf8PathBuf {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl fmt::Display for Utf8PathBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_as_str() {
        let p = Utf8PathBuf::new("/home/user/project");
        assert_eq!(p.as_str(), "/home/user/project");
    }

    #[test]
    fn from_str_and_string_agree() {
        assert_eq!(
            Utf8PathBuf::from("/tmp"),
            Utf8PathBuf::from("/tmp".to_string())
        );
    }

    #[test]
    fn display_matches_as_str() {
        let p = Utf8PathBuf::new("/a/b");
        assert_eq!(p.to_string(), "/a/b");
    }
}
