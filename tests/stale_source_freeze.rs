//! Regression coverage for the source-freshness safety boundary.
//!
//! A collector outage may leave the last normalized snapshot on disk (or in a
//! supervised source cache).  Every provider pool must therefore treat the
//! snapshot's own `observed_at`/`fresh` metadata as controller input, refuse
//! scale-up while it is stale, and expose the condition to operators.

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use subscription_governor::config::{
    AccountConfig, ActuatorConfig, BankedResetConfig, FleetConfig, ObserverReconciliation,
    SourceConfig, StaleBehavior, Strategy, UtilizationConfig, WorkerObserverConfig,
};
use subscription_governor::controller::evaluate;
use subscription_governor::model::QuotaSnapshot;
use subscription_governor::source::{collect, parse_anthropic_usage, parse_codex_rate_limits};
use subscription_governor::state::AccountState;

const STALE_AFTER_SECONDS: u64 = 900;

fn account_config() -> AccountConfig {
    AccountConfig {
        source: SourceConfig::NormalizedFile {
            path: PathBuf::from("unused-snapshot.json"),
        },
        fleet: FleetConfig {
            min_workers: 0,
            max_workers: 8,
            bootstrap_workers: 1,
            max_scale_up_per_cycle: 8,
            max_scale_down_per_cycle: 8,
            observer: Some(WorkerObserverConfig::Static { workers: 3 }),
            actuator: ActuatorConfig::None,
            observer_reconciliation: ObserverReconciliation::default(),
            hosts: None,
            takeover: Vec::new(),
            takeover_helper: None,
        },
        utilization: UtilizationConfig {
            target_utilization: Some(0.9),
            reserve_fraction: None,
            strategy: Strategy::CeilingOnly,
            stale_after_seconds: STALE_AFTER_SECONDS,
            stale_behavior: StaleBehavior::Hold,
            minimum_sample_seconds: 60,
            backpressure: None,
            zai_saturation: None,
            windows: BTreeMap::new(),
        },
        banked_resets: BankedResetConfig::default(),
    }
}

fn stale_times() -> (DateTime<Utc>, DateTime<Utc>) {
    let now = Utc::now();
    (
        now - Duration::seconds((STALE_AFTER_SECONDS + 60) as i64),
        now,
    )
}

fn assert_stale_freeze(provider: &str, snapshot: QuotaSnapshot, now: DateTime<Utc>) {
    let config = account_config();
    let decision = evaluate(
        provider,
        &config,
        &snapshot,
        &AccountState::default(),
        3,
        now,
    )
    .unwrap_or_else(|error| panic!("{provider} stale snapshot should still evaluate: {error}"));

    assert!(decision.stale, "{provider} must be classified as stale");
    assert_eq!(decision.desired_workers, 3, "{provider} must not scale up");
    assert!(
        decision.windows.is_empty(),
        "{provider} must skip quota-window evaluation while stale"
    );
}

#[cfg(unix)]
#[test]
fn stale_observed_at_freezes_claude_zai_codex_and_anthropic_pools() {
    let (observed_at, now) = stale_times();
    let reset = (now + Duration::hours(4)).timestamp();

    // Claude Code with Z.AI uses the generic command/file normalized contract.
    // Keep this on the real `source::collect` path so the source cannot
    // accidentally refresh or discard stale metadata at the transport edge.
    let zai_document = json!({
        "observed_at": observed_at,
        "fresh": true,
        "windows": [{
            "id": "five_hour",
            "used_fraction": 0.1,
            "resets_at": now + Duration::hours(4),
            "duration_minutes": 300
        }]
    });
    let zai_source = SourceConfig::Command {
        argv: vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!("printf '%s' '{}'", zai_document),
        ],
    };
    let zai = collect(&zai_source).expect("the synthetic Z.AI collector must return its cache");
    assert_eq!(zai.observed_at, observed_at);
    assert_stale_freeze("claude-zai", zai, now);

    // Codex's app-server adapter supplies the observation time from the time
    // its full read/notification was received.  Feed that adapter's parser an
    // old timestamp to prove the controller receives and honors it.
    let codex = parse_codex_rate_limits(
        &json!({
            "rateLimitsByLimitId": {
                "codex": {
                    "primary": {
                        "usedPercent": 10.0,
                        "windowDurationMins": 300,
                        "resetsAt": reset
                    }
                }
            }
        }),
        observed_at,
    )
    .expect("the synthetic Codex rate-limit response must parse");
    assert_eq!(codex.observed_at, observed_at);
    assert_stale_freeze("codex", codex, now);

    // Anthropic's OAuth usage adapter likewise passes its collection time
    // through the normalized snapshot into the same controller gate.
    let anthropic = parse_anthropic_usage(
        &json!({
            "five_hour": {
                "utilization": 10.0,
                "resets_at": (now + Duration::hours(4)).to_rfc3339(),
                "is_active": true
            }
        }),
        observed_at,
    )
    .expect("the synthetic Anthropic usage response must parse");
    assert_eq!(anthropic.observed_at, observed_at);
    assert_stale_freeze("claude-anthropic", anthropic, now);
}

