//! Acceptance contract for plan.md §28.15 WP28 (§28.3, §28.7): the peer
//! pull transport over the existing generic command primitive, staleness
//! measured on the receiver's monotonic clock from receipt, freeze-on-stale
//! and expiry, and the 300 s clock-skew bound.
//!
//! Done when, table-driven: a killed peer freezes scale-up within
//! `peer_stale_after` and unfreezes after `peer_expiry`, a partial or
//! failed fetch is a stale peer rather than a partially applied record, a
//! record more than 300 s ahead of the local clock is rejected, and a peer
//! is never removed automatically.

use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use subscription_governor::config::{FederationConfig, PeerConfig};
use subscription_governor::federation::{
    account_fingerprint, default_peer_argv, pull_peer_record_bounded, PeerAccount, PeerFreshness,
    PeerMoment, PeerPool, PeerPullError, PeerReadyDemand, PeerRecord, PeerRecordError,
    PeerRejection, PeerTable, PEER_CLOCK_SKEW_TOLERANCE_SECONDS, PEER_PULL_TIMEOUT,
    PEER_RECORD_SCHEMA_VERSION,
};

/// A fixed wall-clock origin, so record timestamps are reproducible. The
/// monotonic axis uses a real `Instant` advanced by offsets -- no sleeps:
/// §28.7's aging is arithmetic on the receiver's clock, not waiting.
const WALL_ORIGIN: &str = "2026-10-28T12:00:00Z";

fn wall_origin() -> DateTime<Utc> {
    WALL_ORIGIN.parse().expect("fixed test timestamp")
}

fn moment(t0: Instant, wall0: DateTime<Utc>, seconds: u64) -> PeerMoment {
    PeerMoment {
        monotonic: t0 + Duration::from_secs(seconds),
        wall: wall0 + chrono::Duration::seconds(seconds as i64),
    }
}

fn peer(host_id: &str) -> PeerConfig {
    PeerConfig {
        host_id: host_id.to_owned(),
        argv: None,
    }
}

fn federation(peers: Vec<PeerConfig>, stale_after: u64, expiry: u64) -> FederationConfig {
    FederationConfig {
        host_id: "codinghome".to_owned(),
        instance_id_file: None,
        record_seq_file: None,
        host_capacity_state: None,
        account_identities: BTreeMap::new(),
        peers,
        peer_stale_after_seconds: stale_after,
        peer_expiry_seconds: expiry,
        primary: true,
        repo_owners: BTreeMap::new(),
        pending_launches_file: None,
    }
}

