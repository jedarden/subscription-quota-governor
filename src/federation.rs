//! plan.md §28 (WP27, WP28): the versioned federation peer record, its
//! schema gate, account identity, the read-only projection of this
//! instance's state that `subgov peer-state --json` prints, and the pull
//! transport plus receiver-side staleness that age those records (§28.3,
//! §28.7).
//!
//! A peer record is what one subgov instance publishes about itself and
//! what its peers pull (§28.3). It is versioned independently of the
//! persisted state (§28.4): an unknown major version is rejected on its
//! own terms, while unknown fields inside a known major are ignored so
//! minor additions stay compatible. The record builders are read-only with
//! respect to the fleet: no source is polled, no observer is asked, no
//! worker is touched. The only files `build_local_record` writes are this
//! instance's own identity (§28.4: a persisted random `instance_id` and a
//! monotonic `record_seq`).
//!
//! On the receiving side, `PeerTable` ages each peer's last accepted
//! record on this host's monotonic clock from receipt -- never from the
//! peer's timestamp -- and freezes scale-up while any peer is stale
//! (§28.7). WP29 builds on that table: every instance derives the same
//! account total from the freshest held quota sample and the cross-host
//! worker sum, then splits it across hosts with the §22.7 placement
//! function over peer-record headroom (`shared_account_total` and
//! `split_account_total` below).
//!
//! No credentials, prompts or raw provider bodies ever enter a record
//! (§14): every field below is a count, a timestamp, a derived fingerprint,
//! or a workspace origin URL that has had its userinfo stripped.

use crate::config::{AccountConfig, Config, ResourceReserveConfig};
use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rand::Rng;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str;
use std::time::{Duration, Instant};

/// The only peer-record major version this binary publishes, and the only
/// one it accepts (§28.4: "versioned independently of state"). The state
/// file's `schema_version` counts minor migrations of on-disk state and is
/// a separate sequence entirely.
pub const PEER_RECORD_SCHEMA_VERSION: u32 = 1;

/// Length of the truncated account fingerprint in hex characters (64 bits).
/// Enough to distinguish the handful of accounts a federation governs while
/// keeping the provider identifier itself out of every record (§14).
const FINGERPRINT_HEX_CHARS: usize = 16;

/// The §28.4 peer record: one instance's publishable view of its host.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PeerRecord {
    pub schema_version: u32,
    /// Stable random identity, persisted across restarts (§28.4). Never
    /// derived from the hostname, so a cloned host does not inherit one.
    pub instance_id: String,
    pub host_id: String,
    /// Monotonic counter bumped on every publication, so a receiver can
    /// order records from one instance even when timestamps tie.
    pub record_seq: u64,
    pub observed_at: DateTime<Utc>,
    /// Governed accounts keyed by account id. `BTreeMap` keeps the
    /// serialization byte-stable, which §28.6's identical-everywhere
    /// derivations compare.
    #[serde(default)]
    pub accounts: BTreeMap<String, PeerAccount>,
    /// The WP24 measured host worker ceiling, when the host-capacity
    /// adapter has published evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_ceiling: Option<HostCeiling>,
    /// WP24 launch reservations: workers an asynchronous launch has claimed
    /// but whose processes have not been observed yet, keyed by pool.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub host_reservations: BTreeMap<String, HostReservation>,
    /// The host's configured resource reserve (§22.3), when one is declared
    /// for this host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_reserve: Option<ResourceReserveDigest>,
    /// Git origin URLs of the workspaces this instance will seed, keyed by
    /// workspace path (§28.10: peers compare these to detect two hosts
    /// working the same repository).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workspace_origins: BTreeMap<String, String>,
}

/// One governed account as a peer sees it (§28.4/§28.5).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PeerAccount {
    pub account_id: String,
    /// Truncated hash of the provider's non-secret account identifier
    /// (§28.4). Published instead of the identifier so records stay free of
    /// provider-specific identity material while still detecting two
    /// labels that claim one provider account (§28.5).
    pub account_fingerprint: String,
    /// This instance's pools for the account, keyed by worker prefix (or
    /// account id for a pool with no prefix). Today an account governs one
    /// pool; the map keeps the §28.4 per-account-and-pool shape.
    #[serde(default)]
    pub pools: BTreeMap<String, PeerPool>,
}

/// One pool's worker and demand state on this host (§28.4).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PeerPool {
    /// Workers with this pool's prefix currently observed on this host.
    pub governed_workers: u32,
    /// Launches published but not yet observed (§28.8). Always zero until
    /// WP30 persists in-flight launches; the field exists so the schema
    /// does not change when they arrive.
    pub pending_launches: u32,
    /// This host's placement target from the last completed cycle.
    pub desired: u32,
    /// The freshest quota sample this instance holds for the account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_sample: Option<PeerQuotaSample>,
    /// Ready demand for this host's workspaces (§26.4).
    pub ready_demand: PeerReadyDemand,
}

/// The one quota sample a record carries per pool (§28.4). Exactly the
/// fields §28.6's shared-total derivation consumes -- never a raw provider
/// response.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PeerQuotaSample {
    pub window: String,
    pub used_fraction: f64,
    pub resets_at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
}

/// Ready demand as a count plus the backlog capacity it can carry (§26.4).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PeerReadyDemand {
    /// Ready, claimable beads across this host's workspaces for the pool.
    #[serde(default)]
    pub ready_beads: u32,
    /// Worker slots the eligible backlog could fill, when observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eligible_backlog_capacity: Option<u32>,
}

impl PeerReadyDemand {
    /// The worker count this demand can absorb (§26.4): the observed
    /// backlog slot capacity when one was measured, the ready-bead count
    /// otherwise. This is the bound §28.6 caps a host's share with -- spare
    /// quota never launches a worker into work that does not exist.
    pub fn worker_bound(&self) -> u32 {
        self.eligible_backlog_capacity.unwrap_or(self.ready_beads)
    }
}

/// The WP24 measured ceiling (§26.6): how many workers this host can carry,
/// measured from memory pressure and load, not configured.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HostCeiling {
    pub workers: u32,
    pub observed_at: DateTime<Utc>,
}

/// Workers reserved by an in-flight launch whose processes are not yet
/// observable (WP24's reservation high-water).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HostReservation {
    pub count: u32,
    pub reserved_at: DateTime<Utc>,
}

/// The configured resource reserve a placement may not consume (§22.3).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ResourceReserveDigest {
    pub cpu_reserve_fraction: f64,
    pub mem_reserve_mb: u64,
}

impl From<&ResourceReserveConfig> for ResourceReserveDigest {
    fn from(reserve: &ResourceReserveConfig) -> Self {
        Self {
            cpu_reserve_fraction: reserve.cpu_reserve_fraction,
            mem_reserve_mb: reserve.mem_reserve_mb,
        }
    }
}

/// Why a peer record was rejected (§28.4).
#[derive(Debug, thiserror::Error)]
pub enum PeerRecordError {
    /// An unknown major is rejected outright, never best-effort parsed: a
    /// record this binary cannot fully interpret must not be partially
    /// trusted with quota decisions.
    #[error("unknown peer record schema_version {found} (this binary supports major {supported})")]
    UnknownSchemaMajor { found: u32, supported: u32 },
    #[error("peer record is not valid JSON: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(String),
}

impl PeerRecord {
    /// Parses and validates one peer record, rejecting an unknown major
    /// (§28.4) and any field that could not have come from a conforming
    /// publisher.
    pub fn parse(bytes: &[u8]) -> Result<Self, PeerRecordError> {
        #[derive(Deserialize)]
        struct VersionProbe {
            schema_version: u32,
        }
        // Read the version first so an unknown major is rejected on its own
        // terms instead of as a pile of unrelated missing-field errors.
        let probe: VersionProbe = serde_json::from_slice(bytes)?;
        if probe.schema_version != PEER_RECORD_SCHEMA_VERSION {
            return Err(PeerRecordError::UnknownSchemaMajor {
                found: probe.schema_version,
                supported: PEER_RECORD_SCHEMA_VERSION,
            });
        }
        let record: Self = serde_json::from_slice(bytes)?;
        record.validate()?;
        Ok(record)
    }

