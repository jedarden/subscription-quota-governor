use chrono::{Duration, Utc};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use subscription_governor::dashboard::{collect, render};

fn config(dir: &Path, source: &str) -> PathBuf {
    let path = dir.join("governor.yaml");
    fs::write(&path, format!("version: 1\nstate_path: {}\naccounts:\n  account:\n    source: {source}\n    utilization: {{ target_utilization: 0.9, stale_after_seconds: 900, minimum_sample_seconds: 240 }}\n    fleet: {{ max_workers: 20, observer: {{ type: command, argv: [/bin/false] }}, actuator: {{ type: none }} }}\n",serde_json::to_string(&dir.join("state.json")).unwrap())).unwrap();
    path
}
fn write(path: &Path, value: Value) {
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn non_tty_alias_is_one_read_only_frame_even_with_missing_state() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("source-was-called");
    let source = format!(
        "{{ type: command, argv: [/bin/sh, -c, {}] }}",
        serde_json::to_string(&format!("touch {}", marker.display())).unwrap()
    );
    let config = config(dir.path(), &source);
    let output = Command::new(env!("CARGO_BIN_EXE_subgov"))
        .args(["tui", "--configs", config.to_str().unwrap()])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Quota: unknown"));
    assert!(!text.contains('\x1b'));
    assert!(!marker.exists());
    assert!(!dir.path().join("state.json").exists());
    assert!(!dir.path().join("state.lock").exists());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn corrupted_state_is_not_quarantined_or_rewritten_and_bad_config_is_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path(), "{ type: normalized_file, path: missing.json }");
    fs::write(dir.path().join("state.json"), b"not json").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_subgov"))
        .args([
            "dashboard",
            "--configs",
            config.to_str().unwrap(),
            dir.path().join("absent.yaml").to_str().unwrap(),
            "--once",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let frame: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(frame["accounts"].as_array().unwrap().len(), 2);
    assert_eq!(
        fs::read(dir.path().join("state.json")).unwrap(),
        b"not json"
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn fixed_generation_rate_projection_stale_reset_and_quantized_samples() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path(), "{ type: command, argv: [/bin/false] }");
    let now = Utc::now();
    let reset = now + Duration::hours(2);
    let state = |old_used: f64, old_reset: chrono::DateTime<Utc>, old_age: i64| json!({"accounts":{"account":{"last_target":3,"windows":{"weekly":{"observed_at":now,"used_fraction":0.4,"resets_at":reset,"workers":2}},"aggregate_burn_history":{"weekly":[{"observed_at":now-Duration::seconds(old_age),"used_fraction":old_used,"resets_at":old_reset,"governed_workers":2}]}}}});
    write(&dir.path().join("state.json"), state(0.35, reset, 600));
    let frame = collect(std::slice::from_ref(&config), now);
    let window = &frame.accounts[0].windows[0];
    assert!((window.consumption_percentage_points_per_hour.unwrap() - 30.0).abs() < 1e-8);
    assert!(window.expected_remaining_percent_at_reset.unwrap() < 1e-8);
    assert_eq!(window.baseline_seconds, Some(600));
    assert_eq!(window.history.len(), 2);
    assert!((window.history[0].used_percent - 35.0).abs() < 1e-8);
    assert!((window.history[1].used_percent - 40.0).abs() < 1e-8);
    // A healthy new cycle cannot make old quota fresh.
    write(
        &dir.path().join("status.json"),
        json!({"account":{"state":"healthy_learning","reason":"learning_burn_rate","updated_at":now+Duration::hours(1)}}),
    );
    let stale = collect(std::slice::from_ref(&config), now + Duration::hours(1));
    assert!(!stale.accounts[0].windows[0].fresh);
    assert!(stale.accounts[0].windows[0]
        .consumption_percentage_points_per_hour
        .is_none());
    for (used, old_reset, age) in [
        (0.35, reset - Duration::days(7), 600),
        (0.5, reset, 600),
        (0.35, reset, 1200),
        (0.35, reset, 60),
    ] {
        write(&dir.path().join("state.json"), state(used, old_reset, age));
        let frame = collect(std::slice::from_ref(&config), now);
        assert!(frame.accounts[0].windows[0]
            .consumption_percentage_points_per_hour
            .is_none());
        assert_eq!(
            frame.accounts[0].windows[0].history.len(),
            if age == 60 { 2 } else { 1 }
        );
    }
    write(&dir.path().join("state.json"), state(0.4, reset, 600));
    let frame = collect(&[config], now);
    assert_eq!(
        frame.accounts[0].windows[0].consumption_percentage_points_per_hour,
        Some(0.0)
    );
    assert!(frame.accounts[0].windows[0]
        .rate_status
        .contains("quantized"));
    assert!(frame.accounts[0].windows[0]
        .expected_remaining_percent_at_reset
        .is_none());
    assert!(render(&frame, 40)
        .lines()
        .all(|line| line.chars().count() <= 40));
}

#[test]
fn normalized_cache_supplies_optional_signals_and_does_not_break_rate_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache.json");
    let config = config(
        dir.path(),
        &format!(
            "{{ type: normalized_file, path: {} }}",
            serde_json::to_string(&cache).unwrap()
        ),
    );
    let now = Utc::now();
    let reset = now + Duration::hours(2);
    write(
        &cache,
        json!({"observed_at":now,"fresh":true,"windows":[{"id":"weekly","used_fraction":0.4,"resets_at":reset}],"reset_credits":{"available_count":3}}),
    );
    write(
        &dir.path().join("state.json"),
        json!({"accounts":{"account":{"windows":{"weekly":{"observed_at":now,"used_fraction":0.4,"resets_at":reset,"workers":2}},"history":{"weekly":{"resets_at":reset,"samples":[{"observed_at":now-Duration::minutes(10),"used_fraction":0.35,"workers":2},{"observed_at":now,"used_fraction":0.4,"workers":2}]}},"last_target":4},"malformed":{"windows":"invalid"}}}),
    );
    write(
        &dir.path().join("status.json"),
        json!({"account":{"state":"healthy_learning","reason":"\u{1b}[31mlearning\n\u{1b}]0;spoof\u{7}","updated_at":now,"current_workers":9,"desired_workers":8}}),
    );
    let frame = collect(&[config], now);
    assert!(frame.accounts[0].windows[0]
        .consumption_percentage_points_per_hour
        .is_some());
    assert_eq!(frame.accounts[0].controller_observed_workers, Some(9));
    assert_eq!(frame.accounts[0].target_workers, Some(8));
    assert_eq!(frame.accounts[0].reason, "learning");
    assert!(frame.accounts[0]
        .cached_signals
        .iter()
        .any(|s| s.contains("banked credits: 3")));
    assert!(!render(&frame, 120).contains('\x1b'));
}

