//! Opt-in, bounded throughput experiments. No provider slot count is assumed.
use crate::config::AccountConfig;
use crate::controller::Decision;
use crate::model::{same_reset_generation, QuotaSnapshot};
use crate::state::{AccountState, SaturationProbe, SaturationState};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct SaturationDecision {
    pub reason: String,
    pub completed_requests_per_second: Option<f64>,
    pub upstream_concurrency_fraction: Option<f64>,
    pub target_workers: u32,
    pub probe_pending: bool,
}

/// Apply after quota evaluation and before the independent legacy brake.
/// Throughput can lift a learning/no-burn hold, but cannot lift real pacing,
/// reached quota, reset-due or stale bounds. Unknown pressure stays unknown.
pub fn apply_zai_saturation(
    decision: &mut Decision,
    config: &AccountConfig,
    snapshot: &QuotaSnapshot,
    prior: &AccountState,
    now: DateTime<Utc>,
) -> SaturationState {
    let policy = config
        .utilization
        .zai_saturation
        .as_ref()
        .expect("opt-in policy");
    let current = decision.current_workers;
    let mut state = prior.zai_saturation.clone().unwrap_or_default();
    let fraction = snapshot
        .saturation
        .as_ref()
        .and_then(|s| s.upstream_concurrency_fraction);
    // Learning holds are not measured quota limits. All other quota decisions
    // retain the existing arbitration and step limits, including exhaustion.
    let quota_hold_only = decision.windows.iter().all(|window| {
        matches!(
            window.reason.as_str(),
            "learning_burn_rate" | "no_observed_burn" | "bootstrap_burn_rate" | "below_ceiling"
        )
    });
    let quota_cap = if !decision.stale && quota_hold_only {
        config.fleet.max_workers
    } else {
        decision.desired_workers
    };
    // Continue an already requested drain even during signal loss; missing
    // evidence can never create or repeat an upward probe.
    let mut target = state
        .pending
        .as_ref()
        .map_or(current, |p| p.to_workers.min(current));
    let mut reason = "insufficient_signal";
    let mut rate = None;
    let reset = snapshot.windows.iter().any(|window| {
        prior.windows.get(&window.id).is_some_and(|last| {
            !same_reset_generation(last.resets_at, window.resets_at)
                || window.used_fraction < last.used_fraction
        })
    });
    let repeated = state
        .last_sample_at
        .is_some_and(|at| snapshot.observed_at <= at);
    let low_attempt_probe = state.pending.as_ref().is_some_and(|probe| {
        current == probe.to_workers
            && snapshot
                .eligible_backlog_capacity
                .is_some_and(|cap| cap >= current.max(probe.from_workers))
    });
    let valid = snapshot.saturation.as_ref().filter(|sample| {
        sample.validate().is_ok()
            && sample.window_seconds >= policy.minimum_window_seconds
            && sample.window_seconds <= i64::MAX as u64
            && (sample.attempts >= policy.minimum_attempts || low_attempt_probe)
    });

    if decision.stale
        || snapshot.observed_at > now
        || reset
        || decision
            .windows
            .iter()
            .any(|w| w.reason == "reset_due" || w.reason == "target_reached")
    {
        state = SaturationState::default();
        reason = "quota_or_freshness_bound";
    } else if current == 0
        && decision.unowned_workers == 0
        && snapshot
            .eligible_backlog_capacity
            .is_some_and(|cap| cap > 0)
        && config.fleet.max_workers > 0
        && quota_cap > 0
    {
        // A stopped fleet cannot manufacture throughput evidence. Retain the
        // existing bounded startup path, gated by explicit available work.
        target = config.fleet.bootstrap_workers.max(1);
        state = SaturationState::default();
        reason = "bootstrap_for_evidence";
    } else if state.last_sample_at.is_some_and(|at| {
        snapshot.observed_at.signed_duration_since(at).num_seconds()
            > config.utilization.stale_after_seconds as i64
    }) {
        state = SaturationState::default();
        reason = "signal_gap";
    } else if repeated {
        reason = "repeated_sample";
    } else if let Some(sample) = valid {
        let window_start = Duration::try_seconds(sample.window_seconds as i64)
            .and_then(|duration| snapshot.observed_at.checked_sub_signed(duration));
        let measured_rate = sample.completed_requests as f64 / sample.window_seconds as f64;
        rate = Some(measured_rate);
        state.last_sample_at = Some(snapshot.observed_at);
        if let Some(probe) = state.pending.clone() {
            if current != probe.to_workers {
                if current == probe.from_workers
                    || (current > probe.from_workers.min(probe.to_workers)
                        && current < probe.from_workers.max(probe.to_workers))
                {
                    // A requested drain or launch is still in progress. No new
                    // experiment and no changed baseline until it is observed.
                    target = probe.to_workers;
                    reason = "waiting_for_workers";
                    state.settled_at = None;
                } else {
                    // Manual/placement changes invalidate attribution.
                    state = SaturationState::default();
                    reason = "worker_count_changed";
                }
            } else if state.settled_at.is_none() || state.observed_workers != Some(current) {
                state.settled_at = Some(snapshot.observed_at);
                state.observed_workers = Some(current);
                reason = "probe_settling";
            } else if interval_ready(
                &state,
                window_start,
                snapshot.observed_at,
                policy.dwell_seconds,
            ) {
                let retained = measured_rate
                    >= state.plateau_rate.unwrap_or(probe.baseline_rate)
                        * (1.0 - policy.plateau_fraction);
                let useful_gain =
                    measured_rate > probe.baseline_rate * (1.0 + policy.plateau_fraction);
                state.pending = None;
                state.baseline_rate = Some(measured_rate);
                state.settled_at = Some(snapshot.observed_at);
                if probe.recovery {
                    if !state.shrinking {
                        state.plateau_rate = Some(measured_rate);
                    }
                    reason = "recovery_settled";
                } else if probe.to_workers < probe.from_workers && retained {
                    state.shrinking = true;
                    reason = "shrink_retained_throughput";
                } else if probe.to_workers > probe.from_workers && useful_gain {
                    state.plateau_rate = Some(measured_rate);
                    state.shrinking = fraction.is_some_and(|f| f >= policy.concurrency_fraction);
                    reason = "growth_improved_throughput";
                } else {
                    // Roll back both a failed shrink and an unproductive grow.
                    // Recovery itself must settle before any further experiment.
                    target = probe.from_workers;
                    state.pending = Some(SaturationProbe {
                        from_workers: current,
                        to_workers: target,
                        baseline_rate: measured_rate,
                        recovery: true,
                    });
                    // A failed shrink establishes the minimum near plateau;
                    // a failed grow starts a downward search at the old count.
                    state.shrinking = probe.to_workers > probe.from_workers;
                    state.next_probe_at =
                        Some(reprobe_at(snapshot.observed_at, policy.reprobe_seconds));
                    reason = "restore_previous_count";
                }
            } else {
                reason = "probe_dwell";
            }
        } else if state.observed_workers != Some(current) || state.baseline_rate.is_none() {
            state.observed_workers = Some(current);
            state.settled_at = Some(snapshot.observed_at);
            state.baseline_rate = Some(measured_rate);
            state.plateau_rate = Some(measured_rate);
            reason = "baseline";
        } else if interval_ready(
            &state,
            window_start,
            snapshot.observed_at,
            policy.dwell_seconds,
        ) && state
            .next_probe_at
            .is_none_or(|at| snapshot.observed_at >= at)
        {
            let baseline = measured_rate;
            state.baseline_rate = Some(baseline);
            let pressured = fraction.is_some_and(|f| f >= policy.concurrency_fraction);
            let shrinking = state.shrinking || pressured;
            let step = policy.step_workers;
            let candidate = if shrinking {
                current
                    .saturating_sub(step)
                    .max(if decision.unowned_workers == 0 {
                        config.fleet.min_workers.max(1)
                    } else {
                        config.fleet.min_workers
                    })
            } else {
                current.saturating_add(step).min(config.fleet.max_workers)
            };
            let backlog_ok = snapshot
                .eligible_backlog_capacity
                .is_some_and(|cap| cap >= candidate.max(current));
            if candidate != current
                && backlog_ok
                && candidate <= quota_cap
                && (shrinking || measured_rate > 0.0)
            {
                target = candidate;
                if !state.shrinking {
                    state.plateau_rate = Some(baseline);
                }
                state.pending = Some(SaturationProbe {
                    from_workers: current,
                    to_workers: candidate,
                    baseline_rate: baseline,
                    recovery: false,
                });
                state.settled_at = None;
                reason = if shrinking {
                    "probe_shrink"
                } else {
                    "probe_growth"
                };
            } else {
                reason = if !backlog_ok {
                    "insufficient_backlog"
                } else {
                    "worker_or_quota_bound"
                };
                // At a lower bound begin another upward search only after the
                // reprobe interval; avoid a permanent downward-only latch.
                if shrinking && candidate == current {
                    state.shrinking = false;
                    state.next_probe_at =
                        Some(reprobe_at(snapshot.observed_at, policy.reprobe_seconds));
                }
            }
        } else {
            reason = "baseline_dwell";
        }
    }
    let backlog_cap = snapshot.eligible_backlog_capacity.unwrap_or(current);
    target = target.min(backlog_cap).min(config.fleet.max_workers);
    target = target.min(current.saturating_add(config.fleet.max_scale_up_per_cycle));
    target = target.max(current.saturating_sub(config.fleet.max_scale_down_per_cycle));
    // Apply the quota result last: safety reductions must not be undone by an
    // experiment's recovery request, a configured floor or a pending launch.
    decision.desired_workers = target.min(quota_cap);
    if decision.desired_workers != target
        || state
            .pending
            .as_ref()
            .is_some_and(|p| p.to_workers > quota_cap || p.to_workers > backlog_cap)
    {
        state.pending = None;
        state.baseline_rate = None;
        state.settled_at = None;
        reason = "quota_or_backlog_bound";
    }
    if decision.unowned_workers > 0 {
        decision.desired_total = Some(
            decision
                .desired_workers
                .saturating_add(decision.unowned_workers),
        );
    }
    decision.saturation = Some(SaturationDecision {
        reason: reason.into(),
        completed_requests_per_second: rate,
        upstream_concurrency_fraction: fraction,
        target_workers: decision.desired_workers,
        probe_pending: state.pending.is_some(),
    });
    state
}

