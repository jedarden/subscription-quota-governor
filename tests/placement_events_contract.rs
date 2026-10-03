//! End-to-end §22.11 decision and metrics event contract for multi-host
//! placement. The binary runs against temporary normalized files and static
//! observers, so this exercises the actual JSONL surface without external
//! services or actuators.

use chrono::{Duration, Utc};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

#[test]
fn observe_only_cycle_reports_each_host_snapshot_headroom_eligibility_and_placement() {
    let dir = tempfile::tempdir().unwrap();
    let quota_path = dir.path().join("quota.json");
    let east_resource_path = dir.path().join("east-resource.json");
    let west_resource_path = dir.path().join("west-resource.json");
    let state_path = dir.path().join("state.json");
    let config_path = dir.path().join("governor.yaml");
    let now = Utc::now();

    write_json(
        &quota_path,
        &json!({
            "observed_at": now,
            "fresh": true,
            "windows": [{
                "id": "weekly",
                "used_fraction": 0.1,
                "resets_at": now + Duration::hours(4)
            }]
        }),
    );
    write_json(
        &east_resource_path,
        &json!({
            "observed_at": now,
            "fresh": true,
            "host_id": "east",
            "cpu_available_fraction": 0.7,
            "mem_available_mb": 28_672,
            "mem_total_mb": 32_768
        }),
    );
    write_json(
        &west_resource_path,
        &json!({
            "observed_at": now,
            "fresh": false,
            "host_id": "west",
            "cpu_available_fraction": 0.9,
            "mem_available_mb": 30_000,
            "mem_total_mb": 32_768
        }),
    );

    let yaml_path = |path: &Path| serde_json::to_string(path.to_str().unwrap()).unwrap();
    let config = format!(
        r#"version: 1
poll_interval_seconds: 300
state_path: {}
accounts:
  acct:
    source: {{ type: normalized_file, path: {} }}
    fleet:
      min_workers: 0
      max_workers: 6
      bootstrap_workers: 1
      max_scale_up_per_cycle: 10
      max_scale_down_per_cycle: 10
      actuator: {{ type: none }}
      hosts:
        east:
          observer: {{ type: static, workers: 1 }}
          resource_source: {{ type: normalized_file, path: {} }}
          resource_reserve: {{ cpu_reserve_fraction: 0.0, mem_reserve_mb: 0 }}
          actuator: {{ type: none }}
        west:
          observer: {{ type: static, workers: 2 }}
          resource_source: {{ type: normalized_file, path: {} }}
          resource_reserve: {{ cpu_reserve_fraction: 0.0, mem_reserve_mb: 0 }}
          actuator: {{ type: none }}
    utilization:
      target_utilization: 0.9
      strategy: ceiling_only
      stale_after_seconds: 300
"#,
        yaml_path(&state_path),
        yaml_path(&quota_path),
        yaml_path(&east_resource_path),
        yaml_path(&west_resource_path),
    );
    fs::write(&config_path, config).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_subgov"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "run",
            "--once",
            "--observe-only",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "subgov run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let decision = events
        .iter()
        .find(|event| event["event"] == "decision")
        .unwrap();
    let metrics = events
        .iter()
        .find(|event| event["event"] == "metrics")
        .unwrap();

    let hosts = decision["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 2);
    let by_id: BTreeMap<&str, &Value> = hosts
        .iter()
        .map(|host| (host["host_id"].as_str().unwrap(), host))
        .collect();
    assert_eq!(by_id["east"]["resource_snapshot"]["host_id"], "east");
    assert_eq!(by_id["east"]["headroom"], 0.7);
    assert_eq!(by_id["east"]["eligible"], true);
    assert_eq!(by_id["east"]["target_workers"], 4);
    assert_eq!(by_id["west"]["fresh"], false);
    assert_eq!(by_id["west"]["eligible"], false);
    assert_eq!(by_id["west"]["target_workers"], 2);
    assert_eq!(decision["decision"]["account"], "acct");

    let metric_hosts = metrics["hosts"].as_array().unwrap();
    assert_eq!(metric_hosts.len(), 2);
    let metric_by_id: BTreeMap<&str, &Value> = metric_hosts
        .iter()
        .map(|host| (host["host_id"].as_str().unwrap(), host))
        .collect();
    let cpu_used = metric_by_id["east"]["resource_utilization"]["cpu_used_fraction"]
        .as_f64()
        .unwrap();
    assert!((cpu_used - 0.3).abs() < f64::EPSILON);
    assert_eq!(
        metric_by_id["east"]["resource_utilization"]["memory_used_fraction"],
        0.125
    );
    assert_eq!(metric_by_id["east"]["placed_workers"], 4);
    assert_eq!(metric_by_id["west"]["placed_workers"], 2);
    assert_eq!(metrics["account"], "acct");
    assert_eq!(metrics["actuation_attempted"], false);
}
