//! A pane's working directory as tmux reports it (ARCHITECTURE.md §5).
//!
//! tmux reports `pane_current_path` as the empty string when it cannot read
//! the foreground process's working directory (`top` on macOS does this), so
//! "" is an absence with a reason, not a path: [`Cwd::parse`] turns it into
//! [`Cwd::Unreadable`] once at the boundary (`[LAW:parse-dont-validate]`).
//! A `String` rather than `PathBuf` because the value crosses tmux's UTF-8
//! wire both ways and never touches the filesystem from this crate.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Utf8PathBuf(String);

impl Utf8PathBuf {
    /// `None` for the empty string, which is tmux's "unknown".
    pub fn parse(path: impl Into<String>) -> Option<Self> {
        let path = path.into();
        if path.is_empty() {
            None
        } else {
            Some(Self(path))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Utf8PathBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What capture learned about a pane's working directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Cwd {
    Known(Utf8PathBuf),
    /// tmux could not read the foreground process's directory.
    Unreadable,
}

impl Cwd {
    pub fn parse(path: impl Into<String>) -> Self {
        match Utf8PathBuf::parse(path) {
            Some(path) => Cwd::Known(path),
            None => Cwd::Unreadable,
        }
    }

    pub fn known(&self) -> Option<&Utf8PathBuf> {
        match self {
            Cwd::Known(path) => Some(path),
            Cwd::Unreadable => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_empty_string_is_unreadable_not_a_path() {
        assert_eq!(Utf8PathBuf::parse(""), None);
        assert_eq!(Cwd::parse(""), Cwd::Unreadable);
    }

    #[test]
    fn round_trips_through_as_str_and_display() {
        let p = Utf8PathBuf::parse("/home/user/project").unwrap();
        assert_eq!(p.as_str(), "/home/user/project");
        assert_eq!(p.to_string(), "/home/user/project");
        assert_eq!(Cwd::parse("/home/user/project").known(), Some(&p));
    }
}
