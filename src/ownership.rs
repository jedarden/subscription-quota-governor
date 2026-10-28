//! plan.md §28.8/§28.9/§28.10 (WP30): per-host worker identity, workspace
//! ownership, and the bounded overshoot gate.
//!
//! Three facts keep a federation from double-booking the same quota or the
//! same repository. First, a worker belongs to the instance whose prefix it
//! carries (§28.9): the primary host keeps the legacy `sg-zai-`-style
//! prefixes and every other host derives `sg-<pool>-<host_id>-`, so an
//! instance launches, retires and counts only its own workers and learns of
//! the rest through peer records. Second, one repository is worked from one
//! host only (§28.10): the host named in `repo_owners` keeps it, else the
//! lexicographically lowest host id, and the other instance excludes it
//! from demand and seeding. Third, two instances can launch in the same
//! interval before either sees the other (§28.8): the combined overshoot is
//! bounded by the summed per-cycle step limits, which validation holds
//! under the account's worker reserve, and every launch publishes
//! `pending_launches` and re-pulls its peers once before it commits.
//!
//! The run loop refreshes this view at startup and each cycle, and re-pulls
//! peer records after publishing a launch intent.

use crate::config::Config;
use crate::federation::PeerRecord;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// §28.9: worker identity
// ---------------------------------------------------------------------------

/// The pool name a worker prefix names (§28.9): `sg-zai-` is pool `zai`.
/// The `sg-` stem and the trailing separator are conventional, not
/// required -- a bare `workers-` prefix names pool `workers` -- because the
/// prefix is the identity contract and the pool name only feeds the derived
/// form below.
pub fn pool_of_prefix(prefix: &str) -> &str {
    let stem = prefix.strip_prefix("sg-").unwrap_or(prefix);
    stem.strip_suffix('-').unwrap_or(stem)
}

/// The effective worker prefix this instance launches, retires and counts
/// for one pool (§28.9). The primary host keeps the configured legacy
/// prefix unchanged; every other host derives `sg-<pool>-<host_id>-`, e.g.
/// pool `zai` on host `gmktec` runs workers `sg-zai-gmktec-1`,
/// `sg-zai-gmktec-2`, ... . Two hosts can therefore never claim the same
/// workers, and the startup refusal below catches the residual mistakes.
pub fn effective_prefix(configured: &str, host_id: &str, primary: bool) -> String {
    if primary || configured.ends_with(&format!("-{host_id}-")) {
        configured.to_owned()
    } else {
        format!("sg-{}-{host_id}-", pool_of_prefix(configured))
    }
}

/// A worker belongs to a prefix only when the suffix is the numeric worker
/// slot. This prevents the primary `sg-zai-` from claiming a peer's
/// `sg-zai-gmktec-1` on a shared heartbeat or tmux surface.
pub fn matches_worker_prefix(prefix: &str, worker_id: &str) -> bool {
    worker_id.strip_prefix(prefix).is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

/// The prefixes a peer record already claims (§28.9): one per pool entry,
/// keyed by the same prefix strings this instance would derive. A record
/// with no pools claims nothing.
pub fn claimed_prefixes(record: &PeerRecord) -> BTreeSet<String> {
    record
        .accounts
        .values()
        .flat_map(|account| account.pools.keys().cloned())
        .collect()
}

/// Why startup refused this instance's worker identity (§28.9).
#[derive(Debug, thiserror::Error)]
pub enum OwnershipError {
    /// A prefix this instance would use already appears in a peer's record.
    /// Two instances counting and launching the same prefix would each
    /// believe it owns every worker in it, so this is a startup refusal,
    /// never a warning.
    #[error(
        "worker prefix {prefix} is already claimed by peer host {host} \
         (plan.md §28.9: a worker belongs to the instance whose prefix it carries)"
    )]
    PrefixClaimed { prefix: String, host: String },
}