/// One publishable peer record: `workers` governed workers on one pool of
/// one account, published at `observed_at`.
fn record(host_id: &str, workers: u32, observed_at: DateTime<Utc>) -> PeerRecord {
    PeerRecord {
        schema_version: PEER_RECORD_SCHEMA_VERSION,
        instance_id: "0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_owned(),
        host_id: host_id.to_owned(),
        record_seq: 7,
        observed_at,
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

/// A fetch against a peer whose host no longer answers: the transport
/// itself failed (§28.3 -- any failure of the fetch is a stale peer).
fn killed_fetch() -> PeerPullError {
    PeerPullError::Transport {
        command: "ssh -o ConnectTimeout=5 gmktec subgov peer-state --json".to_owned(),
        source: anyhow::anyhow!("ssh: connect to host gmktec port 22: connection refused"),
    }
}

/// A fetch whose stdout was cut off mid-record: the command succeeded as
/// far as the receiver can tell, but the bytes are not a whole record.
fn truncated_stdout_fetch() -> PeerPullError {
    PeerPullError::Record {
        command: "ssh gmktec subgov peer-state --json".to_owned(),
        source: PeerRecordError::Malformed(
            serde_json::from_slice::<PeerRecord>(
                br#"{"schema_version":1,"instance_id":"0f1e2d3c4b5a69788796a5b4"#,
            )
            .unwrap_err(),
        ),
    }
}

/// A fetch from a peer speaking a schema major this binary does not know.
fn unknown_schema_fetch() -> PeerPullError {
    PeerPullError::Record {
        command: "ssh gmktec subgov peer-state --json".to_owned(),
        source: PeerRecordError::UnknownSchemaMajor {
            found: 2,
            supported: PEER_RECORD_SCHEMA_VERSION,
        },
    }
}

/// One row of the §28.7 staleness table: where the peer's last successful
/// receipt sits, when the instance looks, and what the receiver must then
/// conclude.
struct Row {
    name: &'static str,
    /// Offset (seconds from tracker start) of the peer's last successful
    /// receipt, or `None` when the peer was never heard from at all.
    last_receipt: Option<u64>,
    /// Offset (seconds from tracker start) of the observation instant.
    now: u64,
    freshness: PeerFreshness,
    /// Whether §28.7 forbids scaling up at this instant.
    frozen: bool,
    /// Workers the peer contributes to the account's total.
    workers: u32,
    /// Whether a `peer_expired` event has fired by this instant.
    expired_event: bool,
}

/// WP28's headline table (plan.md §28.15): a peer that published 3 workers
/// once and then died -- every pull after the receipt fails -- freezes
/// scale-up within `peer_stale_after` (default 120 s), keeps assuming the
/// last published 3 workers while stale, unfreezes and counts zero after
/// `peer_expiry` (default 30 min), and never leaves the peer set.
#[test]
fn killed_peer_freezes_scale_up_within_stale_after_and_unfreezes_after_expiry() {
    let rows = [
        // name                                               receipt  now    freshness          frozen workers event
        Row {
            name: "fresh record",
            last_receipt: Some(0),
            now: 10,
            freshness: PeerFreshness::Fresh,
            frozen: false,
            workers: 3,
            expired_event: false,
        },
        Row {
            name: "exactly at stale_after is not yet stale (the bound is exceeded, not met)",
            last_receipt: Some(0),
            now: 120,
            freshness: PeerFreshness::Fresh,
            frozen: false,
            workers: 3,
            expired_event: false,
        },
        Row {
            name: "killed peer freezes scale-up within peer_stale_after",
            last_receipt: Some(0),
            now: 121,
            freshness: PeerFreshness::Stale,
            frozen: true,
            workers: 3,
            expired_event: false,
        },
        Row {
            name: "deep stale keeps the freeze and the last published workers",
            last_receipt: Some(0),
            now: 1500,
            freshness: PeerFreshness::Stale,
            frozen: true,
            workers: 3,
            expired_event: false,
        },
        Row {
            name: "one second before expiry still freezes",
            last_receipt: Some(0),
            now: 1799,
            freshness: PeerFreshness::Stale,
            frozen: true,
            workers: 3,
            expired_event: false,
        },
        Row {
            name: "expiry reached: peer counts as zero and scale-up may resume",
            last_receipt: Some(0),
            now: 1800,
            freshness: PeerFreshness::Expired,
            frozen: false,
            workers: 0,
            expired_event: true,
        },
        Row {
            name: "long after expiry stays unfrozen",
            last_receipt: Some(0),
            now: 3600,
            freshness: PeerFreshness::Expired,
            frozen: false,
            workers: 0,
            expired_event: true,
        },
        Row {
            name:
                "a peer never heard from is stale from the start: no record, no basis to scale up",
            last_receipt: None,
            now: 5,
            freshness: PeerFreshness::Stale,
            frozen: true,
            workers: 0,
            expired_event: false,
        },
        Row {
            name: "a peer never heard from expires too",
            last_receipt: None,
            now: 1800,
            freshness: PeerFreshness::Expired,
            frozen: false,
            workers: 0,
            expired_event: true,
        },
    ];

    // The classification drives the freeze directly: only Stale freezes.
    assert!(!PeerFreshness::Fresh.freezes_scale_up());
    assert!(!PeerFreshness::Expired.freezes_scale_up());
    assert!(PeerFreshness::Stale.freezes_scale_up());

    for row in rows {
        let t0 = Instant::now();
        let wall0 = wall_origin();
        // The §28.7 defaults: 120 s stale, 30 min expiry.
        let mut table = PeerTable::new(&federation(vec![peer("gmktec")], 120, 1800), t0);

        if let Some(receipt) = row.last_receipt {
            let at = moment(t0, wall0, receipt);
            let published = record("gmktec", 3, at.wall);
            table.ingest(&peer("gmktec"), Ok(published), &at).unwrap();
        }

        // The peer is dead from its last receipt onward: this pull fails,
        // and the failure must neither count as a receipt nor touch the
        // last accepted record.
        let now = moment(t0, wall0, row.now);
        assert!(
            matches!(
                table.ingest(&peer("gmktec"), Err(killed_fetch()), &now),
                Err(PeerRejection::FetchFailed { .. })
            ),
            "{}: a failed fetch must be rejected",
            row.name
        );

        assert_eq!(
            table.freshness("gmktec", &now),
            Some(row.freshness),
            "{}: freshness",
            row.name
        );
        assert_eq!(
            table.any_peer_stale(&now),
            row.frozen,
            "{}: freeze",
            row.name
        );
        // The freeze gate in actuation terms: while frozen a scale-up
        // decision is clamped back to current, a scale-down passes.
        assert_eq!(
            table.clamp_scale_up(5, 2, &now),
            if row.frozen { 2 } else { 5 },
            "{}: scale-up gate",
            row.name
        );
        assert_eq!(
            table.clamp_scale_up(1, 2, &now),
            1,
            "{}: scaling down stays allowed while frozen",
            row.name
        );
        assert_eq!(
            table.peer_account_workers("gmktec", "acct", &now),
            Some(row.workers),
            "{}: workers contributed to the account total",
            row.name
        );
        let events = table.drain_events();
        assert_eq!(
            events.len(),
            usize::from(row.expired_event),
            "{}: peer_expired events",
            row.name
        );

        // §28.7: a peer is never removed automatically.
        assert_eq!(
            table.configured_peers(),
            vec!["gmktec".to_owned()],
            "{}",
            row.name
        );
    }
}

/// §28.7: the age is measured on the receiver's monotonic clock from
/// receipt. A record published in the past does not arrive pre-staled, and
/// a record whose own timestamp is current does not stay fresh after the
/// receipt ages out -- peer timestamps order samples only.
#[test]
fn staleness_ages_from_receipt_on_the_receivers_clock_never_the_peer_timestamp() {
    let t0 = Instant::now();
    let wall0 = wall_origin();
    let mut table = PeerTable::new(&federation(vec![peer("gmktec")], 120, 1800), t0);

    // Published ten minutes before the pull reached us: on receipt it is
    // fresh, and stays fresh for `peer_stale_after` after the receipt.
    let published_long_ago = record("gmktec", 3, wall0 - chrono::Duration::minutes(10));
    table
        .ingest(
            &peer("gmktec"),
            Ok(published_long_ago),
            &moment(t0, wall0, 0),
        )
        .unwrap();
    assert_eq!(
        table.freshness("gmktec", &moment(t0, wall0, 119)),
        Some(PeerFreshness::Fresh),
        "an old timestamp must not pre-stale a freshly received record"
    );
    assert_eq!(
        table.peer_account_workers("gmktec", "acct", &moment(t0, wall0, 119)),
        Some(3)
    );

    // The same record received once, then never again: by 121 s after the
    // receipt the peer is stale no matter what its timestamp said.
    assert_eq!(
        table.freshness("gmktec", &moment(t0, wall0, 121)),
        Some(PeerFreshness::Stale),
        "age runs from the receipt, not the record's claim about itself"
    );

    // A second peer whose record carries a future timestamp within the
    // 300 s tolerance is accepted on the same terms: the tolerance is the
    // only thing a peer's clock buys it (§28.7).
    let mut table = PeerTable::new(&federation(vec![peer("gmktec")], 120, 1800), t0);
    let slightly_ahead = record("gmktec", 2, wall0 + chrono::Duration::seconds(299));
    table
        .ingest(&peer("gmktec"), Ok(slightly_ahead), &moment(t0, wall0, 0))
        .unwrap();
    assert_eq!(
        table.freshness("gmktec", &moment(t0, wall0, 1)),
        Some(PeerFreshness::Fresh)
    );
}

/// §28.3: `subgov peer-state` prints one record, so any failure of the
/// fetch -- a dead transport, unusable bytes, an unusable schema, a record
/// from the wrong host -- is a stale peer. Nothing is ever partially
/// applied: the tracker stores a whole `PeerRecord` or an error, and a
/// rejected fetch leaves both the last record and its receipt time alone.
/// One constructor of a failing pull, so the failure table can hand each
/// case to a fresh tracker.
type FetchFailure = fn() -> PeerPullError;

#[test]
fn a_partial_or_failed_fetch_is_a_stale_peer_never_a_partial_record() {
    let failures: [(&str, FetchFailure); 3] = [
        ("transport failure", killed_fetch),
        ("truncated stdout", truncated_stdout_fetch),
        ("unknown schema major", unknown_schema_fetch),
    ];

    for (name, failure) in failures {
        let t0 = Instant::now();
        let wall0 = wall_origin();
        let mut table = PeerTable::new(&federation(vec![peer("gmktec")], 120, 1800), t0);
        table
            .ingest(
                &peer("gmktec"),
                Ok(record("gmktec", 3, wall0)),
                &moment(t0, wall0, 0),
            )
            .unwrap();

        // The failing pull arrives while the previous receipt is still
        // fresh: it must not count as new knowledge.
        let at_ten_seconds = moment(t0, wall0, 10);
        assert!(
            matches!(
                table.ingest(&peer("gmktec"), Err(failure()), &at_ten_seconds),
                Err(PeerRejection::FetchFailed { .. })
            ),
            "{name}: rejected"
        );
        assert_eq!(
            table.freshness("gmktec", &at_ten_seconds),
            Some(PeerFreshness::Fresh),
            "{name}: a failed fetch is not a receipt"
        );
        assert_eq!(
            table.peer_account_workers("gmktec", "acct", &at_ten_seconds),
            Some(3),
            "{name}: the last accepted record survives whole"
        );

        // And it keeps aging from the original receipt: stale by 121 s.
        let at_stale = moment(t0, wall0, 121);
        assert_eq!(
            table.freshness("gmktec", &at_stale),
            Some(PeerFreshness::Stale),
            "{name}"
        );
        assert_eq!(
            table.peer_account_workers("gmktec", "acct", &at_stale),
            Some(3),
            "{name}: a stale peer still runs its last published workers"
        );
    }

    // A record claiming to be a different host than the peer it was pulled
    // from is rejected the same way, before it can enter the table.
    let t0 = Instant::now();
    let wall0 = wall_origin();
    let mut table = PeerTable::new(&federation(vec![peer("gmktec")], 120, 1800), t0);
    let impostor = record("lab", 9, wall0);
    assert!(matches!(
        table.ingest(&peer("gmktec"), Ok(impostor), &moment(t0, wall0, 0)),
        Err(PeerRejection::HostMismatch { .. })
    ));
    assert_eq!(
        table.freshness("gmktec", &moment(t0, wall0, 0)),
        Some(PeerFreshness::Stale),
        "the rejected record must leave the peer never-received"
    );
    assert_eq!(
        table.peer_account_workers("gmktec", "acct", &moment(t0, wall0, 0)),
        Some(0)
    );
}

/// §28.7: a record more than 300 s ahead of the receiver's clock is
/// rejected. Exactly 300 s is "at" the tolerance, not beyond it.
#[test]
fn a_record_more_than_300_seconds_ahead_of_the_local_clock_is_rejected() {
    let t0 = Instant::now();
    let wall0 = wall_origin();
    let mut table = PeerTable::new(
        &federation(vec![peer("gmktec"), peer("lab")], 120, 1800),
        t0,
    );
    table
        .ingest(
            &peer("gmktec"),
            Ok(record("gmktec", 3, wall0)),
            &moment(t0, wall0, 0),
        )
        .unwrap();

    for (ahead_by, accepted) in [
        (PEER_CLOCK_SKEW_TOLERANCE_SECONDS - 1, true),
        (PEER_CLOCK_SKEW_TOLERANCE_SECONDS, true),
        (PEER_CLOCK_SKEW_TOLERANCE_SECONDS + 1, false),
        (PEER_CLOCK_SKEW_TOLERANCE_SECONDS * 60, false),
    ] {
        let now = moment(t0, wall0, 5);
        let future_record = record("gmktec", 4, now.wall + chrono::Duration::seconds(ahead_by));
        let outcome = table.ingest(&peer("gmktec"), Ok(future_record), &now);
        if accepted {
            assert!(outcome.is_ok(), "{ahead_by}s ahead is within tolerance");
        } else {
            assert!(
                matches!(outcome, Err(PeerRejection::ClockSkew { ahead_by_seconds, .. }) if ahead_by_seconds == ahead_by),
                "{ahead_by}s ahead must be a clock-skew rejection"
            );
            // The rejected fetch changed nothing: the peer still runs the
            // workers of its last accepted record (the 300 s-ahead one),
            // from its last accepted receipt.
            assert_eq!(
                table.peer_account_workers("gmktec", "acct", &now),
                Some(4),
                "{ahead_by}s: rejection must not touch the last record"
            );
        }
    }

    // A skewed first record leaves the peer never-received -- stale, but
    // not holding fabricated workers.
    let now = moment(t0, wall0, 5);
    let skewed = record("lab", 8, now.wall + chrono::Duration::seconds(301));
    assert!(matches!(
        table.ingest(&peer("lab"), Ok(skewed), &now),
        Err(PeerRejection::ClockSkew { .. })
    ));
    assert_eq!(table.freshness("lab", &now), Some(PeerFreshness::Stale));
    assert_eq!(table.peer_account_workers("lab", "acct", &now), Some(0));
}

/// §28.7: the freeze is per-instance across every configured peer -- one
/// stale peer is enough -- and expiry is what lifts it.
#[test]
fn one_stale_peer_freezes_the_instance_while_fresh_peers_do_not() {
    let t0 = Instant::now();
    let wall0 = wall_origin();
    let mut table = PeerTable::new(
        &federation(vec![peer("gmktec"), peer("lab")], 120, 1800),
        t0,
    );
    let start = moment(t0, wall0, 0);
    table
        .ingest(&peer("gmktec"), Ok(record("gmktec", 2, start.wall)), &start)
        .unwrap();
    table
        .ingest(&peer("lab"), Ok(record("lab", 4, start.wall)), &start)
        .unwrap();

    // Both fresh: scale-up passes the gate unchanged.
    assert!(!table.any_peer_stale(&start));
    assert_eq!(table.clamp_scale_up(6, 1, &start), 6);

    // gmktec's receipts stop; lab keeps answering. One stale peer freezes.
    let later = moment(t0, wall0, 130);
    table
        .ingest(&peer("lab"), Ok(record("lab", 4, later.wall)), &later)
        .unwrap();
    assert!(table.any_peer_stale(&later));
    assert_eq!(table.clamp_scale_up(6, 1, &later), 1);
    // The stale peer still contributes its last published workers; the
    // fresh one contributes its current ones.
    assert_eq!(
        table.peer_account_workers("gmktec", "acct", &later),
        Some(2)
    );
    assert_eq!(table.peer_account_workers("lab", "acct", &later), Some(4));

    // gmktec expires 30 min after its last receipt: the freeze lifts and
    // it stops contributing workers at all. (lab is pulled again at 1700 s
    // so it is still fresh here -- only gmktec has gone silent.)
    let still_fresh = moment(t0, wall0, 1700);
    table
        .ingest(
            &peer("lab"),
            Ok(record("lab", 4, still_fresh.wall)),
            &still_fresh,
        )
        .unwrap();
    let after_expiry = moment(t0, wall0, 1800);
    assert!(!table.any_peer_stale(&after_expiry));
    assert_eq!(table.clamp_scale_up(6, 1, &after_expiry), 6);
    assert_eq!(
        table.peer_account_workers("gmktec", "acct", &after_expiry),
        Some(0)
    );
    assert_eq!(
        table.peer_account_workers("lab", "acct", &after_expiry),
        Some(4)
    );
}

/// §28.7: expiry emits `peer_expired` once per staleness episode, and a
/// peer that answers again recovers immediately and whole. The configured
/// peer set never changes on its own, and an unconfigured host's record is
/// refused.
#[test]
fn an_expired_peer_recovers_whole_and_peers_are_never_removed_automatically() {
    let t0 = Instant::now();
    let wall0 = wall_origin();
    let mut table = PeerTable::new(&federation(vec![peer("gmktec")], 120, 1800), t0);
    table
        .ingest(
            &peer("gmktec"),
            Ok(record("gmktec", 3, wall0)),
            &moment(t0, wall0, 0),
        )
        .unwrap();

    // Expiry fires the event exactly once per episode.
    let expired_at = moment(t0, wall0, 1800);
    assert_eq!(
        table.freshness("gmktec", &expired_at),
        Some(PeerFreshness::Expired)
    );
    assert_eq!(
        table.freshness("gmktec", &moment(t0, wall0, 1900)),
        Some(PeerFreshness::Expired)
    );
    let events = table.drain_events();
    assert_eq!(
        events.len(),
        1,
        "peer_expired fires once per staleness episode"
    );
    let event = &events[0];
    assert_eq!(event["event"], "peer_expired");
    assert_eq!(event["severity"], "warn");
    assert_eq!(event["peer_host_id"], "gmktec");
    assert_eq!(event["last_record_seq"], 7);
    assert_eq!(event["action"], "counted_as_zero_workers");
    assert_eq!(
        event["time"],
        serde_json::to_value(expired_at.wall).unwrap()
    );
    assert!(table.drain_events().is_empty(), "no repeat emission");

    // The peer answers again 2000 s in: it is fresh from that receipt,
    // with its newly published workers, and a later episode can fire again.
    let recovery = moment(t0, wall0, 2000);
    table
        .ingest(
            &peer("gmktec"),
            Ok(record("gmktec", 5, recovery.wall)),
            &recovery,
        )
        .unwrap();
    assert_eq!(
        table.freshness("gmktec", &recovery),
        Some(PeerFreshness::Fresh)
    );
    assert_eq!(
        table.peer_account_workers("gmktec", "acct", &recovery),
        Some(5)
    );
    let second_expiry = moment(t0, wall0, 3800);
    assert_eq!(
        table.freshness("gmktec", &second_expiry),
        Some(PeerFreshness::Expired)
    );
    assert_eq!(
        table.drain_events().len(),
        1,
        "a second episode emits again"
    );

    // Through all of that the configured peer set never moved, and a
    // record from a host nobody configured is refused outright.
    assert_eq!(table.configured_peers(), vec!["gmktec".to_owned()]);
    let stranger = record("hetzner-ex44", 6, wall0);
    assert!(matches!(
        table.ingest(&peer("hetzner-ex44"), Ok(stranger), &recovery),
        Err(PeerRejection::Unconfigured { .. })
    ));
    assert_eq!(table.configured_peers(), vec!["gmktec".to_owned()]);
}

/// §28.3/§22.10: the pull is the generic command primitive with a bounded
/// round trip. A fake `ssh` stands in for the peer host: it prints one
/// record, or fails in every way a real peer can.
fn write_script(path: &Path, body: &str) -> PathBuf {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    path.to_path_buf()
}

#[test]
fn the_pull_transport_runs_argv_through_the_generic_command_primitive() {
    let dir = tempfile::tempdir().unwrap();
    let published = record("gmktec", 3, wall_origin());
    let json = published.to_json().unwrap();

    // A peer that answers: the record round-trips exactly, one line.
    let answering = write_script(
        &dir.path().join("peer-ok.sh"),
        &format!("printf '%s' '{json}'"),
    );
    let argv = vec![answering.to_str().unwrap().to_owned()];
    let pulled = pull_peer_record_bounded(&argv, PEER_PULL_TIMEOUT).unwrap();
    assert_eq!(pulled, published);

    // A nonzero exit is a failed fetch even when stdout holds a record's
    // prefix -- bytes were produced, but no record is trusted from them.
    let half = &json[..json.len() / 2];
    let partial = write_script(
        &dir.path().join("peer-partial.sh"),
        &format!("printf '%s' '{half}'; exit 1"),
    );
    let error =
        pull_peer_record_bounded(&[partial.to_str().unwrap().to_owned()], PEER_PULL_TIMEOUT)
            .unwrap_err();
    assert!(
        matches!(&error, PeerPullError::Transport { command, .. } if command.contains("peer-partial.sh")),
        "a nonzero exit is a transport failure: {error}"
    );

    // Truncated but successful stdout is an unusable record, not half of one.
    let truncated = write_script(
        &dir.path().join("peer-truncated.sh"),
        &format!("printf '%s' '{}'", &json[..json.len() - 40]),
    );
    let error =
        pull_peer_record_bounded(&[truncated.to_str().unwrap().to_owned()], PEER_PULL_TIMEOUT)
            .unwrap_err();
    assert!(
        matches!(&error, PeerPullError::Record { .. }),
        "truncated stdout must not parse as a record: {error}"
    );

    // A complete record followed by more output is equally unusable: the
    // transport reads exactly one record, never a stream it might resync on.
    let chatty = write_script(
        &dir.path().join("peer-chatty.sh"),
        &format!("printf '%s\\n{json}\\n' '{json}'"),
    );
    assert!(matches!(
        pull_peer_record_bounded(&[chatty.to_str().unwrap().to_owned()], PEER_PULL_TIMEOUT),
        Err(PeerPullError::Record { .. })
    ));

    // A hung peer is killed by the round-trip bound: the timeout covers
    // the whole fetch, connection and remote runtime included (§22.10).
    let hung = write_script(&dir.path().join("peer-hung.sh"), "sleep 30");
    let started = Instant::now();
    let error =
        pull_peer_record_bounded(&[hung.to_str().unwrap().to_owned()], Duration::from_secs(2))
            .unwrap_err();
    assert!(
        matches!(&error, PeerPullError::Transport { .. }),
        "a hung peer is a transport failure: {error}"
    );
    assert!(error.to_string().contains("timed out"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the bound must cover the whole round trip, not wait for the child"
    );

    // A command that cannot spawn at all fails the same way.
    let missing = dir.path().join("does-not-exist.sh");
    assert!(matches!(
        pull_peer_record_bounded(&[missing.to_str().unwrap().to_owned()], PEER_PULL_TIMEOUT),
        Err(PeerPullError::Transport { .. })
    ));

    // The sanctioned default argv (§28.3/§22.10): ssh, a bounded
    // ConnectTimeout before the host, then the read-only command.
    assert_eq!(
        default_peer_argv("gmktec", Duration::from_secs(5)),
        vec![
            "ssh",
            "-o",
            "ConnectTimeout=5",
            "gmktec",
            "subgov",
            "peer-state",
            "--json"
        ]
    );
    // The connect timeout never rounds down to zero.
    assert!(default_peer_argv("gmktec", Duration::from_millis(1))
        .contains(&"ConnectTimeout=1".to_owned()));
}