    /// Serializes to the single-line JSON the §28.3 pull transport reads.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("failed to serialize peer record")
    }

    fn validate(&self) -> Result<(), PeerRecordError> {
        validate_identifier("instance_id", &self.instance_id)?;
        validate_identifier("host_id", &self.host_id)?;
        for (account_id, account) in &self.accounts {
            validate_identifier("account_id", account_id)?;
            if account.account_id != *account_id {
                return Err(PeerRecordError::Invalid(format!(
                    "account {account_id}: body carries account_id {}",
                    account.account_id
                )));
            }
            if !is_fingerprint(&account.account_fingerprint) {
                return Err(PeerRecordError::Invalid(format!(
                    "account {account_id}: account_fingerprint must be \
                     {FINGERPRINT_HEX_CHARS} lowercase hex characters"
                )));
            }
            for (pool, state) in &account.pools {
                validate_identifier("pool", pool)?;
                if let Some(sample) = &state.quota_sample {
                    if sample.window.trim().is_empty() {
                        return Err(PeerRecordError::Invalid(format!(
                            "account {account_id} pool {pool}: quota sample window must not be empty"
                        )));
                    }
                    if !sample.used_fraction.is_finite()
                        || !(0.0..=1.0).contains(&sample.used_fraction)
                    {
                        return Err(PeerRecordError::Invalid(format!(
                            "account {account_id} pool {pool}: used_fraction must be in [0, 1]"
                        )));
                    }
                }
            }
        }
        for (path, origin) in &self.workspace_origins {
            if path.trim().is_empty() || origin.trim().is_empty() {
                return Err(PeerRecordError::Invalid(
                    "workspace_origins must not carry empty paths or origins".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

fn validate_identifier(field: &str, value: &str) -> Result<(), PeerRecordError> {
    if value.trim().is_empty() || value.chars().any(char::is_whitespace) {
        return Err(PeerRecordError::Invalid(format!(
            "{field} must be non-empty and free of whitespace"
        )));
    }
    Ok(())
}

fn is_fingerprint(value: &str) -> bool {
    value.len() == FINGERPRINT_HEX_CHARS
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// The truncated hash of a provider's non-secret account identifier
/// (§28.4). The identifier itself never leaves the instance that holds it
/// (§14); peers compare fingerprints.
pub fn account_fingerprint(identifier: &str) -> String {
    let digest = Sha256::digest(identifier.as_bytes());
    let hex: String = digest
        .iter()
        .take(FINGERPRINT_HEX_CHARS / 2)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    debug_assert_eq!(hex.len(), FINGERPRINT_HEX_CHARS);
    hex
}

/// §28.5 account identity: same account only when both the account id and
/// the fingerprint match.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountIdentityVerdict {
    /// Same account: both fields agree. Quota totals may merge.
    Same,
    /// Different account ids: independent accounts that never share a
    /// total (§28.11). Ordinary, not an error.
    Distinct,
    /// Same account id with a different fingerprint (§28.5): an error
    /// event, and the peer record is treated as a different account.
    FingerprintMismatch,
}

impl AccountIdentityVerdict {
    /// Whether the two accounts' quota totals may be derived together.
    pub fn shares_account_total(&self) -> bool {
        matches!(self, Self::Same)
    }

    /// Whether §28.5 requires an error event.
    pub fn is_error(&self) -> bool {
        matches!(self, Self::FingerprintMismatch)
    }
}

/// Classifies a peer's account against the local one (§28.5).
pub fn classify_account(local: &PeerAccount, peer: &PeerAccount) -> AccountIdentityVerdict {
    if local.account_id != peer.account_id {
        return AccountIdentityVerdict::Distinct;
    }
    if local.account_fingerprint != peer.account_fingerprint {
        AccountIdentityVerdict::FingerprintMismatch
    } else {
        AccountIdentityVerdict::Same
    }
}

/// The §28.5 error event for a fingerprint mismatch. Fingerprints are
/// truncated hashes, so the event may carry them without leaking the
/// provider identifier.
pub fn fingerprint_mismatch_event(
    local: &PeerAccount,
    peer: &PeerAccount,
    peer_host_id: &str,
    time: DateTime<Utc>,
) -> Value {
    serde_json::json!({
        "event": "account_fingerprint_mismatch",
        "severity": "error",
        "time": time,
        "peer_host_id": peer_host_id,
        "account_id": local.account_id,
        "local_fingerprint": local.account_fingerprint,
        "peer_fingerprint": peer.account_fingerprint,
        "action": "treated_as_different_account",
    })
}

// ---------------------------------------------------------------------------
// §28.3/§28.7 (WP28): the pull transport, staleness on the receiver's
// clock, freeze-on-stale and expiry
// ---------------------------------------------------------------------------

/// The §7.4 shared command bound, applied to a peer pull's whole round trip
/// -- connection, authentication and the remote `peer-state` run -- not just
/// local execution (§22.10).
pub const PEER_PULL_TIMEOUT: Duration = crate::source::COMMAND_TIMEOUT;

/// The bounded `ConnectTimeout` the default §28.3 argv passes to ssh
/// (§22.10: key-based auth already present on the host; subgov never
/// manages or stores an SSH credential itself). Smaller than the round-trip
/// bound so an unreachable peer fails on connect rather than eating the
/// whole budget.
pub const PEER_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// §28.7: a record more than this many seconds ahead of the receiver's wall
/// clock is rejected. Peer-reported timestamps order samples only; they are
/// never the basis of a staleness or freshness decision.
pub const PEER_CLOCK_SKEW_TOLERANCE_SECONDS: i64 = 300;

/// Why a peer pull did not produce a usable record (§28.3). Every variant
/// is a failed fetch: `subgov peer-state` prints exactly one record or
/// fails, so a peer is ever stale or fresh, never partially applied.
#[derive(Debug, thiserror::Error)]
pub enum PeerPullError {
    /// The generic command primitive failed: the command would not spawn,
    /// exited non-zero, overshot the round-trip bound, wrote past the size
    /// cap, or its stdout could not be read.
    #[error("peer pull command `{command}` failed: {source}")]
    Transport {
        command: String,
        #[source]
        source: anyhow::Error,
    },
    /// The command succeeded but its stdout is not a record this binary
    /// fully understands: malformed JSON, an unknown schema major, or a
    /// field a conforming publisher could not have written.
    #[error("peer pull command `{command}` produced an unusable record: {source}")]
    Record {
        command: String,
        #[source]
        source: PeerRecordError,
    },
}

/// The sanctioned §28.3 pull argv for a peer that does not override one:
/// `ssh` to the peer's host with a bounded `ConnectTimeout` ahead of it
/// (§22.10), then the read-only `subgov peer-state --json`.
pub fn default_peer_argv(host_id: &str, connect_timeout: Duration) -> Vec<String> {
    vec![
        "ssh".to_owned(),
        "-o".to_owned(),
        format!("ConnectTimeout={}", connect_timeout.as_secs().max(1)),
        host_id.to_owned(),
        "subgov".to_owned(),
        "peer-state".to_owned(),
        "--json".to_owned(),
    ]
}

/// Pulls one configured peer's record over the §28.3 transport: the
/// existing generic command primitive (§22.10) under the shared §7.4
/// round-trip bound. The command runs argv directly, never through a local
/// shell; whatever it prints must parse as one whole record or the pull
/// fails.
pub fn pull_peer_record(peer: &crate::config::PeerConfig) -> Result<PeerRecord, PeerPullError> {
    let argv = peer
        .argv
        .clone()
        .unwrap_or_else(|| default_peer_argv(&peer.host_id, PEER_CONNECT_TIMEOUT));
    pull_peer_record_bounded(&argv, PEER_PULL_TIMEOUT)
}

/// The same transport with an explicit argv and round-trip bound -- the
/// seam the staleness contract drives with a fake `ssh` and a short
/// timeout, and the hook a caller with its own bound can use.
pub fn pull_peer_record_bounded(
    argv: &[String],
    timeout: Duration,
) -> Result<PeerRecord, PeerPullError> {
    let command = argv.join(" ");
    let bytes = crate::source::read_generic_command_bytes(argv, timeout).map_err(|source| {
        PeerPullError::Transport {
            command: command.clone(),
            source,
        }
    })?;
    PeerRecord::parse(&bytes).map_err(|source| PeerPullError::Record { command, source })
}

/// The receiver's two clocks at one instant (§28.7): the monotonic clock
/// that ages peer records from receipt, and the wall clock the skew bound
/// compares a record's own timestamp against. Both arrive as plain
/// parameters -- the same no-singleton rule as `controller::evaluate`.
#[derive(Clone, Copy, Debug)]
pub struct PeerMoment {
    /// Monotonic, so an NTP step or manual clock change on the receiver can
    /// neither fake freshness nor strand a peer in stale forever.
    pub monotonic: Instant,
    pub wall: DateTime<Utc>,
}

impl PeerMoment {
    pub fn now() -> Self {
        Self {
            monotonic: Instant::now(),
            wall: Utc::now(),
        }
    }
}

/// §28.7's classification of one peer, measured on the receiver's clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerFreshness {
    /// Last receipt is within `peer_stale_after`: the record is used as
    /// published and does not restrict scaling.
    Fresh,
    /// Older than `peer_stale_after` but not yet `peer_expiry`: the peer is
    /// still assumed to run its last published `governed_workers`, and the
    /// instance may scale down but never up.
    Stale,
    /// Older than `peer_expiry`: the peer counts as zero workers, a
    /// `peer_expired` event has been emitted, and scale-up may resume.
    Expired,
}

impl PeerFreshness {
    /// Whether §28.7 forbids this instance from scaling up while any peer
    /// is in this state.
    pub fn freezes_scale_up(&self) -> bool {
        matches!(self, Self::Stale)
    }
}

/// Why `PeerTable::ingest` did not accept a fetched record. Every variant
/// leaves the peer aging from its previous receipt unchanged: a rejected
/// fetch is a stale peer, never a partial one (§28.3/§28.7).
#[derive(Debug, thiserror::Error)]
pub enum PeerRejection {
    /// The fetch itself failed (transport or unusable record).
    #[error("{source}")]
    FetchFailed {
        #[source]
        source: PeerPullError,
    },
    /// The record's own timestamp is more than 300 s ahead of the
    /// receiver's wall clock (§28.7): peer clocks order samples, they do
    /// not set this host's.
    #[error(
        "record from peer {host_id} is {ahead_by_seconds}s ahead of the receiver's clock \
         (tolerance {PEER_CLOCK_SKEW_TOLERANCE_SECONDS}s)"
    )]
    ClockSkew {
        host_id: String,
        ahead_by_seconds: i64,
    },
    /// The record names a host other than the peer it was pulled from.
    #[error("record pulled from peer {expected} claims to be host {found}")]
    HostMismatch { expected: String, found: String },
    /// A record arrived for a host with no configured peer. Peers are added
    /// and removed only by the operator editing `federation.peers`; the
    /// tracker never grows or shrinks on its own (§28.7).
    #[error("no configured peer named {host_id}")]
    Unconfigured { host_id: String },
}

/// One peer's receiver-side state.
#[derive(Debug, Default)]
struct PeerEntry {
    /// The last accepted record, kept whole: while stale, the peer is
    /// assumed to run exactly what this published (§28.7).
    record: Option<PeerRecord>,
    /// Receipt time on this receiver's monotonic clock -- never the
    /// record's own `observed_at`, which orders samples only.
    received_at: Option<Instant>,
    /// Whether the current staleness episode has already announced
    /// `peer_expired`, so the event fires once per episode.
    expired_announced: bool,
}

/// The receiver-side state of every configured peer (§28.7): each peer's
/// last accepted record, the monotonic instant it was received, and the
/// fresh/stale/expired classification derived from that age. Entries are
/// created from `federation.peers` at construction and are never removed
/// automatically -- removing a peer is the operator's act, not the
/// tracker's.
#[derive(Debug)]
pub struct PeerTable {
    entries: BTreeMap<String, PeerEntry>,
    /// Aging origin for peers with no receipt yet: without a record there
    /// is no basis to scale up against such a peer, so it is stale from
    /// process start and expires `peer_expiry` after it.
    started_at: Instant,
    stale_after: Duration,
    expiry: Duration,
    events: Vec<Value>,
}

impl PeerTable {
    /// Builds the table from the configured peers and §28.7 bounds.
    pub fn new(federation: &crate::config::FederationConfig, started_at: Instant) -> Self {
        Self {
            entries: federation
                .peers
                .iter()
                .map(|peer| (peer.host_id.clone(), PeerEntry::default()))
                .collect(),
            started_at,
            stale_after: federation.peer_stale_after(),
            expiry: federation.peer_expiry(),
            events: Vec::new(),
        }
    }

    /// The configured peer host ids, in configuration order. The set only
    /// changes when the operator edits `federation.peers` (§28.7).
    pub fn configured_peers(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// Offers one pull result to the table (§28.7). A fetched record must
    /// claim the host it was pulled from and must not sit more than 300 s
    /// ahead of the receiver's wall clock; it is then stored whole and
    /// re-aged from this receipt. Any failure or rejection leaves the
    /// peer's previous record and receipt time untouched, so the peer keeps
    /// aging from its last successful receipt.
    pub fn ingest(
        &mut self,
        peer: &crate::config::PeerConfig,
        fetch: Result<PeerRecord, PeerPullError>,
        now: &PeerMoment,
    ) -> Result<(), PeerRejection> {
        let record = fetch.map_err(|source| PeerRejection::FetchFailed { source })?;
        let entry =
            self.entries
                .get_mut(&peer.host_id)
                .ok_or_else(|| PeerRejection::Unconfigured {
                    host_id: peer.host_id.clone(),
                })?;
        if record.host_id != peer.host_id {
            return Err(PeerRejection::HostMismatch {
                expected: peer.host_id.clone(),
                found: record.host_id,
            });
        }
        let ahead_by_seconds = (record.observed_at - now.wall).num_seconds();
        if ahead_by_seconds > PEER_CLOCK_SKEW_TOLERANCE_SECONDS {
            return Err(PeerRejection::ClockSkew {
                host_id: peer.host_id.clone(),
                ahead_by_seconds,
            });
        }
        entry.record = Some(record);
        entry.received_at = Some(now.monotonic);
        // A new record ends any expiry episode; a later one can open another.
        entry.expired_announced = false;
        Ok(())
    }

    /// Classifies one peer on the receiver's clock, expiring peers whose
    /// age has passed `peer_expiry` and queueing their `peer_expired`
    /// event. `None` means the host is not a configured peer.
    pub fn freshness(&mut self, host_id: &str, now: &PeerMoment) -> Option<PeerFreshness> {
        let stale_after = self.stale_after;
        let expiry = self.expiry;
        let started_at = self.started_at;
        let entry = self.entries.get_mut(host_id)?;
        let age = now
            .monotonic
            .saturating_duration_since(entry.received_at.unwrap_or(started_at));
        let freshness = if age >= expiry {
            if !entry.expired_announced {
                entry.expired_announced = true;
                let event = peer_expired_event(host_id, entry, now.wall);
                self.events.push(event);
            }
            PeerFreshness::Expired
        } else if entry.received_at.is_none() || age > stale_after {
            // A peer never heard from is stale immediately: with no record
            // there is nothing to trust it with.
            PeerFreshness::Stale
        } else {
            PeerFreshness::Fresh
        };
        Some(freshness)
    }

    /// §28.7's freeze over every configured peer: while any peer is stale
    /// the instance must not scale up. Expired peers no longer freeze --
    /// expiry is the defined recovery path -- and fresh ones never did.
    pub fn any_peer_stale(&mut self, now: &PeerMoment) -> bool {
        let host_ids: Vec<String> = self.entries.keys().cloned().collect();
        host_ids
            .iter()
            .any(|host_id| self.freshness(host_id, now) == Some(PeerFreshness::Stale))
    }

    /// Applies §28.7's freeze to one scaling decision: with any peer stale,
    /// `desired` may not exceed `current` -- scaling down stays allowed,
    /// scaling up does not. With no stale peer the decision passes through.
    pub fn clamp_scale_up(&mut self, desired: u32, current: u32, now: &PeerMoment) -> u32 {
        if self.any_peer_stale(now) {
            desired.min(current)
        } else {
            desired
        }
    }

    /// The workers a peer contributes to one account's burn denominator on
    /// this receiver (§28.7): the last published `governed_workers` summed
    /// over the peer's pools while fresh or stale, zero once expired (and
    /// for a peer whose record never mentioned the account). `None` means
    /// the host is not a configured peer.
    pub fn peer_account_workers(
        &mut self,
        host_id: &str,
        account_id: &str,
        now: &PeerMoment,
    ) -> Option<u32> {
        if self.freshness(host_id, now)? == PeerFreshness::Expired {
            return Some(0);
        }
        let entry = self.entries.get(host_id)?;
        Some(
            entry
                .record
                .as_ref()
                .map(|record| {
                    record
                        .accounts
                        .get(account_id)
                        .map(|account| {
                            account
                                .pools
                                .values()
                                .map(|pool| pool.governed_workers)
                                .sum()
                        })
                        .unwrap_or(0)
                })
                .unwrap_or(0),
        )
    }

    /// Drains queued §28.7 events (`peer_expired`) for the caller to emit
    /// through the ordinary decision-event surface.
    pub fn drain_events(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.events)
    }

    /// A configured peer's §28.6 view of one account, measured on the
    /// receiver's clock: fresh and stale peers contribute exactly what they
    /// last published (§28.7), expired peers contribute zeros and no
    /// sample, and a host that is not a configured peer is `None`. The
    /// caller folds this into a [`FederationHost`] -- alongside the peer's
    /// ceiling (`record.host_ceiling`, else the account ceiling) -- to
    /// derive the shared total.
    pub fn peer_account_snapshot(
        &mut self,
        host_id: &str,
        account_id: &str,
        now: &PeerMoment,
    ) -> Option<PeerAccountSnapshot> {
        if self.freshness(host_id, now)? == PeerFreshness::Expired {
            return Some(PeerAccountSnapshot::default());
        }
        let entry = self.entries.get(host_id)?;
        let account = entry
            .record
            .as_ref()
            .and_then(|record| record.accounts.get(account_id));
        Some(PeerAccountSnapshot {
            workers: account
                .map(|account| {
                    account
                        .pools
                        .values()
                        .map(|pool| pool.governed_workers)
                        .sum::<u32>()
                })
                .unwrap_or(0),
            ready_demand: account
                .map(|account| {
                    account
                        .pools
                        .values()
                        .map(|pool| pool.ready_demand.worker_bound())
                        .sum::<u32>()
                })
                .unwrap_or(0),
            quota_sample: account.and_then(|account| {
                freshest_sample(
                    account
                        .pools
                        .values()
                        .filter_map(|pool| pool.quota_sample.as_ref()),
                )
            }),
        })
    }
}

/// The §28.7 expiry event: the peer is counted as zero workers from here
/// until it is heard from again. Nothing in it is secret-shaped (§14).
fn peer_expired_event(host_id: &str, entry: &PeerEntry, time: DateTime<Utc>) -> Value {
    serde_json::json!({
        "event": "peer_expired",
        "severity": "warn",
        "time": time,
        "peer_host_id": host_id,
        "last_record_seq": entry.record.as_ref().map(|record| record.record_seq),
        "action": "counted_as_zero_workers",
    })
}

// ---------------------------------------------------------------------------
// §28.6 (WP29): one total derived everywhere, and the cross-host split
// ---------------------------------------------------------------------------

/// §28.6's sample-disagreement threshold: when any held sample's
/// `used_fraction` sits more than this above the freshest sample's, the
/// larger fraction is used instead.
pub const SAMPLE_DISAGREEMENT_FRACTION: f64 = 0.05;

/// A peer's §28.6 inputs for one account, measured on the receiver's clock
/// (`PeerTable::peer_account_snapshot`): what the peer publishes while
/// fresh or stale, and zeros once it has expired.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct PeerAccountSnapshot {
    /// The peer's `governed_workers`, summed over the account's pools.
    pub workers: u32,
    /// The peer's ready demand, as the worker count it can absorb.
    pub ready_demand: u32,
    /// The peer's freshest held quota sample; `None` once expired (a peer
    /// gone for `peer_expiry` no longer constrains the total) or when its
    /// record holds no sample for the account.
    pub quota_sample: Option<PeerQuotaSample>,
}

/// One host's §28.6 inputs for one account: what the account already runs
/// there, the worker capacity the host can carry, and the ready demand
/// that caps its share. The local host's entry comes from this instance's
/// own observation; every other entry comes from that peer's record
/// (§28.4), via [`PeerTable::peer_account_snapshot`].
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct FederationHost {
    pub host_id: String,
    /// Governed workers for the account observed on this host. Pending
    /// launches are deliberately not folded in: the §28.6 burn denominator
    /// is the sum of `governed_workers`, and in-flight launches are a WP30
    /// overshoot concern with their own re-pull gate.
    pub workers: u32,
    /// The host's worker capacity: the WP24 measured ceiling when the
    /// record published one (that ceiling already nets out the configured
    /// `resource_reserve`, §26.6), else the account's `fleet.max_workers`
    /// (the §22.7 ceiling default).
    pub ceiling: u32,
    /// Ready demand for this host's workspaces (§26.4), bounding the host's
    /// share: `PeerReadyDemand::worker_bound` of the record's entry.
    pub ready_demand: u32,
}

/// The §28.6 outcome every instance derives for one account: the shared
/// controller total, the sample it came from, and the cross-host split.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct SharedAccountDecision {
    /// The §9 controller total. Two instances that assemble identical
    /// inputs choose the same number here without exchanging decisions
    /// (§28.6: "one total, derived everywhere").
    pub total: u32,
    /// The `used_fraction` the total was derived from -- the freshest held
    /// sample's, or the largest when a sample disagrees by more than
    /// [`SAMPLE_DISAGREEMENT_FRACTION`]. `None` when no instance holds a
    /// sample and the total is the stale hold at the combined current.
    pub used_fraction: Option<f64>,
    /// The §28.6 burn denominator: `governed_workers` summed across all
    /// hosts.
    pub denominator: u32,
    /// Whether the controller's decision was a stale hold (§9.1).
    pub stale: bool,
    /// Per-host shares of `total` from the §22.7 split. Sums to at most
    /// `total`; an instance actuates only its own host's entry, applying
    /// that host's per-cycle step limits there.
    pub shares: BTreeMap<String, u32>,
}

