use crate::config::{AccountConfig, StaleBehavior, Strategy};
use crate::model::{QuotaSnapshot, QuotaWindow};
use crate::state::{AccountState, WindowSample};
use anyhow::{bail, Result};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Decision {
    pub account: String,
    pub observed_at: DateTime<Utc>,
    pub current_workers: u32,
    pub desired_workers: u32,
    pub stale: bool,
    pub windows: Vec<WindowDecision>,
    /// The window whose raw desired count won the §9.6 multi-window
    /// arbitration (the minimum across `windows`), before banked-reset
    /// floors and step limits are applied. `None` for a stale decision,
    /// which observes no windows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding_window: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub banked_resets: Option<BankedResetDecision>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WindowDecision {
    pub id: String,
    pub used_fraction: f64,
    pub target_utilization: f64,
    pub resets_at: DateTime<Utc>,
    pub desired_workers: u32,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_burn_per_worker_hour: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BankedResetDecision {
    pub available_count: u64,
    pub governing_window: String,
    pub minimum_pace_multiplier: f64,
    pub required_burn_per_hour: f64,
    pub desired_workers: u32,
    /// A human should redeem one credit before resuming this weekly window.
    pub manual_redemption_recommended: bool,
    pub deadline_missed: bool,
    pub reason: String,
    pub known_expirations: Vec<DateTime<Utc>>,
}

pub fn evaluate(
    account_name: &str,
    config: &AccountConfig,
    snapshot: &QuotaSnapshot,
    prior: &AccountState,
    current_workers: u32,
    now: DateTime<Utc>,
) -> Result<Decision> {
    let fleet = &config.fleet;
    let age_seconds = now
        .signed_duration_since(snapshot.observed_at)
        .num_seconds()
        .max(0) as u64;
    let stale = !snapshot.fresh || age_seconds > config.utilization.stale_after_seconds;
    if stale {
        let raw = match config.utilization.stale_behavior {
            StaleBehavior::Hold => current_workers,
            StaleBehavior::MinWorkers => fleet.min_workers,
        };
        // §9.1: neither stale behavior may increase workers. apply_step_limits alone is
        // not enough here -- MinWorkers can raise `raw` above `current_workers` when the
        // fleet's configured floor exceeds the observed count, and its own upward step
        // allowance would then climb toward that floor. Capping at current_workers keeps
        // the downward step limit (still applied inside apply_step_limits) while making
        // the "never increase while stale" invariant hold unconditionally.
        return Ok(Decision {
            account: account_name.to_owned(),
            observed_at: snapshot.observed_at,
            current_workers,
            desired_workers: apply_step_limits(raw, current_workers, config).min(current_workers),
            stale: true,
            windows: Vec::new(),
            binding_window: None,
            banked_resets: None,
        });
    }

    let banked_available = config.banked_resets.enabled
        && snapshot
            .reset_credits
            .as_ref()
            .is_some_and(|credits| credits.available_count > 0);
    let mut decisions = Vec::new();
    for window in &snapshot.windows {
        let Some(policy) = config.utilization.policy_for(&window.id) else {
            continue;
        };
        // A banked-reset generation should be consumed before it is replaced.
        // Raising only weekly windows preserves ordinary policy for shorter
        // provider limits while preventing an account-level 90% target from
        // making a 100%-redemption threshold unreachable.
        let target = if banked_available && is_weekly_window(window.duration_minutes) {
            policy
                .target
                .max(config.banked_resets.redeem_at_utilization)
        } else {
            policy.target
        };
        let mut burn_per_worker = None;
        let (desired, reason) = if window.reached || window.used_fraction >= target {
            (fleet.min_workers, "target_reached")
        } else if window.resets_at <= now {
            (current_workers, "reset_due")
        } else {
            match policy.strategy {
                Strategy::CeilingOnly => (fleet.max_workers, "below_ceiling"),
                Strategy::LinearToReset => {
                    match prior.windows.get(&window.id).filter(|sample| {
                        sample.resets_at == window.resets_at
                            && snapshot
                                .observed_at
                                .signed_duration_since(sample.observed_at)
                                .num_seconds()
                                >= config.utilization.minimum_sample_seconds as i64
                            && window.used_fraction >= sample.used_fraction
                            && sample.workers > 0
                    }) {
                        Some(sample) => {
                            match estimate_burn_per_worker(sample, window, snapshot.observed_at) {
                                Some(per_worker) => {
                                    burn_per_worker = Some(per_worker);
                                    let remaining_hours = window
                                        .resets_at
                                        .signed_duration_since(now)
                                        .num_milliseconds()
                                        as f64
                                        / 3_600_000.0;
                                    let required_rate =
                                        (target - window.used_fraction) / remaining_hours;
                                    let workers = workers_for_rate(required_rate, per_worker);
                                    (workers, "paced_to_reset")
                                }
                                None => (current_workers, "no_observed_burn"),
                            }
                        }
                        None if current_workers == 0 => {
                            (fleet.bootstrap_workers, "bootstrap_burn_rate")
                        }
                        None => (current_workers, "learning_burn_rate"),
                    }
                }
            }
        };
        let desired = desired.clamp(fleet.min_workers, fleet.max_workers);
        decisions.push(WindowDecision {
            id: window.id.clone(),
            used_fraction: window.used_fraction,
            target_utilization: target,
            resets_at: window.resets_at,
            desired_workers: desired,
            reason: reason.to_owned(),
            observed_burn_per_worker_hour: burn_per_worker,
        });
    }
    if decisions.is_empty() {
        bail!("account {account_name}: no enabled quota windows were observed");
    }
    // §9.6: "the account result is the minimum of those counts." Ties fall to
    // the first matching window in snapshot order (Iterator::min_by_key's
    // documented tie-break), since the plan does not prescribe one.
    let binding = decisions
        .iter()
        .min_by_key(|decision| decision.desired_workers);
    let ordinary_desired = binding
        .map(|decision| decision.desired_workers)
        .unwrap_or(current_workers);
    let binding_window = binding.map(|decision| decision.id.clone());
    let banked_resets = banked_reset_decision(config, snapshot, &decisions, current_workers, now);
    let short_window_reached = decisions.iter().any(|decision| {
        decision.reason == "target_reached"
            && snapshot
                .windows
                .iter()
                .find(|window| window.id == decision.id)
                .is_some_and(|window| !is_weekly_window(window.duration_minutes))
    });
    let raw_desired = match &banked_resets {
        Some(plan) if !plan.manual_redemption_recommended && !short_window_reached => {
            ordinary_desired.max(plan.desired_workers)
        }
        _ => ordinary_desired,
    };
    Ok(Decision {
        account: account_name.to_owned(),
        observed_at: snapshot.observed_at,
        current_workers,
        desired_workers: apply_step_limits(raw_desired, current_workers, config),
        stale: false,
        windows: decisions,
        binding_window,
        banked_resets,
    })
}

fn banked_reset_decision(
    config: &AccountConfig,
    snapshot: &QuotaSnapshot,
    decisions: &[WindowDecision],
    current_workers: u32,
    now: DateTime<Utc>,
) -> Option<BankedResetDecision> {
    if !config.banked_resets.enabled {
        return None;
    }
    let credits = snapshot.reset_credits.as_ref()?;
    if credits.available_count == 0 {
        return None;
    }
    let window = snapshot
        .windows
        .iter()
        .filter(|window| is_weekly_window(window.duration_minutes))
        .max_by(|left, right| left.used_fraction.total_cmp(&right.used_fraction))?;
    let decision = decisions.iter().find(|decision| decision.id == window.id)?;
    let duration_hours = window.duration_minutes? as f64 / 60.0;
    let target = decision
        .target_utilization
        .max(config.banked_resets.redeem_at_utilization);
    let mut required_burn_per_hour =
        config.banked_resets.minimum_pace_multiplier * target / duration_hours;

    let mut known_expirations: Vec<_> = credits
        .credits
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|credit| credit.status == "available")
        .filter_map(|credit| credit.expires_at)
        .collect();
    known_expirations.sort();
    let safety = Duration::seconds(config.banked_resets.deadline_safety_seconds as i64);
    let mut deadline_missed = false;
    for (index, expiration) in known_expirations.iter().enumerate() {
        let deadline = *expiration - safety;
        let hours = deadline.signed_duration_since(now).num_milliseconds() as f64 / 3_600_000.0;
        let generations = (target - window.used_fraction).max(0.0) + index as f64 * target;
        if hours > 0.0 {
            required_burn_per_hour = required_burn_per_hour.max(generations / hours);
        } else if generations > 0.0 {
            deadline_missed = true;
        }
    }

    let manual_redemption_recommended =
        window.reached || window.used_fraction >= config.banked_resets.redeem_at_utilization;
    let (desired_workers, reason) = if manual_redemption_recommended {
        (
            config.fleet.min_workers,
            "weekly_window_awaiting_manual_redemption",
        )
    } else if deadline_missed {
        (
            config.fleet.max_workers,
            "banked_reset_expiry_deadline_missed",
        )
    } else if let Some(per_worker) = decision.observed_burn_per_worker_hour {
        (
            workers_for_rate(required_burn_per_hour, per_worker)
                .clamp(config.fleet.min_workers, config.fleet.max_workers),
            if known_expirations.is_empty() {
                "minimum_banked_reset_pace"
            } else {
                "banked_reset_expiry_pace"
            },
        )
    } else if current_workers == 0 {
        (
            config.fleet.bootstrap_workers,
            "bootstrap_banked_reset_burn_rate",
        )
    } else {
        (current_workers, "learning_banked_reset_burn_rate")
    };

    Some(BankedResetDecision {
        available_count: credits.available_count,
        governing_window: window.id.clone(),
        minimum_pace_multiplier: config.banked_resets.minimum_pace_multiplier,
        required_burn_per_hour,
        desired_workers,
        manual_redemption_recommended,
        deadline_missed,
        reason: reason.to_owned(),
        known_expirations,
    })
}

