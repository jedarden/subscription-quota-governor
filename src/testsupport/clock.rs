//! A deterministic, advanceable time source for tests.
//!
//! `controller::evaluate` and the provider parsers all take `now`/`observed_at`
//! as a plain `DateTime<Utc>` parameter rather than sourcing it from a
//! singleton or a `SystemTime::now()` call, so no `Clock` trait exists (or is
//! needed) in production code. What staleness, reset-generation, and pacing
//! tests actually need is an ergonomic way to build a fixed starting instant
//! and advance it deterministically between calls, without wall-clock sleeps.

use chrono::{DateTime, Duration, Utc};
use std::cell::Cell;

/// A fixed instant that tests can read and advance explicitly.
pub(crate) struct FakeClock {
    now: Cell<DateTime<Utc>>,
}

impl FakeClock {
    /// Starts the clock at a fixed, human-readable RFC 3339 instant so test
    /// output stays reproducible across runs and timezones.
    ///
    /// Panics if `rfc3339` is not a valid RFC 3339 timestamp -- a malformed
    /// literal is a bug in the calling test, not a runtime condition to
    /// handle gracefully.
    pub(crate) fn at(rfc3339: &str) -> Self {
        let now = DateTime::parse_from_rfc3339(rfc3339)
            .unwrap_or_else(|error| panic!("invalid test timestamp {rfc3339:?}: {error}"))
            .with_timezone(&Utc);
        Self {
            now: Cell::new(now),
        }
    }

    /// The current instant.
    pub(crate) fn now(&self) -> DateTime<Utc> {
        self.now.get()
    }

    /// Moves the clock forward (or backward, for a negative `delta`).
    pub(crate) fn advance(&self, delta: Duration) {
        self.now.set(self.now.get() + delta);
    }

    /// Jumps the clock to an explicit instant, e.g. to land exactly on a
    /// provider's `resets_at` boundary.
    pub(crate) fn set(&self, when: DateTime<Utc>) {
        self.now.set(when);
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::at("2026-01-01T00:00:00Z")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_the_requested_instant() {
        let clock = FakeClock::at("2026-03-01T10:00:00Z");
        assert_eq!(clock.now().to_rfc3339(), "2026-03-01T10:00:00+00:00");
    }

    #[test]
    fn advance_moves_time_forward_deterministically() {
        let clock = FakeClock::at("2026-03-01T10:00:00Z");
        clock.advance(Duration::hours(2));
        assert_eq!(
            clock.now(),
            "2026-03-01T12:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn advance_accepts_negative_durations() {
        let clock = FakeClock::at("2026-03-01T10:00:00Z");
        clock.advance(Duration::hours(-1));
        assert_eq!(
            clock.now(),
            "2026-03-01T09:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn set_jumps_to_an_explicit_instant() {
        let clock = FakeClock::at("2026-03-01T10:00:00Z");
        let target = "2030-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        clock.set(target);
        assert_eq!(clock.now(), target);
    }

    #[test]
    #[should_panic(expected = "invalid test timestamp")]
    fn at_panics_on_malformed_input() {
        FakeClock::at("not-a-timestamp");
    }
}
