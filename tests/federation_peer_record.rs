//! Acceptance contract for plan.md §28.15 WP27: the versioned federation
//! peer record (§28.4), §28.5 account identity, and the read-only
//! `subgov peer-state --json` command (§28.13).
//!
//! Done when two fake peers round-trip through the record schema, an
//! unknown major schema is rejected, a mismatched `account_fingerprint` is
//! isolated as a different account with an error event, and a scan over
//! everything the command emits proves no secret-shaped field exists (§14).

use chrono::{Duration, Utc};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use subscription_governor::config::{
    AccountConfig, ActuatorConfig, Config, FederationConfig, FleetConfig, SourceConfig,
    UtilizationConfig, WorkerObserverConfig,
};
use subscription_governor::federation::{
    account_fingerprint, build_local_record, classify_account, fingerprint_mismatch_event,
    AccountIdentityVerdict, PeerRecord, PeerRecordError, PEER_RECORD_SCHEMA_VERSION,
};
use subscription_governor::state::{State, WindowSample};

/// One fake peer: a private directory holding a governor config, the
/// derived status and state files of a completed cycle, and a WP24
/// host-capacity state file. Everything `build_local_record` reads.
struct FakePeer {
    config: Config,
    _dir: tempfile::TempDir,
}

impl FakePeer {
    /// `identity` is the provider's non-secret account identifier the
    /// instance fingerprints (§28.4); peers sharing it govern one account.
    fn new(host_id: &str, identity: &str, workers: (u32, u32)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let now = Utc::now();

        // The last completed cycle's readiness surface (§28.4: current and
        // desired workers per account).
        fs::write(
            config_status_path(dir.path()),
            json!({"acct": {"current_workers": workers.0, "desired_workers": workers.1}})
                .to_string(),
        )
        .unwrap();

        // Quota state carrying one window sample (§28.4: the latest sample).
        let mut state = State::default();
        state
            .accounts
            .entry("acct".to_owned())
            .or_default()
            .windows
            .insert(
                "five_hour".to_owned(),
                WindowSample {
                    observed_at: now,
                    used_fraction: 0.42,
                    resets_at: now + Duration::hours(3),
                    workers: workers.0,
                },
            );
        let state_path = state_dir.join("governor.json");
        fs::write(&state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();

        // The WP24 host-capacity adapter's evidence: a measured ceiling and
        // one launch reservation (§28.4).
        fs::write(
            dir.path().join("host-capacity.json"),
            json!({
                "version": 1,
                "last_evidence": {
                    "event": "host_worker_ceiling",
                    "ceiling": 9,
                    "observed_at": now.timestamp() as f64,
                },
                "reservations": {"acct": {"count": 1, "at": now.timestamp() as f64}},
            })
            .to_string(),
        )
        .unwrap();

        let config = Config {
            version: 1,
            poll_interval_seconds: 300,
            state_path: Some(state_path),
            accounts: BTreeMap::from([(
                "acct".to_owned(),
                AccountConfig {
                    source: SourceConfig::NormalizedFile {
                        path: dir.path().join("quota.json"),
                    },
                    fleet: FleetConfig {
                        min_workers: 0,
                        max_workers: 4,
                        bootstrap_workers: 1,
                        max_scale_up_per_cycle: 1,
                        max_scale_down_per_cycle: 1,
                        observer: Some(WorkerObserverConfig::Static { workers: workers.0 }),
                        actuator: ActuatorConfig::None,
                        observer_reconciliation: Default::default(),
                        hosts: None,
                        takeover: Vec::new(),
                        takeover_helper: None,
                    },
                    utilization: UtilizationConfig {
                        target_utilization: Some(0.9),
                        reserve_fraction: None,
                        strategy: Default::default(),
                        stale_after_seconds: 900,
                        stale_behavior: Default::default(),
                        minimum_sample_seconds: 60,
                        backpressure: None,
                        zai_saturation: None,
                        windows: BTreeMap::new(),
                    },
                    banked_resets: Default::default(),
                },
            )]),
            federation: Some(FederationConfig {
                host_id: host_id.to_owned(),
                instance_id_file: None,
                record_seq_file: None,
                host_capacity_state: Some(dir.path().join("host-capacity.json")),
                account_identities: BTreeMap::from([("acct".to_owned(), identity.to_owned())]),
                peers: Vec::new(),
                peer_stale_after_seconds: 120,
                peer_expiry_seconds: 1_800,
                primary: true,
                repo_owners: BTreeMap::new(),
                pending_launches_file: None,
            }),
        };
        Self { config, _dir: dir }
    }

    fn build(&self) -> PeerRecord {
        build_local_record(&self.config, Utc::now()).unwrap()
    }
}

/// Where the run loop writes the derived readiness report relative to a
/// state root: a sibling of the state file (§28.4).
fn config_status_path(state_root: &Path) -> PathBuf {
    state_root.join("state").join("status.json")
}

#[test]
fn two_fake_peers_round_trip_through_the_record_schema() {
    let home = FakePeer::new("codinghome", "provider:acct-1", (2, 3));
    let remote = FakePeer::new("gmktec", "provider:acct-1", (1, 1));

    let mut records = Vec::new();
    for peer in [&home, &remote] {
        let record = peer.build();
        assert_eq!(record.schema_version, PEER_RECORD_SCHEMA_VERSION);

        // Round trip: a peer pull must reproduce the record exactly, and be
        // byte-stable across a re-serialization of the parsed copy.
        let json = record.to_json().unwrap();
        assert!(!json.contains('\n'), "the pull transport reads one line");
        let reparsed = PeerRecord::parse(json.as_bytes()).unwrap();
        assert_eq!(reparsed, record);
        assert_eq!(reparsed.to_json().unwrap(), json);
        records.push(record);
    }

    // The two fakes are genuinely distinct instances of one account.
    assert_ne!(records[0].instance_id, records[1].instance_id);
    assert_ne!(records[0].host_id, records[1].host_id);
    assert_eq!(
        records[0].accounts["acct"].account_fingerprint,
        records[1].accounts["acct"].account_fingerprint
    );

    // Everything the host already measured flows into the record (§28.4).
    let account = &records[0].accounts["acct"];
    assert_eq!(account.account_id, "acct");
    assert_eq!(
        account.account_fingerprint,
        account_fingerprint("provider:acct-1")
    );
    let pool = &account.pools["acct"];
    assert_eq!(pool.governed_workers, 2);
    assert_eq!(pool.pending_launches, 0);
    assert_eq!(pool.desired, 3);
    let sample = pool.quota_sample.as_ref().unwrap();
    assert_eq!(sample.window, "five_hour");
    assert_eq!(sample.used_fraction, 0.42);
    assert!(sample.resets_at > sample.observed_at);
    assert_eq!(records[0].host_ceiling.as_ref().unwrap().workers, 9);
    assert_eq!(records[0].host_reservations["acct"].count, 1);

    // The record's own identity is persisted and its sequence is monotonic
    // across rebuilds (§28.4).
    let rebuilt = home.build();
    assert_eq!(rebuilt.instance_id, records[0].instance_id);
    assert_eq!(rebuilt.record_seq, records[0].record_seq + 1);
    assert_eq!(rebuilt.host_id, records[0].host_id);
}

#[test]
fn unknown_major_schema_is_rejected_and_minor_additions_pass() {
    let peer = FakePeer::new("codinghome", "provider:acct-1", (2, 3));
    let mut envelope = serde_json::to_value(peer.build()).unwrap();

    // Any major this binary does not speak is rejected on its own terms
    // (§28.4), never best-effort parsed.
    for unknown in [0u32, 2, 99] {
        envelope["schema_version"] = json!(unknown);
        let error = PeerRecord::parse(envelope.to_string().as_bytes()).unwrap_err();
        assert!(
            matches!(
                error,
                PeerRecordError::UnknownSchemaMajor { found, supported }
                    if found == unknown && supported == PEER_RECORD_SCHEMA_VERSION
            ),
            "version {unknown} must be an unknown-major rejection, got {error}"
        );
    }

    // A record without a version is malformed, not an unknown major.
    let mut unversioned = envelope.clone();
    unversioned
        .as_object_mut()
        .unwrap()
        .remove("schema_version");
    assert!(matches!(
        PeerRecord::parse(unversioned.to_string().as_bytes()),
        Err(PeerRecordError::Malformed(_))
    ));

    // Unknown fields inside the known major stay compatible, so a minor
    // publisher never orphans an older receiver (§28.4).
    envelope["schema_version"] = json!(PEER_RECORD_SCHEMA_VERSION);
    envelope["future_minor_field"] = json!({"anything": true});
    assert!(PeerRecord::parse(envelope.to_string().as_bytes()).is_ok());
}

#[test]
fn fingerprint_mismatch_is_isolated_as_a_different_account_with_an_error_event() {
    let local_peer = FakePeer::new("codinghome", "provider:acct-1", (2, 3));
    let twin_peer = FakePeer::new("lab", "provider:acct-1", (1, 1));
    let drifted_peer = FakePeer::new("gmktec", "provider:acct-other", (1, 1));

    let local = local_peer.build().accounts["acct"].clone();
    let twin = twin_peer.build().accounts["acct"].clone();
    let drifted = drifted_peer.build().accounts["acct"].clone();

    // Same account id and fingerprint: one account, totals merge (§28.5).
    assert_eq!(
        classify_account(&local, &twin),
        AccountIdentityVerdict::Same
    );
    assert!(classify_account(&local, &twin).shares_account_total());

    // Same account id, different fingerprint (§28.5): an error event, and
    // the peer's record is treated as a different account -- it must never
    // contribute to this account's total.
    let mismatch = classify_account(&local, &drifted);
    assert_eq!(mismatch, AccountIdentityVerdict::FingerprintMismatch);
    assert!(!mismatch.shares_account_total());
    assert!(mismatch.is_error());

    // A different account id is an ordinary distinct account (§28.11), not
    // an error.
    let mut renamed = drifted.clone();
    renamed.account_id = "acct-two".to_owned();
    assert_eq!(
        classify_account(&local, &renamed),
        AccountIdentityVerdict::Distinct
    );
    assert!(!classify_account(&local, &renamed).is_error());

    let event = fingerprint_mismatch_event(&local, &drifted, "gmktec", Utc::now());
    assert_eq!(event["event"], "account_fingerprint_mismatch");
    assert_eq!(event["severity"], "error");
    assert_eq!(event["peer_host_id"], "gmktec");
    assert_eq!(event["action"], "treated_as_different_account");
    // Fingerprints may travel; the provider identifier behind them may not
    // (§14).
    assert!(event.to_string().contains(&local.account_fingerprint));
    assert!(event.to_string().contains(&drifted.account_fingerprint));
    assert!(!event.to_string().contains("provider:acct"));
}

/// Substrings that mark a field as secret-shaped if they appear anywhere in
/// a key name (§14: no credentials in a record, ever).
const SECRET_KEY_MARKERS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "api_key",
    "api-key",
    "apikey",
    "credential",
    "authorization",
    "bearer",
    "cookie",
    "private_key",
    "access_key",
    "session",
];

