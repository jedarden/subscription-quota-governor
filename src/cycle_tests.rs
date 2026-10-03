//! WP5 cycle-level integration tests (plan.md §16 WP5, §17.4): exercise the
//! full per-account cycle -- source -> observer -> `controller::evaluate` ->
//! actuator -> `AccountState::record` -- driven entirely by the WP0/WP4
//! in-memory test doubles (`FakeSource`, `FakeObserver`, `FakeActuator`,
//! `FakeClock`). No network, no real file or child-process I/O anywhere in
//! this module: every adapter here is a scripted in-memory stand-in.
//!
//! This complements, rather than replaces, `main.rs`'s own `run_cycle`
//! tests, which exercise the same behavior through the real
//! `source`/`fleet` config-dispatched adapters (a nonexistent file/command
//! path, a real temp-file actuator, ...). `main::run_cycle` itself lives in
//! the `subgov` binary crate, not this library, so it cannot be called
//! directly from here; [`run_cycle`] below is a from-scratch mirror of its
//! per-account body (collect -> observe -> decide ->
//! actuate-unless-equal-or-observe-only -> record-unless-stale), built
//! against the same public `Observer`/`Actuator` trait boundary (WP4) so the
//! two stay behaviorally comparable.

use crate::config::{
    AccountConfig, ActuatorConfig, BankedResetConfig, FleetConfig, ObserverReconciliation,
    SourceConfig, StaleBehavior, Strategy, UtilizationConfig,
};
use crate::controller::{evaluate, Decision};
use crate::fleet::{Actuator, Observer};
use crate::model::{QuotaSnapshot, QuotaWindow};
use crate::state::AccountState;
use crate::testsupport::fake_fleet::{FakeActuator, FakeObserver};
use crate::testsupport::fake_source::FakeSource;
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

/// One account's fully in-memory fleet: a scripted source, a scripted
/// observer, and a recording actuator -- the injected stand-ins for the
/// three real adapters a production cycle wires together from config.
struct AccountFakes {
    config: AccountConfig,
    source: FakeSource,
    observer: FakeObserver,
    actuator: FakeActuator,
}

fn account(name: &str, config: AccountConfig) -> (String, AccountFakes) {
    (
        name.to_string(),
        AccountFakes {
            config,
            source: FakeSource::new(),
            observer: FakeObserver::new(),
            actuator: FakeActuator::new(),
        },
    )
}

/// Per-cycle results: each account that evaluated successfully gets its
/// `Decision`; each account whose source or observer failed is named in
/// `failed` instead of appearing in `decisions`.
#[derive(Default)]
struct CycleOutcome {
    decisions: BTreeMap<String, Decision>,
    failed: Vec<String>,
}

/// A from-scratch reimplementation of `main::run_cycle`'s per-account body,
/// generic over the injected `Observer`/`Actuator` trait boundary (WP4)
/// instead of config-dispatched adapters. One account's failure never stops
/// the rest, matching plan.md §16 WP5's "isolate account failures within a
/// cycle."
fn run_cycle(
    accounts: &BTreeMap<String, AccountFakes>,
    states: &mut BTreeMap<String, AccountState>,
    observe_only: bool,
    now: DateTime<Utc>,
) -> CycleOutcome {
    let mut outcome = CycleOutcome::default();
    for (name, fakes) in accounts {
        let result: Result<Decision> = (|| {
            let snapshot = fakes.source.collect()?;
            let workers = fakes.observer.current_workers()?;
            let prior = states.get(name).cloned().unwrap_or_default();
            let decision = evaluate(name, &fakes.config, &snapshot, &prior, workers, now)?;
            // plan.md §11.2: skip invocation when there is nothing to
            // reconcile, and never invoke at all in observe-only mode.
            let actuated = if observe_only || decision.desired_workers == workers {
                false
            } else {
                fakes.actuator.actuate(decision.desired_workers)?;
                true
            };
            let state = states.entry(name.clone()).or_default();
            if decision.stale {
                state.last_target = Some(decision.desired_workers);
            } else {
                let sample_workers = if actuated {
                    decision.desired_workers
                } else {
                    workers
                };
                state.record(&snapshot, sample_workers, decision.desired_workers);
            }
            Ok(decision)
        })();
        match result {
            Ok(decision) => {
                outcome.decisions.insert(name.clone(), decision);
            }
            Err(_) => outcome.failed.push(name.clone()),
        }
    }
    outcome
}

