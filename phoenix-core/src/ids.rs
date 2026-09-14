//! Typed identifiers for the domain tree (DESIGN.md §4).
//!
//! `SessionName`/`WindowName`/`Layout`/`ProgramName` are validated newtypes
//! over `String`: tmux never hands back an empty name, layout or
//! `pane_current_command`, so the empty state
//! is rejected once, at [`SessionName::parse`]/[`WindowName::parse`]/
//! [`Layout::parse`], rather than re-checked by every consumer
//! (`[LAW:parse-dont-validate]`, matching `tmux_control::protocol::ids`'s
//! `parse()` boundary-method idiom).
//!
//! `WindowIndex`/`PaneIndex` have no illegal values — any `u32` is a
//! legitimate index — so they're plain `Copy` newtypes with a public field,
//! same as `tmux_control::protocol::ids::PaneId`.

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
validated_name!(ProgramName);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_empty() {
        assert_eq!(SessionName::parse(""), None);
        assert_eq!(WindowName::parse(""), None);
        assert_eq!(Layout::parse(""), None);
        assert_eq!(ProgramName::parse(""), None);
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
}