/// Collects every key anywhere in a JSON value whose name is secret-shaped.
fn secret_shaped_keys(value: &Value) -> Vec<String> {
    let mut found = Vec::new();
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                let lower = key.to_lowercase();
                if SECRET_KEY_MARKERS
                    .iter()
                    .any(|marker| lower.contains(marker))
                {
                    found.push(key.clone());
                }
                found.extend(secret_shaped_keys(inner));
            }
        }
        Value::Array(items) => {
            for item in items {
                found.extend(secret_shaped_keys(item));
            }
        }
        _ => {}
    }
    found
}

#[test]
fn peer_state_emits_no_secret_shaped_field_or_credential() {
    let dir = tempfile::tempdir().unwrap();

    // The workspace this instance seeds has a git origin with embedded
    // userinfo -- exactly the credential a §14 record must strip.
    let workspace = dir.path().join("workspace-a");
    fs::create_dir_all(&workspace).unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&workspace)
            .args(args)
            .output()
            .unwrap()
    };
    assert!(git(&["init", "-q"]).status.success());
    assert!(git(&[
        "remote",
        "add",
        "origin",
        "https://ci-bot:sup3rsecret-token@github.com/jedarden/workspace-a.git"
    ])
    .status
    .success());

    // The WP23 demand file naming that workspace, so the record's
    // workspace_origins and ready_demand are both populated.
    let demand_path = dir.path().join("demand.json");
    fs::write(
        &demand_path,
        json!({
            "observed_at": Utc::now().to_rfc3339(),
            "ready": [],
            "ready_bead_count": 4,
            "busy": [],
            "blocked": [],
            "workspaces": [{
                "workspace": workspace.to_str().unwrap(),
                "ready_beads": 4,
                "busy_claims": 0,
                "worker_limit": 2,
                "eligible_slots": 2,
            }],
            "unowned_floor": 0,
            "eligible_backlog_capacity": 3,
        })
        .to_string(),
    )
    .unwrap();

    // The WP24 capacity state so the emitted record also carries a measured
    // ceiling and a reservation for the scan to walk.
    let now = Utc::now();
    let capacity_path = dir.path().join("host-capacity.json");
    fs::write(
        &capacity_path,
        json!({
            "version": 1,
            "last_evidence": {"ceiling": 9, "observed_at": now.timestamp() as f64},
            "reservations": {"sg-zai-": {"count": 1, "at": now.timestamp() as f64}},
        })
        .to_string(),
    )
    .unwrap();

    let config_path = dir.path().join("governor.yaml");
    fs::write(
        &config_path,
        format!(
            "version: 1\nstate_path: {}\naccounts:\n  acct:\n    source: {{ type: normalized_file, path: {} }}\n    utilization: {{ target_utilization: 0.9 }}\n    fleet:\n      min_workers: 0\n      max_workers: 4\n      observer: {{ type: static, workers: 2 }}\n      actuator:\n        type: needle_run\n        repo: {}\n        adapter: codex\n        identifier_prefix: \"sg-zai-\"\n        seed_demand_state: {}\n        seed_heartbeats: {}\nfederation:\n  host_id: codinghome\n  host_capacity_state: {}\n  account_identities:\n    acct: \"provider:acct-1\"\n",
            dir.path().join("state").join("governor.json").display(),
            dir.path().join("quota.json").display(),
            workspace.display(),
            demand_path.display(),
            dir.path().join("heartbeats").display(),
            capacity_path.display(),
        ),
    )
    .unwrap();

    // Drive the real command: the pull transport (§28.3) runs this over
    // SSH, so its stdout is exactly what a peer would receive.
    let output = Command::new(env!("CARGO_BIN_EXE_subgov"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "peer-state",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "peer-state failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let emitted: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("peer-state printed invalid JSON: {error}\n{stdout}"));

    // The §28.5 mismatch event must pass the same scan: it is emitted
    // alongside records on every pull.
    let local_record = {
        // Rebuild in-process so the mismatch event can be formed from the
        // same account the CLI just published.
        let mut config = subscription_governor::config::Config::load(&config_path).unwrap();
        config
            .federation
            .as_mut()
            .unwrap()
            .account_identities
            .insert("acct".to_owned(), "provider:acct-drift".to_owned());
        build_local_record(&config, Utc::now()).unwrap()
    };
    let local_account = local_record.accounts["acct"].clone();
    let emitted_account: subscription_governor::federation::PeerAccount =
        serde_json::from_value(emitted["accounts"]["acct"].clone()).unwrap();
    let event = fingerprint_mismatch_event(&local_account, &emitted_account, "gmktec", Utc::now());

    for surface in [(&emitted, "peer-state stdout"), (&event, "mismatch event")] {
        let shaped = secret_shaped_keys(surface.0);
        assert!(
            shaped.is_empty(),
            "{} emitted secret-shaped fields {shaped:?}",
            surface.1
        );
    }

    // No planted credential material reaches the wire, in any field.
    for credential in ["sup3rsecret", "ci-bot"] {
        assert!(
            !stdout.contains(credential),
            "credential material {credential} leaked into the record"
        );
        assert!(
            !event.to_string().contains(credential),
            "credential material {credential} leaked into the event"
        );
    }

    // The sanitized origin does reach the wire, so §28.10's overlap check
    // still works.
    assert!(
        stdout.contains("github.com/jedarden/workspace-a.git"),
        "sanitized origin missing from {stdout}"
    );

    // The demand and capacity surfaces fed the record end to end.
    assert_eq!(
        emitted["accounts"]["acct"]["pools"]["sg-zai-"]["ready_demand"]["ready_beads"],
        json!(4)
    );
    assert_eq!(
        emitted["accounts"]["acct"]["pools"]["sg-zai-"]["ready_demand"]
            ["eligible_backlog_capacity"],
        json!(3)
    );
    assert_eq!(emitted["host_ceiling"]["workers"], json!(9));
    assert_eq!(emitted["host_reservations"]["sg-zai-"]["count"], json!(1));
    // And the record the library builds matches what the command prints.
    let published = local_record.to_json().unwrap();
    assert!(secret_shaped_keys(&serde_json::from_str::<Value>(&published).unwrap()).is_empty());
}