fn fleet_config(min_workers: u32, max_workers: u32) -> FleetConfig {
    FleetConfig {
        min_workers,
        max_workers,
        bootstrap_workers: min_workers.max(1).min(max_workers),
        max_scale_up_per_cycle: 10,
        max_scale_down_per_cycle: 10,
        observer: None,
        actuator: ActuatorConfig::None,
        observer_reconciliation: ObserverReconciliation::default(),
        hosts: None,
    }
}

fn account_config(strategy: Strategy, target: f64, fleet: FleetConfig) -> AccountConfig {
    AccountConfig {
        source: SourceConfig::NormalizedFile {
            path: "unused".into(),
        },
        fleet,
        utilization: UtilizationConfig {
            target_utilization: Some(target),
            reserve_fraction: None,
            strategy,
            stale_after_seconds: 900,
            stale_behavior: StaleBehavior::Hold,
            minimum_sample_seconds: 60,
            windows: BTreeMap::new(),
        },
        banked_resets: BankedResetConfig::default(),
    }
}

fn single_window_snapshot(
    observed_at: DateTime<Utc>,
    fresh: bool,
    used_fraction: f64,
    resets_at: DateTime<Utc>,
) -> QuotaSnapshot {
    QuotaSnapshot {
        observed_at,
        fresh,
        windows: vec![QuotaWindow {
            id: "quota".into(),
            used_fraction,
            resets_at,
            duration_minutes: Some(300),
            reached: used_fraction >= 1.0,
        }],
        reset_credits: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::clock::FakeClock;
    use chrono::Duration;

    #[test]
    fn full_observe_only_cycle_across_three_accounts_persists_observations_without_actuating() {
        let clock = FakeClock::default();
        let now = clock.now();
        let resets_at = now + Duration::hours(4);

        let mut accounts = BTreeMap::new();
        for (name, used_fraction, workers) in [("a", 0.1, 0u32), ("b", 1.0, 5u32), ("c", 0.5, 3u32)]
        {
            let (key, fakes) = account(
                name,
                account_config(Strategy::CeilingOnly, 0.9, fleet_config(0, 10)),
            );
            fakes
                .source
                .push_snapshot(single_window_snapshot(now, true, used_fraction, resets_at));
            fakes.observer.push_workers(workers);
            accounts.insert(key, fakes);
        }

        let mut states = BTreeMap::new();
        let outcome = run_cycle(&accounts, &mut states, true, now);

        assert!(outcome.failed.is_empty());
        assert_eq!(outcome.decisions["a"].desired_workers, 10);
        assert_eq!(outcome.decisions["b"].desired_workers, 0);
        assert_eq!(outcome.decisions["c"].desired_workers, 10);

        for (name, fakes) in &accounts {
            assert!(
                fakes.actuator.calls().is_empty(),
                "account {name}: observe-only must never actuate"
            );
        }

        // Observe-only still persists the *observed* worker count, not the
        // computed target, per plan.md's "persist observations without
        // changing targets."
        assert_eq!(states["a"].windows["quota"].workers, 0);
        assert_eq!(states["b"].windows["quota"].workers, 5);
        assert_eq!(states["c"].windows["quota"].workers, 3);
    }

    #[test]
    fn full_actuating_cycle_invokes_the_actuator_only_when_desired_differs_from_observed() {
        let clock = FakeClock::default();
        let now = clock.now();
        let resets_at = now + Duration::hours(4);

        let mut accounts = BTreeMap::new();
        for (name, used_fraction, workers) in [("a", 0.1, 0u32), ("b", 1.0, 5u32), ("c", 0.5, 3u32)]
        {
            let (key, fakes) = account(
                name,
                account_config(Strategy::CeilingOnly, 0.9, fleet_config(0, 10)),
            );
            fakes
                .source
                .push_snapshot(single_window_snapshot(now, true, used_fraction, resets_at));
            fakes.observer.push_workers(workers);
            accounts.insert(key, fakes);
        }
        // Already at its target (target_reached with min_workers == observed):
        // desired == observed, so this account must never see an actuator
        // call even in actuating mode.
        let (key, fakes) = account(
            "d",
            account_config(Strategy::CeilingOnly, 0.9, fleet_config(0, 10)),
        );
        fakes
            .source
            .push_snapshot(single_window_snapshot(now, true, 1.0, resets_at));
        fakes.observer.push_workers(0);
        accounts.insert(key, fakes);

        let mut states = BTreeMap::new();
        let outcome = run_cycle(&accounts, &mut states, false, now);

        assert!(outcome.failed.is_empty());
        assert_eq!(accounts["a"].actuator.calls(), vec![10]);
        assert_eq!(accounts["b"].actuator.calls(), vec![0]);
        assert_eq!(accounts["c"].actuator.calls(), vec![10]);
        assert!(
            accounts["d"].actuator.calls().is_empty(),
            "desired == observed must skip actuation"
        );

        // Once actuated, the recorded sample uses the *post-actuation*
        // worker count, not the pre-cycle observed one.
        assert_eq!(states["a"].windows["quota"].workers, 10);
        assert_eq!(states["b"].windows["quota"].workers, 0);
        assert_eq!(states["c"].windows["quota"].workers, 10);
        assert_eq!(states["d"].windows["quota"].workers, 0);
    }

    #[test]
    fn a_failing_source_or_observer_is_isolated_and_does_not_stop_the_other_accounts() {
        let clock = FakeClock::default();
        let now = clock.now();
        let resets_at = now + Duration::hours(4);

        let mut accounts = BTreeMap::new();

        let (key, fakes) = account(
            "broken_source",
            account_config(Strategy::CeilingOnly, 0.9, fleet_config(0, 10)),
        );
        fakes.source.push_error("simulated provider outage");
        accounts.insert(key, fakes);

        let (key, fakes) = account(
            "broken_observer",
            account_config(Strategy::CeilingOnly, 0.9, fleet_config(0, 10)),
        );
        fakes
            .source
            .push_snapshot(single_window_snapshot(now, true, 0.2, resets_at));
        fakes.observer.push_error("worker observer unreachable");
        accounts.insert(key, fakes);

        let (key, fakes) = account(
            "healthy",
            account_config(Strategy::CeilingOnly, 0.9, fleet_config(0, 10)),
        );
        fakes
            .source
            .push_snapshot(single_window_snapshot(now, true, 0.2, resets_at));
        fakes.observer.push_workers(1);
        accounts.insert(key, fakes);

        let mut states = BTreeMap::new();
        let outcome = run_cycle(&accounts, &mut states, false, now);

        assert_eq!(
            outcome.failed,
            vec!["broken_observer".to_string(), "broken_source".to_string()]
        );
        assert!(!outcome.decisions.contains_key("broken_source"));
        assert!(!outcome.decisions.contains_key("broken_observer"));
        assert_eq!(outcome.decisions["healthy"].desired_workers, 10);

        assert!(
            !states.contains_key("broken_source"),
            "a failed account must never advance state"
        );
        assert!(
            !states.contains_key("broken_observer"),
            "a failed account must never advance state"
        );
        assert_eq!(states["healthy"].windows["quota"].workers, 10);

        assert!(accounts["broken_source"].actuator.calls().is_empty());
        assert!(accounts["broken_observer"].actuator.calls().is_empty());
    }

    #[test]
    fn a_stale_snapshot_holds_and_never_advances_recorded_state() {
        let clock = FakeClock::default();
        let now = clock.now();
        let resets_at = now + Duration::hours(4);

        let mut accounts = BTreeMap::new();
        let (key, fakes) = account(
            "a",
            account_config(Strategy::CeilingOnly, 0.9, fleet_config(0, 10)),
        );
        // fresh: false forces staleness regardless of `observed_at`'s age.
        fakes
            .source
            .push_snapshot(single_window_snapshot(now, false, 0.1, resets_at));
        fakes.observer.push_workers(4);
        accounts.insert(key, fakes);

        let mut states = BTreeMap::new();
        let outcome = run_cycle(&accounts, &mut states, false, now);

        assert!(outcome.failed.is_empty());
        assert!(outcome.decisions["a"].stale);
        assert_eq!(
            outcome.decisions["a"].desired_workers, 4,
            "StaleBehavior::Hold must hold at the observed count"
        );
        assert!(
            accounts["a"].actuator.calls().is_empty(),
            "desired == observed, so a stale hold must never actuate"
        );
        assert_eq!(states["a"].last_target, Some(4));
        assert!(
            states["a"].windows.is_empty(),
            "a stale cycle must never record a new burn-rate sample"
        );
    }

    #[test]
    fn burn_rate_learned_in_one_cycle_paces_the_actuating_decision_in_the_next() {
        let clock = FakeClock::default();
        let resets_at = clock.now() + Duration::hours(8);

        let config = account_config(Strategy::LinearToReset, 0.9, fleet_config(0, 10));
        let mut accounts = BTreeMap::new();
        let (key, fakes) = account("acct", config);
        accounts.insert(key, fakes);

        let mut states = BTreeMap::new();

        // Cycle 1: no prior sample, so the controller holds at the observed
        // count while it learns (plan.md §9.4 "learning_burn_rate").
        accounts["acct"]
            .source
            .push_snapshot(single_window_snapshot(clock.now(), true, 0.1, resets_at));
        accounts["acct"].observer.push_workers(2);
        let outcome1 = run_cycle(&accounts, &mut states, false, clock.now());
        assert_eq!(
            outcome1.decisions["acct"].windows[0].reason,
            "learning_burn_rate"
        );
        assert_eq!(outcome1.decisions["acct"].desired_workers, 2);
        assert!(
            accounts["acct"].actuator.calls().is_empty(),
            "desired == observed on cycle 1"
        );
        assert_eq!(states["acct"].windows["quota"].workers, 2);

        // Cycle 2, one hour later: the (used_fraction, workers, elapsed)
        // delta from cycle 1's recorded sample now yields a real burn rate,
        // which paces this cycle down from 2 workers to 1.
        clock.advance(Duration::hours(1));
        accounts["acct"]
            .source
            .push_snapshot(single_window_snapshot(clock.now(), true, 0.3, resets_at));
        accounts["acct"].observer.push_workers(2);
        let outcome2 = run_cycle(&accounts, &mut states, false, clock.now());
        let decision2 = &outcome2.decisions["acct"];
        assert_eq!(decision2.windows[0].reason, "paced_to_reset");
        let observed_burn = decision2.windows[0]
            .observed_burn_per_worker_hour
            .expect("a real delta must yield an observed burn rate");
        assert!(
            (observed_burn - 0.1).abs() < 1e-9,
            "expected ~0.1 per worker per hour, got {observed_burn}"
        );
        assert_eq!(decision2.desired_workers, 1);
        assert_eq!(accounts["acct"].actuator.calls(), vec![1]);
        assert_eq!(
            states["acct"].windows["quota"].workers, 1,
            "the recorded sample must use the post-actuation worker count"
        );
    }
}
