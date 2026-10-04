//! Typed identifiers for the domain graph (ARCHITECTURE.md §5).
//!
//! `SessionName`/`WindowName`/`Layout`/`GroupName`/`ClientName` are validated
//! newtypes over `String`: tmux never hands back an empty one, so the empty
//! state is rejected once, at `parse`, rather than re-checked by every
//! consumer (`[LAW:parse-dont-validate]`, matching
//! `tmux_control::protocol::ids`'s `parse()` boundary-method idiom).
//!
//! `WindowIndex`/`PaneIndex`/`PaneId`/`WindowId` have no illegal values — any
//! `u32` is a legitimate index or id — so they are plain `Copy` newtypes with
//! a public field, same as `tmux_control::protocol::ids::PaneId`.

use std::fmt;

macro_rules! validated_name {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Rejects the empty string; tmux never produces one for this
            /// field.
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

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

validated_name!(SessionName);
validated_name!(WindowName);
validated_name!(Layout);
validated_name!(GroupName);
validated_name!(ClientName);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowIndex(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneIndex(pub u32);

/// tmux's own globally-assigned pane identifier (`%N` on the wire —
/// `tmux_control::protocol::ids::PaneId` is the same idea at the protocol
/// layer, deliberately a separate type here for the same reason
/// `phoenix_core::TmuxVersion` is separate from `tmux_control`'s: this
/// crate depends on nothing tmux-control-specific, see `version.rs`).
/// Unlike [`PaneIndex`] (a window-relative position that shifts if a
/// sibling pane is added or removed), `PaneId` is stable for a pane's whole
/// lifetime — the correlator content-capture's dirty-tracking needs to
/// recognize "the same pane as last capture" across saves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneId(pub u32);

/// tmux's globally-assigned window identifier (`@N`), stable for the
/// window's lifetime and the same in every session that links it — which is
/// what lets a shared window be held once and referenced by each winlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(pub u32);

impl fmt::Display for WindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@{}", self.0)
    }
}

impl WindowId {
    /// The `@N` spelling tmux uses on the wire and in option values.
    pub fn parse(s: &str) -> Option<Self> {
        s.strip_prefix('@')?.parse().ok().map(Self)
    }
}

impl fmt::Display for PaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "%{}", self.0)
    }
}

impl PaneId {
    /// The `%N` spelling tmux uses on the wire.
    pub fn parse(s: &str) -> Option<Self> {
        s.strip_prefix('%')?.parse().ok().map(Self)
    }
}

/// A store generation's identity: the number in its file name, rising in
/// save order. It is the value phoenix writes into tmux's `@phoenix-*`
/// options, so it is a domain id and not a store detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GenerationId(pub i64);

impl fmt::Display for GenerationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl GenerationId {
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok().map(Self)
    }
}

/// One incarnation of a tmux server: `#{pid}:#{start_time}`. Changes when
/// the server restarts on the same socket; the same across every session of
/// one server (verified live on tmux 3.7b).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ServerId {
    pub pid: u32,
    pub start_time: i64,
}

impl fmt::Display for ServerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.pid, self.start_time)
    }
}

impl ServerId {
    pub fn parse(s: &str) -> Option<Self> {
        let (pid, start_time) = s.split_once(':')?;
        Some(Self {
            pid: pid.parse().ok()?,
            start_time: start_time.parse().ok()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_empty() {
        assert_eq!(SessionName::parse(""), None);
        assert_eq!(WindowName::parse(""), None);
        assert_eq!(Layout::parse(""), None);
        assert_eq!(GroupName::parse(""), None);
        assert_eq!(ClientName::parse(""), None);
    }

    #[test]
    fn parse_accepts_non_empty_and_round_trips() {
        let name = SessionName::parse("main").unwrap();
        assert_eq!(name.as_str(), "main");
        assert_eq!(name.to_string(), "main");
    }

    #[test]
    fn indices_compare_by_value() {
        assert!(WindowIndex(0) < WindowIndex(1));
        assert!(PaneIndex(2) == PaneIndex(2));
    }

    #[test]
    fn ids_round_trip_their_tmux_spelling() {
        assert_eq!(WindowId::parse("@7"), Some(WindowId(7)));
        assert_eq!(WindowId(7).to_string(), "@7");
        assert_eq!(WindowId::parse("7"), None);
        assert_eq!(PaneId::parse("%3"), Some(PaneId(3)));
        assert_eq!(PaneId(3).to_string(), "%3");
        assert_eq!(PaneId::parse("@3"), None);
    }

    #[test]
    fn server_id_round_trips() {
        let id = ServerId {
            pid: 81317,
            start_time: 1_791_109_860,
        };
        assert_eq!(id.to_string(), "81317:1791109860");
        assert_eq!(ServerId::parse("81317:1791109860"), Some(id));
        assert_eq!(ServerId::parse("81317"), None);
        assert_eq!(ServerId::parse("x:1"), None);
    }

    #[test]
    fn generation_id_round_trips() {
        assert_eq!(
            GenerationId::parse("1700000000"),
            Some(GenerationId(1_700_000_000))
        );
        assert_eq!(GenerationId(5).to_string(), "5");
        assert_eq!(GenerationId::parse(""), None);
    }
}