fn yaml_string(path: &Path) -> String {
    serde_json::to_string(path.to_str().expect("temporary paths are UTF-8")).unwrap()
}

#[cfg(unix)]
#[test]
fn one_stale_cached_source_tick_writes_stale_status_and_skips_actuation() {
    let temp = tempfile::tempdir().unwrap();
    let snapshot_path = temp.path().join("zai-ready-demand.json");
    let state_path = temp.path().join("state.json");
    let target_path = temp.path().join("target");
    let config_path = temp.path().join("governor.yaml");
    let now = Utc::now();
    let observed_at = now - Duration::seconds((STALE_AFTER_SECONDS + 60) as i64);

    // This represents the last successful publisher write after the collector
    // stopped.  The file is deliberately still valid, so the governor can
    // classify it as stale rather than collapsing the condition into a source
    // parse failure.
    fs::write(
        &snapshot_path,
        json!({
            "observed_at": observed_at,
            "fresh": true,
            "windows": [{
                "id": "five_hour",
                "used_fraction": 0.1,
                "resets_at": now + Duration::hours(4),
                "duration_minutes": 300
            }]
        })
        .to_string(),
    )
    .unwrap();

    fs::write(
        &config_path,
        format!(
            "version: 1\nstate_path: {}\naccounts:\n  claude-zai:\n    source: {{ type: normalized_file, path: {} }}\n    fleet:\n      min_workers: 0\n      max_workers: 8\n      bootstrap_workers: 1\n      max_scale_up_per_cycle: 8\n      max_scale_down_per_cycle: 8\n      observer: {{ type: static, workers: 3 }}\n      actuator: {{ type: target_file, path: {} }}\n    utilization:\n      target_utilization: 0.9\n      strategy: ceiling_only\n      stale_after_seconds: {}\n      stale_behavior: hold\n",
            yaml_string(&state_path),
            yaml_string(&snapshot_path),
            yaml_string(&target_path),
            STALE_AFTER_SECONDS,
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_subgov"))
        .args(["--config", config_path.to_str().unwrap(), "run", "--once"])
        .output()
        .expect("subgov must run the one-tick stale-source scenario");
    assert!(
        output.status.success(),
        "stale input is a safe successful cycle; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let decision = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event.get("event") == Some(&Value::String("decision".to_owned())))
        .expect("one tick must emit a decision event");
    assert_eq!(decision["decision"]["stale"], true);
    assert_eq!(decision["decision"]["current_workers"], 3);
    assert_eq!(decision["decision"]["desired_workers"], 3);
    assert_eq!(decision["decision"]["windows"], json!([]));

    let status: Value = serde_json::from_slice(&fs::read(temp.path().join("status.json")).unwrap())
        .expect("one completed tick must write status.json");
    assert_eq!(status["claude-zai"]["state"], "stale_drain");
    assert_eq!(status["claude-zai"]["reason"], "stale_data_holding");
    assert_ne!(status["claude-zai"]["state"], "healthy_learning");
    assert_ne!(status["claude-zai"]["reason"], "no_observed_burn");
    assert!(
        !target_path.exists(),
        "stale input must not invoke scale-up actuation"
    );
}