/// The freshest sample by `observed_at`; ties break on the larger fraction
/// and then the window id, so every instance orders an identical sample
/// set identically.
fn freshest_sample<'a, I>(samples: I) -> Option<PeerQuotaSample>
where
    I: IntoIterator<Item = &'a PeerQuotaSample>,
{
    samples
        .into_iter()
        .max_by(|left, right| {
            left.observed_at
                .cmp(&right.observed_at)
                .then_with(|| {
                    left.used_fraction
                        .partial_cmp(&right.used_fraction)
                        .unwrap_or(Ordering::Equal)
                })
                .then_with(|| right.window.cmp(&left.window))
        })
        .cloned()
}

/// §28.6's conservative fraction over one account's held samples (this
/// instance's own plus every fresh-or-stale peer's): the freshest sample's
/// `used_fraction`, replaced by the largest held fraction when any sample
/// disagrees by more than [`SAMPLE_DISAGREEMENT_FRACTION`]. A higher
/// `used_fraction` is the conservative reading -- less quota remains -- and
/// every instance applying the same rule to the same samples lands on the
/// same one. `None` when no instance holds a sample.
pub fn freshest_used_fraction(samples: &[PeerQuotaSample]) -> Option<f64> {
    let base = freshest_sample(samples)?.used_fraction;
    let largest = samples
        .iter()
        .filter(|sample| sample.used_fraction.is_finite())
        .fold(base, |largest, sample| largest.max(sample.used_fraction));
    Some(if largest - base > SAMPLE_DISAGREEMENT_FRACTION {
        largest
    } else {
        base
    })
}

