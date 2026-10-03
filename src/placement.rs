//! plan.md §22.6/§22.7: resource-aware multi-host placement. `place`
//! distributes the controller's already-safe `account_target` (§9's output,
//! unchanged) across an account's configured hosts by machine-resource
//! headroom. It is a pure function of its arguments -- no I/O, exactly like
//! `controller::evaluate` (§22.6: "performs no I/O, exactly like
//! controller"). Placement never authorizes more workers than the
//! controller already decided (§22.15 decision #2); it only decides where.

use crate::config::AccountConfig;
use crate::model::ResourceSnapshot;
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeMap;

/// One host's placement outcome for a single cycle (plan.md §22.7/§22.11).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HostPlacement {
    pub host_id: String,
    pub current: u32,
    pub target: u32,
    pub ceiling: u32,
    pub headroom: f64,
    pub fresh: bool,
    pub eligible: bool,
}

/// Distributes `account_target` across `config.fleet.hosts` (plan.md
/// §22.7). Fails if the account does not declare at least one host --
/// callers are expected to only reach for `place` once `fleet.hosts` is
/// known to be configured (§22.2: an account with no `hosts` key uses the
/// single-implicit-host path directly and never calls `place` at all).
///
/// `current` and `resources` are keyed by host id and are the caller's own
/// per-host I/O results (worker observer counts and resource snapshots) --
/// this function only combines them with the already-loaded configuration:
///
/// - A host whose config has no `resource_source` never appears in
///   `resources`; it is treated as unconstrained and always has full
///   headroom (§22.2).
/// - A host whose config *does* declare a `resource_source` but has no
///   entry in `resources` this cycle is treated as unobserved: never
///   eligible, same as an explicitly stale snapshot (§22.8's "absent a
///   resource_source-based read" case, applied per host).
/// - A host missing from `current` is treated as currently running zero
///   workers.
pub fn place(
    account_name: &str,
    account_target: u32,
    config: &AccountConfig,
    current: &BTreeMap<String, u32>,
    resources: &BTreeMap<String, ResourceSnapshot>,
    now: DateTime<Utc>,
) -> Result<Vec<HostPlacement>> {
    let Some(hosts) = config
        .fleet
        .hosts
        .as_ref()
        .filter(|hosts| !hosts.is_empty())
    else {
        bail!("account {account_name}: place requires a non-empty fleet.hosts configuration");
    };

    let mut current_of = BTreeMap::new();
    let mut ceiling_of = BTreeMap::new();
    let mut step_limits_of = BTreeMap::new();
    let mut headroom_of = BTreeMap::new();
    let mut fresh_of = BTreeMap::new();

    for (host_id, host) in hosts {
        current_of.insert(host_id.as_str(), current.get(host_id).copied().unwrap_or(0));
        ceiling_of.insert(
            host_id.as_str(),
            host.max_workers.unwrap_or(config.fleet.max_workers),
        );
        step_limits_of.insert(
            host_id.as_str(),
            (
                host.max_scale_up_per_cycle
                    .unwrap_or(config.fleet.max_scale_up_per_cycle),
                host.max_scale_down_per_cycle
                    .unwrap_or(config.fleet.max_scale_down_per_cycle),
            ),
        );
        let (fresh, headroom) = match &host.resource_source {
            // §22.2: a host with no resource_source is unconstrained --
            // headroom is always 1, and there is no staleness concept to
            // fail since nothing was ever polled.
            None => (true, 1.0),
            Some(_) => resource_headroom(
                resources.get(host_id),
                host.resource_reserve
                    .as_ref()
                    .map(|reserve| reserve.mem_reserve_mb)
                    .unwrap_or(0),
                config.utilization.stale_after_seconds,
                now,
            ),
        };
        fresh_of.insert(host_id.as_str(), fresh);
        headroom_of.insert(host_id.as_str(), headroom);
    }

    let eligible: Vec<&str> = hosts
        .keys()
        .map(String::as_str)
        .filter(|id| fresh_of[id] && headroom_of[id] > 0.0)
        .collect();

    // Ineligible hosts retain their current allocation while the account
    // target is stable or growing. This is the §22.8 frozen-host guarantee:
    // stale resource data alone must not drain a host. When the account
    // target shrinks, they may step down normally toward zero, as §22.8
    // allows. Reserve that ineligible allocation before distributing the
    // remaining account budget among eligible hosts.
    let current_total = current_of
        .values()
        .fold(0u32, |total, workers| total.saturating_add(*workers));
    let mut ineligible_target = BTreeMap::new();
    for id in hosts.keys().map(String::as_str) {
        if !eligible.contains(&id) {
            let current = current_of[id];
            let target = if account_target >= current_total {
                current
            } else {
                0
            };
            ineligible_target.insert(
                id,
                apply_step_limit(
                    target,
                    current,
                    ceiling_of[id],
                    step_limits_of[id].0,
                    step_limits_of[id].1,
                ),
            );
        }
    }

    // §22.7: "if eligible is empty: target[h] = current[h] for every host --
    // hold, never scale up blind."
    let raw_target: BTreeMap<&str, u32> = if eligible.is_empty() {
        current_of.clone()
    } else {
        let all_hosts: Vec<&str> = hosts.keys().map(String::as_str).collect();
        let ineligible_total = ineligible_target
            .values()
            .fold(0u32, |total, workers| total.saturating_add(*workers));
        let mut distributed = distribute(
            account_target.saturating_sub(ineligible_total),
            &eligible,
            &headroom_of,
            &ceiling_of,
            &all_hosts,
        );
        for (id, target) in ineligible_target {
            distributed.insert(id, target);
        }
        distributed
    };

    let placements = hosts
        .keys()
        .map(|host_id| {
            let id = host_id.as_str();
            let cur = current_of[id];
            let ceiling = ceiling_of[id];
            let target = apply_step_limit(
                raw_target[id],
                cur,
                ceiling,
                step_limits_of[id].0,
                step_limits_of[id].1,
            );
            HostPlacement {
                host_id: host_id.clone(),
                current: cur,
                target,
                ceiling,
                headroom: headroom_of[id],
                fresh: fresh_of[id],
                eligible: fresh_of[id] && headroom_of[id] > 0.0,
            }
        })
        .collect();
    Ok(placements)
}

