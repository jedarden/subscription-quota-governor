//! Acceptance contract for plan.md §28.15 WP29 (§28.6): one account total
//! derived everywhere from the federation's peer records, and the
//! cross-host §22.7 split of that total.
//!
//! Done when: two in-process instances with identical inputs choose the
//! same total; the host shares sum to that total; no share exceeds its
//! host's reserve ceiling; a host with zero ready demand receives none of
//! the new placement; and a disagreeing quota sample picks the
//! conservative larger fraction. The receiver-side assembly -- fresh,
//! stale and expired peers through the §28.7 table -- is exercised too.

use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;
use std::time::{Duration as StdDuration, Instant};

use subscription_governor::config::{
    AccountConfig, ActuatorConfig, FederationConfig, FleetConfig, PeerConfig, SourceConfig,
    Strategy, UtilizationConfig,
};
use subscription_governor::federation::{
    account_fingerprint, freshest_used_fraction, shared_account_total, split_account_total,
    FederationHost, HostCeiling, PeerAccount, PeerAccountSnapshot, PeerPool, PeerQuotaSample,
    PeerReadyDemand, PeerRecord, PeerTable, PEER_RECORD_SCHEMA_VERSION,
    SAMPLE_DISAGREEMENT_FRACTION,
};
use subscription_governor::state::{AccountState, WindowSample};

/// A fixed wall-clock origin, so every timestamp in every fixture is
/// reproducible and identical on both simulated instances.
const WALL_ORIGIN: &str = "2026-10-28T12:00:00Z";

fn now() -> DateTime<Utc> {
    WALL_ORIGIN.parse().expect("fixed test timestamp")
}

fn config(max_workers: u32) -> AccountConfig {
    AccountConfig {
        source: SourceConfig::NormalizedFile {
            path: "quota.json".into(),
        },
        fleet: FleetConfig {
            min_workers: 0,
            max_workers,
            bootstrap_workers: 0,
            max_scale_up_per_cycle: 4,
            max_scale_down_per_cycle: 4,
            observer: None,
            actuator: ActuatorConfig::None,
            observer_reconciliation: Default::default(),
            hosts: None,
            takeover: Vec::new(),
            takeover_helper: None,
        },
        utilization: UtilizationConfig {
            target_utilization: Some(0.85),
            reserve_fraction: None,
            strategy: Strategy::LinearToReset,
            stale_after_seconds: 900,
            stale_behavior: Default::default(),
            minimum_sample_seconds: 60,
            backpressure: None,
            zai_saturation: None,
            windows: BTreeMap::new(),
        },
        banked_resets: Default::default(),
    }
}

fn host(id: &str, workers: u32, ceiling: u32, ready_demand: u32) -> FederationHost {
    FederationHost {
        host_id: id.to_owned(),
        workers,
        ceiling,
        ready_demand,
    }
}

/// Host views from `(id, workers, ceiling, ready_demand)` rows.
fn fleet(rows: &[(&str, u32, u32, u32)]) -> Vec<FederationHost> {
    rows.iter()
        .map(|(id, workers, ceiling, demand)| host(id, *workers, *ceiling, *demand))
        .collect()
}

fn sample(used_fraction: f64, observed_at: DateTime<Utc>) -> PeerQuotaSample {
    PeerQuotaSample {
        window: "five_hour".to_owned(),
        used_fraction,
        resets_at: now() + Duration::hours(4),
        observed_at,
    }
}

/// A prior window sample one hour old at 0.30 with eight workers, so the
/// §9.4 pacing path has a measured burn rate to pace against. With the
/// freshest sample at 0.42 the paced total is 8; at a conservative 0.60 it
/// is 2.
fn paced_prior() -> AccountState {
    AccountState {
        windows: BTreeMap::from([(
            "five_hour".to_owned(),
            WindowSample {
                observed_at: now() - Duration::hours(1),
                used_fraction: 0.30,
                resets_at: now() + Duration::hours(4),
                workers: 8,
            },
        )]),
        ..AccountState::default()
    }
}

/// The shared total one instance derives for a single-sample account, as a
/// function of that sample's fraction and age.
fn total_for(used_fraction: f64, observed_at: DateTime<Utc>) -> u32 {
    let accounts = config(12);
    let hosts = fleet(&[("codinghome", 4, 8, 12), ("gmktec", 2, 8, 12)]);
    let samples = vec![sample(used_fraction, observed_at)];
    shared_account_total("acct", &accounts, &paced_prior(), &hosts, &samples, now())
        .expect("derivation succeeds")
        .total
}

