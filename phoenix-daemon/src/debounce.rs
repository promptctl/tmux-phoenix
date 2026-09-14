//! When-to-save decision logic (DESIGN.md §8): pure, driven by explicit
//! [`Instant`]s the caller supplies rather than reading the clock itself —
//! testable with synthetic timestamps, no real sleeping needed.
//! `[LAW:no-ambient-temporal-coupling]`: "owns *when to save* as explicit
//! state" means this state (and the decision it drives) is a value you can
//! construct, inspect, and advance by hand, not a side effect buried in a
//! sleep loop.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebouncePolicy {
    /// Save once activity has been quiet for this long.
    pub debounce: Duration,
    /// Backstop: force a save if this long has passed since the last save,
    /// even if the session is still actively changing.
    pub max_interval: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct DebounceState {
    /// `None` means no activity has been recorded since the last save —
    /// there's nothing new to debounce toward saving.
    last_activity: Option<Instant>,
    last_save: Instant,
}

impl DebounceState {
    pub fn new(now: Instant) -> Self {
        Self {
            last_activity: None,
            last_save: now,
        }
    }

    /// Call on each `%subscription-changed` (or any other sign of activity).
    pub fn record_activity(&mut self, now: Instant) {
        self.last_activity = Some(now);
    }

    /// Call once a save has completed — clears pending activity (it's now
    /// captured) and resets the max-interval clock.
    pub fn record_save(&mut self, now: Instant) {
        self.last_activity = None;
        self.last_save = now;
    }

    /// Event-driven save (primary): quiet long enough since the last
    /// activity. Interval ceiling (backstop): forced regardless of
    /// activity once `max_interval` has elapsed since the last save —
    /// "long steady sessions still checkpoint" even without triggering the
    /// debounce path at all.
    pub fn should_save(&self, policy: &DebouncePolicy, now: Instant) -> bool {
        let quiet_long_enough = self
            .last_activity
            .is_some_and(|t| now.duration_since(t) >= policy.debounce);
        let interval_elapsed = now.duration_since(self.last_save) >= policy.max_interval;
        quiet_long_enough || interval_elapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> DebouncePolicy {
        DebouncePolicy {
            debounce: Duration::from_secs(5),
            max_interval: Duration::from_secs(60),
        }
    }

    #[test]
    fn no_activity_and_no_elapsed_interval_never_saves() {
        let now = Instant::now();
        let state = DebounceState::new(now);
        assert!(!state.should_save(&policy(), now + Duration::from_secs(1)));
    }

    #[test]
    fn saves_once_quiet_for_the_debounce_duration() {
        let now = Instant::now();
        let mut state = DebounceState::new(now);
        state.record_activity(now + Duration::from_secs(1));

        assert!(!state.should_save(&policy(), now + Duration::from_secs(3)));
        assert!(state.should_save(&policy(), now + Duration::from_secs(6)));
    }

    #[test]
    fn fresh_activity_keeps_pushing_the_debounce_window_out() {
        let now = Instant::now();
        let mut state = DebounceState::new(now);
        state.record_activity(now + Duration::from_secs(1));
        // More activity arrives before the first would have settled.
        state.record_activity(now + Duration::from_secs(4));

        // 5s after the *first* activity, but only 2s after the second --
        // still not quiet long enough.
        assert!(!state.should_save(&policy(), now + Duration::from_secs(6)));
        assert!(state.should_save(&policy(), now + Duration::from_secs(9)));
    }

    #[test]
    fn max_interval_backstop_fires_even_with_zero_activity() {
        let now = Instant::now();
        let state = DebounceState::new(now);
        assert!(!state.should_save(&policy(), now + Duration::from_secs(59)));
        assert!(state.should_save(&policy(), now + Duration::from_secs(60)));
    }

    #[test]
    fn max_interval_backstop_fires_even_during_continuous_activity() {
        let now = Instant::now();
        let mut state = DebounceState::new(now);
        // Activity keeps arriving faster than the debounce window, so the
        // event-driven path never fires on its own.
        for t in [10u64, 20, 30, 40, 50] {
            state.record_activity(now + Duration::from_secs(t));
            assert!(!state.should_save(&policy(), now + Duration::from_secs(t + 1)));
        }
        assert!(state.should_save(&policy(), now + Duration::from_secs(61)));
    }

    #[test]
    fn record_save_clears_pending_activity_and_resets_the_interval_clock() {
        let now = Instant::now();
        let mut state = DebounceState::new(now);
        state.record_activity(now + Duration::from_secs(1));
        state.record_save(now + Duration::from_secs(6));

        // Right after the save: no new activity, interval clock restarted.
        assert!(!state.should_save(&policy(), now + Duration::from_secs(7)));
    }
}