/// §22.8 freshness gate plus the §22.7 headroom formula for one host with a
/// configured `resource_source`.
fn resource_headroom(
    snapshot: Option<&ResourceSnapshot>,
    mem_reserve_mb: u64,
    stale_after_seconds: u64,
    now: DateTime<Utc>,
) -> (bool, f64) {
    let Some(snapshot) = snapshot else {
        return (false, 0.0);
    };
    let age_seconds = now
        .signed_duration_since(snapshot.observed_at)
        .num_seconds()
        .max(0) as u64;
    let fresh = snapshot.fresh && age_seconds <= stale_after_seconds;
    if !fresh {
        return (false, 0.0);
    }
    let mem_fraction = if snapshot.mem_total_mb == 0 {
        0.0
    } else {
        (snapshot.mem_available_mb as f64 - mem_reserve_mb as f64) / snapshot.mem_total_mb as f64
    };
    let headroom = snapshot
        .cpu_available_fraction
        .min(mem_fraction)
        .clamp(0.0, 1.0);
    (true, headroom)
}

fn apply_step_limit(desired: u32, current: u32, ceiling: u32, max_up: u32, max_down: u32) -> u32 {
    let bounded = desired.min(ceiling);
    let stepped = if bounded > current {
        bounded.min(current.saturating_add(max_up))
    } else {
        bounded.max(current.saturating_sub(max_down))
    };
    // Ceiling is a hard cap in every branch, mirroring
    // controller::apply_step_limits' own unconditional re-clamp: the bound
    // must hold even when `current` itself already sits above `ceiling`
    // (e.g. after a fleet reconfiguration lowered a host's max_workers).
    stepped.min(ceiling)
}

