use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn write_config(dir: &Path, fleet: &str) -> PathBuf {
    let path = dir.join("governor.yaml");
    fs::write(
        &path,
        format!(
            "version: 1\naccounts:\n  acct:\n    source: {{ type: normalized_file, path: /tmp/unused-source.json }}\n    utilization: {{ target_utilization: 0.9 }}\n    fleet:\n{fleet}"
        ),
    )
    .unwrap();
    path
}

#[cfg(unix)]
fn fake_needle(dir: &TempDir, script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let path = bin_dir.join("needle");
    fs::write(&path, script).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    bin_dir
}

#[cfg(unix)]
fn doctor(config: &Path, path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_subgov"))
        .args(["--config", config.to_str().unwrap(), "doctor"])
        .env("PATH", path)
        .output()
        .unwrap()
}

#[cfg(unix)]
#[test]
fn doctor_checks_each_configured_host_adapter() {
    let temp = TempDir::new().unwrap();
    let config = write_config(
        temp.path(),
        "      min_workers: 0\n      max_workers: 2\n      hosts:\n        east:\n          observer: { type: static, workers: 0 }\n          actuator: { type: needle_run, repo: /tmp/east, adapter: codex }\n        west:\n          observer: { type: static, workers: 0 }\n          actuator: { type: needle_run, repo: /tmp/west, adapter: claude }\n",
    );
    let bin_dir = fake_needle(
        &temp,
        "#!/bin/sh\ncase \"$2\" in codex|claude) ;; *) exit 2 ;; esac\nprintf 'Adapter: %s\\nProbe: exit 0 (1ms)\\nStatus:  READY\\n' \"$2\"\n",
    );

    let output = doctor(&config, &bin_dir);
    assert!(
        output.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let checks: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(checks.len(), 2);
    assert!(checks
        .iter()
        .all(|check| { check["check"] == "needle_adapter_parity" && check["status"] == "pass" }));
    assert_eq!(checks[0]["host"], "east");
    assert_eq!(checks[1]["host"], "west");
    assert_eq!(checks[0]["adapter"], "codex");
    assert_eq!(checks[1]["adapter"], "claude");
}

#[cfg(unix)]
#[test]
fn doctor_checks_single_account_needle_actuator() {
    let temp = TempDir::new().unwrap();
    let config = write_config(
        temp.path(),
        "      max_workers: 2\n      observer: { type: static, workers: 0 }\n      actuator: { type: needle_run, repo: /tmp/repo, adapter: gemini }\n",
    );
    let bin_dir = fake_needle(
        &temp,
        "#!/bin/sh\n[ \"$1\" = test-agent ] && [ \"$2\" = gemini ] || exit 2\nprintf 'Probe: exit 0 (1ms)\\nStatus:  READY\\n'\n",
    );

    let output = doctor(&config, &bin_dir);
    assert!(output.status.success());
    let check: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(check["check"], "needle_adapter_parity");
    assert_eq!(check["status"], "pass");
    assert_eq!(check["host"], Value::Null);
    assert_eq!(check["adapter"], "gemini");
}

#[cfg(unix)]
#[test]
fn doctor_fails_closed_on_missing_adapter() {
    let temp = TempDir::new().unwrap();
    let config = write_config(
        temp.path(),
        "      max_workers: 2\n      observer: { type: static, workers: 0 }\n      actuator: { type: needle_run, repo: /tmp/repo, adapter: absent }\n",
    );
    let bin_dir = fake_needle(
        &temp,
        "#!/bin/sh\necho 'unknown adapter: absent' >&2\nexit 2\n",
    );

    let output = doctor(&config, &bin_dir);
    assert_eq!(output.status.code(), Some(6));
    let checks: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(checks[0]["check"], "needle_adapter_parity");
    assert_eq!(checks[0]["status"], "fail");
    assert!(checks[0]["message"]
        .as_str()
        .unwrap()
        .contains("unknown adapter"));
}

#[cfg(unix)]
#[test]
fn doctor_rejects_a_nonzero_probe_even_if_needle_reports_ready() {
    let temp = TempDir::new().unwrap();
    let config = write_config(
        temp.path(),
        "      max_workers: 2\n      observer: { type: static, workers: 0 }\n      actuator: { type: needle_run, repo: /tmp/repo, adapter: codex }\n",
    );
    let bin_dir = fake_needle(
        &temp,
        "#!/bin/sh\nprintf 'Probe: exit 127 (1ms)\\nStatus:  READY\\n'\n",
    );

    let output = doctor(&config, &bin_dir);
    assert_eq!(output.status.code(), Some(6));
    let check: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(check["status"], "fail");
    assert!(check["message"]
        .as_str()
        .unwrap()
        .contains("probe exited with 127"));
}

#[cfg(unix)]
#[test]
fn doctor_skips_when_no_needle_actuator_is_configured() {
    let temp = TempDir::new().unwrap();
    let config = write_config(
        temp.path(),
        "      max_workers: 2\n      observer: { type: static, workers: 0 }\n      actuator: { type: none }\n",
    );

    let output = doctor(&config, temp.path());
    assert!(output.status.success());
    let check: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(check["check"], "needle_adapter_parity");
    assert_eq!(check["status"], "skipped");
}