/// §28.9's startup gate: refuses any effective local prefix that a peer
/// record already claims. `local_prefixes` maps the account name to the
/// prefix this instance would run it with (see [`effective_prefix`]);
/// `records` are the peer records on file at startup.
pub fn refuse_prefix_collisions(
    local_prefixes: &BTreeMap<String, String>,
    records: &[PeerRecord],
) -> Result<()> {
    for prefix in local_prefixes.values() {
        for record in records {
            if claimed_prefixes(record).contains(prefix) {
                return Err(OwnershipError::PrefixClaimed {
                    prefix: prefix.clone(),
                    host: record.host_id.clone(),
                }
                .into());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// §28.10: workspace ownership
// ---------------------------------------------------------------------------

/// The host that keeps a workspace with this origin (§28.10): the host
/// named in `repo_owners` when the origin has an entry, else the
/// lexicographically lowest host id among the hosts that list it.
/// `claimants` carries every host holding the origin, this instance
/// included -- the owner need not be a configured peer if `repo_owners`
/// names a third host.
pub fn workspace_owner<'a>(
    origin: &str,
    repo_owners: &BTreeMap<String, String>,
    claimants: impl IntoIterator<Item = &'a str>,
) -> String {
    if let Some(owner) = repo_owners.get(origin) {
        return owner.clone();
    }
    claimants.into_iter().min().unwrap_or("").to_owned()
}

/// The outcome of one §28.10 overlap check against this instance's
/// workspaces.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OverlapVerdict {
    /// Local workspace paths this instance must exclude from demand and
    /// seeding, keyed by path with the contested origin as the value. A
    /// path is here only when another host keeps it.
    pub excluded: BTreeMap<String, String>,
    /// One `workspace_overlap` event per excluded workspace, for the
    /// ordinary decision-event surface.
    pub events: Vec<Value>,
}

impl OverlapVerdict {
    /// Whether `path` stays seedable after this verdict.
    pub fn is_seedable(&self, path: &str) -> bool {
        !self.excluded.contains_key(path)
    }

    /// The workspace paths that survive the verdict, in order.
    pub fn seedable<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        paths
            .into_iter()
            .filter(|path| self.is_seedable(path))
            .map(str::to_owned)
            .collect()
    }
}

/// Persist the latest overlap decision for the local actuator and the
/// read-only peer-state projection. Publishing an empty verdict clears a
/// prior exclusion after a peer stops advertising that origin.
pub fn publish_workspace_exclusions(path: &Path, verdict: &OverlapVerdict) -> Result<()> {
    write_private_json(path, &verdict.excluded)
}

pub fn read_workspace_exclusions(path: &Path) -> BTreeSet<PathBuf> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BTreeMap<String, String>>(&bytes).ok())
        .unwrap_or_default()
        .into_keys()
        .map(PathBuf::from)
        .collect()
}