#[test]
fn banked_weekly_target_override_preserves_short_window_policy_and_invalid_signal_is_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache.json");
    let config = config(
        dir.path(),
        &format!(
            "{{ type: normalized_file, path: {} }}",
            serde_json::to_string(&cache).unwrap()
        ),
    );
    let yaml=fs::read_to_string(&config).unwrap().replace("target_utilization: 0.9","target_utilization: 0.95").replace("    fleet:","    banked_resets: { enabled: true, full_speed: true, redeem_at_utilization: 1.0 }\n    fleet:");
    fs::write(&config, yaml).unwrap();
    let now = Utc::now();
    let reset = now + Duration::hours(2);
    write(
        &cache,
        json!({"observed_at":now,"windows":[{"id":"weekly","used_fraction":0.4,"resets_at":reset,"duration_minutes":10080},{"id":"five_hour","used_fraction":0.2,"resets_at":reset,"duration_minutes":300}],"reset_credits":{"available_count":3},"eligible_backlog_capacity":0,"saturation":{"completed_requests":100,"attempts":1,"window_seconds":300}}),
    );
    let frame = collect(&[config], now);
    let weekly = frame.accounts[0]
        .windows
        .iter()
        .find(|w| w.id == "weekly")
        .unwrap();
    let short = frame.accounts[0]
        .windows
        .iter()
        .find(|w| w.id == "five_hour")
        .unwrap();
    assert_eq!(weekly.target_percent, 100.0);
    assert_eq!(weekly.configured_target_percent, 95.0);
    assert_eq!(short.target_percent, 95.0);
    assert!(frame.accounts[0]
        .cached_signals
        .iter()
        .any(|s| s.contains("eligible capacity 0")));
    assert!(!frame.accounts[0]
        .cached_signals
        .iter()
        .any(|s| s.contains("throughput")));
}
