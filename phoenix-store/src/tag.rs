//! A name a user gives a generation (ARCHITECTURE.md §7): a header field
//! that exempts it from pruning, not a second store. Non-empty, parsed once
//! (`[LAW:parse-dont-validate]`).

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Tag(String);

impl Tag {
    pub fn parse(s: impl Into<String>) -> Option<Self> {
        let s = s.into();
        if s.is_empty() {
            None
        } else {
            Some(Self(s))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_the_empty_string() {
        assert_eq!(Tag::parse(""), None);
        assert_eq!(
            Tag::parse("before-upgrade").unwrap().as_str(),
            "before-upgrade"
        );
    }
}
