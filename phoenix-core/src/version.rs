//! Versioning types for the on-disk snapshot format and the tmux server a
//! snapshot was captured from (DESIGN.md §4, §7).
//!
//! [`TmuxVersion`] here is deliberately a separate type from
//! `tmux_control::TmuxVersion` — DESIGN.md §2's crate graph has `phoenix-core`
//! and `tmux-control` as two independent foundation crates, neither
//! depending on the other (`[LAW:one-way-deps]`). `tmux-control`'s version
//! type drives live protocol gating; this one is inert captured data. A
//! future `phoenix-capture` converts one into the other at the boundary.

/// The on-disk snapshot schema version (DESIGN.md §7): "an unknown
/// `format_version` is refused loudly, never guessed."
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FormatVersion(pub u32);

impl FormatVersion {
    pub const CURRENT: FormatVersion = FormatVersion(1);
}

/// The `<major>.<minor>` version of the tmux server a snapshot was captured
/// from — recorded, not probed; see the module doc for why this isn't
/// `tmux_control::TmuxVersion`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TmuxVersion {
    pub major: u32,
    pub minor: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_version_current_is_one() {
        assert_eq!(FormatVersion::CURRENT, FormatVersion(1));
    }

    #[test]
    fn tmux_version_orders_major_then_minor() {
        assert!(TmuxVersion { major: 3, minor: 9 } < TmuxVersion { major: 4, minor: 0 });
        assert!(TmuxVersion { major: 3, minor: 2 } < TmuxVersion { major: 3, minor: 5 });
    }
}