/// Refreshes the ownership view used by a cycle. A failed peer pull freezes
/// launches and preserves the previous exclusions; a successful full pull
/// publishes the new overlap decision before any seed is chosen.
pub fn refresh_ownership(
    config: &Config,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<OwnershipCycle> {
    let Some(federation) = &config.federation else {
        return Ok(OwnershipCycle::default());
    };
    if federation.peers.is_empty() {
        return Ok(OwnershipCycle::default());
    }
    let mut records = Vec::new();
    let mut fresh = true;
    for peer in &federation.peers {
        match crate::federation::pull_peer_record(peer) {
            Ok(record) if record.host_id == peer.host_id => records.push(record),
            _ => fresh = false,
        }
    }
    refuse_prefix_collisions(&crate::federation::local_worker_prefixes(config), &records)?;
    let path = federation.workspace_exclusions_path(config);
    let (excluded, events) = if fresh {
        let verdict = resolve_workspace_overlap(
            &federation.host_id,
            &crate::federation::local_workspace_origins(config),
            &records,
            &federation.repo_owners,
            now,
        );
        publish_workspace_exclusions(&path, &verdict)?;
        (
            verdict.excluded.into_keys().map(PathBuf::from).collect(),
            verdict.events,
        )
    } else {
        (read_workspace_exclusions(&path), Vec::new())
    };
    Ok(OwnershipCycle {
        fresh,
        excluded,
        events,
        records,
    })
}

#[derive(Debug)]
pub struct OwnershipCycle {
    /// Every configured peer answered this pull with its claimed host id.
    pub fresh: bool,
    /// Local workspaces held by another host, both from demand and seeding.
    pub excluded: BTreeSet<PathBuf>,
    pub events: Vec<Value>,
    pub records: Vec<PeerRecord>,
}

impl Default for OwnershipCycle {
    fn default() -> Self {
        Self {
            fresh: true,
            excluded: BTreeSet::new(),
            events: Vec::new(),
            records: Vec::new(),
        }
    }
}

/// §28.10's overlap check, run at startup and on every pull: compares this
/// instance's workspace origins against every peer record's. A workspace
/// whose origin also appears on another host is kept by the §28.10 owner
/// (the `repo_owners` entry, else the lowest host id); when this instance
/// is not the owner it excludes the workspace from demand and seeding and
/// emits one `workspace_overlap` event. Uncontested workspaces and origins
/// that could not be read are never touched.
pub fn resolve_workspace_overlap(
    local_host: &str,
    local_origins: &BTreeMap<String, String>,
    records: &[PeerRecord],
    repo_owners: &BTreeMap<String, String>,
    now: chrono::DateTime<chrono::Utc>,
) -> OverlapVerdict {
    let mut verdict = OverlapVerdict::default();
    for (path, origin) in local_origins {
        let mut claimants: BTreeSet<&str> = BTreeSet::from([local_host]);
        let mut peers: Vec<String> = Vec::new();
        for record in records {
            if record
                .workspace_origins
                .values()
                .any(|peer_origin| peer_origin == origin)
            {
                claimants.insert(record.host_id.as_str());
                peers.push(record.host_id.clone());
            }
        }
        if peers.is_empty() {
            continue;
        }
        let owner = workspace_owner(origin, repo_owners, claimants);
        if owner == local_host {
            continue;
        }
        verdict.events.push(serde_json::json!({
            "event": "workspace_overlap",
            "severity": "warn",
            "time": now,
            "workspace": path,
            "origin": origin,
            "owner": owner,
            "excluded_host": local_host,
            "peer_hosts": peers,
            "action": "excluded_from_demand_and_seeding",
        }));
        verdict.excluded.insert(path.clone(), origin.clone());
    }
    verdict
}

// ---------------------------------------------------------------------------
// §28.8: bounded overshoot
// ---------------------------------------------------------------------------

/// The federation's overshoot bound (§28.8): the sum of every instance's
/// `max_step_up` (its per-cycle scale-up limit, §9.6/§22.7). Two instances
/// can both launch in the interval before either sees the other, so the
/// combined overshoot above the controller total is bounded by this sum --
/// which §28.8's validation holds under the account's worker reserve.
pub fn overshoot_bound(per_host_step_up: impl IntoIterator<Item = u32>) -> u32 {
    per_host_step_up
        .into_iter()
        .fold(0u32, |total, step| total.saturating_add(step))
}

/// The launches §28.8 permits right now: the smallest of what was asked
/// for and the room left under the controller total, after this instance's own workers and in-flight launches and
/// every peer's published workers and pending launches. Zero means the
/// re-pulled total no longer permits the launch and it is abandoned.
///
/// The controller total itself is derived identically everywhere (§28.6),
/// so the same arithmetic on the same inputs yields the same verdict on
/// every host.
pub fn permitted_launch(
    want: u32,
    own_workers: u32,
    own_pending: u32,
    peers_committed: u32,
    controller_total: u32,
    _overshoot_bound: u32,
) -> u32 {
    let room = controller_total
        .saturating_sub(own_workers)
        .saturating_sub(own_pending)
        .saturating_sub(peers_committed);
    want.min(room)
}

/// The pending-launch file's JSON shape (§28.8): launches this instance
/// has decided but whose processes are not observable yet, keyed by pool.
/// `subgov peer-state` publishes it verbatim per pool, so a peer's
/// simultaneous-launch gate sees this instance's intent before its workers
/// exist.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum PendingEntry {
    Legacy(u32),
    Reserved { count: u32, baseline: u32 },
}

impl PendingEntry {
    fn count(self) -> u32 {
        match self {
            Self::Legacy(count) | Self::Reserved { count, .. } => count,
        }
    }
}

fn read_pending_map(path: &Path) -> BTreeMap<String, PendingEntry> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// The instance's published in-flight launches for one pool (§28.8). A
/// missing or unreadable file is zero pending: an instance that cannot
/// prove an in-flight launch is treated as having none, which is the safe
/// direction for its peers (they may then overshoot into the §28.8 bound,
/// never past it) and irrelevant for this instance, which re-publishes
/// before it launches.
pub fn read_pending_launches(path: &Path, pool: &str) -> u32 {
    read_pending_map(path)
        .get(pool)
        .copied()
        .map_or(0, PendingEntry::count)
}

