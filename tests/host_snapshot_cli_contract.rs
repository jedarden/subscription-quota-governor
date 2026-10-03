//! CLI contract for per-host snapshots and the backward-compatible
//! single-host decision event shape (§22.12).

use chrono::{Duration, Utc};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Command;

fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap()
}

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

#[cfg(unix)]
#[test]
fn snapshot_host_prints_only_the_requested_resource_snapshot_without_observing_or_actuating() {
    let dir = tempfile::tempdir().unwrap();
    let resource_path = dir.path().join("east-resource.json");
    let config_path = dir.path().join("governor.yaml");
    let west_called = dir.path().join("west-called");
    let observer_called = dir.path().join("observer-called");
    let actuator_called = dir.path().join("actuator-called");
    let resource = json!({
        "observed_at": Utc::now(),
        "fresh": true,
        "host_id": "east",
        "cpu_available_fraction": 0.42,
        "mem_available_mb": 12288,
        "mem_total_mb": 65536
    });
    write_json(&resource_path, &resource);

    let config = format!(
        "version: 1\naccounts:\n  acct:\n    source: {{ type: normalized_file, path: {} }}\n    utilization: {{ target_utilization: 0.9 }}\n    fleet:\n      min_workers: 0\n      max_workers: 4\n      hosts:\n        east:\n          observer: {{ type: command, argv: [\"/bin/sh\", \"-c\", {}] }}\n          actuator: {{ type: command, argv: [\"/bin/sh\", \"-c\", {}] }}\n          resource_source: {{ type: normalized_file, path: {} }}\n          resource_reserve: {{ cpu_reserve_fraction: 0.0, mem_reserve_mb: 0 }}\n        west:\n          observer: {{ type: static, workers: 0 }}\n          resource_source: {{ type: command, argv: [\"/bin/sh\", \"-c\", {}] }}\n          resource_reserve: {{ cpu_reserve_fraction: 0.0, mem_reserve_mb: 0 }}\n",
        yaml_string("missing-quota.json"),
        yaml_string(&format!("touch '{}'", observer_called.display())),
        yaml_string(&format!(
            "touch '{}'; : {{desired_workers}}",
            actuator_called.display()
        )),
        yaml_string(resource_path.to_str().unwrap()),
        yaml_string(&format!("touch '{}'", west_called.display())),
    );
    fs::write(&config_path, config).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_subgov"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "snapshot",
            "acct",
            "--host",
            "east",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "snapshot --host failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(printed, resource);
    assert!(!west_called.exists(), "the unselected host source was read");
    assert!(
        !observer_called.exists(),
        "snapshot queried a worker observer"
    );
    assert!(!actuator_called.exists(), "snapshot invoked an actuator");
}

#[test]
fn single_host_decision_keeps_the_existing_event_shape() {
    let dir = tempfile::tempdir().unwrap();
    let quota_path = dir.path().join("quota.json");
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
    let config = format!(
        "version: 1\nstate_path: {}\naccounts:\n  acct:\n    source: {{ type: normalized_file, path: {} }}\n    utilization: {{ target_utilization: 0.9, strategy: ceiling_only }}\n    fleet: {{ min_workers: 0, max_workers: 2, observer: {{ type: static, workers: 1 }}, actuator: {{ type: none }} }}\n",
        yaml_string(dir.path().join("state.json").to_str().unwrap()),
        yaml_string(quota_path.to_str().unwrap()),
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
        "run --once failed: {}",
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
    let mut keys: Vec<_> = decision
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["actuated", "decision", "event", "observe_only"]);
}
