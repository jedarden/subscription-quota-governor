//! The additive telemetry contract must survive the real normalized transport.
use serde_json::json;
use subscription_governor::config::SourceConfig;
use subscription_governor::source::collect;

#[test]
fn saturation_transport_checks_ranges_counts_and_old_payloads() {
    let base = json!({"observed_at":"2026-10-12T12:00:00Z", "windows":[{
        "id":"five_hour","used_fraction":0.2,"resets_at":"2026-10-12T16:00:00Z"
    }]});
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.json");
    let source = SourceConfig::NormalizedFile { path: path.clone() };
    std::fs::write(&path, serde_json::to_vec(&base).unwrap()).unwrap();
    let older = collect(&source).unwrap();
    assert!(older.saturation.is_none());
    assert!(serde_json::to_value(older)
        .unwrap()
        .get("saturation")
        .is_none());
    for (signal, valid) in [
        (
            json!({"completed_requests":90,"attempts":100,"window_seconds":300}),
            true,
        ),
        (
            json!({"completed_requests":90,"attempts":100,"window_seconds":300,"upstream_concurrency_fraction":0.1}),
            true,
        ),
        (
            json!({"completed_requests":0,"attempts":0,"window_seconds":300,"upstream_concurrency_fraction":null}),
            true,
        ),
        (
            json!({"completed_requests":101,"attempts":100,"window_seconds":300}),
            false,
        ),
        (
            json!({"completed_requests":90,"attempts":100,"window_seconds":300,"upstream_concurrency_fraction":1.1}),
            false,
        ),
        (
            json!({"completed_requests":90,"attempts":100,"window_seconds":300,"upstream_concurrency_fraction":-0.1}),
            false,
        ),
        (
            json!({"completed_requests":-1,"attempts":100,"window_seconds":300}),
            false,
        ),
        (json!({"attempts":100,"window_seconds":300}), false),
    ] {
        let mut payload = base.clone();
        payload["saturation"] = signal.clone();
        std::fs::write(&path, serde_json::to_vec(&payload).unwrap()).unwrap();
        assert_eq!(collect(&source).is_ok(), valid, "{signal}");
    }
}

#[cfg(unix)]
#[test]
fn saturation_live_command_timestamps_are_evaluated_after_worker_observation() {
    use std::process::Command;

    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("governor.yaml");
    let state_path = dir.path().join("state.json");
    // Produce real wall-clock timestamps inside the command, after the cycle
    // has started. No frozen timestamp or injected controller clock is used.
    let source_script = r#"
import datetime,json,time
time.sleep(0.05)
now=datetime.datetime.now(datetime.timezone.utc)
print(json.dumps({
    "observed_at":now.isoformat(),
    "windows":[{"id":"five_hour","used_fraction":0.1,
        "resets_at":(now+datetime.timedelta(hours=5)).isoformat()}],
    "eligible_backlog_capacity":4,
    "saturation":{"completed_requests":60,"attempts":70,"window_seconds":300}
}))
"#;
    let source_argv = serde_json::to_string(&["python3", "-c", source_script]).unwrap();
    // An evaluation time taken immediately after quota collection would also
    // give the wrong sample age; wait in the worker observation to catch it.
    let observer_argv =
        serde_json::to_string(&["python3", "-c", "import time; time.sleep(1.1); print(1)"])
            .unwrap();
    let config = format!(
        r#"version: 1
state_path: {}
accounts:
  zai:
    source:
      type: command
      argv: {}
    fleet:
      max_workers: 4
      observer:
        type: command
        argv: {}
      actuator: {{type: none}}
    utilization:
      reserve_fraction: 0.1
      strategy: linear_to_reset
      zai_saturation: {{}}
"#,
        serde_json::to_string(&state_path).unwrap(),
        source_argv,
        observer_argv
    );
    std::fs::write(&config_path, config).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_subgov"))
        .arg("--config")
        .arg(&config_path)
        .args(["run", "--once", "--observe-only"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let decision = events
        .iter()
        .find(|event| event["event"] == "decision")
        .unwrap();
    assert_eq!(decision["decision"]["saturation"]["reason"], "baseline");
    assert_eq!(decision["decision"]["stale"], false);
    let metrics = events
        .iter()
        .find(|event| event["event"] == "metrics")
        .unwrap();
    assert!(
        metrics["sample_age_seconds"].as_u64().unwrap() >= 1,
        "{metrics}"
    );
}
