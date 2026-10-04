//! The tmux server version a snapshot was captured from (ARCHITECTURE.md
//! §5).
//!
//! [`TmuxVersion`] here is deliberately a separate type from
//! `tmux_control::TmuxVersion` — `phoenix-core` and `tmux-control` are two
//! independent foundation crates, neither depending on the other
//! (`[LAW:one-way-deps]`). `tmux-control`'s version type drives live
//! protocol gating; this one is inert captured data. `phoenix-capture`
//! converts one into the other at the boundary.

/// The `<major>.<minor>` version of the tmux server a snapshot was captured
/// from — recorded, not probed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TmuxVersion {
    pub major: u32,
    pub minor: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_version_orders_major_then_minor() {
        assert!(TmuxVersion { major: 3, minor: 9 } < TmuxVersion { major: 4, minor: 0 });
        assert!(TmuxVersion { major: 3, minor: 2 } < TmuxVersion { major: 3, minor: 5 });
    }
}