/// Publishes this instance's in-flight launches for one pool (§28.8). The
/// write is atomic and mode 600; other pools' entries are preserved.
pub fn publish_pending_launches(path: &Path, pool: &str, count: u32) -> Result<()> {
    publish_pending_entry(
        path,
        pool,
        (count > 0).then_some(PendingEntry::Legacy(count)),
    )
}

fn publish_pending_entry(path: &Path, pool: &str, entry: Option<PendingEntry>) -> Result<()> {
    let mut pending = read_pending_map(path);
    if let Some(entry) = entry {
        pending.insert(pool.to_owned(), entry);
    } else {
        pending.remove(pool);
    }
    write_private_json(path, &pending)
}

/// A reservation remains published until the next observation has seen all
/// of its workers. The caller clears it only after publishing that observed
/// count to the peer-state status file.
pub fn pending_observed(path: &Path, pool: &str, observed_workers: u32) -> bool {
    matches!(read_pending_map(path).get(pool), Some(PendingEntry::Reserved { count, baseline })
        if observed_workers >= baseline.saturating_add(*count))
}

pub fn clear_pending_launches_pool(path: &Path, pool: &str) -> Result<()> {
    publish_pending_entry(path, pool, None)
}

/// Clears every published in-flight launch (§28.8): the workers are now
/// observable and count as `governed_workers` instead. Removes the file
/// when it exists, so an idle instance publishes no pending state at all.
pub fn clear_pending_launches(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to clear pending launches {}", path.display())),
    }
}

/// The verdict of one §28.8 launch gate.
#[derive(Clone, Debug, PartialEq)]
pub struct LaunchDecision {
    /// How many of the requested launches may proceed this cycle.
    pub permitted: u32,
    /// Whether the launch was abandoned outright: the re-pulled total left
    /// no room, so nothing launches this cycle.
    pub abandoned: bool,
    /// The `launch_abandoned` event when the launch was abandoned, for the
    /// ordinary decision-event surface.
    pub events: Vec<Value>,
}

/// Runs §28.8's launch gate for one pool: publishes `want` as this
/// instance's `pending_launches`, re-pulls every peer once, and admits
/// only what the re-pulled total still permits. Publishing happens before
/// the re-pull so a peer gating in the same interval sees this launch even
/// if this instance never sees theirs. On abandonment the pending marker
/// is withdrawn and a `launch_abandoned` event is queued; the caller then
/// actuates `own_workers + permitted` instead of `own_workers + want`.
///
/// `repull` fetches the freshest record of every configured peer. It
/// returns whatever records arrived: a peer that fails to answer
/// contributes nothing here, because §28.7's staleness freeze already
/// holds scale-up while a peer is stale -- this gate is the second,
/// overshoot-specific check, not a freshness decision.
#[allow(clippy::too_many_arguments)] // Callers state every bound and identity used by the launch safety gate.
pub fn gate_launch(
    want: u32,
    local_step_limit: u32,
    own_workers: u32,
    own_current_pending: u32,
    account_fingerprint: &str,
    pool: &str,
    pending_path: &Path,
    controller_total: u32,
    bound: u32,
    now: chrono::DateTime<chrono::Utc>,
    repull: impl FnOnce() -> Result<Vec<PeerRecord>>,
) -> Result<LaunchDecision> {
    let want = want.min(local_step_limit);
    let previous = read_pending_map(pending_path).get(pool).copied();
    let baseline = match previous {
        Some(PendingEntry::Reserved { baseline, .. }) => baseline,
        _ => own_workers,
    };
    // §28.8: publish first, then re-pull once. The published count is the
    // launch this cycle is attempting, so a concurrent gate on a peer sees
    // the full intent.
    publish_pending_entry(
        pending_path,
        pool,
        Some(PendingEntry::Reserved {
            count: own_current_pending.saturating_add(want),
            baseline,
        }),
    )
    .with_context(|| {
        format!(
            "failed to publish pending launches {}",
            pending_path.display()
        )
    })?;
    let records = match repull() {
        Ok(records) => records,
        Err(error) => {
            publish_pending_entry(pending_path, pool, previous)?;
            return Err(error.context("peer re-pull failed; launch abandoned"));
        }
    };
    let peers_committed: u32 = records
        .iter()
        .map(|record| {
            record
                .accounts
                .values()
                .filter(|account| account.account_fingerprint == account_fingerprint)
                .flat_map(|account| account.pools.values())
                .map(|state| {
                    state
                        .governed_workers
                        .saturating_add(state.pending_launches)
                })
                .fold(0u32, u32::saturating_add)
        })
        .fold(0u32, u32::saturating_add);
    let permitted = permitted_launch(
        want,
        own_workers,
        own_current_pending,
        peers_committed,
        controller_total,
        bound,
    );
    if permitted == 0 {
        publish_pending_entry(pending_path, pool, previous).with_context(|| {
            format!(
                "failed to withdraw pending launches {}",
                pending_path.display()
            )
        })?;
        return Ok(LaunchDecision {
            permitted: 0,
            abandoned: true,
            events: vec![serde_json::json!({
                "event": "launch_abandoned",
                "severity": "warn",
                "time": now,
                "pool": pool,
                "requested": want,
                "peers_committed": peers_committed,
                "controller_total": controller_total,
                "overshoot_bound": bound,
                "action": "no_launch_this_cycle",
            })],
        });
    }
    // Publish only what survived the gate: a peer gating next sees the
    // reduced intent, not the abandoned remainder.
    publish_pending_entry(
        pending_path,
        pool,
        Some(PendingEntry::Reserved {
            count: own_current_pending.saturating_add(permitted),
            baseline,
        }),
    )
    .with_context(|| {
        format!(
            "failed to publish pending launches {}",
            pending_path.display()
        )
    })?;
    Ok(LaunchDecision {
        permitted,
        abandoned: false,
        events: Vec::new(),
    })
}