/// Splits one account's controller total across the federation's hosts
/// (§28.6 over §22.7, reusing `placement::distribute`). Weights are each
/// host's worker headroom (`ceiling - workers`, from the peer records),
/// and each host's share is capped by the smaller of its reserve ceiling
/// and the work its ready demand can absorb (`workers + ready_demand`,
/// never past the ceiling): no host is asked to exceed its reserve or to
/// run more workers than it has work for.
///
/// A host with zero ready demand receives none of the new placement.
/// While the total is stable or growing it keeps the workers it already
/// runs (the §22.8 frozen-host rule, applied to demand instead of
/// staleness); when the total shrinks it is the first to step down, and
/// hosts with ready demand carry what remains as absolute targets.
///
/// The split is a pure function of its arguments and never exceeds
/// `total`: it may come in under it when no host has the headroom or the
/// demand to place the rest. Per-cycle step limits are deliberately *not*
/// applied here -- each instance applies its own host's limits when it
/// actuates its own share. Host ids must be unique, as peer records are
/// keyed by host.
pub fn split_account_total(total: u32, hosts: &[FederationHost]) -> BTreeMap<String, u32> {
    // Ascending host id, so distribute's remainder tie-break lands on the
    // smallest key regardless of which side assembled the list first --
    // §28.6's split must not depend on the caller's perspective.
    let mut ids: Vec<&str> = hosts.iter().map(|host| host.host_id.as_str()).collect();
    ids.sort_unstable();
    let workers_of: BTreeMap<&str, u32> = hosts
        .iter()
        .map(|host| (host.host_id.as_str(), host.workers))
        .collect();
    let ready_of: BTreeMap<&str, u32> = hosts
        .iter()
        .map(|host| (host.host_id.as_str(), host.ready_demand))
        .collect();
    // The share cap: the reserve ceiling, never past the work the host can
    // absorb on top of what it already runs.
    let cap_of: BTreeMap<&str, u32> = hosts
        .iter()
        .map(|host| {
            (
                host.host_id.as_str(),
                host.ceiling
                    .min(host.workers.saturating_add(host.ready_demand)),
            )
        })
        .collect();
    // §28.6: headroom from the peer records, in workers.
    let headroom_of: BTreeMap<&str, f64> = hosts
        .iter()
        .map(|host| {
            (
                host.host_id.as_str(),
                f64::from(host.ceiling.saturating_sub(host.workers)),
            )
        })
        .collect();
    let current_total = workers_of
        .values()
        .fold(0u32, |total, workers| total.saturating_add(*workers));

    let (base_of, budget, share_cap_of, eligible): (
        BTreeMap<&str, u32>,
        u32,
        BTreeMap<&str, u32>,
        Vec<&str>,
    ) = if total >= current_total {
        // Stable or growing: hold every host at what it already runs and
        // distribute only the surplus to hosts with marginal room -- free
        // ceiling AND demand to fill it.
        let base_of = workers_of.clone();
        let share_cap_of: BTreeMap<&str, u32> = cap_of
            .iter()
            .map(|(id, cap)| (*id, cap.saturating_sub(workers_of[id])))
            .collect();
        let eligible = ids
            .iter()
            .copied()
            .filter(|id| share_cap_of[id] > 0)
            .collect();
        (base_of, total - current_total, share_cap_of, eligible)
    } else {
        // Shrinking: §22.7's raw-0 for hosts without demand -- no work to
        // hold workers for -- while the demanding hosts split the total as
        // absolute targets, capped as always.
        let base_of = ids.iter().map(|id| (*id, 0)).collect();
        let eligible = ids.iter().copied().filter(|id| ready_of[id] > 0).collect();
        (base_of, total, cap_of, eligible)
    };

    let distributed =
        crate::placement::distribute(budget, &eligible, &headroom_of, &share_cap_of, &ids);
    ids.into_iter()
        .map(|id| (id.to_owned(), base_of[id].saturating_add(distributed[id])))
        .collect()
}

