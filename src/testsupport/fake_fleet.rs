//! An in-memory, scripted stand-in for `crate::fleet::Observer`.
//!
//! The real `Observer` implementations in `fleet.rs` read a file or spawn a
//! command. A controller-cycle test needs a fleet's reported worker count
//! without touching disk or a process -- mirroring `FakeSource`'s role for
//! `crate::source::collect`.

use crate::fleet::Observer;
use anyhow::{anyhow, Result};
use std::cell::RefCell;
use std::collections::VecDeque;

/// A queue of scripted worker-count responses, returned in the order they
/// were pushed.
#[derive(Default)]
pub(crate) struct FakeObserver {
    responses: RefCell<VecDeque<Result<u32, String>>>,
}

impl FakeObserver {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Queues a successful observation for the next `current_workers` call.
    pub(crate) fn push_workers(&self, workers: u32) {
        self.responses.borrow_mut().push_back(Ok(workers));
    }

    /// Queues a failure, simulating an unreadable file or a failing command.
    pub(crate) fn push_error(&self, message: impl Into<String>) {
        self.responses.borrow_mut().push_back(Err(message.into()));
    }

    /// The number of responses still queued.
    pub(crate) fn remaining(&self) -> usize {
        self.responses.borrow().len()
    }
}

impl Observer for FakeObserver {
    /// Panics if the script is exhausted -- a test calling this more times
    /// than it scripted responses has a bug in its own setup, not a
    /// condition production code needs to handle.
    fn current_workers(&self) -> Result<u32> {
        match self.responses.borrow_mut().pop_front() {
            Some(Ok(workers)) => Ok(workers),
            Some(Err(message)) => Err(anyhow!(message)),
            None => panic!("FakeObserver script exhausted: no more scripted responses"),
        }
    }
}

#[cfg(test)]
mod observer_tests {
    use super::*;

    #[test]
    fn returns_scripted_worker_counts_in_order() {
        let observer = FakeObserver::new();
        observer.push_workers(2);
        observer.push_workers(5);
        assert_eq!(observer.current_workers().unwrap(), 2);
        assert_eq!(observer.current_workers().unwrap(), 5);
        assert_eq!(observer.remaining(), 0);
    }

    #[test]
    fn returns_scripted_errors() {
        let observer = FakeObserver::new();
        observer.push_error("simulated observer failure");
        let error = observer.current_workers().unwrap_err();
        assert_eq!(error.to_string(), "simulated observer failure");
    }

    #[test]
    fn interleaves_successes_and_failures_in_push_order() {
        let observer = FakeObserver::new();
        observer.push_workers(3);
        observer.push_error("boom");
        assert!(observer.current_workers().is_ok());
        assert!(observer.current_workers().is_err());
    }

    #[test]
    #[should_panic(expected = "FakeObserver script exhausted")]
    fn panics_when_the_script_runs_out() {
        let observer = FakeObserver::new();
        let _ = observer.current_workers();
    }
}