fn is_weekly_window(duration_minutes: Option<u64>) -> bool {
    duration_minutes.is_some_and(|minutes| minutes >= 6 * 24 * 60)
}

/// §9.4/§9.8 two-point burn-rate estimator: the change in `used_fraction`
/// per worker per hour between two same-generation samples.
///
/// Callers must only invoke this with a `sample` that already passed the
/// same-generation, minimum-sample-age, non-negative-delta, and
/// `workers > 0` checks (see the `prior.windows.get(...).filter(...)` call
/// site) -- those preconditions are asserted in debug builds, not
/// re-validated here.
///
/// The v0.1 baseline is this plain two-point ratio; the exact statistical
/// upgrade (robust regression, an explicit quantization-interval model, ...)
/// is deferred pending observation-mode evidence (§21). What this function
/// guarantees unconditionally, per §9.8, is the one property that does not
/// need that evidence: provider utilization is reported as a quantized
/// percentage, so two samples reporting an equal value are *censored* --
/// consistent with any true delta in `[0, one_quantization_step)`, never
/// proof of exactly zero consumption -- and must never be reported as a
/// learned rate of zero. Returning `None` ("no usable rate") rather than
/// `Some(0.0)` makes that a structural guarantee: a caller cannot mistake
/// censored data for confirmed idleness, because a confirmed rate of
/// precisely zero is not a value this function can ever produce.
fn estimate_burn_per_worker(
    sample: &WindowSample,
    window: &QuotaWindow,
    observed_at: DateTime<Utc>,
) -> Option<f64> {
    debug_assert!(window.used_fraction >= sample.used_fraction);
    debug_assert!(sample.workers > 0);
    let elapsed_hours = observed_at
        .signed_duration_since(sample.observed_at)
        .num_milliseconds() as f64
        / 3_600_000.0;
    let per_worker =
        (window.used_fraction - sample.used_fraction) / elapsed_hours / f64::from(sample.workers);
    (per_worker.is_finite() && per_worker > 0.0).then_some(per_worker)
}

fn workers_for_rate(required_rate: f64, per_worker_rate: f64) -> u32 {
    let ratio = required_rate / per_worker_rate;
    if !ratio.is_finite() {
        return u32::MAX;
    }
    let rounded = ratio.round();
    let workers = if (ratio - rounded).abs() < 1e-9 {
        rounded
    } else {
        ratio.ceil()
    };
    workers.max(0.0).min(f64::from(u32::MAX)) as u32
}