/// plan.md §28.6: two in-process instances that assemble the same host
/// views and samples -- each from its own side, so the lists arrive in
/// opposite orders -- choose the same total, the shares they read out sum
/// to it, and no host is placed past its reserve.
#[test]
fn identical_inputs_choose_the_same_total_and_shares_sum_to_it() {
    let accounts = config(12);
    let prior = paced_prior();
    let samples = vec![
        sample(0.42, now()),
        sample(0.44, now() - Duration::seconds(600)),
    ];
    // codinghome's view: its own entry first, gmktec from the peer record.
    let local_view = fleet(&[("codinghome", 4, 8, 5), ("gmktec", 2, 6, 6)]);
    // gmktec's view: the same numbers, assembled from the other side.
    let peer_view = fleet(&[("gmktec", 2, 6, 6), ("codinghome", 4, 8, 5)]);

    let local =
        shared_account_total("acct", &accounts, &prior, &local_view, &samples, now()).unwrap();
    let peer =
        shared_account_total("acct", &accounts, &prior, &peer_view, &samples, now()).unwrap();
    assert_eq!(
        local, peer,
        "identical inputs must derive identical decisions"
    );
    assert_eq!(
        local.denominator, 6,
        "burn denominator: workers across all hosts"
    );
    assert_eq!(local.used_fraction, Some(0.42));
    assert!(!local.stale);

    // Paced by the prior burn sample: 0.12/h across 8 workers, 0.43 of
    // headroom over 4 remaining hours -- a total of 8, above the combined
    // current of 6, so this is a growth cycle.
    assert_eq!(local.total, 8);
    // Each instance actuates only its own host's share; the shares sum to
    // the total, none exceeds its reserve, and the equal-headroom
    // remainder lands on the smallest host key.
    assert_eq!(local.shares["codinghome"], 5);
    assert_eq!(local.shares["gmktec"], 3);
    assert_eq!(
        local.shares.values().sum::<u32>(),
        local.total,
        "shares sum to the controller total"
    );
    assert!(
        local.shares["codinghome"] <= 8,
        "never past codinghome's reserve"
    );
    assert!(local.shares["gmktec"] <= 6, "never past gmktec's reserve");
}

/// plan.md §28.6: a host with zero ready demand receives none of the new
/// placement, but keeps the workers it already runs while the total is
/// stable or growing.
#[test]
fn a_host_with_zero_ready_demand_receives_none() {
    // Fresh fleet, one host with work and one without: the whole total
    // lands on the host that has demand for it, and the shares still sum
    // to the total.
    let fresh = fleet(&[("codinghome", 0, 6, 6), ("gmktec", 0, 4, 0)]);
    let shares = split_account_total(6, &fresh);
    assert_eq!(shares["gmktec"], 0, "no ready demand, no placement");
    assert_eq!(shares["codinghome"], 6);
    assert_eq!(shares.values().sum::<u32>(), 6);

    // The workless host holds its running workers but never gains: spare
    // quota never launches a worker into work that does not exist (§26.4).
    let holding = fleet(&[("codinghome", 0, 6, 4), ("gmktec", 2, 4, 0)]);
    let shares = split_account_total(8, &holding);
    assert_eq!(shares["gmktec"], 2, "held at current, never scaled up");
    assert_eq!(shares["codinghome"], 4, "capped by its own ready demand");
    assert_eq!(
        shares.values().sum::<u32>(),
        6,
        "under the total of 8: no host had the demand to place the rest"
    );
}