/// §28.6's one-total derivation for one account (WP29). `hosts` carries the
/// local host's view plus every fresh-or-stale peer's, and `samples` the
/// quota samples this instance holds for the account, its own and its
/// peers' (an expired peer contributes nothing, §28.7, so its snapshot is
/// already zeros). Every instance that assembles the same views and
/// samples derives the same [`SharedAccountDecision`] without exchanging
/// decisions: [`freshest_used_fraction`] picks one `used_fraction`, the
/// §28.6 denominator sums `governed_workers` across hosts, the pure §9
/// controller (`controller::evaluate`) turns both into the account total,
/// and [`split_account_total`] places it. The caller actuates only its own
/// host's share; placement never exceeds the controller total (§22.15(2)).
pub fn shared_account_total(
    account_name: &str,
    config: &AccountConfig,
    prior: &crate::state::AccountState,
    hosts: &[FederationHost],
    samples: &[PeerQuotaSample],
    now: DateTime<Utc>,
) -> Result<SharedAccountDecision> {
    let denominator = hosts
        .iter()
        .fold(0u32, |total, host| total.saturating_add(host.workers));
    let decision = match freshest_sample(samples) {
        Some(freshest) => {
            let snapshot = crate::model::QuotaSnapshot {
                observed_at: freshest.observed_at,
                fresh: true,
                windows: vec![crate::model::QuotaWindow {
                    id: freshest.window.clone(),
                    used_fraction: freshest_used_fraction(samples)
                        .expect("freshest_sample implies a fraction"),
                    resets_at: freshest.resets_at,
                    duration_minutes: None,
                    reached: false,
                }],
                eligible_backlog_capacity: None,
                reset_credits: None,
                backpressure: None,
                saturation: None,
            };
            crate::controller::evaluate(account_name, config, &snapshot, prior, denominator, now)?
        }
        // No instance holds a sample: §9.1's stale rule applied to the
        // combined fleet holds the total at the combined current.
        None => {
            let snapshot = crate::model::QuotaSnapshot {
                observed_at: now,
                fresh: false,
                windows: Vec::new(),
                eligible_backlog_capacity: None,
                reset_credits: None,
                backpressure: None,
                saturation: None,
            };
            crate::controller::evaluate(account_name, config, &snapshot, prior, denominator, now)?
        }
    };
    let shares = split_account_total(decision.desired_workers, hosts);
    Ok(SharedAccountDecision {
        total: decision.desired_workers,
        used_fraction: freshest_used_fraction(samples),
        denominator,
        stale: decision.stale,
        shares,
    })
}

/// Loads this instance's persisted random identity, creating it on first
/// use (§28.4). The file is mode 600 and holds 32 hex characters.
pub fn load_or_create_instance_id(path: &Path) -> Result<String> {
    if let Ok(bytes) = fs::read(path) {
        let id = String::from_utf8(bytes)
            .with_context(|| format!("instance id {} is not UTF-8", path.display()))?;
        let id = id.trim();
        validate_identifier("instance_id", id).map_err(anyhow::Error::from)?;
        return Ok(id.to_owned());
    }
    let id: String = {
        let mut bytes = [0u8; 16];
        rand::thread_rng().fill(&mut bytes);
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    };
    write_private_file(path, format!("{id}\n").as_bytes())
        .with_context(|| format!("failed to persist instance id {}", path.display()))?;
    Ok(id)
}