fn apply_step_limits(desired: u32, current: u32, config: &AccountConfig) -> u32 {
    let fleet = &config.fleet;
    let bounded = desired.clamp(fleet.min_workers, fleet.max_workers);
    let stepped = if bounded > current {
        bounded.min(current.saturating_add(fleet.max_scale_up_per_cycle))
    } else {
        bounded.max(current.saturating_sub(fleet.max_scale_down_per_cycle))
    };
    // A zero-sized step cap combined with `current` already sitting outside
    // [min_workers, max_workers] (e.g. after a fleet reconfiguration lowered
    // max_workers or raised min_workers below/above the live worker count)
    // would otherwise leave `stepped` outside the fleet bounds indefinitely.
    // The bounds invariant is unconditional (§17.2); re-clamping here is what
    // makes it hold even in that edge case, without weakening the step cap in
    // the ordinary case where `current` is already within bounds.
    stepped.clamp(fleet.min_workers, fleet.max_workers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use crate::model::{QuotaWindow, ResetCredit, ResetCreditsSnapshot};
    use crate::state::WindowSample;
    use chrono::Duration;
    use std::collections::BTreeMap;

    fn account() -> AccountConfig {
        AccountConfig {
            source: SourceConfig::NormalizedFile { path: "x".into() },
            fleet: FleetConfig {
                min_workers: 0,
                max_workers: 10,
                bootstrap_workers: 1,
                max_scale_up_per_cycle: 10,
                max_scale_down_per_cycle: 10,
                observer: Some(WorkerObserverConfig::Static { workers: 4 }),
                actuator: ActuatorConfig::None,
                observer_reconciliation: ObserverReconciliation::default(),
                hosts: None,
            },
            utilization: UtilizationConfig {
                target_utilization: Some(0.9),
                reserve_fraction: None,
                strategy: Strategy::LinearToReset,
                stale_after_seconds: 900,
                stale_behavior: StaleBehavior::Hold,
                minimum_sample_seconds: 60,
                windows: BTreeMap::new(),
            },
            banked_resets: BankedResetConfig::default(),
        }
    }

    #[test]
    fn chooses_most_conservative_window() {
        let now = Utc::now();
        let snapshot = QuotaSnapshot {
            observed_at: now,
            fresh: true,
            windows: vec![
                QuotaWindow {
                    id: "five_hour".into(),
                    used_fraction: 0.2,
                    resets_at: now + Duration::hours(2),
                    duration_minutes: Some(300),
                    reached: false,
                },
                QuotaWindow {
                    id: "weekly".into(),
                    used_fraction: 0.9,
                    resets_at: now + Duration::days(2),
                    duration_minutes: Some(10080),
                    reached: false,
                },
            ],
            reset_credits: None,
        };
        let decision = evaluate(
            "test",
            &account(),
            &snapshot,
            &AccountState::default(),
            4,
            now,
        )
        .unwrap();
        assert_eq!(decision.desired_workers, 0);
        assert_eq!(decision.binding_window.as_deref(), Some("weekly"));
    }

    #[test]
    fn stale_decision_has_no_binding_window() {
        let now = Utc::now();
        let snapshot = QuotaSnapshot {
            observed_at: now - Duration::hours(1),
            fresh: true,
            windows: vec![QuotaWindow {
                id: "weekly".into(),
                used_fraction: 0.1,
                resets_at: now + Duration::days(2),
                duration_minutes: None,
                reached: false,
            }],
            reset_credits: None,
        };
        let decision = evaluate(
            "test",
            &account(),
            &snapshot,
            &AccountState::default(),
            3,
            now,
        )
        .unwrap();
        assert!(decision.stale);
        assert_eq!(decision.binding_window, None);
    }

    #[test]
    fn binding_window_ties_favor_the_first_window_in_snapshot_order() {
        let now = Utc::now();
        let mut config = account();
        config.utilization.strategy = Strategy::CeilingOnly;
        config.utilization.target_utilization = Some(0.9);
        let snapshot = QuotaSnapshot {
            observed_at: now,
            fresh: true,
            windows: vec![
                QuotaWindow {
                    id: "first_at_target".into(),
                    used_fraction: 1.0,
                    resets_at: now + Duration::hours(2),
                    duration_minutes: Some(300),
                    reached: false,
                },
                QuotaWindow {
                    id: "second_at_target".into(),
                    used_fraction: 1.0,
                    resets_at: now + Duration::days(2),
                    duration_minutes: Some(10_080),
                    reached: false,
                },
            ],
            reset_credits: None,
        };
        let decision =
            evaluate("test", &config, &snapshot, &AccountState::default(), 4, now).unwrap();
        // Both windows tie at fleet.min_workers (target_reached); the first
        // window in snapshot order wins the tie.
        assert_eq!(decision.binding_window.as_deref(), Some("first_at_target"));
    }

    #[test]
    fn learns_per_worker_burn_and_paces() {
        let now = Utc::now();
        let reset = now + Duration::hours(8);
        let snapshot = QuotaSnapshot {
            observed_at: now,
            fresh: true,
            windows: vec![QuotaWindow {
                id: "weekly".into(),
                used_fraction: 0.5,
                resets_at: reset,
                duration_minutes: None,
                reached: false,
            }],
            reset_credits: None,
        };
        let mut prior = AccountState::default();
        prior.windows.insert(
            "weekly".into(),
            WindowSample {
                observed_at: now - Duration::hours(1),
                used_fraction: 0.4,
                resets_at: reset,
                workers: 2,
            },
        );
        let decision = evaluate("test", &account(), &snapshot, &prior, 2, now).unwrap();
        assert_eq!(decision.desired_workers, 1);
        assert_eq!(decision.windows[0].reason, "paced_to_reset");
    }

    #[test]
    fn stale_data_never_scales_up() {
        let now = Utc::now();
        let snapshot = QuotaSnapshot {
            observed_at: now - Duration::hours(1),
            fresh: true,
            windows: vec![QuotaWindow {
                id: "weekly".into(),
                used_fraction: 0.1,
                resets_at: now + Duration::days(2),
                duration_minutes: None,
                reached: false,
            }],
            reset_credits: None,
        };
        let decision = evaluate(
            "test",
            &account(),
            &snapshot,
            &AccountState::default(),
            3,
            now,
        )
        .unwrap();
        assert_eq!(decision.desired_workers, 3);
        assert!(decision.stale);
    }

    #[test]
    fn bootstraps_an_empty_fleet_to_learn_burn() {
        let now = Utc::now();
        let snapshot = QuotaSnapshot {
            observed_at: now,
            fresh: true,
            windows: vec![QuotaWindow {
                id: "weekly".into(),
                used_fraction: 0.1,
                resets_at: now + Duration::days(2),
                duration_minutes: None,
                reached: false,
            }],
            reset_credits: None,
        };
        let decision = evaluate(
            "test",
            &account(),
            &snapshot,
            &AccountState::default(),
            0,
            now,
        )
        .unwrap();
        assert_eq!(decision.desired_workers, 1);
        assert_eq!(decision.windows[0].reason, "bootstrap_burn_rate");
    }

    // --- §9.8: estimate_burn_per_worker cannot learn a zero rate from
    // censored (quantized) data ---

    #[test]
    fn estimate_burn_per_worker_never_reports_a_learned_zero_rate_for_a_censored_delta() {
        let now = Utc::now();
        let resets_at = now + Duration::hours(4);
        let sample = WindowSample {
            observed_at: now - Duration::hours(1),
            used_fraction: 0.42,
            resets_at,
            workers: 3,
        };
        // Identical to the prior sample: consistent with any true delta in
        // [0, one_quantization_step), not proof of exactly zero consumption.
        let window = QuotaWindow {
            id: "weekly".into(),
            used_fraction: 0.42,
            resets_at,
            duration_minutes: Some(10_080),
            reached: false,
        };
        assert_eq!(estimate_burn_per_worker(&sample, &window, now), None);
    }

    #[test]
    fn estimate_burn_per_worker_reports_the_expected_rate_for_a_real_delta() {
        let now = Utc::now();
        let resets_at = now + Duration::hours(4);
        let sample = WindowSample {
            observed_at: now - Duration::hours(2),
            used_fraction: 0.25,
            resets_at,
            workers: 2,
        };
        let window = QuotaWindow {
            id: "weekly".into(),
            used_fraction: 0.75,
            resets_at,
            duration_minutes: Some(10_080),
            reached: false,
        };
        // (0.75 - 0.25) / 2h / 2 workers = 0.125 per worker per hour.
        assert_eq!(estimate_burn_per_worker(&sample, &window, now), Some(0.125));
    }

    #[test]
    fn censored_zero_delta_holds_without_learning_a_zero_rate() {
        let now = Utc::now();
        let reset = now + Duration::hours(8);
        let snapshot = QuotaSnapshot {
            observed_at: now,
            fresh: true,
            windows: vec![QuotaWindow {
                id: "weekly".into(),
                used_fraction: 0.5,
                resets_at: reset,
                duration_minutes: None,
                reached: false,
            }],
            reset_credits: None,
        };
        let mut prior = AccountState::default();
        prior.windows.insert(
            "weekly".into(),
            WindowSample {
                observed_at: now - Duration::hours(1),
                used_fraction: 0.5,
                resets_at: reset,
                workers: 2,
            },
        );
        let decision = evaluate("test", &account(), &snapshot, &prior, 2, now).unwrap();
        assert_eq!(decision.windows[0].reason, "no_observed_burn");
        assert_eq!(decision.windows[0].observed_burn_per_worker_hour, None);
        assert_eq!(decision.desired_workers, 2);
    }

    #[test]
    fn censored_zero_delta_at_zero_workers_holds_at_zero_per_plan_9_4() {
        let now = Utc::now();
        let reset = now + Duration::hours(8);
        let snapshot = QuotaSnapshot {
            observed_at: now,
            fresh: true,
            windows: vec![QuotaWindow {
                id: "weekly".into(),
                used_fraction: 0.5,
                resets_at: reset,
                duration_minutes: None,
                reached: false,
            }],
            reset_credits: None,
        };
        let mut prior = AccountState::default();
        prior.windows.insert(
            "weekly".into(),
            WindowSample {
                observed_at: now - Duration::hours(1),
                used_fraction: 0.5,
                resets_at: reset,
                workers: 2,
            },
        );
        // §9.4: "a zero observed delta holds because quantized percentages
        // do not prove zero consumption" -- unconditionally, even though a
        // *missing* sample at zero workers would instead bootstrap.
        let decision = evaluate("test", &account(), &snapshot, &prior, 0, now).unwrap();
        assert_eq!(decision.windows[0].reason, "no_observed_burn");
        assert_eq!(decision.desired_workers, 0);
    }

    #[test]
    fn banked_reset_enforces_two_times_weekly_pace() {
        let now = Utc::now();
        let reset = now + Duration::days(6);
        let mut config = account();
        config.banked_resets.enabled = true;
        config.fleet.max_workers = 20;
        let snapshot = QuotaSnapshot {
            observed_at: now,
            fresh: true,
            windows: vec![QuotaWindow {
                id: "codex.secondary".into(),
                used_fraction: 0.11,
                resets_at: reset,
                duration_minutes: Some(10_080),
                reached: false,
            }],
            reset_credits: Some(ResetCreditsSnapshot {
                available_count: 1,
                credits: None,
            }),
        };
        let mut prior = AccountState::default();
        prior.windows.insert(
            "codex.secondary".into(),
            WindowSample {
                observed_at: now - Duration::hours(1),
                used_fraction: 0.10,
                resets_at: reset,
                workers: 1,
            },
        );

        let decision = evaluate("test", &config, &snapshot, &prior, 1, now).unwrap();
        let banked = decision.banked_resets.unwrap();
        assert_eq!(decision.windows[0].target_utilization, 1.0);
        assert_eq!(banked.reason, "minimum_banked_reset_pace");
        assert_eq!(banked.desired_workers, 2);
        assert_eq!(decision.desired_workers, 2);
    }

    #[test]
    fn credit_expiry_can_raise_the_banked_reset_pace() {
        let now = Utc::now();
        let reset = now + Duration::days(6);
        let mut config = account();
        config.banked_resets.enabled = true;
        config.fleet.max_workers = 20;
        let snapshot = QuotaSnapshot {
            observed_at: now,
            fresh: true,
            windows: vec![QuotaWindow {
                id: "codex.secondary".into(),
                used_fraction: 0.20,
                resets_at: reset,
                duration_minutes: Some(10_080),
                reached: false,
            }],
            reset_credits: Some(ResetCreditsSnapshot {
                available_count: 1,
                credits: Some(vec![ResetCredit {
                    id: "credit-1".into(),
                    reset_type: Some("weekly".into()),
                    status: "available".into(),
                    granted_at: None,
                    expires_at: Some(now + Duration::hours(24)),
                    title: None,
                    description: None,
                }]),
            }),
        };
        let mut prior = AccountState::default();
        prior.windows.insert(
            "codex.secondary".into(),
            WindowSample {
                observed_at: now - Duration::hours(1),
                used_fraction: 0.19,
                resets_at: reset,
                workers: 1,
            },
        );

        let decision = evaluate("test", &config, &snapshot, &prior, 1, now).unwrap();
        let banked = decision.banked_resets.unwrap();
        assert_eq!(banked.reason, "banked_reset_expiry_pace");
        assert_eq!(banked.desired_workers, 5);
        assert_eq!(decision.desired_workers, 5);
    }

    #[test]
    fn recommends_redemption_at_the_configured_threshold() {
        let now = Utc::now();
        let mut config = account();
        config.banked_resets.enabled = true;
        let snapshot = QuotaSnapshot {
            observed_at: now,
            fresh: true,
            windows: vec![QuotaWindow {
                id: "codex.secondary".into(),
                used_fraction: 1.0,
                resets_at: now + Duration::days(6),
                duration_minutes: Some(10_080),
                reached: true,
            }],
            reset_credits: Some(ResetCreditsSnapshot {
                available_count: 1,
                credits: None,
            }),
        };

        let decision =
            evaluate("test", &config, &snapshot, &AccountState::default(), 3, now).unwrap();
        let banked = decision.banked_resets.unwrap();
        assert!(banked.manual_redemption_recommended);
        assert_eq!(decision.desired_workers, 0);
    }

    #[test]
    fn missed_credit_deadline_uses_max_workers_and_serializes() {
        let now = Utc::now();
        let mut config = account();
        config.banked_resets.enabled = true;
        let snapshot = QuotaSnapshot {
            observed_at: now,
            fresh: true,
            windows: vec![QuotaWindow {
                id: "codex.primary".into(),
                used_fraction: 0.25,
                resets_at: now + Duration::days(6),
                duration_minutes: Some(10_080),
                reached: false,
            }],
            reset_credits: Some(ResetCreditsSnapshot {
                available_count: 1,
                credits: Some(vec![ResetCredit {
                    id: "credit-1".into(),
                    reset_type: Some("weekly".into()),
                    status: "available".into(),
                    granted_at: None,
                    expires_at: Some(now + Duration::hours(1)),
                    title: None,
                    description: None,
                }]),
            }),
        };

        let decision =
            evaluate("test", &config, &snapshot, &AccountState::default(), 3, now).unwrap();
        let banked = decision.banked_resets.as_ref().unwrap();
        assert!(banked.deadline_missed);
        assert_eq!(banked.desired_workers, config.fleet.max_workers);
        serde_json::to_string(&decision).unwrap();
    }

    // --- §9.2 policy resolution: table-driven target/reserve boundary tests ---
    //
    // Each case pins Strategy::CeilingOnly so the resolved target's
    // reached/not-reached outcome (fleet.max_workers vs fleet.min_workers) is
    // determined purely by policy resolution, independent of burn-rate
    // pacing (§9.4), which is out of scope for this boundary sweep.

    struct PolicyBoundaryCase {
        name: &'static str,
        account_target: Option<f64>,
        account_reserve: Option<f64>,
        window_override: Option<WindowPolicy>,
        used_fraction: f64,
        reached_flag: bool,
        expect_target: f64,
        expect_reason: &'static str,
    }

    #[test]
    fn policy_resolution_boundary_values() {
        let now = Utc::now();
        let resets_at = now + Duration::hours(4);
        let cases = [
            PolicyBoundaryCase {
                name: "target at maximum edge (1.0) reached exactly at full utilization",
                account_target: Some(1.0),
                account_reserve: None,
                window_override: None,
                used_fraction: 1.0,
                reached_flag: false,
                expect_target: 1.0,
                expect_reason: "target_reached",
            },
            PolicyBoundaryCase {
                name: "target at maximum edge (1.0) not reached just below full utilization",
                account_target: Some(1.0),
                account_reserve: None,
                window_override: None,
                used_fraction: 0.999_999_999,
                reached_flag: false,
                expect_target: 1.0,
                expect_reason: "below_ceiling",
            },
            PolicyBoundaryCase {
                name: "near-zero target not reached at zero utilization",
                account_target: Some(0.000_1),
                account_reserve: None,
                window_override: None,
                used_fraction: 0.0,
                reached_flag: false,
                expect_target: 0.000_1,
                expect_reason: "below_ceiling",
            },
            PolicyBoundaryCase {
                name: "zero utilization stays below a maximal target",
                account_target: Some(1.0),
                account_reserve: None,
                window_override: None,
                used_fraction: 0.0,
                reached_flag: false,
                expect_target: 1.0,
                expect_reason: "below_ceiling",
            },
            PolicyBoundaryCase {
                name: "reserve_fraction at minimum edge (0.0) converts to target 1.0 and reaches at full utilization",
                account_target: None,
                account_reserve: Some(0.0),
                window_override: None,
                used_fraction: 1.0,
                reached_flag: false,
                expect_target: 1.0,
                expect_reason: "target_reached",
            },
            PolicyBoundaryCase {
                name: "reserve_fraction at minimum edge (0.0) does not reach just below full utilization",
                account_target: None,
                account_reserve: Some(0.0),
                window_override: None,
                used_fraction: 0.999_999_999,
                reached_flag: false,
                expect_target: 1.0,
                expect_reason: "below_ceiling",
            },
            PolicyBoundaryCase {
                name: "window override target_utilization wins over the account default target",
                account_target: Some(0.5),
                account_reserve: None,
                window_override: Some(WindowPolicy {
                    target_utilization: Some(1.0),
                    ..Default::default()
                }),
                used_fraction: 1.0,
                reached_flag: false,
                expect_target: 1.0,
                expect_reason: "target_reached",
            },
            PolicyBoundaryCase {
                name: "window override reserve_fraction at minimum edge wins over the account default target",
                account_target: Some(0.5),
                account_reserve: None,
                window_override: Some(WindowPolicy {
                    reserve_fraction: Some(0.0),
                    ..Default::default()
                }),
                used_fraction: 1.0,
                reached_flag: false,
                expect_target: 1.0,
                expect_reason: "target_reached",
            },
            PolicyBoundaryCase {
                name: "window override with enabled=true and no target falls back to the account default",
                account_target: Some(0.9),
                account_reserve: None,
                window_override: Some(WindowPolicy {
                    enabled: Some(true),
                    ..Default::default()
                }),
                used_fraction: 0.9,
                reached_flag: false,
                expect_target: 0.9,
                expect_reason: "target_reached",
            },
            PolicyBoundaryCase {
                name: "provider-reported reached flag wins even far below the resolved target",
                account_target: Some(0.9),
                account_reserve: None,
                window_override: None,
                used_fraction: 0.1,
                reached_flag: true,
                expect_target: 0.9,
                expect_reason: "target_reached",
            },
        ];

        for case in cases {
            let mut windows = BTreeMap::new();
            if let Some(policy) = case.window_override {
                windows.insert("weekly".to_string(), policy);
            }
            let mut config = account();
            config.utilization.target_utilization = case.account_target;
            config.utilization.reserve_fraction = case.account_reserve;
            config.utilization.strategy = Strategy::CeilingOnly;
            config.utilization.windows = windows;

            let snapshot = QuotaSnapshot {
                observed_at: now,
                fresh: true,
                windows: vec![QuotaWindow {
                    id: "weekly".into(),
                    used_fraction: case.used_fraction,
                    resets_at,
                    duration_minutes: Some(10_080),
                    reached: case.reached_flag,
                }],
                reset_credits: None,
            };

            let decision = evaluate("test", &config, &snapshot, &AccountState::default(), 2, now)
                .unwrap_or_else(|err| panic!("case {:?}: evaluate failed: {err}", case.name));
            assert_eq!(
                decision.windows[0].target_utilization, case.expect_target,
                "case {:?}: resolved target mismatch",
                case.name
            );
            assert_eq!(
                decision.windows[0].reason, case.expect_reason,
                "case {:?}: reason mismatch",
                case.name
            );
            let expected_window_workers = if case.expect_reason == "target_reached" {
                config.fleet.min_workers
            } else {
                config.fleet.max_workers
            };
            assert_eq!(
                decision.windows[0].desired_workers, expected_window_workers,
                "case {:?}: per-window desired_workers mismatch",
                case.name
            );
        }
    }

    #[test]
    fn disabled_window_is_skipped_while_other_windows_are_still_evaluated() {
        let now = Utc::now();
        let mut windows = BTreeMap::new();
        windows.insert(
            "disabled_window".to_string(),
            WindowPolicy {
                enabled: Some(false),
                ..Default::default()
            },
        );
        let mut config = account();
        config.utilization.windows = windows;

        let snapshot = QuotaSnapshot {
            observed_at: now,
            fresh: true,
            windows: vec![
                QuotaWindow {
                    id: "disabled_window".into(),
                    used_fraction: 1.0,
                    resets_at: now + Duration::hours(1),
                    duration_minutes: Some(300),
                    reached: true,
                },
                QuotaWindow {
                    id: "weekly".into(),
                    used_fraction: 0.1,
                    resets_at: now + Duration::days(1),
                    duration_minutes: Some(10_080),
                    reached: false,
                },
            ],
            reset_credits: None,
        };

        let decision =
            evaluate("test", &config, &snapshot, &AccountState::default(), 2, now).unwrap();
        assert_eq!(decision.windows.len(), 1);
        assert_eq!(decision.windows[0].id, "weekly");
    }

    #[test]
    fn all_windows_disabled_fails_evaluation() {
        let now = Utc::now();
        let mut windows = BTreeMap::new();
        windows.insert(
            "weekly".to_string(),
            WindowPolicy {
                enabled: Some(false),
                ..Default::default()
            },
        );
        let mut config = account();
        config.utilization.windows = windows;

        let snapshot = QuotaSnapshot {
            observed_at: now,
            fresh: true,
            windows: vec![QuotaWindow {
                id: "weekly".into(),
                used_fraction: 0.5,
                resets_at: now + Duration::days(1),
                duration_minutes: Some(10_080),
                reached: false,
            }],
            reset_credits: None,
        };

        let error =
            evaluate("test", &config, &snapshot, &AccountState::default(), 2, now).unwrap_err();
        assert!(error.to_string().contains("no enabled quota windows"));
    }

    // --- §17.2 property tests: clamping and monotonic safety ---

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        prop_compose! {
            fn arb_fleet()(
                min_workers in 0u32..6,
                extra_for_max in 0u32..10,
                bootstrap_extra in 0u32..6,
                max_scale_up_per_cycle in 0u32..6,
                max_scale_down_per_cycle in 0u32..6,
            ) -> FleetConfig {
                let max_workers = min_workers + extra_for_max;
                FleetConfig {
                    min_workers,
                    max_workers,
                    bootstrap_workers: bootstrap_extra.min(max_workers),
                    max_scale_up_per_cycle,
                    max_scale_down_per_cycle,
                    observer: Some(WorkerObserverConfig::Static { workers: 0 }),
                    actuator: ActuatorConfig::None,
                    observer_reconciliation: ObserverReconciliation::default(),
                    hosts: None,
                }
            }
        }

        prop_compose! {
            fn arb_account()(
                fleet in arb_fleet(),
                target in 0.01f64..=1.0,
                ceiling_only in any::<bool>(),
                stale_after_seconds in 1u64..3600,
                minimum_sample_seconds in 1u64..600,
            ) -> AccountConfig {
                AccountConfig {
                    source: SourceConfig::NormalizedFile { path: "x".into() },
                    fleet,
                    utilization: UtilizationConfig {
                        target_utilization: Some(target),
                        reserve_fraction: None,
                        strategy: if ceiling_only {
                            crate::config::Strategy::CeilingOnly
                        } else {
                            crate::config::Strategy::LinearToReset
                        },
                        stale_after_seconds,
                        stale_behavior: StaleBehavior::Hold,
                        minimum_sample_seconds,
                        windows: BTreeMap::new(),
                    },
                    banked_resets: BankedResetConfig::default(),
                }
            }
        }

        fn single_window_snapshot(
            now: DateTime<Utc>,
            id: &str,
            used_fraction: f64,
            resets_in_hours: i64,
            reached: bool,
        ) -> QuotaSnapshot {
            QuotaSnapshot {
                observed_at: now,
                fresh: true,
                windows: vec![QuotaWindow {
                    id: id.into(),
                    used_fraction,
                    resets_at: now + Duration::hours(resets_in_hours),
                    duration_minutes: Some(300),
                    reached,
                }],
                reset_credits: None,
            }
        }

        proptest! {
            /// §17.2: "desired count is always within fleet bounds."
            #[test]
            fn desired_workers_always_within_fleet_bounds(
                config in arb_account(),
                used_fraction in 0.0f64..=1.0,
                current_workers in 0u32..20,
                reached in any::<bool>(),
                resets_in_hours in 1i64..200,
            ) {
                let now = Utc::now();
                let snapshot = single_window_snapshot(now, "weekly", used_fraction, resets_in_hours, reached);
                let decision = evaluate(
                    "prop",
                    &config,
                    &snapshot,
                    &AccountState::default(),
                    current_workers,
                    now,
                )
                .unwrap();
                prop_assert!(decision.desired_workers >= config.fleet.min_workers);
                prop_assert!(decision.desired_workers <= config.fleet.max_workers);
                for window in &decision.windows {
                    prop_assert!(window.desired_workers >= config.fleet.min_workers);
                    prop_assert!(window.desired_workers <= config.fleet.max_workers);
                }
            }

            /// §9.1 / §17.2: "stale decisions never exceed current workers" -- true for
            /// both StaleBehavior variants, regardless of how the fleet's min_workers
            /// floor relates to the currently observed worker count.
            #[test]
            fn stale_decision_never_exceeds_current_workers(
                mut config in arb_account(),
                current_workers in 0u32..20,
                stale_via_fresh_flag in any::<bool>(),
                extra_staleness_seconds in 0u64..10_000,
                use_min_workers_behavior in any::<bool>(),
                used_fraction in 0.0f64..=1.0,
            ) {
                config.utilization.stale_behavior = if use_min_workers_behavior {
                    StaleBehavior::MinWorkers
                } else {
                    StaleBehavior::Hold
                };
                let now = Utc::now();
                let observed_at = if stale_via_fresh_flag {
                    now
                } else {
                    now - Duration::seconds(
                        (config.utilization.stale_after_seconds + extra_staleness_seconds + 1) as i64,
                    )
                };
                let snapshot = QuotaSnapshot {
                    observed_at,
                    fresh: !stale_via_fresh_flag,
                    windows: vec![QuotaWindow {
                        id: "weekly".into(),
                        used_fraction,
                        resets_at: now + Duration::hours(4),
                        duration_minutes: Some(10_080),
                        reached: false,
                    }],
                    reset_credits: None,
                };
                let decision = evaluate(
                    "prop",
                    &config,
                    &snapshot,
                    &AccountState::default(),
                    current_workers,
                    now,
                )
                .unwrap();
                prop_assert!(decision.stale);
                prop_assert!(decision.desired_workers <= current_workers);
            }

            /// §17.2: "adding another enabled window cannot increase the raw account
            /// target." Compares the same account/current_workers against a snapshot
            /// with one window vs. the same snapshot plus a second enabled window;
            /// apply_step_limits is monotonic non-decreasing in its raw input for a
            /// fixed (current_workers, config), so this holds at the emitted
            /// desired_workers level too. banked_resets stays disabled (the default)
            /// so it cannot confound which window is binding.
            #[test]
            fn adding_an_enabled_window_never_raises_the_desired_workers(
                config in arb_account(),
                current_workers in 0u32..20,
                used_fraction_a in 0.0f64..=1.0,
                reached_a in any::<bool>(),
                resets_in_hours_a in 1i64..200,
                used_fraction_b in 0.0f64..=1.0,
                reached_b in any::<bool>(),
                resets_in_hours_b in 1i64..200,
            ) {
                prop_assert!(!config.banked_resets.enabled);
                let now = Utc::now();
                let window_a = QuotaWindow {
                    id: "w0".into(),
                    used_fraction: used_fraction_a,
                    resets_at: now + Duration::hours(resets_in_hours_a),
                    duration_minutes: Some(300),
                    reached: reached_a,
                };
                let window_b = QuotaWindow {
                    id: "w1".into(),
                    used_fraction: used_fraction_b,
                    resets_at: now + Duration::hours(resets_in_hours_b),
                    duration_minutes: Some(300),
                    reached: reached_b,
                };
                let snapshot_one = QuotaSnapshot {
                    observed_at: now,
                    fresh: true,
                    windows: vec![window_a.clone()],
                    reset_credits: None,
                };
                let snapshot_two = QuotaSnapshot {
                    observed_at: now,
                    fresh: true,
                    windows: vec![window_a, window_b],
                    reset_credits: None,
                };
                let decision_one = evaluate(
                    "prop",
                    &config,
                    &snapshot_one,
                    &AccountState::default(),
                    current_workers,
                    now,
                )
                .unwrap();
                let decision_two = evaluate(
                    "prop",
                    &config,
                    &snapshot_two,
                    &AccountState::default(),
                    current_workers,
                    now,
                )
                .unwrap();
                prop_assert!(decision_two.desired_workers <= decision_one.desired_workers);
            }
        }
    }

    // --- §17.5/§9.8: controller trace simulations (bursty, idle, reset-heavy) ---
    //
    // Unlike the single- and two-cycle unit tests above, these replay a full
    // week-long synthetic trace through `evaluate`, closing the feedback loop
    // the way `main::run_cycle` does: each cycle's `desired_workers` becomes
    // the next cycle's `current_workers`, and a non-stale snapshot is folded
    // into `AccountState` via `record` before the next cycle runs.
    mod trace_simulations {
        use super::*;
        use crate::testsupport::clock::FakeClock;

        /// Evaluates one simulated cycle exactly like `main::run_cycle`: computes
        /// the decision, and -- for a non-stale cycle only, matching production
        /// (a stale cycle updates only `last_target`, never `record`) -- folds
        /// the observed snapshot into `prior` using the just-decided
        /// `desired_workers` as the sample's worker count. The trace always
        /// simulates successful actuation, so this mirrors production's
        /// `actuated == true` path.
        fn step(
            config: &AccountConfig,
            prior: &mut AccountState,
            workers: u32,
            snapshot: &QuotaSnapshot,
            now: DateTime<Utc>,
        ) -> Decision {
            let decision = evaluate("trace", config, snapshot, prior, workers, now).unwrap();
            if !decision.stale {
                prior.record(snapshot, decision.desired_workers, decision.desired_workers);
            }
            decision
        }

        fn trace_account(target: f64, max_workers: u32, step_limit: u32) -> AccountConfig {
            let mut config = account();
            config.utilization.strategy = Strategy::LinearToReset;
            config.utilization.target_utilization = Some(target);
            config.fleet.max_workers = max_workers;
            config.fleet.max_scale_up_per_cycle = step_limit;
            config.fleet.max_scale_down_per_cycle = step_limit;
            config
        }

        /// §17.5: "oscillation remains bounded" -- re-asserted directly on every
        /// simulated cycle as a regression net, on top of the structural
        /// guarantee `apply_step_limits` already provides.
        fn assert_bounded_step(previous: u32, decision: &Decision, config: &AccountConfig) {
            let delta = i64::from(decision.desired_workers) - i64::from(previous);
            assert!(
                delta <= i64::from(config.fleet.max_scale_up_per_cycle),
                "scale-up step {delta} exceeded max_scale_up_per_cycle {}",
                config.fleet.max_scale_up_per_cycle
            );
            assert!(
                -delta <= i64::from(config.fleet.max_scale_down_per_cycle),
                "scale-down step {delta} exceeded max_scale_down_per_cycle {}",
                config.fleet.max_scale_down_per_cycle
            );
            assert!(decision.desired_workers >= config.fleet.min_workers);
            assert!(decision.desired_workers <= config.fleet.max_workers);
        }

        fn hours(value: f64) -> Duration {
            Duration::milliseconds((value * 3_600_000.0) as i64)
        }

        /// §17.5 idle scenario / §9.8: a genuinely silent account -- zero
        /// consumption for a full week, replayed as 40 cycles. Every cycle
        /// after the first observes an exact-zero delta, which
        /// `estimate_burn_per_worker` must treat as censored (consistent with
        /// any true delta in `[0, one_quantization_step)`, never confirmed
        /// zero consumption -- see its doc comment), not as a learned rate of
        /// zero that would otherwise leave the fleet stuck. The controller
        /// must settle at `bootstrap_workers` once and hold there for the
        /// entire trace: never scaling up further (nothing to pace to) and
        /// never scaling back down to `min_workers` on the mere absence of a
        /// usable rate.
        #[test]
        fn idle_trace_holds_at_bootstrap_workers_without_runaway_scale_up() {
            let clock = FakeClock::default();
            let config = trace_account(0.9, 20, 5);
            let resets_at = clock.now() + Duration::days(7);
            let cycle_hours = 24.0 * 7.0 / 40.0;

            let mut prior = AccountState::default();
            let mut workers = 0u32;
            for cycle in 0..40u32 {
                clock.advance(hours(cycle_hours));
                let snapshot = QuotaSnapshot {
                    observed_at: clock.now(),
                    fresh: true,
                    windows: vec![QuotaWindow {
                        id: "weekly".into(),
                        used_fraction: 0.0,
                        resets_at,
                        duration_minutes: Some(10_080),
                        reached: false,
                    }],
                    reset_credits: None,
                };
                let decision = step(&config, &mut prior, workers, &snapshot, clock.now());
                assert_bounded_step(workers, &decision, &config);
                assert_eq!(
                    decision.desired_workers, config.fleet.bootstrap_workers,
                    "cycle {cycle}: a genuinely idle account must settle at bootstrap_workers \
                     and never scale up or down from there"
                );
                workers = decision.desired_workers;
            }
        }

        /// §17.5 bursty scenario / §9.8: irregular consumption spikes against
        /// an otherwise silent baseline, plus one stale gap mid-trace.
        /// `used_fraction` is monotonic non-decreasing within the one
        /// generation this trace covers, so once a burst pushes it past the
        /// target, `target_reached` is structurally permanent for every
        /// remaining cycle -- the burst itself can overshoot the target (the
        /// v0.1 two-point estimator cannot see a burst coming; §9.8 lists
        /// exactly this as future estimator hardening), but the controller
        /// must never oscillate back up afterward, and the injected stale gap
        /// must never scale workers up regardless of where it lands.
        #[test]
        fn bursty_trace_settles_at_target_reached_and_never_scales_up_through_a_stale_gap() {
            let clock = FakeClock::default();
            let config = trace_account(0.9, 5, 3);
            let target = config.utilization.target_utilization.unwrap();
            let resets_at = clock.now() + Duration::days(7);
            let cycle_hours = 24.0 * 7.0 / 40.0;
            let stale_cycle = 6usize;

            let mut prior = AccountState::default();
            let mut workers = 0u32;
            let mut used_fraction = 0.0f64;
            let mut trace = Vec::new();
            for cycle in 0..40usize {
                clock.advance(hours(cycle_hours));
                let stale = cycle == stale_cycle;
                if !stale {
                    // A burst every fifth cycle; silent otherwise.
                    let rate = if cycle % 5 == 4 { 0.08 } else { 0.0 };
                    used_fraction = (used_fraction + workers as f64 * rate * cycle_hours).min(1.0);
                }
                let snapshot = QuotaSnapshot {
                    observed_at: clock.now(),
                    fresh: !stale,
                    windows: vec![QuotaWindow {
                        id: "weekly".into(),
                        used_fraction,
                        resets_at,
                        duration_minutes: Some(10_080),
                        reached: used_fraction >= 1.0,
                    }],
                    reset_credits: None,
                };
                let decision = step(&config, &mut prior, workers, &snapshot, clock.now());
                assert_bounded_step(workers, &decision, &config);
                assert!(used_fraction <= 1.0 + 1e-9);
                if stale {
                    assert!(decision.stale);
                    assert!(
                        decision.desired_workers <= workers,
                        "cycle {cycle}: a stale interval must never scale up"
                    );
                }
                trace.push((decision.clone(), used_fraction));
                workers = decision.desired_workers;
            }

            let crossing = trace
                .iter()
                .position(|(_, observed)| *observed >= target)
                .unwrap_or_else(|| {
                    panic!("bursty trace should cross the target at least once: {trace:#?}")
                });
            for (decision, observed) in &trace[crossing..] {
                assert!(*observed >= target - 1e-9);
                assert_eq!(decision.desired_workers, config.fleet.min_workers);
                if !decision.stale {
                    assert_eq!(decision.windows[0].reason, "target_reached");
                }
            }
        }

        /// §17.5 reset-heavy scenario / §9.3/§9.8: a short window resetting
        /// every 5 hours across a week-long trace (33 generations, 4 cycles
        /// each). `max_workers` is chosen low enough that even a full
        /// generation spent entirely at the ceiling cannot reach the target
        /// (`4 workers * 0.03/hr * 5h = 0.6 < 0.8`), so utilization never
        /// exceeding the target is a safety-scenario property, not a
        /// coincidence of tuning. Each generation must restart learning from
        /// scratch -- no sample from a prior generation may be extrapolated
        /// across the boundary (§9.3) -- and a reset the clock has reached
        /// but the provider has not yet published must hold rather than
        /// guess.
        #[test]
        fn reset_heavy_trace_restarts_learning_each_generation_and_holds_at_the_boundary() {
            let clock = FakeClock::default();
            let config = trace_account(0.8, 4, 3);
            let target = config.utilization.target_utilization.unwrap();
            let generation_hours = 5.0;
            let cycles_per_generation = 4;
            let cycle_gap_hours = generation_hours / f64::from(cycles_per_generation);
            let per_worker_rate = 0.03;

            let mut prior = AccountState::default();
            let mut workers = 0u32;

            for generation in 0..33u32 {
                let resets_at = clock.now() + hours(generation_hours);
                let mut used_fraction = 0.0f64;
                for cycle in 0..cycles_per_generation {
                    clock.advance(hours(cycle_gap_hours));
                    used_fraction = (used_fraction
                        + workers as f64 * per_worker_rate * cycle_gap_hours)
                        .min(1.0);
                    let snapshot = QuotaSnapshot {
                        observed_at: clock.now(),
                        fresh: true,
                        windows: vec![QuotaWindow {
                            id: "five_hour".into(),
                            used_fraction,
                            resets_at,
                            duration_minutes: Some(300),
                            reached: used_fraction >= 1.0,
                        }],
                        reset_credits: None,
                    };
                    let decision = step(&config, &mut prior, workers, &snapshot, clock.now());
                    assert_bounded_step(workers, &decision, &config);
                    assert!(
                        used_fraction <= target + 1e-9,
                        "generation {generation} cycle {cycle}: utilization {used_fraction} \
                         exceeded target {target}"
                    );
                    if generation > 0 && cycle == 0 {
                        assert_ne!(
                            decision.windows[0].reason, "paced_to_reset",
                            "generation {generation}: the first cycle of a new generation must \
                             not extrapolate the prior generation's rate across the boundary"
                        );
                    }
                    workers = decision.desired_workers;
                }

                // The clock has reached this generation's boundary, but the
                // "provider" has not yet published a new one -- §9.3 requires
                // holding, not extrapolating.
                clock.set(resets_at + Duration::seconds(1));
                let pending_rollover = QuotaSnapshot {
                    observed_at: clock.now(),
                    fresh: true,
                    windows: vec![QuotaWindow {
                        id: "five_hour".into(),
                        used_fraction,
                        resets_at,
                        duration_minutes: Some(300),
                        reached: used_fraction >= 1.0,
                    }],
                    reset_credits: None,
                };
                let decision = step(&config, &mut prior, workers, &pending_rollover, clock.now());
                assert_eq!(
                    decision.windows[0].reason, "reset_due",
                    "generation {generation}: a reset the clock has passed but the provider has \
                     not published must hold"
                );
                assert_eq!(
                    decision.desired_workers, workers,
                    "generation {generation}: reset_due must hold at the current worker count, \
                     not extrapolate"
                );
                workers = decision.desired_workers;
            }
        }
    }
}
