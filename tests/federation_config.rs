//! Plan §28.13: federation configuration remains compatible with a single
//! host, validates the multi-host safety bounds, and keeps examples loadable.

use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use subscription_governor::config::Config;
use subscription_governor::controller::evaluate_pool;
use subscription_governor::federation::{
    account_fingerprint, PeerAccount, PeerPool, PeerReadyDemand, PeerRecord,
    PEER_RECORD_SCHEMA_VERSION,
};
use subscription_governor::model::QuotaSnapshot;
use subscription_governor::state::{AccountState, State, STATE_SCHEMA_VERSION};

fn config_yaml(accounts: &str, federation: &str) -> String {
    format!("version: 1\naccounts:\n{accounts}{federation}")
}

fn account(name: &str, prefix: Option<&str>, max_workers: u32, step: u32) -> String {
    let observer = match prefix {
        Some(prefix) => {
            format!("{{ type: needle_status, agent: zai, identifier_prefix: {prefix} }}")
        }
        None => "{ type: static, workers: 2 }".to_owned(),
    };
    format!(
        "  {name}:\n    source: {{ type: normalized_file, path: /tmp/subgov-quota.json }}\n    utilization: {{ reserve_fraction: 0.15 }}\n    fleet:\n      max_workers: {max_workers}\n      max_scale_up_per_cycle: {step}\n      observer: {observer}\n      actuator: {{ type: none }}\n"
    )
}

fn load_text(text: &str) -> anyhow::Result<Config> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("governor.yaml");
    fs::write(&path, text)?;
    Config::load(&path)
}

