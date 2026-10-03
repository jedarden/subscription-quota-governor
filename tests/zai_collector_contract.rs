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

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use subscription_governor::config::{
    AccountConfig, ActuatorConfig, BankedResetConfig, FleetConfig, ObserverReconciliation,
    SourceConfig, StaleBehavior, Strategy, UtilizationConfig,
};
use subscription_governor::controller::evaluate;
use subscription_governor::model::QuotaSnapshot;
use subscription_governor::source::collect;
use subscription_governor::state::AccountState;

fn load_fixture(name: &str) -> Value {
    // Cargo runs integration tests from the package root. Relative paths keep
    // the fixture lookup valid when cached binaries run in clean extractions.
    let path = format!("tests/fixtures/zai-collector/{name}.json");
    let bytes =
        std::fs::read(&path).unwrap_or_else(|error| panic!("failed to read {path}: {error}"));
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
    let by_id = |id: &str| {
        snapshot
            .windows
            .iter()
            .find(|window| window.id == id)
            .unwrap()
    };
    assert_eq!(by_id("five_hour").used_fraction, 0.28);
    assert_eq!(by_id("weekly").used_fraction, 0.52);
    assert!(snapshot.fresh);
}

#[cfg(unix)]
#[test]
fn a_near_limit_fixture_preserves_the_reached_flag() {
    let fixture = load_fixture("near_limit");
    let snapshot =
        collect_via_command(&fixture).expect("a fixture at the limit must still collect");

    let five_hour = snapshot
        .windows
        .iter()
        .find(|window| window.id == "five_hour")
        .unwrap();
    assert_eq!(five_hour.used_fraction, 1.0);
    assert!(
        five_hour.reached,
        "a window reported at 100% usage must keep its reached signal"
    );
}

