//! Typed targets (ARCHITECTURE.md §4): what `-t` names, rendered to tmux's
//! target syntax in exactly one place, exact-match where tmux allows it.
//!
//! tmux resolves a bare name loosely — `fnmatch` patterns, prefixes, and the
//! `$`/`@`/`%` id sigils all take part — so `-t main` can land on a session
//! other than the one called `main`. The `=` prefix asks for the exact name,
//! and the trailing `:` says the name is the whole session part: without it
//! a command that takes a window or pane target reads `=main` as an exact
//! *window* name and falls back to a session prefix match (verified live on
//! tmux 3.7b: `list-panes -t =s2-lon` lists `s2-longer`, `-t =s2-lon:` is
//! "can't find session"). The typed target renders both unconditionally so
//! no caller can forget either.

use std::fmt;

use crate::protocol::{PaneId, SessionId, WindowId};

/// A session name tmux can address exactly. Parsed once, here, so nothing
/// downstream re-checks it (`[LAW:parse-dont-validate]`).
///
/// tmux's target grammar splits a target on its first `:` (session from
/// window) and reads a leading `$` as a session id even behind `=` — so a
/// session whose name holds a `:`, or starts with `$`, exists but cannot be
/// named by name in a `-t` argument at all (verified live: `=a:b:` is
/// "can't find session: a", `=$0:` resolves to session id 0 whatever it is
/// called). A `.` is fine once the trailing `:` marks where the session part
/// ends (`=a.b:` finds `a.b`). tmux itself accepts every name at
/// `new-session -s`, which is why this type, and not tmux, is the checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionName(String);

/// A name no `-t` argument can address; see [`SessionName`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnaddressableSessionName {
    pub name: String,
}

impl fmt::Display for UnaddressableSessionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "session name {:?} cannot be a tmux target: it is empty, holds ':', or starts with '$'",
            self.name
        )
    }
}

impl std::error::Error for UnaddressableSessionName {}

impl SessionName {
    pub fn parse(name: impl Into<String>) -> Result<Self, UnaddressableSessionName> {
        let name = name.into();
        let addressable = !name.is_empty() && !name.contains(':') && !name.starts_with('$');
        if addressable {
            Ok(Self(name))
        } else {
            Err(UnaddressableSessionName { name })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A window's position within a session. Any `u32` is a legitimate index,
/// so there is nothing to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowIndex(pub u32);

/// What a `-t` argument can name. The id forms are exact by construction
/// and are how the ids a `-P` reply hands back ([`crate::commands::create`])
/// come back to tmux as targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Session(SessionName),
    SessionId(SessionId),
    /// The window at an index of a session.
    Window(SessionName, WindowIndex),
    WindowId(WindowId),
    Pane(PaneId),
}

/// The one rendering to tmux's target syntax (`[LAW:one-source-of-truth]`).
impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Session(session) => write!(f, "={session}:"),
            Target::SessionId(session) => write!(f, "${}", session.0),
            Target::Window(session, index) => write!(f, "={session}:{}", index.0),
            Target::WindowId(window) => write!(f, "@{}", window.0),
            Target::Pane(pane) => write!(f, "%{}", pane.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(raw: &str) -> SessionName {
        SessionName::parse(raw).expect("addressable")
    }

    #[test]
    fn a_session_name_is_addressable_or_it_is_not_a_session_name() {
        assert_eq!(name("main").as_str(), "main");
        assert_eq!(name("a-b_c d").as_str(), "a-b_c d");
        assert_eq!(name("a.b").as_str(), "a.b");
        assert_eq!(name("a$b").as_str(), "a$b");
        for bad in ["", "a:b", "$0", "$name"] {
            assert_eq!(
                SessionName::parse(bad),
                Err(UnaddressableSessionName {
                    name: bad.to_owned()
                }),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn targets_render_exact_match_where_tmux_allows_it() {
        assert_eq!(Target::Session(name("main")).to_string(), "=main:");
        assert_eq!(Target::Session(name("a.b")).to_string(), "=a.b:");
        assert_eq!(Target::SessionId(SessionId(2)).to_string(), "$2");
        assert_eq!(
            Target::Window(name("main"), WindowIndex(3)).to_string(),
            "=main:3"
        );
        assert_eq!(Target::WindowId(WindowId(4)).to_string(), "@4");
        assert_eq!(Target::Pane(PaneId(7)).to_string(), "%7");
    }
}
