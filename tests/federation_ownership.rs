//! Acceptance checks for plan §28.8–10 (WP30): host worker identity,
//! workspace ownership, and a second peer pull before launch.

use chrono::Utc;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use subscription_governor::config::{ActuatorConfig, Config, WorkerObserverConfig};
use subscription_governor::federation::{
    account_fingerprint, PeerAccount, PeerPool, PeerReadyDemand, PeerRecord,
    PEER_RECORD_SCHEMA_VERSION,
};
use subscription_governor::fleet::observer_for;
use subscription_governor::ownership::{
    effective_prefix, gate_launch, overshoot_bound, read_pending_launches,
    refuse_prefix_collisions, resolve_workspace_overlap,
};

fn peer(host: &str, prefix: &str, workers: u32, pending: u32) -> PeerRecord {
    PeerRecord {
        schema_version: PEER_RECORD_SCHEMA_VERSION,
        instance_id: format!("instance-{host}"),
        host_id: host.to_owned(),
        record_seq: 1,
        observed_at: Utc::now(),
        accounts: BTreeMap::from([(
            "acct".to_owned(),
            PeerAccount {
                account_id: "acct".to_owned(),
                account_fingerprint: account_fingerprint("provider:shared-account"),
                pools: BTreeMap::from([(
                    prefix.to_owned(),
                    PeerPool {
                        governed_workers: workers,
                        pending_launches: pending,
                        desired: workers,
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
    }
}

fn heartbeat(dir: &Path, agent: &str, worker_id: &str) {
    let qualified_id = format!("{agent}-{worker_id}");
    fs::write(
        dir.join(format!("{qualified_id}.json")),
        json!({
            "qualified_id": qualified_id,
            "worker_id": worker_id,
            "last_heartbeat": Utc::now(),
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn derived_prefix_counts_only_workers_on_the_instances_host() {
    let home = tempfile::tempdir().unwrap();
    let remote = tempfile::tempdir().unwrap();
    let primary_prefix = effective_prefix("sg-zai-", "codinghome", true);
    let remote_prefix = effective_prefix("sg-zai-", "gmktec", false);
    assert_eq!(primary_prefix, "sg-zai-");
    assert_eq!(remote_prefix, "sg-zai-gmktec-");

    // Each host observes its own heartbeat directory. The remote directory
    // deliberately contains a legacy worker too: it must not count as a
    // gmktec-governed worker.
    heartbeat(home.path(), "zai", "sg-zai-1");
    heartbeat(home.path(), "zai", "manual-1");
    heartbeat(remote.path(), "zai", "sg-zai-gmktec-1");
    heartbeat(remote.path(), "zai", "sg-zai-99");

    let observer = |dir: &Path, prefix: String| {
        observer_for(
            &WorkerObserverConfig::NeedleStatus {
                agent: "zai".to_owned(),
                heartbeat_dir: dir.to_path_buf(),
                stale_after_seconds: 60,
                identifier_prefix: Some(prefix),
            },
            &[],
        )
    };
    let primary = observer(home.path(), primary_prefix);
    let secondary = observer(remote.path(), remote_prefix);
    assert_eq!(primary.current_workers().unwrap(), 1);
    assert_eq!(secondary.current_workers().unwrap(), 1);
    assert_eq!(secondary.unowned_workers().unwrap(), 1);
}

#[test]
fn secondary_config_rewrites_observer_and_actuator_to_one_host_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("governor.yaml");
    fs::write(&path, "version: 1\naccounts:\n  acct:\n    source: { type: normalized_file, path: /tmp/source.json }\n    utilization: { reserve_fraction: 0.1 }\n    fleet:\n      max_workers: 4\n      observer: { type: needle_status, agent: codex, identifier_prefix: sg-codex- }\n      actuator: { type: needle_run, repo: /tmp/work, adapter: codex, identifier_prefix: sg-codex- }\nfederation:\n  host_id: gmktec\n  primary: false\n  peers: [{ host_id: codinghome }]\n").unwrap();
    let config = Config::load(&path).unwrap();
    let fleet = &config.accounts["acct"].fleet;
    let Some(WorkerObserverConfig::NeedleStatus {
        identifier_prefix: Some(observed),
        ..
    }) = &fleet.observer
    else {
        panic!("missing needle observer")
    };
    let ActuatorConfig::NeedleRun {
        identifier_prefix: Some(launched),
        ..
    } = &fleet.actuator
    else {
        panic!("missing needle actuator")
    };
    assert_eq!(observed, "sg-codex-gmktec-");
    assert_eq!(launched, observed);
    assert_eq!(
        subscription_governor::federation::local_worker_prefixes(&config)["acct"],
        *observed
    );
}

#[test]
fn startup_refuses_a_peer_claiming_the_same_worker_prefix() {
    let primary = effective_prefix("sg-zai-", "codinghome", true);
    let duplicate = peer("gmktec", &primary, 1, 0);
    let local = BTreeMap::from([("acct".to_owned(), primary.clone())]);
    let error = refuse_prefix_collisions(&local, &[duplicate]).unwrap_err();
    assert!(error.to_string().contains(&primary));
    assert!(error.to_string().contains("gmktec"));

    let secondary = effective_prefix("sg-zai-", "gmktec", false);
    let legitimate_primary = peer("codinghome", &primary, 1, 0);
    let local = BTreeMap::from([("acct".to_owned(), secondary)]);
    refuse_prefix_collisions(&local, &[legitimate_primary]).unwrap();
}

#[test]
fn run_command_refuses_colliding_prefix_before_first_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let peer_path = dir.path().join("peer.json");
    fs::write(
        &peer_path,
        serde_json::to_vec(&peer("gmktec", "sg-zai-", 1, 0)).unwrap(),
    )
    .unwrap();
    let config_path = dir.path().join("governor.yaml");
    fs::write(&config_path, format!(
        "version: 1\nstate_path: {}\naccounts:\n  acct:\n    source: {{ type: normalized_file, path: /tmp/no-source.json }}\n    utilization: {{ reserve_fraction: 0.1 }}\n    fleet:\n      max_workers: 4\n      observer: {{ type: needle_status, agent: zai, identifier_prefix: sg-zai- }}\n      actuator: {{ type: none }}\nfederation:\n  host_id: codinghome\n  peers:\n    - host_id: gmktec\n      argv: [cat, {}]\n",
        dir.path().join("state.json").display(), peer_path.display(),
    )).unwrap();
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
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("already claimed"));
}

#[test]
fn overlap_excludes_the_losing_hosts_workspace_from_seed_candidates() {
    let origin = "https://git.ardenone.com/jedarden/SEAM.git".to_owned();
    let home_path = "/home/coding/SEAM".to_owned();
    let remote_path = "/srv/coding/SEAM".to_owned();
    let home_origins = BTreeMap::from([(home_path.clone(), origin.clone())]);
    let remote_origins = BTreeMap::from([(remote_path.clone(), origin.clone())]);
    let mut home_record = peer("codinghome", "sg-zai-", 1, 0);
    home_record.workspace_origins = home_origins.clone();
    let mut remote_record = peer("gmktec", "sg-zai-gmktec-", 0, 0);
    remote_record.workspace_origins = remote_origins.clone();

    // The lowest host id keeps the repo without an explicit owner.
    let home = resolve_workspace_overlap(
        "codinghome",
        &home_origins,
        &[remote_record.clone()],
        &BTreeMap::new(),
        Utc::now(),
    );
    let remote = resolve_workspace_overlap(
        "gmktec",
        &remote_origins,
        &[home_record.clone()],
        &BTreeMap::new(),
        Utc::now(),
    );
    assert_eq!(home.seedable([home_path.as_str()]), vec![home_path.clone()]);
    assert!(remote.seedable([remote_path.as_str()]).is_empty());
    assert_eq!(remote.events[0]["event"], "workspace_overlap");
    assert_eq!(remote.events[0]["owner"], "codinghome");

    // An operator's repo_owners entry reverses that decision on both hosts.
    let owners = BTreeMap::from([(origin, "gmktec".to_owned())]);
    let home = resolve_workspace_overlap(
        "codinghome",
        &home_origins,
        &[remote_record],
        &owners,
        Utc::now(),
    );
    let remote = resolve_workspace_overlap(
        "gmktec",
        &remote_origins,
        &[home_record],
        &owners,
        Utc::now(),
    );
    assert!(home.seedable([home_path.as_str()]).is_empty());
    assert_eq!(remote.seedable([remote_path.as_str()]), vec![remote_path]);
}

#[test]
fn simultaneous_launch_repulled_pending_prevents_a_second_launch() {
    let home = tempfile::tempdir().unwrap();
    let remote = tempfile::tempdir().unwrap();
    let home_pending = home.path().join("pending.json");
    let remote_pending = remote.path().join("pending.json");
    let home_prefix = effective_prefix("sg-zai-", "codinghome", true);
    let remote_prefix = effective_prefix("sg-zai-", "gmktec", false);
    let bound = overshoot_bound([2, 1]);
    assert_eq!(bound, 3);

    // One worker already runs on codinghome. Its two intended launches
    // fill the account controller's total of three. gmktec's re-pull must
    // observe that intent even though the peer pool has a different prefix.
    let home_decision = gate_launch(
        2,
        2,
        1,
        0,
        &account_fingerprint("provider:shared-account"),
        &home_prefix,
        &home_pending,
        3,
        bound,
        Utc::now(),
        || Ok(vec![peer("gmktec", &remote_prefix, 0, 0)]),
    )
    .unwrap();
    assert_eq!(home_decision.permitted, 2);
    assert_eq!(read_pending_launches(&home_pending, &home_prefix), 2);

    let remote_decision = gate_launch(
        1,
        1,
        0,
        0,
        &account_fingerprint("provider:shared-account"),
        &remote_prefix,
        &remote_pending,
        3,
        bound,
        Utc::now(),
        || Ok(vec![peer("codinghome", &home_prefix, 1, 2)]),
    )
    .unwrap();
    assert!(remote_decision.abandoned);
    assert_eq!(remote_decision.permitted, 0);
    assert_eq!(read_pending_launches(&remote_pending, &remote_prefix), 0);
    assert!(home_decision.permitted + remote_decision.permitted <= bound);
}