fn interval_ready(
    state: &SaturationState,
    window_start: Option<DateTime<Utc>>,
    at: DateTime<Utc>,
    dwell: u64,
) -> bool {
    state.settled_at.is_some_and(|settled| {
        window_start.is_some_and(|start| start >= settled)
            && at.signed_duration_since(settled).num_seconds() >= dwell as i64
    })
}

fn reprobe_at(at: DateTime<Utc>, seconds: u64) -> DateTime<Utc> {
    Duration::try_seconds(seconds as i64)
        .and_then(|duration| at.checked_add_signed(duration))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Strategy};
    use crate::controller::evaluate_pool;
    use crate::model::{BackpressureSnapshot, QuotaWindow, SaturationSnapshot};

    fn config() -> AccountConfig {
        let parsed: Config = serde_yaml::from_str(
            r#"
version: 1
accounts:
  zai:
    source: {type: normalized_file, path: /unused}
    fleet:
      max_workers: 40
      max_scale_up_per_cycle: 1
      max_scale_down_per_cycle: 1
      observer: {type: static, workers: 5}
    utilization:
      reserve_fraction: 0.1
      strategy: ceiling_only
      zai_saturation: {}
"#,
        )
        .unwrap();
        parsed.validate().unwrap();
        parsed.accounts["zai"].clone()
    }

    fn snapshot(t: i64, completed: u64) -> QuotaSnapshot {
        let now = DateTime::from_timestamp(1_800_000_000 + t, 0).unwrap();
        QuotaSnapshot {
            observed_at: now,
            fresh: true,
            windows: vec![QuotaWindow {
                id: "five_hour".into(),
                used_fraction: 0.1,
                resets_at: DateTime::from_timestamp(1_800_100_000, 0).unwrap(),
                duration_minutes: Some(300),
                reached: false,
            }],
            eligible_backlog_capacity: Some(40),
            reset_credits: None,
            backpressure: None,
            saturation: Some(SaturationSnapshot {
                completed_requests: completed,
                attempts: 1000,
                window_seconds: 300,
                upstream_concurrency_fraction: None,
            }),
        }
    }

    fn cycle(
        config: &AccountConfig,
        prior: &mut AccountState,
        current: u32,
        sample: &QuotaSnapshot,
    ) -> Decision {
        let mut decision =
            evaluate_pool("zai", config, sample, prior, current, 0, sample.observed_at).unwrap();
        prior.zai_saturation = Some(apply_zai_saturation(
            &mut decision,
            config,
            sample,
            prior,
            sample.observed_at,
        ));
        decision
    }

    fn reason(d: &Decision) -> &str {
        &d.saturation.as_ref().unwrap().reason
    }

    #[test]
    fn saturation_plateau_shrinks_then_recovers_smallest_useful_count() {
        let config = config();
        let mut state = AccountState::default();
        assert_eq!(
            cycle(&config, &mut state, 5, &snapshot(0, 600)).desired_workers,
            5
        );
        assert_eq!(
            cycle(&config, &mut state, 5, &snapshot(600, 600)).desired_workers,
            6
        );
        assert_eq!(
            reason(&cycle(&config, &mut state, 6, &snapshot(900, 600))),
            "probe_settling"
        );
        let d = cycle(&config, &mut state, 6, &snapshot(1500, 600));
        assert_eq!(reason(&d), "restore_previous_count");
        assert_eq!(d.desired_workers, 5);
        cycle(&config, &mut state, 5, &snapshot(1800, 600));
        assert_eq!(
            reason(&cycle(&config, &mut state, 5, &snapshot(2400, 600))),
            "recovery_settled"
        );
        assert_eq!(
            cycle(&config, &mut state, 5, &snapshot(2700, 600)).desired_workers,
            5
        );
        for t in [3300, 3900, 4500] {
            cycle(&config, &mut state, 5, &snapshot(t, 600));
        }
        let d = cycle(&config, &mut state, 5, &snapshot(5100, 600));
        assert_eq!(reason(&d), "probe_shrink");
        assert_eq!(d.desired_workers, 4);
        cycle(&config, &mut state, 4, &snapshot(5400, 600));
        assert_eq!(
            reason(&cycle(&config, &mut state, 4, &snapshot(6000, 590))),
            "shrink_retained_throughput"
        );
        assert_eq!(
            cycle(&config, &mut state, 4, &snapshot(6600, 590)).desired_workers,
            3
        );
        cycle(&config, &mut state, 3, &snapshot(6900, 450));
        let d = cycle(&config, &mut state, 3, &snapshot(7500, 450));
        assert_eq!(reason(&d), "restore_previous_count");
        assert_eq!(d.desired_workers, 4);
        cycle(&config, &mut state, 4, &snapshot(7800, 600));
        assert_eq!(
            reason(&cycle(&config, &mut state, 4, &snapshot(8400, 600))),
            "recovery_settled"
        );
        assert_eq!(
            cycle(&config, &mut state, 4, &snapshot(9000, 600)).desired_workers,
            4
        );
    }

    #[test]
    fn saturation_shrink_tolerance_is_relative_to_plateau_not_compounded() {
        let config = config();
        let mut state = AccountState::default();
        let mut sample = snapshot(0, 600);
        sample
            .saturation
            .as_mut()
            .unwrap()
            .upstream_concurrency_fraction = Some(0.2);
        cycle(&config, &mut state, 5, &sample);
        sample.observed_at += Duration::seconds(600);
        assert_eq!(cycle(&config, &mut state, 5, &sample).desired_workers, 4);
        cycle(&config, &mut state, 4, &snapshot(900, 580));
        assert_eq!(
            reason(&cycle(&config, &mut state, 4, &snapshot(1500, 580))),
            "shrink_retained_throughput"
        );
        assert_eq!(
            cycle(&config, &mut state, 4, &snapshot(2100, 580)).desired_workers,
            3
        );
        cycle(&config, &mut state, 3, &snapshot(2400, 560));
        let d = cycle(&config, &mut state, 3, &snapshot(3000, 560));
        assert_eq!(reason(&d), "restore_previous_count");
        assert_eq!(d.desired_workers, 4);
    }

    #[test]
    fn saturation_useful_growth_is_bounded_even_when_quota_requests_max() {
        let config = config();
        let mut state = AccountState::default();
        cycle(&config, &mut state, 5, &snapshot(0, 400));
        assert_eq!(
            cycle(&config, &mut state, 5, &snapshot(600, 400)).desired_workers,
            6
        );
        cycle(&config, &mut state, 6, &snapshot(900, 500));
        assert_eq!(
            reason(&cycle(&config, &mut state, 6, &snapshot(1500, 500))),
            "growth_improved_throughput"
        );
        assert_eq!(
            cycle(&config, &mut state, 6, &snapshot(2100, 500)).desired_workers,
            7
        );
    }

    #[test]
    fn saturation_provider_1302_prompts_shrink_generic_429_does_not() {
        for (pressure, target) in [(None, 6), (Some(0.2), 4)] {
            let config = config();
            let mut state = AccountState::default();
            let mut sample = snapshot(0, 600);
            sample.backpressure = Some(BackpressureSnapshot {
                upstream_429_fraction: Some(0.9),
                attempts: 1000,
                window_seconds: 300,
            });
            sample
                .saturation
                .as_mut()
                .unwrap()
                .upstream_concurrency_fraction = pressure;
            cycle(&config, &mut state, 5, &sample);
            sample.observed_at += Duration::seconds(600);
            assert_eq!(
                cycle(&config, &mut state, 5, &sample).desired_workers,
                target
            );
        }
    }

    #[test]
    fn saturation_missing_short_low_attempt_repeated_and_stale_hold() {
        for case in 0..7 {
            let config = config();
            let mut state = AccountState::default();
            cycle(&config, &mut state, 5, &snapshot(0, 600));
            let mut sample = snapshot(600, 600);
            match case {
                0 => sample.saturation = None,
                1 => sample.saturation.as_mut().unwrap().window_seconds = 30,
                2 => sample.saturation.as_mut().unwrap().attempts = 1,
                3 => sample.fresh = false,
                4 => sample.observed_at = snapshot(0, 600).observed_at,
                5 => sample.eligible_backlog_capacity = None,
                _ => sample.eligible_backlog_capacity = Some(5),
            }
            assert_eq!(
                cycle(&config, &mut state, 5, &sample).desired_workers,
                5,
                "case {case}"
            );
        }
    }

    #[test]
    fn saturation_pending_drain_waits_and_restart_preserves_experiment() {
        let config = config();
        let mut state = AccountState::default();
        let mut first = snapshot(0, 600);
        first
            .saturation
            .as_mut()
            .unwrap()
            .upstream_concurrency_fraction = Some(0.2);
        cycle(&config, &mut state, 5, &first);
        first.observed_at += Duration::seconds(600);
        assert_eq!(cycle(&config, &mut state, 5, &first).desired_workers, 4);
        state = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        let d = cycle(&config, &mut state, 5, &snapshot(900, 600));
        assert_eq!(reason(&d), "waiting_for_workers");
        assert_eq!(d.desired_workers, 4);
        assert_eq!(
            reason(&cycle(&config, &mut state, 4, &snapshot(1200, 600))),
            "probe_settling"
        );
        assert_eq!(
            reason(&cycle(&config, &mut state, 4, &snapshot(1500, 600))),
            "probe_dwell"
        );
        assert_eq!(
            reason(&cycle(&config, &mut state, 4, &snapshot(1800, 600))),
            "shrink_retained_throughput"
        );
    }

    #[test]
    fn saturation_zero_completion_regression_recovers_and_counter_reset_holds() {
        let config = config();
        let mut state = AccountState::default();
        cycle(&config, &mut state, 5, &snapshot(0, 600));
        cycle(&config, &mut state, 5, &snapshot(600, 600));
        cycle(&config, &mut state, 6, &snapshot(900, 0));
        assert_eq!(
            cycle(&config, &mut state, 6, &snapshot(1500, 0)).desired_workers,
            5
        );
        let mut reset = snapshot(1800, 600);
        reset.saturation = None;
        assert_eq!(cycle(&config, &mut state, 5, &reset).desired_workers, 5);
    }

    #[test]
    fn saturation_preserves_one_producing_worker_and_bootstraps_with_backlog() {
        let config = config();
        let mut state = AccountState::default();
        let mut sample = snapshot(0, 0);
        sample.saturation = None;
        assert_eq!(cycle(&config, &mut state, 0, &sample).desired_workers, 1);
        sample.eligible_backlog_capacity = None;
        assert_eq!(cycle(&config, &mut state, 0, &sample).desired_workers, 0);
        let mut sample = snapshot(0, 600);
        sample
            .saturation
            .as_mut()
            .unwrap()
            .upstream_concurrency_fraction = Some(0.8);
        cycle(&config, &mut state, 1, &sample);
        sample.observed_at += Duration::seconds(600);
        assert_eq!(cycle(&config, &mut state, 1, &sample).desired_workers, 1);
        sample.windows[0].reached = true;
        assert_eq!(cycle(&config, &mut state, 1, &sample).desired_workers, 0);
    }

    #[test]
    fn saturation_low_attempt_shrink_regression_restores_previous_count() {
        let config = config();
        let mut state = AccountState::default();
        let mut sample = snapshot(0, 600);
        sample
            .saturation
            .as_mut()
            .unwrap()
            .upstream_concurrency_fraction = Some(0.2);
        cycle(&config, &mut state, 5, &sample);
        sample.observed_at += Duration::seconds(600);
        assert_eq!(cycle(&config, &mut state, 5, &sample).desired_workers, 4);
        let mut sample = snapshot(900, 3);
        sample.saturation.as_mut().unwrap().attempts = 3;
        assert_eq!(
            reason(&cycle(&config, &mut state, 4, &sample)),
            "probe_settling"
        );
        sample.observed_at += Duration::seconds(600);
        let d = cycle(&config, &mut state, 4, &sample);
        assert_eq!(reason(&d), "restore_previous_count");
        assert_eq!(d.desired_workers, 5);
    }

    #[test]
    fn saturation_learning_can_probe_but_pacing_exhaustion_and_reset_remain_bounds() {
        let mut config = config();
        config.utilization.strategy = Strategy::LinearToReset;
        let mut state = AccountState::default();
        cycle(&config, &mut state, 5, &snapshot(0, 600));
        assert_eq!(
            cycle(&config, &mut state, 5, &snapshot(600, 600)).desired_workers,
            6
        );
        let mut sample = snapshot(900, 600);
        sample.windows[0].reached = true;
        assert_eq!(cycle(&config, &mut state, 5, &sample).desired_workers, 4);
        assert!(state.zai_saturation.as_ref().unwrap().pending.is_none());
        let mut sample = snapshot(1200, 600);
        sample.windows[0].resets_at = sample.observed_at;
        assert_eq!(cycle(&config, &mut state, 5, &sample).desired_workers, 5);
        let old = snapshot(0, 600);
        state.windows.insert(
            "five_hour".into(),
            crate::state::WindowSample {
                observed_at: old.observed_at,
                used_fraction: 0.0,
                resets_at: old.windows[0].resets_at,
                workers: 5,
            },
        );
        let sample = snapshot(1800, 600);
        let d = cycle(&config, &mut state, 5, &sample);
        assert!(
            d.desired_workers <= 5,
            "measured quota pacing remains restrictive"
        );
        let mut sample = snapshot(2100, 600);
        sample.windows[0].resets_at += Duration::hours(5);
        assert_eq!(
            reason(&cycle(&config, &mut state, 5, &sample)),
            "quota_or_freshness_bound"
        );
    }
}
