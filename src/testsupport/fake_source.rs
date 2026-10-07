//! An in-memory, scripted stand-in for `crate::source::collect`.
//!
//! `source.rs` has no `Source` trait today -- `collect` is a free function
//! that matches on the `SourceConfig` enum and performs real file/HTTP/child
//! process I/O directly (introducing a trait so controller-cycle code can
//! take an injected adapter is WP4's job, not WP0's). Until that trait
//! exists, `FakeSource` gives tests a way to script a sequence of snapshots
//! (or failures) that a future cycle test can pull from, without depending on
//! a trait shape that doesn't exist yet.

use crate::model::QuotaSnapshot;
use anyhow::{anyhow, Result};
use std::cell::RefCell;
use std::collections::VecDeque;

/// A queue of scripted responses, returned in the order they were pushed.
#[derive(Default)]
pub(crate) struct FakeSource {
    responses: RefCell<VecDeque<Result<QuotaSnapshot, String>>>,
}

impl FakeSource {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Queues a successful snapshot for the next `collect` call.
    pub(crate) fn push_snapshot(&self, snapshot: QuotaSnapshot) {
        self.responses.borrow_mut().push_back(Ok(snapshot));
    }

    /// Queues a failure, simulating a provider or transport error.
    pub(crate) fn push_error(&self, message: impl Into<String>) {
        self.responses.borrow_mut().push_back(Err(message.into()));
    }

    /// Returns the next scripted response.
    ///
    /// Panics if the script is exhausted -- a test calling this more times
    /// than it scripted responses has a bug in its own setup, not a
    /// condition production code needs to handle.
    pub(crate) fn collect(&self) -> Result<QuotaSnapshot> {
        match self.responses.borrow_mut().pop_front() {
            Some(Ok(snapshot)) => Ok(snapshot),
            Some(Err(message)) => Err(anyhow!(message)),
            None => panic!("FakeSource script exhausted: no more scripted responses"),
        }
    }

    /// The number of responses still queued.
    pub(crate) fn remaining(&self) -> usize {
        self.responses.borrow().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::QuotaWindow;
    use chrono::{Duration, Utc};

    fn snapshot(used_fraction: f64) -> QuotaSnapshot {
        let now = Utc::now();
        QuotaSnapshot {
            observed_at: now,
            fresh: true,
            windows: vec![QuotaWindow {
                id: "five_hour".into(),
                used_fraction,
                resets_at: now + Duration::hours(1),
                duration_minutes: Some(300),
                reached: false,
            }],
            eligible_backlog_capacity: None,
            reset_credits: None,
        }
    }

    #[test]
    fn returns_scripted_snapshots_in_order() {
        let source = FakeSource::new();
        source.push_snapshot(snapshot(0.1));
        source.push_snapshot(snapshot(0.2));
        assert_eq!(source.collect().unwrap().windows[0].used_fraction, 0.1);
        assert_eq!(source.collect().unwrap().windows[0].used_fraction, 0.2);
        assert_eq!(source.remaining(), 0);
    }

    #[test]
    fn returns_scripted_errors() {
        let source = FakeSource::new();
        source.push_error("simulated transport failure");
        let error = source.collect().unwrap_err();
        assert_eq!(error.to_string(), "simulated transport failure");
    }

    #[test]
    fn interleaves_successes_and_failures_in_push_order() {
        let source = FakeSource::new();
        source.push_snapshot(snapshot(0.5));
        source.push_error("boom");
        assert!(source.collect().is_ok());
        assert!(source.collect().is_err());
    }

    #[test]
    #[should_panic(expected = "FakeSource script exhausted")]
    fn panics_when_the_script_runs_out() {
        let source = FakeSource::new();
        let _ = source.collect();
    }
}
