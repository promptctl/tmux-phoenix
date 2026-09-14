//! A UTC point in time, standing in for `time::OffsetDateTime` (DESIGN.md
//! §4). This environment has no network access to fetch external crates
//! (`cargo add time` against crates.io stalls and times out), so — as with
//! `tmux-control` — this crate stays std-only. Represented as a Unix
//! timestamp in seconds: sufficient precision for "when was this snapshot
//! captured," and trivially roundtrippable once persistence (a later
//! ticket) picks a wire format.

use std::time::{SystemTime, SystemTimeError, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OffsetDateTime(i64);

impl OffsetDateTime {
    pub const fn from_unix_timestamp(secs: i64) -> Self {
        Self(secs)
    }

    pub const fn unix_timestamp(&self) -> i64 {
        self.0
    }
}

/// Reading the wall clock is the one effect a capture layer needs to turn
/// into an [`OffsetDateTime`]; this crate stays pure by only offering the
/// conversion, never reading the clock itself.
impl TryFrom<SystemTime> for OffsetDateTime {
    type Error = SystemTimeError;

    fn try_from(t: SystemTime) -> Result<Self, Self::Error> {
        let secs = t.duration_since(UNIX_EPOCH)?.as_secs();
        Ok(Self(secs as i64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn round_trips_unix_timestamp() {
        let t = OffsetDateTime::from_unix_timestamp(1_700_000_000);
        assert_eq!(t.unix_timestamp(), 1_700_000_000);
    }

    #[test]
    fn converts_from_system_time() {
        let sys = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let t = OffsetDateTime::try_from(sys).unwrap();
        assert_eq!(t.unix_timestamp(), 1_700_000_000);
    }

    #[test]
    fn rejects_system_time_before_the_epoch() {
        let sys = UNIX_EPOCH - Duration::from_secs(1);
        assert!(OffsetDateTime::try_from(sys).is_err());
    }

    #[test]
    fn ordering_matches_timestamp_ordering() {
        let earlier = OffsetDateTime::from_unix_timestamp(100);
        let later = OffsetDateTime::from_unix_timestamp(200);
        assert!(earlier < later);
    }
}
