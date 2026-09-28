//! Contract tests for the §22.4/§22.5 resource-source collector, driven
//! against recorded fixtures in `tests/fixtures/resource-source/` instead of
//! a live resource-probe script. Mirrors the Anthropic/Codex per-source
//! contract-test pattern (`tests/anthropic_usage_contract.rs`,
//! `tests/codex_app_server_contract.rs`): a dedicated fixture directory, one
//! fixture per shape under test, and every fixture swept by the
//! fixture-safety scanner (`src/testsupport/fixture_scan.rs`'s
//! `real_fixtures_are_clean` test) alongside every other file under
//! `tests/fixtures/`.
//!
//! Tests go through the same public entry point production code uses
//! (`subscription_governor::source::collect_resource`), pointed at the
//! `normalized_file` transport so no network or child process is involved.

use subscription_governor::config::SourceConfig;
use subscription_governor::source::collect_resource;

fn fixture_source(name: &str) -> SourceConfig {
    let path = format!(
        "{}/tests/fixtures/resource-source/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    )
    .into();
    SourceConfig::NormalizedFile { path }
}

#[test]
fn a_valid_snapshot_is_collected() {
    let snapshot = collect_resource(&fixture_source("valid")).expect("valid fixture should parse");
    assert_eq!(snapshot.host_id, "lab");
    assert_eq!(snapshot.cpu_available_fraction, 0.42);
    assert_eq!(snapshot.mem_available_mb, 12288);
    assert_eq!(snapshot.mem_total_mb, 65536);
    assert!(snapshot.fresh);
}

/// §22.4: "A missing or malformed field fails the snapshot, same as §6.2."
#[test]
fn a_missing_required_field_fails_the_snapshot() {
    let result = collect_resource(&fixture_source("missing_field"));
    assert!(
        result.is_err(),
        "a snapshot missing mem_total_mb must not be collected"
    );
}

/// §22.4: "cpu_available_fraction is finite and inclusive in [0, 1]."
#[test]
fn an_out_of_range_cpu_available_fraction_fails_the_snapshot() {
    let result = collect_resource(&fixture_source("out_of_range_cpu_fraction"));
    assert!(
        result.is_err(),
        "cpu_available_fraction: 1.5 is outside [0, 1] and must be rejected"
    );
}

/// §22.4: "mem_available_mb <= mem_total_mb."
#[test]
fn mem_available_exceeding_mem_total_fails_the_snapshot() {
    let result = collect_resource(&fixture_source("mem_available_exceeds_total"));
    assert!(
        result.is_err(),
        "mem_available_mb (100000) > mem_total_mb (65536) must be rejected"
    );
}
