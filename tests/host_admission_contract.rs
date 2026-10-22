//! Exercise Rust's equal-target fast path against the real host admission
//! module with synthetic metrics. No services or worker processes are touched.
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use std::fs;
use std::process::Command;

fn cycle(busy: bool, observe_only: bool) {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let now = Utc::now();
    let host_config = dir.join("host-config.json");
    let host_state = dir.join("host-state.json");
    let demand = dir.join("demand.json");
    let count = dir.join("count");
    let marker = dir.join("called");
    let quota = dir.join("quota.json");
    let state = dir.join("governor-state.json");
    let script = dir.join("actuate.py");
    let config = dir.join("governor.yaml");
    fs::write(&count, "2").unwrap();
    fs::write(
        &demand,
        json!({"observed_at": now, "ready_bead_count": 2}).to_string(),
    )
    .unwrap();
    fs::write(
        &host_config,
        json!({"version": 1, "state_path": host_state, "pools": {
            "test": {"prefixes": ["sg-test-"], "demand_state": demand}
        }})
        .to_string(),
    )
    .unwrap();
    fs::write(
        &host_state,
        json!({"version": 1, "pressure_since": now.timestamp() - 120,
        "last_probe_at": now.timestamp() - 30})
        .to_string(),
    )
    .unwrap();
    fs::write(
        &quota,
        json!({"observed_at": now, "fresh": true, "eligible_backlog_capacity": 2,
        "windows": [{"id": "hour", "used_fraction": 0.2,
            "resets_at": now + Duration::hours(1), "duration_minutes": 60}]})
        .to_string(),
    )
    .unwrap();
    let module_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts");
    fs::write(&script, r#"import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import subgov_host_capacity as host
config, counter, marker, busy, desired = sys.argv[2:]
current = int(Path(counter).read_text())
host.probe = lambda pools: dict(counts={'test': current}, unmanaged=0,
    rss=[host.GIB] * current, mem_available=64 * host.GIB,
    slice_current=2 * host.GIB, slice_high=100 * host.GIB,
    memory_psi=20, io_psi=0, load=0, cpus=8)
with host.admission({'host_capacity': config, 'host_pool': 'test'}, int(desired), current=current) as granted:
    Path(marker).write_text(str(granted))
    Path(counter).write_text(str(current if busy == 'true' else granted))
"#).unwrap();
    let document = json!({"version": 1, "state_path": state,
    "accounts": {"acct": {
        "source": {"type": "normalized_file", "path": quota},
        "utilization": {"target_utilization": 0.9},
        "fleet": {"min_workers": 0, "max_workers": 2, "bootstrap_workers": 2,
            "observer": {"type": "file", "path": count},
            "actuator": {"type": "command", "reconcile_unchanged": true,
                "argv": ["python3", "-B", script, module_dir, host_config, count, marker,
                    if busy { "true" } else { "false" }, "{desired_workers}"]}}
    }}});
    fs::write(&config, serde_yaml::to_string(&document).unwrap()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_subgov"));
    command.args(["--config", config.to_str().unwrap(), "run", "--once"]);
    if observe_only {
        command.arg("--observe-only");
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
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
    assert_eq!(decision["decision"]["current_workers"], 2);
    assert_eq!(
        decision["decision"]["desired_workers"], 2,
        "quota target must be unchanged"
    );
    let actual = if busy || observe_only { 2 } else { 1 };
    assert_eq!(fs::read_to_string(&count).unwrap(), actual.to_string());
    let saved: Value = serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(
        saved["accounts"]["acct"]["windows"]["hour"]["workers"], actual,
        "burn learning must use actual workers, including pending busy retirees"
    );
    if observe_only {
        assert!(
            !marker.exists(),
            "observe-only must not invoke host reconciliation"
        );
        assert!(decision.get("actual_workers").is_none());
    } else {
        assert_eq!(fs::read_to_string(&marker).unwrap(), "1");
        assert_eq!(decision["actual_workers"], actual);
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("\"ceiling\": 1"));
    }
}

#[test]
fn unchanged_quota_target_still_retires_under_persistent_host_pressure() {
    cycle(false, false);
}

#[test]
fn pending_busy_retirement_is_counted_in_actual_burn_sample() {
    cycle(true, false);
}

#[test]
fn observe_only_never_runs_per_cycle_host_actuation() {
    cycle(false, true);
}