fn distribute<'a>(
    account_target: u32,
    eligible: &[&'a str],
    headroom_of: &BTreeMap<&'a str, f64>,
    ceiling_of: &BTreeMap<&'a str, u32>,
    all_hosts: &[&'a str],
) -> BTreeMap<&'a str, u32> {
    let total_headroom: f64 = eligible.iter().map(|id| headroom_of[id]).sum();
    let mut clamped: BTreeMap<&str, u32> = BTreeMap::new();
    for &id in all_hosts {
        if eligible.contains(&id) {
            let weight = headroom_of[id] / total_headroom;
            // Deliberately floor (not round-half-up) each host's raw share.
            // Rounding half up can push the *sum* of independently-rounded
            // shares above account_target (e.g. two hosts at weight 0.5
            // each with account_target=3 round to 2+2=4); flooring never
            // can, because sum(floor(x_i)) <= sum(x_i) = account_target
            // always. The while-loop below is exactly the standard
            // largest-remainder-method fixup for the fractional part that
            // flooring discards, so it recovers the shortfall this
            // introduces without ever risking an overshoot.
            let raw = floor_share(account_target, weight);
            clamped.insert(id, raw.min(ceiling_of[id]));
        } else {
            // §22.7: "raw[h] = 0 for h not in eligible."
            clamped.insert(id, 0);
        }
    }

    let mut sum: u32 = clamped.values().sum();
    while sum < account_target {
        let mut best: Option<&str> = None;
        let mut best_score = f64::NEG_INFINITY;
        for &id in eligible {
            if clamped[id] >= ceiling_of[id] {
                continue;
            }
            let score = headroom_of[id] - (clamped[id] as f64 / ceiling_of[id] as f64);
            // Ties break by host key, ascending (§22.7): `eligible` is
            // already built in ascending BTreeMap order, and a strict `>`
            // here keeps the first (smallest-key) host on an exact tie.
            if score > best_score {
                best_score = score;
                best = Some(id);
            }
        }
        let Some(winner) = best else {
            // Every eligible host is already at its ceiling -- the account
            // total cannot be fully placed this cycle.
            break;
        };
        *clamped.get_mut(winner).unwrap() += 1;
        sum += 1;
    }
    clamped
}

