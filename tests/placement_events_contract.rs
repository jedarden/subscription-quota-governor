//! End-to-end §22.11 decision and metrics event contract for multi-host
//! placement. The binary runs against temporary normalized files, a scripted
//! resource command, and static observers, so this exercises the actual JSONL
//! surface without external services or actuators.

use chrono::{Duration, Utc};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

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

    let saved_state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    assert_eq!(saved_state["schema_version"], 4);
    assert_eq!(
        saved_state["host_states"]["acct"]["east"]["placement_history"][0]["target_workers"],
        4
    );
    assert_eq!(
        saved_state["host_states"]["acct"]["west"]["placement_history"][0]["target_workers"],
        2
    );
}

#[cfg(unix)]
#[test]
fn failed_host_resource_source_mid_run_holds_that_host_and_fills_the_healthy_ceiling() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let quota_path = dir.path().join("quota.json");
    let east_resource_path = dir.path().join("east-resource.json");
    let west_resource_script = dir.path().join("west-resource.sh");
    let west_calls_path = dir.path().join("west-resource.sh.calls");
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

    // The west source succeeds for the first complete cycle, then fails on
    // every poll after that. This makes the second decision exercise the
    // missing-resource path produced by a real source error, rather than a
    // snapshot that was already stale before the run began.
    let west_snapshot = serde_json::to_string(&json!({
        "observed_at": now,
        "fresh": true,
        "host_id": "west",
        "cpu_available_fraction": 0.9,
        "mem_available_mb": 30_000,
        "mem_total_mb": 32_768
    }))
    .unwrap();
    fs::write(
        &west_resource_script,
        format!(
            "#!/bin/sh\ncalls=0\nif [ -e \"$0.calls\" ]; then calls=$(cat \"$0.calls\"); fi\ncalls=$((calls + 1))\nprintf '%s\\n' \"$calls\" > \"$0.calls\"\nif [ \"$calls\" -gt 1 ]; then exit 23; fi\nprintf '%s\\n' '{west_snapshot}'\n"
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&west_resource_script).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&west_resource_script, permissions).unwrap();

    let yaml_path = |path: &Path| serde_json::to_string(path.to_str().unwrap()).unwrap();
    let config = format!(
        r#"version: 1
poll_interval_seconds: 1
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
          max_workers: 4
          observer: {{ type: static, workers: 1 }}
          resource_source: {{ type: normalized_file, path: {} }}
          resource_reserve: {{ cpu_reserve_fraction: 0.0, mem_reserve_mb: 0 }}
          actuator: {{ type: none }}
        west:
          max_workers: 2
          observer: {{ type: static, workers: 2 }}
          resource_source:
            type: command
            argv: [{}]
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
        yaml_path(&west_resource_script),
    );
    fs::write(&config_path, config).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_subgov"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "run",
            "--observe-only",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Observe the script's second invocation instead of relying on a fixed
    // sleep. Give the cycle time to emit its decision, then interrupt the
    // long-running command cleanly so its buffered JSONL output is retained.
    let deadline = Instant::now() + StdDuration::from_secs(15);
    let mut source_failed_mid_run = false;
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        let calls = fs::read_to_string(&west_calls_path)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(0);
        if calls >= 2 {
            source_failed_mid_run = true;
            thread::sleep(StdDuration::from_millis(250));
            break;
        }
        thread::sleep(StdDuration::from_millis(20));
    }

    if child.try_wait().unwrap().is_none() {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.id() as i32),
            nix::sys::signal::Signal::SIGINT,
        )
        .unwrap();
    }
    let shutdown_deadline = Instant::now() + StdDuration::from_secs(5);
    while child.try_wait().unwrap().is_none() && Instant::now() < shutdown_deadline {
        thread::sleep(StdDuration::from_millis(20));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        source_failed_mid_run,
        "west resource source was not polled a second time; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "subgov run did not shut down cleanly: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let events: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let decisions: Vec<&Value> = events
        .iter()
        .filter(|event| event["event"] == "decision")
        .collect();
    assert_eq!(
        decisions.len(),
        2,
        "expected the initial and degraded cycles"
    );

    let first_hosts: BTreeMap<&str, &Value> = decisions[0]["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|host| (host["host_id"].as_str().unwrap(), host))
        .collect();
    assert_eq!(first_hosts["east"]["eligible"], true);
    assert_eq!(first_hosts["west"]["eligible"], true);

    let degraded_hosts: BTreeMap<&str, &Value> = decisions[1]["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|host| (host["host_id"].as_str().unwrap(), host))
        .collect();
    assert_eq!(degraded_hosts.len(), 2);
    assert_eq!(decisions[1]["decision"]["desired_workers"], 6);
    assert_eq!(degraded_hosts["east"]["fresh"], true);
    assert_eq!(degraded_hosts["east"]["eligible"], true);
    // east is configured with max_workers: 4, so it absorbs the remaining
    // account target to its own ceiling while west stays frozen at two.
    assert_eq!(degraded_hosts["east"]["target_workers"], 4);
    assert_eq!(degraded_hosts["west"]["resource_snapshot"], Value::Null);
    assert_eq!(degraded_hosts["west"]["fresh"], false);
    assert_eq!(degraded_hosts["west"]["eligible"], false);
    assert_eq!(degraded_hosts["west"]["current_workers"], 2);
    assert_eq!(degraded_hosts["west"]["target_workers"], 2);

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.lines().any(|line| {
            serde_json::from_str::<Value>(line).is_ok_and(|event| {
                event["event"] == "host_resource_error"
                    && event["account"] == "acct"
                    && event["host_id"] == "west"
            })
        }),
        "expected a west-only resource observation error; stderr: {stderr}"
    );
}