/// plan.md §28.6: samples that disagree on `used_fraction` by more than
/// 0.05 resolve to the larger -- conservative -- fraction, and within the
/// threshold the freshest sample stands.
#[test]
fn a_disagreeing_sample_picks_the_conservative_fraction() {
    assert_eq!(SAMPLE_DISAGREEMENT_FRACTION, 0.05);

    // The peer's fraction sits 0.18 above the freshest sample: past the
    // threshold, so the larger fraction wins...
    let disagreeing = vec![
        sample(0.42, now()),
        sample(0.60, now() - Duration::seconds(600)),
    ];
    assert_eq!(freshest_used_fraction(&disagreeing), Some(0.60));
    // ...and the conservative sample never yields a larger total (paced:
    // 8 workers at 0.42, 2 at 0.60).
    let optimistic = total_for(0.42, now());
    let conservative = total_for(0.60, now());
    assert_eq!(optimistic, 8);
    assert_eq!(conservative, 2);
    assert!(conservative < optimistic);

    // The rule is symmetric in which instance holds which sample.
    let flipped = vec![
        sample(0.60, now()),
        sample(0.42, now() - Duration::seconds(600)),
    ];
    assert_eq!(freshest_used_fraction(&flipped), Some(0.60));

    // Within the threshold the freshest sample stands: 0.02 of drift is
    // ordinary observation jitter, not disagreement.
    let close = vec![
        sample(0.42, now()),
        sample(0.44, now() - Duration::seconds(600)),
    ];
    assert_eq!(freshest_used_fraction(&close), Some(0.42));
}

/// plan.md §28.6: the burn denominator is the sum of `governed_workers`
/// across all hosts. With no burn history the §9 controller holds at the
/// current count, so the derived total is exactly the cross-host sum, and
/// the split holds each host at its own current.
#[test]
fn the_burn_denominator_is_the_cross_host_worker_sum() {
    let accounts = config(12);
    let hosts = fleet(&[("codinghome", 3, 8, 6), ("gmktec", 2, 8, 6)]);
    let samples = vec![sample(0.42, now())];
    let decision = shared_account_total(
        "acct",
        &accounts,
        &AccountState::default(),
        &hosts,
        &samples,
        now(),
    )
    .unwrap();
    assert_eq!(decision.denominator, 5);
    assert_eq!(
        decision.total, 5,
        "no burn history: hold at the combined current"
    );
    assert_eq!(decision.shares["codinghome"], 3);
    assert_eq!(decision.shares["gmktec"], 2);
    assert_eq!(decision.shares.values().sum::<u32>(), 5);
    assert!(!decision.stale);
}

/// plan.md §22.7's shrink path through the §28.6 split: hosts without
/// ready demand step toward zero first, and the demanding host carries
/// what remains.
#[test]
fn a_shrinking_total_steps_workless_hosts_down_first() {
    let hosts = fleet(&[("codinghome", 4, 6, 3), ("gmktec", 2, 8, 0)]);
    let shares = split_account_total(3, &hosts);
    assert_eq!(shares["codinghome"], 3);
    assert_eq!(
        shares["gmktec"], 0,
        "no ready demand on a shrinking account"
    );
    assert_eq!(shares.values().sum::<u32>(), 3);
}

/// plan.md §22.15(2) on the §28.6 split: no share ever exceeds its host's
/// reserve ceiling, even when the total would place more -- the split
/// comes in under the total instead.
#[test]
fn no_share_exceeds_its_host_reserve() {
    let hosts = fleet(&[("a", 0, 3, 9), ("b", 0, 5, 9)]);
    let shares = split_account_total(20, &hosts);
    assert_eq!(shares["a"], 3);
    assert_eq!(shares["b"], 5);
    assert_eq!(
        shares.values().sum::<u32>(),
        8,
        "both hosts at their reserves, under the total of 20"
    );
}

