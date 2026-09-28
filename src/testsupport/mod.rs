//! Test-only support shared across this crate's unit tests: an injectable
//! clock, a scripted in-memory quota source, and the fixture-safety scanner
//! required by plan.md section 14 requirement 9.
//!
//! Gated on `cfg(test)` so none of it ships in the `subgov` binary.

pub(crate) mod clock;
pub(crate) mod fake_fleet;
pub(crate) mod fake_source;
pub(crate) mod fixture_scan;