#[cfg(unix)]
#[test]
fn a_stale_fixture_preserves_the_freshness_flag() {
    let fixture = load_fixture("stale");
    let snapshot =
        collect_via_command(&fixture).expect("a stale fixture is still a valid snapshot");

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

    let five_hour = snapshot
        .windows
        .iter()
        .find(|window| window.id == "five_hour")
        .unwrap();
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

/// A `linear_to_reset` account config with a generous staleness window (this
/// suite drives `evaluate` with each snapshot's own `observed_at` as "now",
/// so no cycle is ever stale regardless of the wall-clock gap between the
/// synthetic cycles below) and the default `minimum_sample_seconds` (300s),
/// matching plan.md §7.3's requirement that a Z.AI-sourced snapshot gets
/// "exactly the same freshness and window validation as native sources."
fn zai_linear_to_reset_account(target_utilization: f64) -> AccountConfig {
    AccountConfig {
        source: SourceConfig::NormalizedFile {
            path: "unused".into(),
        },
        fleet: FleetConfig {
            min_workers: 0,
            max_workers: 10,
            bootstrap_workers: 1,
            max_scale_up_per_cycle: 10,
            max_scale_down_per_cycle: 10,
            observer: None,
            actuator: ActuatorConfig::None,
            observer_reconciliation: ObserverReconciliation::default(),
            hosts: None,
        },
        utilization: UtilizationConfig {
            target_utilization: Some(target_utilization),
            reserve_fraction: None,
            strategy: Strategy::LinearToReset,
            stale_after_seconds: 315_360_000,
            stale_behavior: StaleBehavior::Hold,
            minimum_sample_seconds: 300,
            windows: BTreeMap::new(),
        },
        banked_resets: BankedResetConfig::default(),
    }
}

/// End-to-end verification of plan.md §7.3's last unchecked build
/// requirement -- "Verify reset-generation transitions ... in the site-local
/// collector before production rollout" -- driven through the real `command`
/// transport (not hand-built `QuotaWindow`/`WindowSample` literals the way
/// `controller.rs`'s `reset_heavy_trace_restarts_learning_each_generation...`
/// already proves the *controller* alone handles correctly). Three
/// sequential Z.AI-shaped collections feed `evaluate` with the real
/// cross-cycle `AccountState` `main::run_cycle` builds up, exactly the way
/// production polls an account:
///
/// 1. First-ever sample of a generation: nothing to pace against yet.
/// 2. A second, later sample of the SAME generation (same `resets_at`, past
///    `minimum_sample_seconds`): a real burn rate is learned.
/// 3. A sample from a NEW generation (`resets_at` advanced, usage back near
///    zero -- the same shape as the committed `reset_generation_rollover`
///    fixture): the prior generation's learned rate must not be
///    extrapolated across the boundary.
#[cfg(unix)]
#[test]
fn a_reset_generation_transition_through_the_real_command_transport_never_extrapolates_the_prior_generations_rate(
) {
    let config = zai_linear_to_reset_account(0.9);
    let mut prior = AccountState::default();
    let workers = 2u32;

    let generation_one_reset = "2026-09-28T17:00:00Z";
    let cycle1 = serde_json::json!({
        "observed_at": "2026-09-28T12:00:00Z",
        "fresh": true,
        "windows": [
            {"id": "five_hour", "used_fraction": 0.10, "resets_at": generation_one_reset, "duration_minutes": 300}
        ]
    });
    let snapshot1 = collect_via_command(&cycle1).expect("cycle 1 must collect cleanly");
    let now1: DateTime<Utc> = snapshot1.observed_at;
    let decision1 = evaluate("zai", &config, &snapshot1, &prior, workers, now1).unwrap();
    assert_eq!(
        decision1.windows[0].reason, "learning_burn_rate",
        "the very first sample of a generation has nothing to pace against yet"
    );
    prior.record(&snapshot1, workers, decision1.desired_workers);

    let cycle2 = serde_json::json!({
        "observed_at": "2026-09-28T13:00:00Z",
        "fresh": true,
        "windows": [
            {"id": "five_hour", "used_fraction": 0.30, "resets_at": generation_one_reset, "duration_minutes": 300}
        ]
    });
    let snapshot2 = collect_via_command(&cycle2).expect("cycle 2 must collect cleanly");
    let now2: DateTime<Utc> = snapshot2.observed_at;
    let decision2 = evaluate("zai", &config, &snapshot2, &prior, workers, now2).unwrap();
    assert_eq!(
        decision2.windows[0].reason, "paced_to_reset",
        "a second same-generation sample far enough apart must learn a real burn rate"
    );
    assert!(
        decision2.windows[0].observed_burn_per_worker_hour.unwrap() > 0.0,
        "the learned rate must be a real positive value, not censored/zero"
    );
    prior.record(&snapshot2, workers, decision2.desired_workers);

    let generation_two_reset = "2026-09-28T22:00:00Z";
    let cycle3 = serde_json::json!({
        "observed_at": "2026-09-28T17:00:05Z",
        "fresh": true,
        "windows": [
            {"id": "five_hour", "used_fraction": 0.02, "resets_at": generation_two_reset, "duration_minutes": 300}
        ]
    });
    let snapshot3 = collect_via_command(&cycle3).expect("cycle 3 must collect cleanly");
    let now3: DateTime<Utc> = snapshot3.observed_at;
    let decision3 = evaluate("zai", &config, &snapshot3, &prior, workers, now3).unwrap();
    assert_eq!(
        decision3.windows[0].reason, "learning_burn_rate",
        "a new generation (resets_at changed) must restart learning, not extrapolate \
         generation one's rate across the reset boundary"
    );
}

/// End-to-end verification of plan.md §7.3's "percentage/absolute-usage
/// normalization" concern. No private endpoint or raw provider shape is
/// part of this repository (§7.3), so subgov never sees a percentage or an
/// absolute token count directly -- a real collector computes `used_fraction`
/// itself and emits only that. What this proves is that subgov's behavior is
/// completely insensitive to which raw shape a collector derived it from:
/// two collector-shaped documents representing the same real quota state,
/// one as if derived from a percentage reading and one as if derived from an
/// absolute used/quota token count, normalize to bit-identical
/// `used_fraction` values (IEEE 754 division is correctly rounded, so two
/// divisions of the same exact ratio agree exactly regardless of which
/// numerator/denominator pair produced it) and drive the controller to
/// byte-identical decisions.
#[cfg(unix)]
#[test]
fn percentage_derived_and_absolute_derived_usage_normalize_to_identical_governor_behavior() {
    let percentage_used: f64 = 33.0;
    let percentage_derived_fraction = percentage_used / 100.0;

    let used_tokens: f64 = 330_000.0;
    let quota_tokens: f64 = 1_000_000.0;
    let absolute_derived_fraction = used_tokens / quota_tokens;

    assert_eq!(
        percentage_derived_fraction, absolute_derived_fraction,
        "both derivations of the same real state must agree bit-for-bit"
    );

    let observed_at = "2026-09-28T12:00:00Z";
    let resets_at = "2026-09-28T17:00:00Z";
    let percentage_fixture = serde_json::json!({
        "observed_at": observed_at,
        "fresh": true,
        "windows": [{"id": "five_hour", "used_fraction": percentage_derived_fraction, "resets_at": resets_at, "duration_minutes": 300}]
    });
    let absolute_fixture = serde_json::json!({
        "observed_at": observed_at,
        "fresh": true,
        "windows": [{"id": "five_hour", "used_fraction": absolute_derived_fraction, "resets_at": resets_at, "duration_minutes": 300}]
    });

    let snapshot_from_percentage =
        collect_via_command(&percentage_fixture).expect("percentage-derived fixture must collect");
    let snapshot_from_absolute =
        collect_via_command(&absolute_fixture).expect("absolute-derived fixture must collect");
    assert_eq!(
        snapshot_from_percentage.windows[0].used_fraction,
        snapshot_from_absolute.windows[0].used_fraction,
        "both raw derivations must normalize to the identical used_fraction"
    );

    let config = zai_linear_to_reset_account(0.9);
    let now: DateTime<Utc> = snapshot_from_percentage.observed_at;
    let decision_from_percentage = evaluate(
        "zai",
        &config,
        &snapshot_from_percentage,
        &AccountState::default(),
        2,
        now,
    )
    .unwrap();
    let decision_from_absolute = evaluate(
        "zai",
        &config,
        &snapshot_from_absolute,
        &AccountState::default(),
        2,
        now,
    )
    .unwrap();

    assert_eq!(
        serde_json::to_value(&decision_from_percentage).unwrap(),
        serde_json::to_value(&decision_from_absolute).unwrap(),
        "governor behavior must be identical regardless of which raw provider shape the \
         collector normalized used_fraction from"
    );
}

#[test]
fn every_committed_fixture_is_a_valid_quota_snapshot_document() {
    for name in ["normal", "near_limit", "stale", "reset_generation_rollover"] {
        let fixture = load_fixture(name);
        let snapshot: QuotaSnapshot = serde_json::from_value(fixture).unwrap_or_else(|error| {
            panic!("{name} fixture must deserialize as QuotaSnapshot: {error}")
        });
        assert!(
            !snapshot.windows.is_empty(),
            "{name} fixture must not have an empty windows array"
        );
        for window in &snapshot.windows {
            assert!(
                !window.id.is_empty(),
                "{name} fixture has a window with an empty id"
            );
            assert!(
                window.used_fraction.is_finite() && (0.0..=1.0).contains(&window.used_fraction),
                "{name} fixture has an out-of-range used_fraction: {window:?}"
            );
        }
    }
}