fn write_private_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create directory {}", parent.display()))?;
    let temp = path.with_extension("tmp");
    let mut file =
        fs::File::create(&temp).with_context(|| format!("failed to write {}", temp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(serde_json::to_vec(value)?.as_slice())
        .with_context(|| format!("failed to write {}", temp.display()))?;
    drop(file);
    fs::rename(&temp, path).with_context(|| format!("failed to publish {}", path.display()))?;
    Ok(())
}

/// The per-account §28.8 reserve check behind config validation: the
/// summed per-cycle step limits of the account's hosts may not exceed the
/// account's worker reserve (`max_workers`). With a federation of one
/// (`peers` empty) the check does not apply: there is no second actuator
/// to overshoot with, and §28.13 requires single-host configs to be
/// accepted unchanged. Returns the account name and the offending sum on
/// violation, for the caller's error message.
pub fn overshoot_violation(config: &Config) -> Option<(String, u32, u32)> {
    let federation = config.federation.as_ref()?;
    if federation.peers.is_empty() {
        return None;
    }
    for (name, account) in &config.accounts {
        let summed = step_up_bound(config, name);
        if summed > account.fleet.max_workers {
            return Some((name.clone(), summed, account.fleet.max_workers));
        }
    }
    None
}

pub fn step_up_bound(config: &Config, account_name: &str) -> u32 {
    let account = &config.accounts[account_name];
    let steps = match &account.fleet.hosts {
        Some(hosts) => {
            // A peer omitted from this local host map can still run the
            // account. Count its account-level step limit so an incomplete
            // map cannot understate the federation's overshoot bound.
            let mut host_ids: BTreeSet<&str> = hosts.keys().map(String::as_str).collect();
            if let Some(federation) = &config.federation {
                host_ids.insert(&federation.host_id);
                host_ids.extend(federation.peers.iter().map(|peer| peer.host_id.as_str()));
            }
            host_ids
                .into_iter()
                .map(|host_id| {
                    hosts
                        .get(host_id)
                        .and_then(|host| host.max_scale_up_per_cycle)
                        .unwrap_or(account.fleet.max_scale_up_per_cycle)
                })
                .collect()
        }
        None => vec![
            account.fleet.max_scale_up_per_cycle;
            config.federation.as_ref().map_or(1, |f| f.peers.len() + 1)
        ],
    };
    overshoot_bound(steps)
}

pub fn local_step_limit(config: &Config, account_name: &str) -> u32 {
    let account = &config.accounts[account_name];
    match (&account.fleet.hosts, &config.federation) {
        (Some(hosts), Some(federation)) => hosts
            .get(&federation.host_id)
            .and_then(|host| host.max_scale_up_per_cycle)
            .unwrap_or(account.fleet.max_scale_up_per_cycle),
        _ => account.fleet.max_scale_up_per_cycle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccountConfig, FederationConfig, FleetConfig, SourceConfig};
    use crate::federation::{PeerAccount, PeerPool, PeerReadyDemand};
    use chrono::Utc;
    use std::time::Duration;

    fn record(host_id: &str, origins: BTreeMap<String, String>) -> PeerRecord {
        PeerRecord {
            schema_version: crate::federation::PEER_RECORD_SCHEMA_VERSION,
            instance_id: "0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_owned(),
            host_id: host_id.to_owned(),
            record_seq: 1,
            observed_at: Utc::now(),
            accounts: BTreeMap::new(),
            host_ceiling: None,
            host_reservations: BTreeMap::new(),
            resource_reserve: None,
            workspace_origins: origins,
        }
    }

    fn pool_record(host_id: &str, prefix: &str, workers: u32, pending: u32) -> PeerRecord {
        let mut record = record(host_id, BTreeMap::new());
        record.accounts.insert(
            "acct".to_owned(),
            PeerAccount {
                account_id: "acct".to_owned(),
                account_fingerprint: crate::federation::account_fingerprint("provider:acct"),
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
        );
        record
    }

    #[test]
    fn derived_prefixes_never_collide_with_the_primary() {
        assert_eq!(effective_prefix("sg-zai-", "codinghome", true), "sg-zai-");
        assert_eq!(
            effective_prefix("sg-zai-", "gmktec", false),
            "sg-zai-gmktec-"
        );
        assert_eq!(pool_of_prefix("sg-zai-"), "zai");
        assert_eq!(pool_of_prefix("sg-claude-anthropic-"), "claude-anthropic");
        assert_eq!(
            effective_prefix("sg-zai-gmktec-", "gmktec", false),
            "sg-zai-gmktec-"
        );
        // Every host's derived prefix names its own host, so two hosts can
        // never derive the same string.
        assert_ne!(
            effective_prefix("sg-zai-", "gmktec", false),
            effective_prefix("sg-zai-", "codinghome", false)
        );
        assert!(matches_worker_prefix("sg-zai-", "sg-zai-1"));
        assert!(matches_worker_prefix("sg-zai-gmktec-", "sg-zai-gmktec-3"));
        assert!(!matches_worker_prefix("sg-zai-", "sg-zai-gmktec-3"));
        assert!(!matches_worker_prefix("sg-zai-", "sg-zai-"));
    }

    #[test]
    fn startup_refuses_a_prefix_a_peer_record_claims() {
        let mut local = BTreeMap::from([("acct".to_owned(), "sg-zai-".to_owned())]);
        let peers = vec![pool_record("gmktec", "sg-zai-", 2, 0)];
        let error = refuse_prefix_collisions(&local, &peers).unwrap_err();
        assert!(error.to_string().contains("sg-zai-"), "{error}");
        assert!(error.to_string().contains("gmktec"), "{error}");

        // The derived form of a non-primary host is free when the primary
        // holds the legacy prefix.
        local.insert("acct".to_owned(), "sg-zai-gmktec-".to_owned());
        refuse_prefix_collisions(&local, &peers).unwrap();
    }

    #[test]
    fn overlap_keeps_one_host_and_excludes_the_other() {
        let origin = "https://git.ardenone.com/jedarden/SEAM.git".to_owned();
        let local_origins = BTreeMap::from([("/w/SEAM".to_owned(), origin.clone())]);
        let peer = record(
            "gmktec",
            BTreeMap::from([("/srv/SEAM".to_owned(), origin.clone())]),
        );
        let now = Utc::now();

        // No repo_owners entry: the lowest host id keeps the repo. Here
        // that is the local host, so nothing is excluded.
        let verdict = resolve_workspace_overlap(
            "codinghome",
            &local_origins,
            std::slice::from_ref(&peer),
            &BTreeMap::new(),
            now,
        );
        assert!(verdict.excluded.is_empty());
        assert!(verdict.events.is_empty());
        assert!(verdict.is_seedable("/w/SEAM"));

        // The same origin on a lower-named peer host: the local workspace
        // is excluded and the event names the owner.
        let peer = record(
            "aardvark",
            BTreeMap::from([("/srv/SEAM".to_owned(), origin.clone())]),
        );
        let verdict =
            resolve_workspace_overlap("codinghome", &local_origins, &[peer], &BTreeMap::new(), now);
        assert_eq!(verdict.excluded.get("/w/SEAM").unwrap(), &origin);
        assert!(!verdict.is_seedable("/w/SEAM"));
        assert_eq!(verdict.events.len(), 1);
        assert_eq!(verdict.events[0]["event"], "workspace_overlap");
        assert_eq!(verdict.events[0]["owner"], "aardvark");

        // A repo_owners entry overrides the lexicographic default in both
        // directions.
        let owners = BTreeMap::from([(origin.clone(), "codinghome".to_owned())]);
        let peer = record(
            "aardvark",
            BTreeMap::from([("/srv/SEAM".to_owned(), origin.clone())]),
        );
        let verdict =
            resolve_workspace_overlap("codinghome", &local_origins, &[peer], &owners, now);
        assert!(verdict.excluded.is_empty());

        let peer = record(
            "aardvark",
            BTreeMap::from([("/srv/SEAM".to_owned(), origin.clone())]),
        );
        let verdict = resolve_workspace_overlap("aardvark", &local_origins, &[peer], &owners, now);
        assert_eq!(verdict.excluded.get("/w/SEAM").unwrap(), &origin);

        // An origin only this instance holds is never contested.
        let lone = BTreeMap::from([("/w/alone".to_owned(), "https://x/y.git".to_owned())]);
        let verdict = resolve_workspace_overlap(
            "codinghome",
            &lone,
            &[record("gmktec", BTreeMap::new())],
            &BTreeMap::new(),
            now,
        );
        assert!(verdict.excluded.is_empty());
    }

    #[test]
    fn simultaneous_launches_never_pass_the_summed_step_limit() {
        let dir = tempfile::tempdir().unwrap();
        let pending_path = dir.path().join("pending.json");
        let now = Utc::now();

        // Account total 5, summed step limit 3 + 2 = 5. Both hosts start at
        // zero workers and both want 3.
        let bound = overshoot_bound([3, 2]);
        assert_eq!(bound, 5);

        // codinghome gates first: no peer commits are visible yet, so it
        // gets its full step.
        let a = gate_launch(
            3,
            3,
            0,
            0,
            &crate::federation::account_fingerprint("provider:acct"),
            "sg-zai-",
            &pending_path,
            5,
            bound,
            now,
            || Ok(Vec::new()),
        )
        .unwrap();
        assert_eq!(a.permitted, 3);
        assert!(!a.abandoned);
        assert_eq!(read_pending_launches(&pending_path, "sg-zai-"), 3);

        // gmktec gates in the same interval: its re-pull now sees
        // codinghome's pending 3, so it is held to what the bound allows.
        let b = gate_launch(
            3,
            2,
            0,
            0,
            &crate::federation::account_fingerprint("provider:acct"),
            "sg-zai-gmktec-",
            &pending_path,
            5,
            bound,
            now,
            || Ok(vec![pool_record("codinghome", "sg-zai-", 0, 3)]),
        )
        .unwrap();
        assert_eq!(b.permitted, 2);

        // The combined launch stays within the summed step limit and the
        // combined worker count within the total plus the bound.
        assert!(a.permitted + b.permitted <= bound);
        assert!(a.permitted + b.permitted <= 5);
    }

    #[test]
    fn a_launch_is_abandoned_when_the_repulled_total_no_longer_permits_it() {
        let dir = tempfile::tempdir().unwrap();
        let pending_path = dir.path().join("pending.json");
        let now = Utc::now();

        // Peers already hold everything the account total plus the bound
        // allows: nothing may launch and the pending marker is withdrawn.
        let a = gate_launch(
            3,
            3,
            4,
            0,
            &crate::federation::account_fingerprint("provider:acct"),
            "sg-zai-",
            &pending_path,
            1,
            5,
            now,
            || Ok(vec![pool_record("gmktec", "sg-zai-", 4, 3)]),
        )
        .unwrap();
        assert!(a.abandoned);
        assert_eq!(a.permitted, 0);
        assert_eq!(a.events.len(), 1);
        assert_eq!(a.events[0]["event"], "launch_abandoned");
        assert_eq!(read_pending_launches(&pending_path, "sg-zai-"), 0);

        // A launch already in flight (own_current_pending) is preserved
        // across an abandoned follow-up.
        publish_pending_launches(&pending_path, "sg-zai-", 2).unwrap();
        let a = gate_launch(
            3,
            3,
            2,
            2,
            &crate::federation::account_fingerprint("provider:acct"),
            "sg-zai-",
            &pending_path,
            0,
            5,
            now,
            || Ok(vec![pool_record("gmktec", "sg-zai-", 8, 0)]),
        )
        .unwrap();
        assert!(a.abandoned);
        assert_eq!(read_pending_launches(&pending_path, "sg-zai-"), 2);

        clear_pending_launches(&pending_path).unwrap();
        assert_eq!(read_pending_launches(&pending_path, "sg-zai-"), 0);
        // Clearing twice is fine.
        clear_pending_launches(&pending_path).unwrap();
    }

    #[test]
    fn overshoot_validation_holds_the_summed_step_limit_under_the_reserve() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("governor.yaml");
        let base = |hosts: &str| {
            format!(
                "version: 1\naccounts:\n  acct:\n    source: {{ type: normalized_file, path: /tmp/subgov-test-source.json }}\n    utilization: {{ reserve_fraction: 0.1 }}\n    fleet:\n      max_workers: 4\n{hosts}\nfederation:\n  host_id: codinghome\n  peers:\n    - host_id: gmktec\n"
            )
        };
        // Two hosts stepping 3 + 2 = 5 over a reserve of 4: rejected.
        fs::write(
            &config_path,
            base("      max_scale_up_per_cycle: 3\n      hosts:\n        codinghome:\n          max_scale_up_per_cycle: 3\n          observer: { type: static, workers: 1 }\n          actuator: { type: none }\n        gmktec:\n          max_scale_up_per_cycle: 2\n          observer: { type: static, workers: 1 }\n          actuator: { type: none }"),
        )
        .unwrap();
        let error = Config::load(&config_path).unwrap_err();
        assert!(error.to_string().contains("max_step_up"), "{error}");

        // 3 + 1 = 4 under a reserve of 4: accepted.
        fs::write(
            &config_path,
            base("      max_scale_up_per_cycle: 3\n      hosts:\n        codinghome:\n          max_scale_up_per_cycle: 3\n          observer: { type: static, workers: 1 }\n          actuator: { type: none }\n        gmktec:\n          max_scale_up_per_cycle: 1\n          observer: { type: static, workers: 1 }\n          actuator: { type: none }"),
        )
        .unwrap();
        Config::load(&config_path).unwrap();

        // A peer can be absent from this instance's host map while still
        // running the account. Its fallback step must count toward the bound.
        fs::write(
            &config_path,
            base("      max_scale_up_per_cycle: 3\n      hosts:\n        codinghome:\n          max_scale_up_per_cycle: 2\n          observer: { type: static, workers: 1 }\n          actuator: { type: none }"),
        )
        .unwrap();
        let error = Config::load(&config_path).unwrap_err();
        assert!(error.to_string().contains("max_step_up"), "{error}");
    }

    #[test]
    fn pending_launches_publish_through_the_local_record() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let now = Utc::now();
        let federation = config.federation.as_ref().unwrap();
        let pending_path = federation.pending_launches_path(&config);

        publish_pending_launches(&pending_path, "acct", 2).unwrap();
        let record = crate::federation::build_local_record(&config, now).unwrap();
        assert_eq!(record.accounts["acct"].pools["acct"].pending_launches, 2);

        clear_pending_launches(&pending_path).unwrap();
        let record = crate::federation::build_local_record(&config, now).unwrap();
        assert_eq!(record.accounts["acct"].pools["acct"].pending_launches, 0);
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
                account_identities: BTreeMap::new(),
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
    fn peer_staleness_bounds_still_default() {
        // Guards against a silent drift of the §28.7 bounds this module's
        // callers rely on.
        let federation = FederationConfig {
            host_id: "codinghome".to_owned(),
            instance_id_file: None,
            record_seq_file: None,
            host_capacity_state: None,
            account_identities: BTreeMap::new(),
            peers: Vec::new(),
            peer_stale_after_seconds: 120,
            peer_expiry_seconds: 1_800,
            primary: false,
            repo_owners: BTreeMap::new(),
            pending_launches_file: None,
        };
        assert_eq!(federation.peer_stale_after(), Duration::from_secs(120));
        assert_eq!(federation.peer_expiry(), Duration::from_secs(1_800));
    }
}