/// plan.md §28.7 meeting §28.6: the receiver-side table feeds the
/// derivation -- a fresh or stale peer contributes exactly what it last
/// published, an expired peer contributes zeros and no sample, and a host
/// that is not a configured peer has no view at all.
#[test]
fn peer_records_feed_the_derivation_through_the_receiver_clock() {
    let federation = FederationConfig {
        host_id: "codinghome".to_owned(),
        instance_id_file: None,
        record_seq_file: None,
        host_capacity_state: None,
        account_identities: BTreeMap::new(),
        peers: vec![peer_config("gmktec")],
        peer_stale_after_seconds: 120,
        peer_expiry_seconds: 1_800,
        primary: true,
        repo_owners: BTreeMap::new(),
        pending_launches_file: None,
    };
    let t0 = Instant::now();
    let mut table = PeerTable::new(&federation, t0);
    let moment = |seconds: u64| -> subscription_governor::federation::PeerMoment {
        subscription_governor::federation::PeerMoment {
            monotonic: t0 + StdDuration::from_secs(seconds),
            wall: now() + Duration::seconds(seconds as i64),
        }
    };

    let published = peer_record(
        "gmktec",
        4,
        Some(sample(0.42, now() - Duration::seconds(60))),
    );
    table
        .ingest(&peer_config("gmktec"), Ok(published), &moment(0))
        .expect("a well-formed record is accepted");

    let expected = PeerAccountSnapshot {
        workers: 4,
        ready_demand: 5,
        quota_sample: Some(sample(0.42, now() - Duration::seconds(60))),
    };
    let fresh = table
        .peer_account_snapshot("gmktec", "acct", &moment(30))
        .unwrap();
    assert_eq!(fresh, expected, "fresh: the record as published");

    // Stale: still the last published numbers (§28.7) -- the instance
    // assumes the peer keeps running them.
    let stale = table
        .peer_account_snapshot("gmktec", "acct", &moment(121))
        .unwrap();
    assert_eq!(stale, expected);

    // Expired: zero workers, no demand, no sample -- the peer no longer
    // constrains the total (§28.7).
    let expired = table
        .peer_account_snapshot("gmktec", "acct", &moment(1_801))
        .unwrap();
    assert_eq!(expired, PeerAccountSnapshot::default());
    assert_eq!(
        table.peer_account_snapshot("nowhere", "acct", &moment(1_801)),
        None,
        "an unconfigured host has no view"
    );

    // A peer whose record does not mention the account contributes zeros,
    // not an error: its other accounts are none of this account's total.
    let mut unrelated = peer_record("gmktec", 4, None);
    let account = unrelated.accounts.remove("acct").unwrap();
    unrelated.accounts.insert(
        "other-acct".to_owned(),
        PeerAccount {
            account_id: "other-acct".to_owned(),
            ..account
        },
    );
    table
        .ingest(&peer_config("gmktec"), Ok(unrelated), &moment(1_802))
        .unwrap();
    let renewed = table
        .peer_account_snapshot("gmktec", "acct", &moment(1_802))
        .unwrap();
    assert_eq!(
        renewed,
        PeerAccountSnapshot {
            workers: 0,
            ready_demand: 0,
            quota_sample: None
        }
    );
}

/// The record's worker ceiling: the WP24 measured ceiling when the record
/// published one, else the account ceiling the §22.7 default prescribes.
#[test]
fn federation_host_ceiling_prefers_the_measured_ceiling() {
    let mut measured = peer_record("gmktec", 4, None);
    measured.host_ceiling = Some(HostCeiling {
        workers: 6,
        observed_at: now(),
    });
    let account_max = 12;
    let ceiling = measured
        .host_ceiling
        .as_ref()
        .map(|ceiling| ceiling.workers)
        .unwrap_or(account_max);
    assert_eq!(ceiling, 6);

    let unmeasured = peer_record("gmktec", 4, None);
    let ceiling = unmeasured
        .host_ceiling
        .as_ref()
        .map(|ceiling| ceiling.workers)
        .unwrap_or(account_max);
    assert_eq!(ceiling, 12, "unmeasured: the §22.7 account-ceiling default");
}

fn peer_config(host_id: &str) -> PeerConfig {
    PeerConfig {
        host_id: host_id.to_owned(),
        argv: None,
    }
}

/// One publishable peer record for account `acct`: `workers` governed
/// workers on its one pool, carrying `quota_sample` and a ready demand of
/// six beads with five observed backlog slots.
fn peer_record(host_id: &str, workers: u32, quota_sample: Option<PeerQuotaSample>) -> PeerRecord {
    PeerRecord {
        schema_version: PEER_RECORD_SCHEMA_VERSION,
        instance_id: "0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_owned(),
        host_id: host_id.to_owned(),
        record_seq: 7,
        observed_at: now(),
        accounts: BTreeMap::from([(
            "acct".to_owned(),
            PeerAccount {
                account_id: "acct".to_owned(),
                account_fingerprint: account_fingerprint("provider:acct-1"),
                pools: BTreeMap::from([(
                    "sg-acct-".to_owned(),
                    PeerPool {
                        governed_workers: workers,
                        pending_launches: 0,
                        desired: workers,
                        quota_sample,
                        ready_demand: PeerReadyDemand {
                            ready_beads: 6,
                            eligible_backlog_capacity: Some(5),
                        },
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