/// Reads and bumps the monotonic record counter (§28.4). The first record
/// is sequence 1; the count only ever moves forward, including across
/// restarts. An exclusive lock covers the read-modify-write so two
/// concurrent `peer-state` runs cannot publish the same sequence.
pub fn next_record_seq(path: &Path) -> Result<u64> {
    let _lock = crate::state::StateLock::acquire(path)
        .with_context(|| format!("failed to lock record sequence {}", path.display()))?;
    let current = match fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes)
            .with_context(|| format!("record sequence {} is not UTF-8", path.display()))?
            .trim()
            .parse::<u64>()
            .with_context(|| format!("record sequence {} is not a counter", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read record sequence {}", path.display()))
        }
    };
    let next = current
        .checked_add(1)
        .context("record sequence counter exhausted")?;
    write_private_file(path, format!("{next}\n").as_bytes())
        .with_context(|| format!("failed to persist record sequence {}", path.display()))?;
    Ok(next)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create directory {}", parent.display()))?;
    let temp = path.with_extension("tmp");
    let mut file =
        fs::File::create(&temp).with_context(|| format!("failed to write {}", temp.display()))?;
    restrict_permissions(&file)?;
    file.write_all(bytes)
        .with_context(|| format!("failed to write {}", temp.display()))?;
    drop(file);
    fs::rename(&temp, path).with_context(|| format!("failed to publish {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(file: &std::fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_file: &std::fs::File) -> Result<()> {
    Ok(())
}

/// Assembles this instance's peer record (§28.4) from files the running
/// governor already maintains. Nothing here contacts a provider, observes
/// workers or actuates: a peer pull must never perturb the host it reads.
pub fn build_local_record(config: &Config, now: DateTime<Utc>) -> Result<PeerRecord> {
    let federation = config
        .federation
        .as_ref()
        .context("peer-state requires a federation: block (federation.host_id)")?;
    let instance_id = load_or_create_instance_id(&federation.instance_id_path(config))?;
    let record_seq = next_record_seq(&federation.record_seq_path(config))?;

    let status = read_status(config);
    let state = crate::state::State::load(&config.state_path()).map(|(state, _)| state)?;
    let capacity = federation
        .host_capacity_state
        .as_deref()
        .map(read_host_capacity)
        .transpose()?
        .unwrap_or_default();

    let mut accounts = BTreeMap::new();
    let mut workspace_origins = BTreeMap::new();
    let excluded =
        crate::ownership::read_workspace_exclusions(&federation.workspace_exclusions_path(config));
    for (name, account) in &config.accounts {
        let provider_identifier = federation.account_identities.get(name);
        let pools = pool_states(
            account,
            name,
            &federation.host_id,
            &status,
            &state,
            &mut workspace_origins,
            federation.pending_launches_path(config).as_path(),
            &excluded,
        );
        accounts.insert(
            name.clone(),
            PeerAccount {
                account_id: name.clone(),
                account_fingerprint: account_fingerprint(
                    provider_identifier.map(String::as_str).unwrap_or(name),
                ),
                pools,
            },
        );
    }

    Ok(PeerRecord {
        schema_version: PEER_RECORD_SCHEMA_VERSION,
        instance_id,
        host_id: federation.host_id.clone(),
        record_seq,
        observed_at: now,
        accounts,
        host_ceiling: capacity.last_evidence.and_then(|evidence| {
            Some(HostCeiling {
                workers: evidence.ceiling?,
                observed_at: epoch(evidence.observed_at, now)?,
            })
        }),
        host_reservations: capacity
            .reservations
            .into_iter()
            .filter_map(|(pool, reservation)| {
                Some((
                    pool,
                    HostReservation {
                        count: reservation.count,
                        reserved_at: epoch(reservation.at, now)?,
                    },
                ))
            })
            .collect(),
        resource_reserve: host_resource_reserve(config, &federation.host_id)
            .map(ResourceReserveDigest::from),
        workspace_origins,
    })
}

/// Per-account, per-pool state plus the origin URLs of the workspaces the
/// pool's actuator would seed (§28.4/§28.10).
#[allow(clippy::too_many_arguments)] // Read-only projection needs the account, host and on-disk views together.
fn pool_states(
    account: &AccountConfig,
    name: &str,
    local_host: &str,
    status: &BTreeMap<String, StatusDigest>,
    state: &crate::state::State,
    origins: &mut BTreeMap<String, String>,
    pending_path: &Path,
    excluded: &BTreeSet<PathBuf>,
) -> BTreeMap<String, PeerPool> {
    let readiness = status.get(name);
    let quota_sample = freshest_window(state, name);
    let mut pools = BTreeMap::new();
    let demand_path = seed_demand_state(account, local_host);
    let demand = demand_path.as_deref().map(read_demand).unwrap_or_default();
    for (path, url) in local_seed_origins(account, local_host, &demand) {
        origins.insert(path, url);
    }
    let key = pool_key(account, name, local_host);
    pools.insert(
        key.clone(),
        PeerPool {
            governed_workers: readiness
                .and_then(|readiness| readiness.current_workers)
                .unwrap_or(0),
            pending_launches: crate::ownership::read_pending_launches(pending_path, &key),
            desired: readiness
                .and_then(|readiness| readiness.desired_workers)
                .unwrap_or(0),
            quota_sample,
            ready_demand: demand.ready_demand(excluded),
        },
    );
    pools
}

/// A pool is identified by its worker prefix when one is configured
/// (§28.9) and by the account id otherwise.
fn pool_key(account: &AccountConfig, name: &str, local_host: &str) -> String {
    identifier_prefix(account, local_host).unwrap_or_else(|| name.to_owned())
}

fn identifier_prefix(account: &AccountConfig, local_host: &str) -> Option<String> {
    let actuator = match &account.fleet.hosts {
        Some(hosts) => hosts.get(local_host).and_then(|host| match &host.actuator {
            crate::config::ActuatorConfig::NeedleRun {
                identifier_prefix, ..
            } => identifier_prefix.clone(),
            _ => None,
        }),
        None => match &account.fleet.actuator {
            crate::config::ActuatorConfig::NeedleRun {
                identifier_prefix, ..
            } => identifier_prefix.clone(),
            _ => None,
        },
    };
    actuator.or_else(|| {
        match &account.fleet.hosts {
            Some(hosts) => hosts.get(local_host).map(|host| &host.observer),
            None => account.fleet.observer.as_ref(),
        }
        .and_then(|observer| match observer {
            crate::config::WorkerObserverConfig::NeedleStatus {
                identifier_prefix, ..
            } => identifier_prefix.clone(),
            _ => None,
        })
    })
}

/// Worker prefixes this instance claims. Startup compares these with the
/// pools in every freshly pulled peer record before any actuation.
pub fn local_worker_prefixes(config: &Config) -> BTreeMap<String, String> {
    let Some(federation) = &config.federation else {
        return BTreeMap::new();
    };
    config
        .accounts
        .iter()
        .filter_map(|(name, account)| {
            identifier_prefix(account, &federation.host_id).map(|prefix| (name.clone(), prefix))
        })
        .collect()
}

/// The demand-state file of the actuator on this instance's own host
/// (§26.5); `None` when the account has no needle_run actuator.
fn seed_demand_state(account: &AccountConfig, local_host: &str) -> Option<PathBuf> {
    match &account.fleet.hosts {
        Some(hosts) => hosts.get(local_host).and_then(|host| match &host.actuator {
            crate::config::ActuatorConfig::NeedleRun {
                seed_demand_state, ..
            } => seed_demand_state.clone(),
            _ => None,
        }),
        None => match &account.fleet.actuator {
            crate::config::ActuatorConfig::NeedleRun {
                seed_demand_state, ..
            } => seed_demand_state.clone(),
            _ => None,
        },
    }
}

/// Origins of this instance's own seedable workspace candidates. A pull
/// compares this view with peer records before updating overlap exclusions.
pub fn local_workspace_origins(config: &Config) -> BTreeMap<String, String> {
    let Some(federation) = &config.federation else {
        return BTreeMap::new();
    };
    let mut origins = BTreeMap::new();
    for account in config.accounts.values() {
        let demand = seed_demand_state(account, &federation.host_id)
            .as_deref()
            .map(read_demand)
            .unwrap_or_default();
        origins.extend(local_seed_origins(account, &federation.host_id, &demand));
    }
    origins
}

fn local_seed_origins(
    account: &AccountConfig,
    local_host: &str,
    demand: &DemandDigest,
) -> BTreeMap<String, String> {
    let mut origins = seed_origins(demand);
    if seed_demand_state(account, local_host).is_none() {
        let actuator = match &account.fleet.hosts {
            Some(hosts) => hosts.get(local_host).map(|host| &host.actuator),
            None => Some(&account.fleet.actuator),
        };
        if let Some(crate::config::ActuatorConfig::NeedleRun { repo, .. }) = actuator {
            if let Some(url) = git_origin(repo) {
                origins.insert(
                    repo.display().to_string(),
                    sanitize_origin(&url).into_owned(),
                );
            }
        }
    }
    origins
}

/// The account's freshest window sample, keyed by window id (§28.4: "the
/// latest quota sample"). Ties break on window id so the choice is stable.
fn freshest_window(state: &crate::state::State, account: &str) -> Option<PeerQuotaSample> {
    state
        .accounts
        .get(account)?
        .windows
        .iter()
        .max_by(|(left_id, left), (right_id, right)| {
            left.observed_at
                .cmp(&right.observed_at)
                .then_with(|| right_id.cmp(left_id))
        })
        .map(|(id, sample)| PeerQuotaSample {
            window: id.clone(),
            used_fraction: sample.used_fraction,
            resets_at: sample.resets_at,
            observed_at: sample.observed_at,
        })
}

/// The configured resource reserve of this instance's own host, when the
/// account placement declares one (§22.3). The first account that names
/// the host wins; hosts sharing a federation must declare the same reserve.
fn host_resource_reserve<'a>(
    config: &'a Config,
    host_id: &'a str,
) -> Option<&'a ResourceReserveConfig> {
    config.accounts.values().find_map(|account| {
        account
            .fleet
            .hosts
            .as_ref()?
            .get(host_id)?
            .resource_reserve
            .as_ref()
    })
}

