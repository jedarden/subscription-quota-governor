//! Contract tests for the Claude Code / Anthropic usage normalizer (plan.md
//! §7.1), driven against recorded fixtures in
//! `tests/fixtures/anthropic-usage/` instead of a live `/api/oauth/usage`
//! endpoint. Covers the legacy, generic, scoped (same-ID override), null
//! (inactive/omitted window), and forward-compatible (unrecognized `kind`
//! and unknown top-level fields) payload shapes named in that section's
//! build requirements.
//!
//! Every fixture is anonymized synthetic data -- no real account, token, or
//! infrastructure identifiers -- and is swept by the fixture-safety scanner
//! (`src/testsupport/fixture_scan.rs`'s `real_fixtures_are_clean` test)
//! alongside every other file under `tests/fixtures/`.
//!
//! Tests go through the same public entry point production code uses
//! (`subscription_governor::source::parse_anthropic_usage`), which is also
//! what `collect_anthropic` calls once it has a decoded response body.

use chrono::{DateTime, Utc};
use serde_json::Value;
use subscription_governor::source::parse_anthropic_usage;

fn observed() -> DateTime<Utc> {
    "2026-09-28T12:00:00Z".parse().unwrap()
}

fn load_fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/anthropic-usage/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("failed to read {path}: {error}"));
    serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("failed to parse {path}: {error}"))
}

#[test]
fn legacy_payload_normalizes_all_three_named_windows() {
    let payload = load_fixture("legacy");
    let snapshot = parse_anthropic_usage(&payload, observed()).expect("legacy fixture should parse");

    assert_eq!(snapshot.windows.len(), 3);
    let by_id = |id: &str| snapshot.windows.iter().find(|window| window.id == id).unwrap();
    assert_eq!(by_id("five_hour").used_fraction, 0.225);
    assert_eq!(by_id("seven_day").used_fraction, 0.61);
    assert_eq!(by_id("weekly_scoped").used_fraction, 0.0825);
}

#[test]
fn generic_payload_normalizes_limits_array_entries() {
    let payload = load_fixture("generic");
    let snapshot = parse_anthropic_usage(&payload, observed()).expect("generic fixture should parse");

    assert_eq!(snapshot.windows.len(), 2);
    let by_id = |id: &str| snapshot.windows.iter().find(|window| window.id == id).unwrap();
    assert_eq!(by_id("five_hour").used_fraction, 0.335);
    assert_eq!(by_id("seven_day").used_fraction, 0.70);
}

#[test]
fn scoped_payload_prefers_the_generic_limit_over_the_same_id_legacy_field() {
    let payload = load_fixture("scoped");
    let snapshot = parse_anthropic_usage(&payload, observed()).expect("scoped fixture should parse");

    // weekly_scoped appears both as a legacy field (12.0%) and a generic
    // limit (46.5%) with the same id; the generic limit must win, per
    // plan.md §7.1: "Prefer a generic limit over a same-ID legacy field."
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].id, "weekly_scoped");
    assert_eq!(snapshot.windows[0].used_fraction, 0.465);
}

#[test]
fn null_payload_skips_null_and_inactive_windows_without_failing_the_snapshot() {
    let payload = load_fixture("null");
    let snapshot = parse_anthropic_usage(&payload, observed()).expect("null fixture should parse");

    // five_hour is JSON null and weekly_scoped is explicitly inactive; only
    // seven_day should survive, per plan.md §7.1: "Accept null/inactive
    // legacy windows without failing the entire account."
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].id, "seven_day");
    assert_eq!(snapshot.windows[0].used_fraction, 0.05);
}

#[test]
fn forward_compatible_payload_normalizes_an_unrecognized_limit_kind() {
    let payload = load_fixture("forward_compatible");
    let snapshot =
        parse_anthropic_usage(&payload, observed()).expect("forward-compatible fixture should parse");

    // "monthly_bonus_pool" is not one of the three hardcoded legacy names,
    // and the payload carries unknown top-level fields (account_tier,
    // future_feature_flags) that must be silently ignored rather than
    // breaking parsing, per plan.md §6.3.
    assert_eq!(snapshot.windows.len(), 2);
    let by_id = |id: &str| snapshot.windows.iter().find(|window| window.id == id).unwrap();
    assert_eq!(by_id("five_hour").used_fraction, 0.18);
    assert_eq!(by_id("monthly_bonus_pool").used_fraction, 0.40);
}

#[test]
fn every_fixture_produces_only_finite_in_range_used_fractions() {
    for name in ["legacy", "generic", "scoped", "null", "forward_compatible"] {
        let payload = load_fixture(name);
        let snapshot = parse_anthropic_usage(&payload, observed())
            .unwrap_or_else(|error| panic!("{name} fixture should parse: {error}"));
        for window in &snapshot.windows {
            assert!(
                window.used_fraction.is_finite() && (0.0..=1.0).contains(&window.used_fraction),
                "{name} fixture produced an out-of-range used_fraction: {window:?}"
            );
            assert!(!window.id.is_empty(), "{name} fixture produced an empty window id");
        }
    }
}