#[test]
fn absent_federation_and_an_empty_peer_list_make_byte_identical_decisions() {
    let accounts = account("acct", None, 8, 1);
    let legacy = load_text(&config_yaml(&accounts, "")).unwrap();
    let single = load_text(&config_yaml(
        &accounts,
        "federation:\n  host_id: codinghome\n  peers: []\n",
    ))
    .unwrap();
    assert!(legacy.federation.is_none());
    assert!(single.federation.as_ref().unwrap().peers.is_empty());
    let now: DateTime<Utc> = "2026-10-28T12:00:00Z".parse().unwrap();
    let snapshot: QuotaSnapshot = serde_json::from_str(
        r#"{"observed_at":"2026-10-28T12:00:00Z","fresh":true,"windows":[{"id":"weekly","used_fraction":0.42,"resets_at":"2026-10-29T12:00:00Z","duration_minutes":10080}]}"#,
    )
    .unwrap();
    let prior = AccountState::default();
    let decision = |config: &Config| {
        serde_json::to_vec(
            &evaluate_pool(
                "acct",
                &config.accounts["acct"],
                &snapshot,
                &prior,
                2,
                0,
                now,
            )
            .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(decision(&legacy), decision(&single));
}

#[test]
fn invalid_peers_and_unsafe_bounds_are_rejected_by_check() {
    let accounts = account("acct", Some("sg-zai-"), 6, 1);
    for (federation, expected) in [
        ("  host_id: codinghome\n  peers: [{host_id: codinghome}]\n", "own host"),
        ("  host_id: codinghome\n  peers: [{host_id: gmktec}, {host_id: gmktec}]\n", "more than once"),
        ("  host_id: codinghome\n  peers: [{host_id: 'gmktec;bad'}]\n", "only a-z, 0-9"),
        ("  host_id: codinghome\n  peers: [{host_id: gmktec, argv: []}]\n", "argv must contain"),
        ("  host_id: codinghome\n  peers: [{host_id: gmktec}]\n  repo_owners: {https://example.org/repo.git: absent}\n", "unconfigured host"),
    ] {
        let yaml = config_yaml(&accounts, &format!("federation:\n{federation}"));
        let error = load_text(&yaml).unwrap_err().to_string();
        assert!(error.contains(expected), "{expected}: {error}");
    }

    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.yaml");
    fs::write(
        &bad,
        config_yaml(
            &account("acct", Some("sg-zai-"), 3, 2),
            "federation:\n  host_id: codinghome\n  peers: [{host_id: gmktec}]\n",
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_subgov"))
        .args(["--config", bad.to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("summed max_step_up"));
}

#[test]
fn duplicate_effective_prefixes_are_rejected_before_secondary_host_rewrite() {
    let accounts = format!(
        "{}{}",
        account("one", Some("sg-zai-"), 6, 1),
        account("two", Some("sg-zai-gmktec-"), 6, 1),
    );
    let error = load_text(&config_yaml(
        &accounts,
        "federation:\n  host_id: gmktec\n  primary: false\n  peers: [{host_id: codinghome}]\n",
    ))
    .unwrap_err()
    .to_string();
    assert!(error.contains("already claimed"), "{error}");
    assert!(error.contains("sg-zai-gmktec-"), "{error}");
}

#[test]
fn shipped_example_configs_pass_the_strict_loader() {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut checked = 0;
    for entry in fs::read_dir(&examples).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "yaml")
        {
            Config::load(&path).unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
            checked += 1;
        }
    }
    assert!(checked >= 10, "expected all shipped YAML examples");
}

#[test]
fn pre_federation_state_migrates_and_retains_an_exact_rollback_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let old = b"{\"schema_version\":7,\"accounts\":{\"acct\":{\"last_target\":3}}}\n";
    fs::write(&path, old).unwrap();

    let (state, quarantined) = State::load(&path).unwrap();
    assert!(quarantined.is_none());
    assert_eq!(state.schema_version, STATE_SCHEMA_VERSION);
    assert_eq!(state.accounts["acct"].last_target, Some(3));
    state.save(&path).unwrap();

    let rollback = dir.path().join("state.json.pre-v8");
    assert_eq!(fs::read(&rollback).unwrap(), old);
    let (reloaded, quarantined) = State::load(&path).unwrap();
    assert!(quarantined.is_none());
    assert_eq!(reloaded.accounts["acct"].last_target, Some(3));
    assert_eq!(reloaded.schema_version, STATE_SCHEMA_VERSION);

    // Restoring the retained file gives the previous schema and data back.
    fs::copy(&rollback, &path).unwrap();
    assert_eq!(fs::read(&path).unwrap(), old);
    let (restored, quarantined) = State::load(&path).unwrap();
    assert!(quarantined.is_none());
    assert_eq!(restored.accounts["acct"].last_target, Some(3));
}

#[test]
fn peers_cli_reports_fresh_workers_and_failed_peer_as_stale() {
    let dir = tempfile::tempdir().unwrap();
    let peer_path = dir.path().join("peer.json");
    let record = PeerRecord {
        schema_version: PEER_RECORD_SCHEMA_VERSION,
        instance_id: "instance-gmktec".to_owned(),
        host_id: "gmktec".to_owned(),
        record_seq: 1,
        observed_at: Utc::now(),
        accounts: BTreeMap::from([(
            "acct".to_owned(),
            PeerAccount {
                account_id: "acct".to_owned(),
                account_fingerprint: account_fingerprint("acct"),
                pools: BTreeMap::from([(
                    "sg-zai-gmktec-".to_owned(),
                    PeerPool {
                        governed_workers: 2,
                        pending_launches: 0,
                        desired: 2,
                        quota_sample: None,
                        ready_demand: PeerReadyDemand::default(),
                    },
                )]),
            },
        )]),
        host_ceiling: None,
        host_reservations: BTreeMap::new(),
        resource_reserve: None,
        workspace_origins: BTreeMap::new(),
    };
    fs::write(&peer_path, record.to_json().unwrap()).unwrap();
    let config_path = dir.path().join("governor.yaml");
    let write_config = |command: &str| {
        fs::write(
            &config_path,
            config_yaml(
                &account("acct", None, 6, 1),
                &format!(
                    "federation:\n  host_id: codinghome\n  peers:\n    - host_id: gmktec\n      argv: [{command}]\n"
                ),
            ),
        )
        .unwrap();
    };
    let run = || {
        let output = Command::new(env!("CARGO_BIN_EXE_subgov"))
            .args(["--config", config_path.to_str().unwrap(), "peers", "--json"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    write_config(&format!("cat, {}", peer_path.display()));
    let report = run();
    assert_eq!(report["peers"][0]["staleness"], "fresh");
    assert_eq!(report["workers_per_host"]["gmktec"]["acct"], 2);

    write_config("false");
    let report = run();
    assert_eq!(report["peers"][0]["staleness"], "stale");
    assert!(report["workers_per_host"]["gmktec"]["acct"].is_null());
}