/// Per-account readiness from the last completed cycle. The status file is
/// a derived, best-effort surface (see `merge_status_report`), so a missing
/// or corrupt file simply leaves every account unobserved.
fn read_status(config: &Config) -> BTreeMap<String, StatusDigest> {
    fs::read(config.status_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

#[derive(Clone, Debug, Default, Deserialize)]
struct StatusDigest {
    #[serde(default)]
    current_workers: Option<u32>,
    #[serde(default)]
    desired_workers: Option<u32>,
}

/// The WP23 demand-state file (scripts/subgov_ready_beads.py). Lenient by
/// the same rule as the status file: unreadable demand is zero demand in a
/// record, never a failed pull.
fn read_demand(path: &Path) -> DemandDigest {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

#[derive(Clone, Debug, Default, Deserialize)]
struct DemandDigest {
    #[serde(default)]
    ready_bead_count: Option<u32>,
    #[serde(default)]
    eligible_backlog_capacity: Option<u32>,
    #[serde(default)]
    workspaces: Vec<DemandWorkspace>,
    #[serde(default)]
    ready: Vec<DemandWorkspace>,
}

impl DemandDigest {
    fn ready_demand(&self, excluded: &BTreeSet<PathBuf>) -> PeerReadyDemand {
        if excluded.is_empty() {
            return PeerReadyDemand {
                ready_beads: self.ready_bead_count.unwrap_or(0),
                eligible_backlog_capacity: self.eligible_backlog_capacity,
            };
        }
        PeerReadyDemand {
            ready_beads: self
                .workspaces
                .iter()
                .filter(|w| {
                    w.workspace
                        .as_ref()
                        .is_some_and(|p| !excluded.contains(Path::new(p)))
                })
                .map(|w| w.ready_beads.unwrap_or(0))
                .fold(0u32, u32::saturating_add),
            eligible_backlog_capacity: Some(
                self.workspaces
                    .iter()
                    .filter(|w| {
                        w.workspace
                            .as_ref()
                            .is_some_and(|p| !excluded.contains(Path::new(p)))
                    })
                    .map(|w| w.eligible_slots.unwrap_or(0))
                    .fold(0u32, u32::saturating_add),
            ),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct DemandWorkspace {
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    ready_beads: Option<u32>,
    #[serde(default)]
    eligible_slots: Option<u32>,
}

/// Git origin URLs for the workspaces this instance will seed (§28.4),
/// sanitized of any embedded userinfo. A workspace whose origin cannot be
/// read is omitted: §28.10's overlap check must not treat an unknown
/// origin as either matching or free.
fn seed_origins(demand: &DemandDigest) -> BTreeMap<String, String> {
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for workspace in demand.workspaces.iter().chain(demand.ready.iter()) {
        if let Some(path) = workspace.workspace.as_deref() {
            if !path.trim().is_empty() {
                paths.insert(path.to_owned());
            }
        }
    }
    paths
        .into_iter()
        .filter_map(|path| {
            Some((
                path.clone(),
                sanitize_origin(&git_origin(Path::new(&path))?).into_owned(),
            ))
        })
        .collect()
}

fn git_origin(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = str::from_utf8(&output.stdout).ok()?.trim();
    if url.is_empty() {
        None
    } else {
        Some(url.to_owned())
    }
}

/// Strips userinfo (`user:token@`) from a URL or scp-style remote so a
/// credential embedded in a git remote never reaches a peer record (§14).
fn sanitize_origin(url: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        // scp-like `git@host:path` remotes carry userinfo with no scheme.
        None => (None, url),
    };
    let authority_end = rest.find('/').unwrap_or(match scheme {
        // `host:path` form: the whole remainder is the authority.
        None => rest.len(),
        Some(_) => rest.find(':').unwrap_or(rest.len()),
    });
    let (authority, tail) = rest.split_at(authority_end);
    match authority.split_once('@') {
        Some((_userinfo, host)) => match scheme {
            Some(scheme) => Cow::Owned(format!("{scheme}://{host}{tail}")),
            None => Cow::Owned(format!("{host}{tail}")),
        },
        None => Cow::Borrowed(url),
    }
}

/// The WP24 host-capacity adapter's state file: `last_evidence` carries the
/// measured ceiling this record publishes (§26.6). Unreadable or
/// error-marked evidence publishes no ceiling -- a peer must never see a
/// ceiling that was not measured.
fn read_host_capacity(path: &Path) -> Result<HostCapacityDigest> {
    let bytes = fs::read(path).with_context(|| {
        format!(
            "failed to read federation.host_capacity_state {}",
            path.display()
        )
    })?;
    let digest: HostCapacityDigest = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "failed to parse federation.host_capacity_state {}",
            path.display()
        )
    })?;
    Ok(digest)
}

#[derive(Clone, Debug, Default, Deserialize)]
struct HostCapacityDigest {
    #[serde(default)]
    last_evidence: Option<HostEvidence>,
    #[serde(default)]
    reservations: BTreeMap<String, Reservation>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct HostEvidence {
    /// `null` when the adapter could not measure (unknown evidence holds
    /// the fleet rather than inventing an admission).
    #[serde(default)]
    ceiling: Option<u32>,
    #[serde(default)]
    observed_at: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Reservation {
    #[serde(default)]
    count: u32,
    #[serde(default)]
    at: Option<f64>,
}

/// Converts the adapter's `time.time()` float to a timestamp, falling back
/// to the record's own `observed_at` when the adapter wrote no usable one.
fn epoch(seconds: Option<f64>, fallback: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let seconds = seconds?;
    let secs = seconds.trunc() as i64;
    let nanos = ((seconds - seconds.trunc()) * 1_000_000_000.0) as u32;
    Utc.timestamp_opt(secs, nanos).single().or(Some(fallback))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FederationConfig, FleetConfig, SourceConfig};
    use chrono::Duration;

    #[test]
    fn excluded_workspace_contributes_no_ready_demand() {
        let demand = DemandDigest {
            ready_bead_count: Some(5),
            eligible_backlog_capacity: Some(3),
            workspaces: vec![
                DemandWorkspace {
                    workspace: Some("/work/contested".into()),
                    ready_beads: Some(4),
                    eligible_slots: Some(2),
                },
                DemandWorkspace {
                    workspace: Some("/work/allowed".into()),
                    ready_beads: Some(1),
                    eligible_slots: Some(1),
                },
            ],
            ready: Vec::new(),
        };
        let allowed = demand.ready_demand(&BTreeSet::from([PathBuf::from("/work/contested")]));
        assert_eq!(allowed.ready_beads, 1);
        assert_eq!(allowed.eligible_backlog_capacity, Some(1));
    }

    fn sample_record() -> PeerRecord {
        PeerRecord {
            schema_version: PEER_RECORD_SCHEMA_VERSION,
            instance_id: "0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_owned(),
            host_id: "codinghome".to_owned(),
            record_seq: 7,
            observed_at: Utc::now(),
            accounts: BTreeMap::from([(
                "claude-zai".to_owned(),
                PeerAccount {
                    account_id: "claude-zai".to_owned(),
                    account_fingerprint: account_fingerprint("zai:user-42"),
                    pools: BTreeMap::from([(
                        "sg-zai-".to_owned(),
                        PeerPool {
                            governed_workers: 4,
                            pending_launches: 0,
                            desired: 5,
                            quota_sample: Some(PeerQuotaSample {
                                window: "five_hour".to_owned(),
                                used_fraction: 0.42,
                                resets_at: Utc::now() + Duration::hours(3),
                                observed_at: Utc::now(),
                            }),
                            ready_demand: PeerReadyDemand {
                                ready_beads: 6,
                                eligible_backlog_capacity: Some(5),
                            },
                        },
                    )]),
                },
            )]),
            host_ceiling: Some(HostCeiling {
                workers: 12,
                observed_at: Utc::now(),
            }),
            host_reservations: BTreeMap::from([(
                "sg-zai-".to_owned(),
                HostReservation {
                    count: 1,
                    reserved_at: Utc::now(),
                },
            )]),
            resource_reserve: Some(ResourceReserveDigest {
                cpu_reserve_fraction: 0.1,
                mem_reserve_mb: 8192,
            }),
            workspace_origins: BTreeMap::from([(
                "/home/coding/SEAM".to_owned(),
                "https://git.ardenone.com/jedarden/SEAM.git".to_owned(),
            )]),
        }
    }

    #[test]
    fn fingerprint_is_a_stable_truncated_hash() {
        // Standard SHA-256("zai:user-42") truncated to 16 hex characters.
        assert_eq!(
            account_fingerprint("zai:user-42").len(),
            FINGERPRINT_HEX_CHARS
        );
        assert_eq!(
            account_fingerprint("zai:user-42"),
            account_fingerprint("zai:user-42")
        );
        assert_ne!(
            account_fingerprint("zai:user-42"),
            account_fingerprint("zai:user-43")
        );
        assert!(is_fingerprint(&account_fingerprint("anything")));
    }

    #[test]
    fn parse_round_trips_a_full_record() {
        let record = sample_record();
        let parsed = PeerRecord::parse(record.to_json().unwrap().as_bytes()).unwrap();
        assert_eq!(parsed, record);
    }

    #[test]
    fn unknown_major_is_rejected_but_unknown_minor_fields_pass() {
        let mut future = serde_json::to_value(sample_record()).unwrap();
        future["schema_version"] = serde_json::json!(2);
        let error = PeerRecord::parse(future.to_string().as_bytes()).unwrap_err();
        assert!(matches!(
            error,
            PeerRecordError::UnknownSchemaMajor {
                found: 2,
                supported: 1
            }
        ));

        let mut extended = serde_json::to_value(sample_record()).unwrap();
        extended["accounts"]["claude-zai"]["future_field"] = serde_json::json!(true);
        assert!(PeerRecord::parse(extended.to_string().as_bytes()).is_ok());

        let mut unversioned = serde_json::to_value(sample_record()).unwrap();
        unversioned
            .as_object_mut()
            .unwrap()
            .remove("schema_version");
        assert!(PeerRecord::parse(unversioned.to_string().as_bytes()).is_err());
    }

    #[test]
    fn invalid_records_are_rejected_with_field_context() {
        let mut bad = serde_json::to_value(sample_record()).unwrap();
        bad["accounts"]["claude-zai"]["account_fingerprint"] = serde_json::json!("not-hex");
        assert!(PeerRecord::parse(bad.to_string().as_bytes()).is_err());

        let mut empty_host = serde_json::to_value(sample_record()).unwrap();
        empty_host["host_id"] = serde_json::json!("");
        assert!(PeerRecord::parse(empty_host.to_string().as_bytes()).is_err());

        let mut out_of_range = serde_json::to_value(sample_record()).unwrap();
        out_of_range["accounts"]["claude-zai"]["pools"]["sg-zai-"]["quota_sample"]
            ["used_fraction"] = serde_json::json!(1.5);
        assert!(PeerRecord::parse(out_of_range.to_string().as_bytes()).is_err());
    }

    #[test]
    fn identity_classification_isolates_a_fingerprint_mismatch() {
        let local = PeerAccount {
            account_id: "claude-zai".to_owned(),
            account_fingerprint: account_fingerprint("zai:user-42"),
            pools: BTreeMap::new(),
        };
        let same = PeerAccount {
            account_fingerprint: account_fingerprint("zai:user-42"),
            ..local.clone()
        };
        let drifted = PeerAccount {
            account_fingerprint: account_fingerprint("zai:user-99"),
            ..local.clone()
        };
        let other = PeerAccount {
            account_id: "codex".to_owned(),
            account_fingerprint: account_fingerprint("codex:org-1"),
            ..local.clone()
        };

        assert_eq!(
            classify_account(&local, &same),
            AccountIdentityVerdict::Same
        );
        let mismatch = classify_account(&local, &drifted);
        assert_eq!(mismatch, AccountIdentityVerdict::FingerprintMismatch);
        assert!(!mismatch.shares_account_total());
        assert!(mismatch.is_error());
        assert_eq!(
            classify_account(&local, &other),
            AccountIdentityVerdict::Distinct
        );
        assert!(!classify_account(&local, &other).is_error());

        let event = fingerprint_mismatch_event(&local, &drifted, "gmktec", Utc::now());
        assert_eq!(event["event"], "account_fingerprint_mismatch");
        assert_eq!(event["severity"], "error");
        assert_eq!(event["action"], "treated_as_different_account");
        // The raw provider identifier must never appear in the event (§14).
        assert!(!event.to_string().contains("zai:user"));
    }

    #[test]
    fn origin_sanitization_strips_userinfo() {
        use sanitize_origin as clean;
        assert_eq!(
            clean("https://user:tok3n@git.ardenone.com/jedarden/SEAM.git"),
            "https://git.ardenone.com/jedarden/SEAM.git"
        );
        assert_eq!(
            clean("https://token@github.com/jedarden/x.git"),
            "https://github.com/jedarden/x.git"
        );
        assert_eq!(clean("git@host:repo/path.git"), "host:repo/path.git");
        assert_eq!(
            clean("https://git.ardenone.com/jedarden/SEAM.git"),
            "https://git.ardenone.com/jedarden/SEAM.git"
        );
    }

    fn test_config(dir: &Path) -> Config {
        Config {
            version: 1,
            poll_interval_seconds: 300,
            state_path: Some(dir.join("state").join("governor.json")),
            accounts: BTreeMap::from([(
                "acct".to_owned(),
                AccountConfig {
                    source: SourceConfig::NormalizedFile {
                        path: dir.join("quota.json"),
                    },
                    fleet: FleetConfig {
                        min_workers: 0,
                        max_workers: 4,
                        bootstrap_workers: 1,
                        max_scale_up_per_cycle: 1,
                        max_scale_down_per_cycle: 1,
                        observer: Some(crate::config::WorkerObserverConfig::Static { workers: 0 }),
                        actuator: crate::config::ActuatorConfig::None,
                        observer_reconciliation: Default::default(),
                        hosts: None,
                        takeover: Vec::new(),
                        takeover_helper: None,
                    },
                    utilization: crate::config::UtilizationConfig {
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
                host_id: "codinghome".to_owned(),
                instance_id_file: None,
                record_seq_file: None,
                host_capacity_state: None,
                account_identities: BTreeMap::from([(
                    "acct".to_owned(),
                    "provider:acct-1".to_owned(),
                )]),
                peers: Vec::new(),
                peer_stale_after_seconds: 120,
                peer_expiry_seconds: 1_800,
                primary: true,
                repo_owners: BTreeMap::new(),
                pending_launches_file: None,
            }),
        }
    }

    #[test]
    fn instance_id_persists_and_record_seq_is_monotonic() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let federation = config.federation.as_ref().unwrap();
        let id_path = federation.instance_id_path(&config);
        let seq_path = federation.record_seq_path(&config);

        let first = load_or_create_instance_id(&id_path).unwrap();
        assert_eq!(first, load_or_create_instance_id(&id_path).unwrap());
        assert_eq!(first.len(), 32);
        assert!(seq_path.starts_with(dir.path()));

        assert_eq!(next_record_seq(&seq_path).unwrap(), 1);
        assert_eq!(next_record_seq(&seq_path).unwrap(), 2);
        assert_eq!(next_record_seq(&seq_path).unwrap(), 3);
        use std::os::unix::fs::PermissionsExt;
        let actual = fs::metadata(&id_path).unwrap().permissions().mode();
        assert_eq!(actual & 0o777, 0o600, "identity file must be private");
    }

    #[test]
    fn local_record_carries_status_quota_demand_and_reserve() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        let now = Utc::now();

        // No files yet: an unobserved instance still publishes a valid record.
        let record = build_local_record(&config, now).unwrap();
        assert_eq!(record.schema_version, PEER_RECORD_SCHEMA_VERSION);
        assert_eq!(record.host_id, "codinghome");
        let pool = &record.accounts["acct"].pools[&"acct".to_owned()];
        assert_eq!(pool.governed_workers, 0);
        assert_eq!(pool.pending_launches, 0);
        assert_eq!(pool.ready_demand.ready_beads, 0);
        assert_eq!(
            record.accounts["acct"].account_fingerprint,
            account_fingerprint("provider:acct-1")
        );
        PeerRecord::parse(record.to_json().unwrap().as_bytes()).unwrap();

        // A completed cycle's artifacts flow into the record.
        let status = serde_json::json!({
            "acct": {"state": "healthy_learning", "reason": "", "updated_at": now,
                "current_workers": 2, "desired_workers": 3}
        });
        fs::create_dir_all(config.state_path().parent().unwrap()).unwrap();
        fs::write(config.status_path(), status.to_string()).unwrap();
        let mut state = crate::state::State::default();
        state
            .accounts
            .entry("acct".to_owned())
            .or_default()
            .windows
            .insert(
                "five_hour".to_owned(),
                crate::state::WindowSample {
                    observed_at: now,
                    used_fraction: 0.31,
                    resets_at: now + Duration::hours(4),
                    workers: 2,
                },
            );
        fs::write(
            config.state_path(),
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();

        let record = build_local_record(&config, now).unwrap();
        let pool = &record.accounts["acct"].pools[&"acct".to_owned()];
        assert_eq!(pool.governed_workers, 2);
        assert_eq!(pool.desired, 3);
        let sample = pool.quota_sample.as_ref().unwrap();
        assert_eq!(sample.window, "five_hour");
        assert_eq!(sample.used_fraction, 0.31);

        // A measured ceiling and reservations flow from the WP24 state file.
        let capacity_path = dir.path().join("host-capacity.json");
        fs::write(
            &capacity_path,
            serde_json::json!({
                "version": 1,
                "last_evidence": {
                    "event": "host_worker_ceiling", "ceiling": 12,
                    "observed_at": now.timestamp() as f64,
                },
                "reservations": {"acct": {"count": 1, "at": now.timestamp() as f64}},
            })
            .to_string(),
        )
        .unwrap();
        config.federation.as_mut().unwrap().host_capacity_state = Some(capacity_path);
        let record = build_local_record(&config, now).unwrap();
        assert_eq!(record.host_ceiling.as_ref().unwrap().workers, 12);
        assert_eq!(record.host_reservations["acct"].count, 1);

        // A failed measurement publishes no ceiling, never a zero.
        fs::write(
            dir.path().join("host-capacity.json"),
            serde_json::json!({"version": 1,
                "last_evidence": {"ceiling": None::<u32>, "observed_at": now.timestamp() as f64}})
            .to_string(),
        )
        .unwrap();
        let record = build_local_record(&config, now).unwrap();
        assert!(record.host_ceiling.is_none());
    }

    #[test]
    fn record_requires_a_federation_block() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.federation = None;
        assert!(build_local_record(&config, Utc::now())
            .err()
            .unwrap()
            .to_string()
            .contains("federation"));
    }
}