fn floor_share(account_target: u32, weight: f64) -> u32 {
    let share = account_target as f64 * weight;
    if !share.is_finite() || share <= 0.0 {
        return 0;
    }
    share.floor().min(f64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AccountConfig, ActuatorConfig, BankedResetConfig, FleetConfig, HostConfig,
        ObserverReconciliation, ResourceReserveConfig, ResourceSourceConfig, SourceConfig,
        Strategy, UtilizationConfig, WorkerObserverConfig,
    };
    use chrono::Duration;

    fn host(
        max_workers: Option<u32>,
        resource_source: Option<ResourceSourceConfig>,
        resource_reserve: Option<ResourceReserveConfig>,
    ) -> HostConfig {
        HostConfig {
            max_workers,
            max_scale_up_per_cycle: None,
            max_scale_down_per_cycle: None,
            resource_reserve,
            resource_source,
            observer: WorkerObserverConfig::Static { workers: 0 },
            actuator: ActuatorConfig::None,
        }
    }

    fn unconstrained_host(max_workers: Option<u32>) -> HostConfig {
        host(max_workers, None, None)
    }

    fn resource_host(max_workers: Option<u32>, mem_reserve_mb: u64) -> HostConfig {
        host(
            max_workers,
            Some(ResourceSourceConfig::Command {
                argv: vec!["resource-probe".into()],
            }),
            Some(ResourceReserveConfig {
                cpu_reserve_fraction: 0.0,
                mem_reserve_mb,
            }),
        )
    }

    fn snapshot(
        now: DateTime<Utc>,
        host_id: &str,
        fresh: bool,
        cpu_available_fraction: f64,
        mem_available_mb: u64,
        mem_total_mb: u64,
    ) -> ResourceSnapshot {
        ResourceSnapshot {
            observed_at: now,
            fresh,
            host_id: host_id.into(),
            cpu_available_fraction,
            mem_available_mb,
            mem_total_mb,
        }
    }

    fn account(
        hosts: BTreeMap<String, HostConfig>,
        max_workers: u32,
        max_scale_up_per_cycle: u32,
        max_scale_down_per_cycle: u32,
    ) -> AccountConfig {
        AccountConfig {
            source: SourceConfig::NormalizedFile { path: "x".into() },
            fleet: FleetConfig {
                min_workers: 0,
                max_workers,
                bootstrap_workers: 0,
                max_scale_up_per_cycle,
                max_scale_down_per_cycle,
                observer: None,
                actuator: ActuatorConfig::None,
                observer_reconciliation: ObserverReconciliation::default(),
                hosts: Some(hosts),
            },
            utilization: UtilizationConfig {
                target_utilization: Some(0.85),
                reserve_fraction: None,
                strategy: Strategy::LinearToReset,
                stale_after_seconds: 300,
                stale_behavior: Default::default(),
                minimum_sample_seconds: 60,
                windows: BTreeMap::new(),
            },
            banked_resets: BankedResetConfig::default(),
        }
    }

    fn find<'a>(placements: &'a [HostPlacement], host_id: &str) -> &'a HostPlacement {
        placements
            .iter()
            .find(|p| p.host_id == host_id)
            .unwrap_or_else(|| panic!("no placement for host {host_id}"))
    }

    #[test]
    fn bails_when_hosts_is_not_configured() {
        let mut config = account(BTreeMap::new(), 8, 1, 1);
        config.fleet.hosts = None;
        let now = Utc::now();
        let error = place("acct", 4, &config, &BTreeMap::new(), &BTreeMap::new(), now).unwrap_err();
        assert!(error.to_string().contains("non-empty fleet.hosts"));
    }

    #[test]
    fn bails_when_hosts_is_empty() {
        let config = account(BTreeMap::new(), 8, 1, 1);
        let now = Utc::now();
        let error = place("acct", 4, &config, &BTreeMap::new(), &BTreeMap::new(), now).unwrap_err();
        assert!(error.to_string().contains("non-empty fleet.hosts"));
    }

    /// plan.md §22.2: a host with no `resource_source` always has full
    /// headroom and is always fresh/eligible.
    #[test]
    fn unconstrained_host_gets_the_full_account_target() {
        let mut hosts = BTreeMap::new();
        hosts.insert("only".to_string(), unconstrained_host(Some(6)));
        let config = account(hosts, 8, 10, 10);
        let now = Utc::now();
        let placements =
            place("acct", 4, &config, &BTreeMap::new(), &BTreeMap::new(), now).unwrap();
        assert_eq!(placements.len(), 1);
        let p = &placements[0];
        assert_eq!(p.headroom, 1.0);
        assert!(p.fresh);
        assert!(p.eligible);
        assert_eq!(p.target, 4);
    }

    /// A single resource-constrained host receives the entire account
    /// target, clamped to its ceiling.
    #[test]
    fn single_host_receives_the_full_target_up_to_its_ceiling() {
        let mut hosts = BTreeMap::new();
        hosts.insert("solo".to_string(), resource_host(Some(3), 1024));
        let config = account(hosts, 8, 10, 10);
        let now = Utc::now();
        let mut resources = BTreeMap::new();
        resources.insert(
            "solo".to_string(),
            snapshot(now, "solo", true, 0.9, 8192, 16384),
        );
        let placements = place("acct", 8, &config, &BTreeMap::new(), &resources, now).unwrap();
        let p = find(&placements, "solo");
        assert_eq!(p.target, 3, "clamped to the host's own ceiling of 3");
    }

    /// plan.md §22.7: "if eligible is empty: target[h] = current[h] for
    /// every host -- hold, never scale up blind."
    #[test]
    fn no_eligible_host_holds_every_host_at_current() {
        let mut hosts = BTreeMap::new();
        hosts.insert("a".to_string(), resource_host(Some(6), 1024));
        hosts.insert("b".to_string(), resource_host(Some(6), 1024));
        let config = account(hosts, 8, 10, 10);
        let now = Utc::now();
        // Both hosts stale.
        let mut resources = BTreeMap::new();
        resources.insert(
            "a".to_string(),
            snapshot(now - Duration::seconds(9999), "a", true, 0.9, 8192, 16384),
        );
        resources.insert("b".to_string(), snapshot(now, "b", false, 0.9, 8192, 16384));
        let mut current = BTreeMap::new();
        current.insert("a".to_string(), 2u32);
        current.insert("b".to_string(), 3u32);
        let placements = place("acct", 10, &config, &current, &resources, now).unwrap();
        let a = find(&placements, "a");
        let b = find(&placements, "b");
        assert!(!a.eligible);
        assert!(!b.eligible);
        assert_eq!(a.target, 2, "held exactly at current, not reduced");
        assert_eq!(b.target, 3, "held exactly at current, not reduced");
    }

    /// plan.md §22.8: "a resource_source-based read" that never showed up
    /// this cycle is treated the same as an explicitly stale snapshot.
    #[test]
    fn a_host_missing_from_resources_is_never_eligible() {
        let mut hosts = BTreeMap::new();
        hosts.insert("unseen".to_string(), resource_host(Some(6), 1024));
        let config = account(hosts, 8, 10, 10);
        let now = Utc::now();
        let placements =
            place("acct", 5, &config, &BTreeMap::new(), &BTreeMap::new(), now).unwrap();
        let p = &placements[0];
        assert!(!p.fresh);
        assert!(!p.eligible);
        assert_eq!(p.headroom, 0.0);
    }

    /// One host at its ceiling; the shortfall is redistributed to the
    /// other host rather than left unplaced.
    #[test]
    fn ceiling_clamped_host_redistributes_remainder_to_the_other_host() {
        let mut hosts = BTreeMap::new();
        hosts.insert("small".to_string(), resource_host(Some(2), 0));
        hosts.insert("big".to_string(), resource_host(Some(10), 0));
        let config = account(hosts, 10, 10, 10);
        let now = Utc::now();
        let mut resources = BTreeMap::new();
        // Equal headroom on both hosts so the naive weighted split alone
        // would give 3 each for an account_target of 6 -- "small" can only
        // take 2, so "big" must absorb the other 4.
        resources.insert(
            "small".to_string(),
            snapshot(now, "small", true, 0.5, 8192, 16384),
        );
        resources.insert(
            "big".to_string(),
            snapshot(now, "big", true, 0.5, 8192, 16384),
        );
        let placements = place("acct", 6, &config, &BTreeMap::new(), &resources, now).unwrap();
        let small = find(&placements, "small");
        let big = find(&placements, "big");
        assert_eq!(small.target, 2, "clamped to its ceiling");
        assert_eq!(big.target, 6 - 2, "absorbs the shortfall from small");
        assert_eq!(small.target + big.target, 6);
    }

    /// plan.md §22.7's largest-remainder redistribution: three equal-weight
    /// hosts splitting an account_target not evenly divisible by three.
    #[test]
    fn remainder_redistribution_breaks_ties_by_ascending_host_key() {
        let mut hosts = BTreeMap::new();
        for id in ["a", "b", "c"] {
            hosts.insert(id.to_string(), resource_host(Some(10), 0));
        }
        let config = account(hosts, 10, 10, 10);
        let now = Utc::now();
        let mut resources = BTreeMap::new();
        for id in ["a", "b", "c"] {
            resources.insert(id.to_string(), snapshot(now, id, true, 0.5, 8192, 16384));
        }
        // account_target = 10 across three equal-weight hosts: floor(10/3)
        // = 3 each (sum 9), one worker of remainder goes to "a" (smallest
        // key) on the tie.
        let placements = place("acct", 10, &config, &BTreeMap::new(), &resources, now).unwrap();
        assert_eq!(find(&placements, "a").target, 4);
        assert_eq!(find(&placements, "b").target, 3);
        assert_eq!(find(&placements, "c").target, 3);
    }

    /// plan.md §22.8: "existing workers on it are not force-killed by
    /// staleness alone" -- a newly-stale host only loses share through the
    /// normal step-limited scale-down path, never instantly.
    #[test]
    fn stale_host_is_frozen_not_instantly_drained() {
        let mut hosts = BTreeMap::new();
        hosts.insert("healthy".to_string(), resource_host(Some(10), 0));
        hosts.insert("stale".to_string(), resource_host(Some(10), 0));
        // max_scale_down_per_cycle = 1: the stale host may only lose one
        // worker this cycle no matter how large its raw (0) target is.
        let config = account(hosts, 10, 10, 1);
        let now = Utc::now();
        let mut resources = BTreeMap::new();
        resources.insert(
            "healthy".to_string(),
            snapshot(now, "healthy", true, 0.5, 8192, 16384),
        );
        resources.insert(
            "stale".to_string(),
            snapshot(now, "stale", false, 0.5, 8192, 16384),
        );
        let mut current = BTreeMap::new();
        current.insert("healthy".to_string(), 2u32);
        current.insert("stale".to_string(), 5u32);
        let placements = place("acct", 5, &config, &current, &resources, now).unwrap();
        let stale = find(&placements, "stale");
        assert!(!stale.eligible);
        assert_eq!(
            stale.target, 4,
            "stepped down by at most max_scale_down_per_cycle"
        );
        assert!(
            stale.target <= stale.current,
            "never gains while ineligible"
        );
    }

    /// plan.md §9.6/§22.7: per-cycle scale-up is bounded even when the raw
    /// target calls for a much larger jump.
    #[test]
    fn scale_up_is_bounded_by_max_scale_up_per_cycle() {
        let mut hosts = BTreeMap::new();
        hosts.insert("solo".to_string(), unconstrained_host(Some(20)));
        let config = account(hosts, 20, 2, 20);
        let now = Utc::now();
        let mut current = BTreeMap::new();
        current.insert("solo".to_string(), 0u32);
        let placements = place("acct", 20, &config, &current, &BTreeMap::new(), now).unwrap();
        assert_eq!(find(&placements, "solo").target, 2);
    }

    #[test]
    fn host_step_limits_override_account_limits_independently() {
        let mut constrained_up = unconstrained_host(Some(20));
        constrained_up.max_scale_up_per_cycle = Some(1);
        let mut constrained_down = unconstrained_host(Some(20));
        constrained_down.max_scale_down_per_cycle = Some(1);
        let mut hosts = BTreeMap::new();
        hosts.insert("up".to_string(), constrained_up);
        hosts.insert("down".to_string(), constrained_down);
        let config = account(hosts, 20, 4, 3);
        let now = Utc::now();

        let mut current = BTreeMap::new();
        current.insert("up".to_string(), 0);
        current.insert("down".to_string(), 9);
        let growing = place("acct", 10, &config, &current, &BTreeMap::new(), now).unwrap();
        assert_eq!(find(&growing, "up").target, 1);
        assert_eq!(find(&growing, "down").target, 8);

        current.insert("up".to_string(), 4);
        current.insert("down".to_string(), 4);
        let shrinking = place("acct", 0, &config, &current, &BTreeMap::new(), now).unwrap();
        // up has no host scale-down override and inherits the account's 3.
        assert_eq!(find(&shrinking, "up").target, 1);
        assert_eq!(find(&shrinking, "down").target, 3);
    }

    /// The pure function is deterministic given identical inputs.
    #[test]
    fn identical_inputs_produce_identical_outputs() {
        let mut hosts = BTreeMap::new();
        hosts.insert("a".to_string(), resource_host(Some(6), 512));
        hosts.insert("b".to_string(), resource_host(Some(6), 512));
        let config = account(hosts, 10, 5, 5);
        let now = Utc::now();
        let mut resources = BTreeMap::new();
        resources.insert("a".to_string(), snapshot(now, "a", true, 0.4, 4096, 8192));
        resources.insert("b".to_string(), snapshot(now, "b", true, 0.6, 6144, 8192));
        let mut current = BTreeMap::new();
        current.insert("a".to_string(), 1u32);
        current.insert("b".to_string(), 1u32);
        let first = place("acct", 6, &config, &current, &resources, now).unwrap();
        let second = place("acct", 6, &config, &current, &resources, now).unwrap();
        assert_eq!(first, second);
    }

    /// §22.4: mem_total_mb == 0 must not produce NaN/inf headroom.
    #[test]
    fn zero_mem_total_does_not_panic_or_propagate_nan() {
        let mut hosts = BTreeMap::new();
        hosts.insert("weird".to_string(), resource_host(Some(6), 0));
        let config = account(hosts, 8, 10, 10);
        let now = Utc::now();
        let mut resources = BTreeMap::new();
        resources.insert("weird".to_string(), snapshot(now, "weird", true, 0.9, 0, 0));
        let placements = place("acct", 5, &config, &BTreeMap::new(), &resources, now).unwrap();
        let p = &placements[0];
        assert_eq!(p.headroom, 0.0);
        assert!(!p.eligible);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        prop_compose! {
            fn arb_host()(
                ceiling in 0u32..12,
                cpu in 0.0f64..=1.0,
                mem_available in 0u64..65536,
                mem_total in 1u64..65536,
                mem_reserve_mb in 0u64..4096,
                fresh in any::<bool>(),
                current in 0u32..12,
            ) -> (u32, bool, f64, u64, u64, u64, u32) {
                (ceiling, fresh, cpu, mem_available.min(mem_total), mem_total, mem_reserve_mb, current)
            }
        }

        proptest! {
            /// The public placement result never exceeds the controller's
            /// account target when no existing workers need a step-limited
            /// scale-down. Step limits intentionally let a shrinking target
            /// lag behind current workers for several cycles.
            #[test]
            fn placed_total_never_exceeds_account_target(
                account_target in 0u32..50,
                specs in prop::collection::vec(arb_host(), 1..6),
            ) {
                let now = Utc::now();
                let mut hosts = BTreeMap::new();
                let mut resources = BTreeMap::new();
                for (index, (ceiling, fresh, cpu, mem_available, mem_total, mem_reserve_mb, _)) in specs.iter().enumerate() {
                    let id = format!("h{index}");
                    hosts.insert(id.clone(), resource_host(Some(*ceiling), *mem_reserve_mb));
                    resources.insert(
                        id.clone(),
                        snapshot(now, &id, *fresh, *cpu, *mem_available, *mem_total),
                    );
                }
                let config = account(hosts, 12, 100, 100);
                let placements = place(
                    "acct",
                    account_target,
                    &config,
                    &BTreeMap::new(),
                    &resources,
                    now,
                ).unwrap();
                let total: u32 = placements.iter().map(|placement| placement.target).sum();
                prop_assert!(total <= account_target, "placed {total} workers for target {account_target}");
            }

            /// A host's reported headroom comes only from its own resource
            /// snapshot. Marking a different host stale can change eligibility
            /// and distribution, but cannot inflate this host's own headroom.
            #[test]
            fn disabling_one_host_does_not_inflate_other_hosts_headroom(
                specs in prop::collection::vec(
                    (0.01f64..=1.0, 1u64..4096, 1u64..4096),
                    2..6,
                ),
                disabled_seed in any::<usize>(),
            ) {
                let now = Utc::now();
                let disabled_index = disabled_seed % specs.len();
                let mut hosts = BTreeMap::new();
                let mut resources = BTreeMap::new();
                for (index, (cpu, mem_available, mem_total)) in specs.iter().enumerate() {
                    let id = format!("h{index}");
                    hosts.insert(id.clone(), resource_host(Some(100), 0));
                    resources.insert(
                        id.clone(),
                        snapshot(now, &id, true, *cpu, (*mem_available).min(*mem_total), *mem_total),
                    );
                }
                let config = account(hosts, 100, 100, 100);
                let before = place(
                    "acct",
                    20,
                    &config,
                    &BTreeMap::new(),
                    &resources,
                    now,
                ).unwrap();
                resources.get_mut(&format!("h{disabled_index}")).unwrap().fresh = false;
                let after = place(
                    "acct",
                    20,
                    &config,
                    &BTreeMap::new(),
                    &resources,
                    now,
                ).unwrap();

                for index in 0..specs.len() {
                    if index == disabled_index {
                        continue;
                    }
                    let id = format!("h{index}");
                    prop_assert_eq!(find(&before, &id).headroom, find(&after, &id).headroom);
                    prop_assert!(find(&after, &id).eligible);
                }
            }

            /// Equal-headroom ties always hand remainder workers to the
            /// lexicographically smallest host keys, independent of map
            /// insertion order or repeated evaluation.
            #[test]
            fn equal_headroom_ties_are_deterministic_by_host_key(
                host_count in 2usize..8,
                account_target in 0u32..50,
            ) {
                let now = Utc::now();
                let mut forward_hosts = BTreeMap::new();
                for index in 0..host_count {
                    let id = format!("host-{index:02}");
                    forward_hosts.insert(id.clone(), unconstrained_host(Some(100)));
                }
                let mut reverse_hosts = BTreeMap::new();
                for index in (0..host_count).rev() {
                    let id = format!("host-{index:02}");
                    reverse_hosts.insert(id, unconstrained_host(Some(100)));
                }
                // Insert the same entries in reverse order to make the
                // canonical host-key tie-break observable at the API.
                let forward_config = account(forward_hosts, 100, 100, 100);
                let reverse_config = account(reverse_hosts, 100, 100, 100);
                let first = place(
                    "acct",
                    account_target,
                    &forward_config,
                    &BTreeMap::new(),
                    &BTreeMap::new(),
                    now,
                ).unwrap();
                let repeated = place(
                    "acct",
                    account_target,
                    &forward_config,
                    &BTreeMap::new(),
                    &BTreeMap::new(),
                    now,
                ).unwrap();
                let reordered = place(
                    "acct",
                    account_target,
                    &reverse_config,
                    &BTreeMap::new(),
                    &BTreeMap::new(),
                    now,
                ).unwrap();
                prop_assert_eq!(&first, &repeated);
                prop_assert_eq!(&first, &reordered);

                let base = account_target / host_count as u32;
                let remainder = account_target % host_count as u32;
                for (index, placement) in first.iter().enumerate() {
                    let expected = base + u32::from((index as u32) < remainder);
                    prop_assert_eq!(&placement.host_id, &format!("host-{index:02}"));
                    prop_assert_eq!(placement.target, expected);
                }
            }

            /// plan.md §22.7's central invariant: placement never asks for
            /// more workers, in aggregate, than the controller already
            /// authorized. Checked against the pre-step-limit distribution
            /// (`distribute`) directly, since step-limited targets are
            /// deliberately allowed to lag a *shrinking* total for a few
            /// cycles (§22.8), exactly as controller::evaluate's own
            /// stale/step-limited desired_workers can.
            #[test]
            fn distributed_total_never_exceeds_account_target(
                account_target in 0u32..50,
                specs in prop::collection::vec(arb_host(), 1..6),
            ) {
                let now = Utc::now();
                let mut headroom_of = BTreeMap::new();
                let mut ceiling_of = BTreeMap::new();
                let mut ids = Vec::new();
                let mut eligible = Vec::new();
                let host_names: Vec<String> = (0..specs.len()).map(|i| format!("h{i}")).collect();
                for (i, (ceiling, fresh, cpu, mem_available, mem_total, mem_reserve_mb, _current)) in specs.iter().enumerate() {
                    let id: &'static str = Box::leak(host_names[i].clone().into_boxed_str());
                    ids.push(id);
                    ceiling_of.insert(id, *ceiling);
                    let (is_fresh, headroom) = resource_headroom(
                        Some(&snapshot(now, id, *fresh, *cpu, *mem_available, *mem_total)),
                        *mem_reserve_mb,
                        300,
                        now,
                    );
                    headroom_of.insert(id, headroom);
                    if is_fresh && headroom > 0.0 {
                        eligible.push(id);
                    }
                }
                prop_assume!(!eligible.is_empty());
                let clamped = distribute(account_target, &eligible, &headroom_of, &ceiling_of, &ids);
                let total: u32 = clamped.values().sum();
                prop_assert!(total <= account_target);
            }

            /// A host absent from `eligible` never receives a positive raw
            /// share -- it can only ever hold or lose ground, never gain.
            #[test]
            fn ineligible_hosts_never_receive_a_positive_share(
                account_target in 0u32..50,
                specs in prop::collection::vec(arb_host(), 1..6),
            ) {
                let now = Utc::now();
                let mut headroom_of = BTreeMap::new();
                let mut ceiling_of = BTreeMap::new();
                let mut ids = Vec::new();
                let mut eligible = Vec::new();
                let host_names: Vec<String> = (0..specs.len()).map(|i| format!("h{i}")).collect();
                for (i, (ceiling, fresh, cpu, mem_available, mem_total, mem_reserve_mb, _current)) in specs.iter().enumerate() {
                    let id: &'static str = Box::leak(host_names[i].clone().into_boxed_str());
                    ids.push(id);
                    ceiling_of.insert(id, *ceiling);
                    let (is_fresh, headroom) = resource_headroom(
                        Some(&snapshot(now, id, *fresh, *cpu, *mem_available, *mem_total)),
                        *mem_reserve_mb,
                        300,
                        now,
                    );
                    headroom_of.insert(id, headroom);
                    if is_fresh && headroom > 0.0 {
                        eligible.push(id);
                    }
                }
                prop_assume!(!eligible.is_empty());
                let clamped = distribute(account_target, &eligible, &headroom_of, &ceiling_of, &ids);
                for id in &ids {
                    if !eligible.contains(id) {
                        prop_assert_eq!(clamped[id], 0);
                    }
                }
            }
        }
    }
}
