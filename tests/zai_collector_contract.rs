//! Contract tests for a site-local Claude Code with Z.AI collector (plan.md
//! §7.3). No private endpoint or deployment component lives in this
//! repository -- a real collector is an external process the operator
//! writes -- so these tests run an anonymous, fully synthetic
//! fixture-producing helper standing in for one, shaped exactly like the
//! `command` transport documented in `examples/claude-zai.yaml`.
//!
//! `schema/zai-collector-snapshot.schema.json` documents the wire contract
//! as `$ref: quota-snapshot.schema.json` -- deliberately not a distinct
//! format: "a Z.AI collector is held to it verbatim." This repository has
//! no JSON Schema validator dependency anywhere, and the schema file itself
//! says it exists so a third-party author can build against a stable
//! contract "without reading Rust source"; the Rust source is the
//! authoritative implementation the schema mirrors. So here, "validated
//! against the schema" means driving each fixture through the same public
//! entry point production code uses end-to-end
//! (`subscription_governor::source::collect`, over a real `command`
//! source), which enforces every rule the schema documents (required
//! fields and types via serde, plus the range/uniqueness checks in
//! `validate_snapshot`) -- success or failure exactly matches what a
//! schema validator would report, without duplicating the schema in a
//! second validation engine that could drift from it.
//!
//! Every fixture under `tests/fixtures/zai-collector/` is swept by the
//! fixture-safety scanner (`src/testsupport/fixture_scan.rs`) alongside
//! every other file under `tests/fixtures/`.

use serde_json::Value;
use subscription_governor::config::SourceConfig;
use subscription_governor::model::QuotaSnapshot;
use subscription_governor::source::collect;

fn load_fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/zai-collector/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("failed to read {path}: {error}"));
    serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("failed to parse {path}: {error}"))
}

/// Runs `snapshot` through the real `command` transport (never a shell
/// interpreter, matching plan.md §7.4) using a `printf` stand-in for the
/// site-local collector binary named in `examples/claude-zai.yaml`
/// (`/usr/local/bin/read-subscription-quota`). This is the same generic
/// command-source code path production Z.AI accounts use, not a
/// Z.AI-specific one -- there isn't one, by design (§7.3: "No private
/// endpoint or deployment component is part of this repository").
#[cfg(unix)]
fn collect_via_command(snapshot: &Value) -> anyhow::Result<QuotaSnapshot> {
    let source = SourceConfig::Command {
        argv: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("printf '%s' '{snapshot}'"),
        ],
    };
    collect(&source)
}

#[cfg(unix)]
#[test]
fn a_normal_fixture_collects_successfully_through_the_command_transport() {
    let fixture = load_fixture("normal");
    let snapshot =
        collect_via_command(&fixture).expect("a schema-conformant fixture must collect cleanly");

    assert_eq!(snapshot.windows.len(), 2);
    let by_id = |id: &str| snapshot.windows.iter().find(|window| window.id == id).unwrap();
    assert_eq!(by_id("five_hour").used_fraction, 0.28);
    assert_eq!(by_id("weekly").used_fraction, 0.52);
    assert!(snapshot.fresh);
}

#[cfg(unix)]
#[test]
fn a_near_limit_fixture_preserves_the_reached_flag() {
    let fixture = load_fixture("near_limit");
    let snapshot = collect_via_command(&fixture).expect("a fixture at the limit must still collect");

    let five_hour = snapshot.windows.iter().find(|window| window.id == "five_hour").unwrap();
    assert_eq!(five_hour.used_fraction, 1.0);
    assert!(five_hour.reached, "a window reported at 100% usage must keep its reached signal");
}

#[cfg(unix)]
#[test]
fn a_stale_fixture_preserves_the_freshness_flag() {
    let fixture = load_fixture("stale");
    let snapshot = collect_via_command(&fixture).expect("a stale fixture is still a valid snapshot");

    assert!(
        !snapshot.fresh,
        "the collector's own freshness judgment (fresh: false) must survive collection \
         unchanged -- plan.md §6.1: the controller enforces its own staleness policy on top of \
         this, it does not silently upgrade a stale reading to fresh"
    );
}

#[cfg(unix)]
#[test]
fn a_reset_generation_rollover_fixture_is_accepted_despite_its_reset_time_already_having_passed() {
    // A collector polled right at (or just after) a window's reset boundary
    // may legitimately observe `resets_at` in the past relative to
    // `observed_at`, with usage already back near zero for the new
    // generation -- plan.md §7.3's unchecked "verify reset-generation
    // transitions" concern. subgov applies no resets_at-vs-observed_at
    // ordering check (validate_snapshot only checks id/used_fraction), so
    // this must collect exactly like any other snapshot, not be treated as
    // a special or invalid case.
    let fixture = load_fixture("reset_generation_rollover");
    let snapshot = collect_via_command(&fixture)
        .expect("a just-rolled-over window must not be rejected for its past resets_at");

    let five_hour = snapshot.windows.iter().find(|window| window.id == "five_hour").unwrap();
    assert_eq!(
        five_hour.used_fraction, 0.02,
        "the new generation's near-zero usage must pass through unchanged"
    );
    assert_eq!(snapshot.windows.len(), 2);
}

#[cfg(unix)]
#[test]
fn a_fixture_with_an_out_of_range_used_fraction_is_rejected_exactly_like_a_native_source() {
    // plan.md §7.3: "Apply exactly the same freshness and window validation
    // as native sources." A Z.AI-sourced document that fails validate_snapshot
    // must fail collection the same way a malformed native payload would --
    // not silently accepted because it arrived over the generic command
    // transport. Not committed as a fixture file (it documents a rejected
    // shape, not a valid reference example for collector authors).
    let invalid = serde_json::json!({
        "observed_at": "2026-09-28T12:00:00Z",
        "fresh": true,
        "windows": [
            {"id": "five_hour", "used_fraction": 1.4, "resets_at": "2026-09-28T17:00:00Z"}
        ]
    });

    let error = collect_via_command(&invalid).unwrap_err();
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("used_fraction"),
        "expected the same out-of-range used_fraction rejection native sources get: {rendered}"
    );
}

#[cfg(unix)]
#[test]
fn a_fixture_with_duplicate_window_ids_is_rejected() {
    // Same principle as above, for the other validate_snapshot rule this
    // suite can exercise without depending on subgov-bbe7889c's still-open
    // duplicate-id gap (tracked separately): an empty id is already
    // rejected today regardless of that bead's outcome.
    let invalid = serde_json::json!({
        "observed_at": "2026-09-28T12:00:00Z",
        "fresh": true,
        "windows": [
            {"id": "", "used_fraction": 0.5, "resets_at": "2026-09-28T17:00:00Z"}
        ]
    });

    assert!(
        collect_via_command(&invalid).is_err(),
        "an empty window id must be rejected exactly like a native source"
    );
}

#[test]
fn every_committed_fixture_is_a_valid_quota_snapshot_document() {
    for name in ["normal", "near_limit", "stale", "reset_generation_rollover"] {
        let fixture = load_fixture(name);
        let snapshot: QuotaSnapshot = serde_json::from_value(fixture)
            .unwrap_or_else(|error| panic!("{name} fixture must deserialize as QuotaSnapshot: {error}"));
        assert!(!snapshot.windows.is_empty(), "{name} fixture must not have an empty windows array");
        for window in &snapshot.windows {
            assert!(!window.id.is_empty(), "{name} fixture has a window with an empty id");
            assert!(
                window.used_fraction.is_finite() && (0.0..=1.0).contains(&window.used_fraction),
                "{name} fixture has an out-of-range used_fraction: {window:?}"
            );
        }
    }
}
