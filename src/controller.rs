use crate::config::{AccountConfig, StaleBehavior, Strategy};
use crate::model::{QuotaSnapshot, QuotaWindow};
use crate::state::{AccountState, AggregateBurnSample, WindowSample};
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
    /// Account-level burn measured over at least 24 hours and multiple
    /// quantized samples. This includes interactive Codex sessions.
    pub aggregate_burn_per_hour: Option<f64>,
    /// Estimated usage attributable to governed workers after the
    /// exogenous account component is removed.
    pub governed_worker_burn_per_hour: Option<f64>,
    pub exogenous_burn_per_hour: Option<f64>,
    /// Highest pace supported by the measured worker rate and worker cap.
    pub max_feasible_burn_per_hour: Option<f64>,
    pub feasible: bool,
    /// Worker ceiling derived from eligible backlog, or the current worker
    /// count when no backlog observation is available.
    pub eligible_backlog_capacity: u32,
    pub desired_workers: u32,
    /// A human should redeem one credit before resuming this weekly window.
    pub manual_redemption_recommended: bool,
    pub deadline_missed: bool,
    pub reason: String,
    pub known_expirations: Vec<DateTime<Utc>>,
    pub credit_deadlines: Vec<CreditDeadlineAssessment>,
    /// Newly crossed advisory thresholds to publish to the operator.
    pub advisories: Vec<CreditDeadlineAdvisory>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdvisorySeverity {
    Warn,
    Page,
    Infeasible,
}

impl AdvisorySeverity {
    pub fn rank(self) -> u8 {
        match self {
            Self::Warn => 1,
            Self::Page => 2,
            Self::Infeasible => 3,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CreditDeadlineAssessment {
    pub credit_id: String,
    pub expires_at: DateTime<Utc>,
    pub need: f64,
    pub required_burn_per_hour: Option<f64>,
    pub slack_hours: Option<f64>,
    pub severity: Option<AdvisorySeverity>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CreditDeadlineAdvisory {
    pub credit_id: String,
    pub expires_at: DateTime<Utc>,
    pub severity: AdvisorySeverity,
    pub slack_hours: f64,
    pub required_burn_per_hour: Option<f64>,
    pub max_feasible_burn_per_hour: Option<f64>,
    pub feasible: bool,
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
    let banked_resets =
        banked_reset_decision(config, snapshot, &decisions, prior, current_workers, now);
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
    let stepped_desired = apply_step_limits(raw_desired, current_workers, config);
    let desired_workers = banked_resets.as_ref().map_or(stepped_desired, |plan| {
        stepped_desired.min(plan.eligible_backlog_capacity)
    });
    Ok(Decision {
        account: account_name.to_owned(),
        observed_at: snapshot.observed_at,
        current_workers,
        desired_workers,
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
    prior: &AccountState,
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

    let mut known_credits: Vec<_> = credits
        .credits
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|credit| credit.status == "available")
        .filter_map(|credit| credit.expires_at.map(|expires_at| (expires_at, credit)))
        .collect();
    known_credits.sort_by_key(|(expires_at, credit)| (*expires_at, credit.id.clone()));
    let known_expirations = known_credits
        .iter()
        .map(|(expires_at, _)| *expires_at)
        .collect::<Vec<_>>();
    let safety_hours = config.banked_resets.deadline_safety_seconds as f64 / 3_600.0;
    let burn = estimate_aggregate_burn(
        prior.aggregate_burn_history.get(&window.id),
        window,
        snapshot.observed_at,
        current_workers,
    );
    let eligible_backlog_capacity = snapshot
        .eligible_backlog_capacity
        .unwrap_or(current_workers)
        .min(config.fleet.max_workers);
    let max_feasible_burn_per_hour = burn.as_ref().and_then(|burn| {
        burn.exogenous_burn_per_hour
            .zip(burn.per_worker_burn_per_hour)
            .map(|(exogenous, per_worker)| {
                exogenous + per_worker * f64::from(eligible_backlog_capacity)
            })
    });
    let mut deadline_missed = false;
    let mut credit_deadlines = Vec::with_capacity(known_credits.len());
    let mut advisories = Vec::new();
    for (index, (expiration, credit)) in known_credits.iter().enumerate() {
        let hours_to_expiry =
            expiration.signed_duration_since(now).num_milliseconds() as f64 / 3_600_000.0;
        let available_hours = hours_to_expiry - safety_hours;
        let need = (target - window.used_fraction).max(0.0) + index as f64 * target;
        let deadline_rate = (available_hours > 0.0).then_some(need / available_hours);
        if let Some(rate) = deadline_rate {
            required_burn_per_hour = required_burn_per_hour.max(rate);
        } else if need > 0.0 {
            deadline_missed = true;
        }

        let slack_hours = burn
            .as_ref()
            .filter(|burn| burn.aggregate_burn_per_hour > 0.0)
            .map(|burn| hours_to_expiry - safety_hours - need / burn.aggregate_burn_per_hour);
        let severity = slack_hours.and_then(advisory_severity);
        let rank_before = prior
            .credit_alerts
            .get(&credit.id)
            .filter(|state| state.resets_at == window.resets_at)
            .map(|state| state.severity)
            .unwrap_or(0);
        let required = deadline_rate;
        credit_deadlines.push(CreditDeadlineAssessment {
            credit_id: credit.id.clone(),
            expires_at: *expiration,
            need,
            required_burn_per_hour: required,
            slack_hours,
            severity,
        });
        if let (Some(severity), Some(slack_hours)) = (severity, slack_hours) {
            if severity.rank() > rank_before {
                advisories.push(CreditDeadlineAdvisory {
                    credit_id: credit.id.clone(),
                    expires_at: *expiration,
                    severity,
                    slack_hours,
                    required_burn_per_hour: required,
                    max_feasible_burn_per_hour,
                    feasible: required
                        .zip(max_feasible_burn_per_hour)
                        .is_some_and(|(required, maximum)| maximum >= required),
                });
            }
        }
    }

    let feasible =
        max_feasible_burn_per_hour.is_some_and(|maximum| maximum >= required_burn_per_hour);

    let manual_redemption_recommended =
        window.reached || window.used_fraction >= config.banked_resets.redeem_at_utilization;
    let (desired_workers, reason) = if manual_redemption_recommended {
        (
            config.fleet.min_workers,
            "weekly_window_awaiting_manual_redemption",
        )
    } else if deadline_missed {
        (
            eligible_backlog_capacity,
            "banked_reset_expiry_deadline_missed",
        )
    } else if let Some(burn) = &burn {
        if let (Some(per_worker), Some(exogenous)) = (
            burn.per_worker_burn_per_hour.filter(|rate| *rate > 0.0),
            burn.exogenous_burn_per_hour,
        ) {
            let worker_rate = (required_burn_per_hour - exogenous).max(0.0);
            let desired = workers_for_rate(worker_rate, per_worker).clamp(
                config.fleet.min_workers.min(eligible_backlog_capacity),
                eligible_backlog_capacity,
            );
            (
                desired,
                if known_expirations.is_empty() {
                    "minimum_banked_reset_pace"
                } else {
                    "banked_reset_expiry_pace"
                },
            )
        } else {
            (current_workers, "learning_banked_reset_worker_split")
        }
    } else if current_workers == 0 {
        (
            config
                .fleet
                .bootstrap_workers
                .min(eligible_backlog_capacity),
            "bootstrap_banked_reset_burn_rate",
        )
    } else {
        (
            current_workers.min(eligible_backlog_capacity),
            "learning_banked_reset_24h_burn_rate",
        )
    };

    Some(BankedResetDecision {
        available_count: credits.available_count,
        governing_window: window.id.clone(),
        minimum_pace_multiplier: config.banked_resets.minimum_pace_multiplier,
        required_burn_per_hour,
        aggregate_burn_per_hour: burn.as_ref().map(|burn| burn.aggregate_burn_per_hour),
        governed_worker_burn_per_hour: burn
            .as_ref()
            .and_then(|burn| burn.governed_worker_burn_per_hour),
        exogenous_burn_per_hour: burn.as_ref().and_then(|burn| burn.exogenous_burn_per_hour),
        max_feasible_burn_per_hour,
        feasible,
        eligible_backlog_capacity,
        desired_workers,
        manual_redemption_recommended,
        deadline_missed,
        reason: reason.to_owned(),
        known_expirations,
        credit_deadlines,
        advisories,
    })
}

fn advisory_severity(slack_hours: f64) -> Option<AdvisorySeverity> {
    if slack_hours < 0.0 {
        Some(AdvisorySeverity::Infeasible)
    } else if slack_hours < 12.0 {
        Some(AdvisorySeverity::Page)
    } else if slack_hours < 48.0 {
        Some(AdvisorySeverity::Warn)
    } else {
        None
    }
}

#[cfg(test)]
mod advisory_threshold_tests {
    use super::*;

    #[test]
    fn deadline_advisory_thresholds_are_strict_and_ordered() {
        assert_eq!(
            advisory_severity(-0.001),
            Some(AdvisorySeverity::Infeasible)
        );
        assert_eq!(advisory_severity(0.0), Some(AdvisorySeverity::Page));
        assert_eq!(advisory_severity(11.999), Some(AdvisorySeverity::Page));
        assert_eq!(advisory_severity(12.0), Some(AdvisorySeverity::Warn));
        assert_eq!(advisory_severity(47.999), Some(AdvisorySeverity::Warn));
        assert_eq!(advisory_severity(48.0), None);
    }
}

#[derive(Clone, Copy, Debug)]
struct BurnInterval {
    elapsed_hours: f64,
    burned_fraction: f64,
    governed_workers_x2: u32,
    split_interval: bool,
}

#[derive(Clone, Copy, Debug)]
struct AggregateBurnEstimate {
    aggregate_burn_per_hour: f64,
    exogenous_burn_per_hour: Option<f64>,
    governed_worker_burn_per_hour: Option<f64>,
    per_worker_burn_per_hour: Option<f64>,
}

/// Measure account burn over a 24-hour-or-longer baseline using every
/// quantized observation in the interval. Generation changes contribute the
/// new generation's observed usage, while same-generation decreases are
/// treated as censored/reset data and never as negative burn.
fn estimate_aggregate_burn(
    history: Option<&std::collections::VecDeque<AggregateBurnSample>>,
    current: &QuotaWindow,
    observed_at: DateTime<Utc>,
    governed_workers: u32,
) -> Option<AggregateBurnEstimate> {
    let mut samples = history
        .into_iter()
        .flat_map(|samples| samples.iter().cloned())
        .collect::<Vec<_>>();
    if samples
        .last()
        .is_none_or(|sample| sample.observed_at < observed_at)
    {
        samples.push(AggregateBurnSample {
            observed_at,
            used_fraction: current.used_fraction,
            resets_at: current.resets_at,
            governed_workers,
        });
    }
    samples.sort_by_key(|sample| sample.observed_at);
    samples.dedup_by_key(|sample| sample.observed_at);

    let cutoff = observed_at - Duration::hours(24);
    let first = samples
        .iter()
        .rposition(|sample| sample.observed_at <= cutoff)?;
    let samples = &samples[first..];
    if samples.len() < 3 {
        return None;
    }
    let elapsed_hours = samples
        .last()?
        .observed_at
        .signed_duration_since(samples[0].observed_at)
        .num_milliseconds() as f64
        / 3_600_000.0;
    if elapsed_hours < 24.0 {
        return None;
    }

    let mut intervals = Vec::new();
    for pair in samples.windows(2) {
        let [previous, next] = pair else {
            unreachable!()
        };
        let elapsed = next
            .observed_at
            .signed_duration_since(previous.observed_at)
            .num_milliseconds() as f64
            / 3_600_000.0;
        if elapsed <= 0.0 {
            continue;
        }
        let burned_fraction = if previous.resets_at == next.resets_at {
            (next.used_fraction - previous.used_fraction).max(0.0)
        } else {
            next.used_fraction.max(0.0)
        };
        intervals.push(BurnInterval {
            elapsed_hours: elapsed,
            burned_fraction,
            governed_workers_x2: previous.governed_workers + next.governed_workers,
            split_interval: previous.resets_at == next.resets_at,
        });
    }
    let measured = intervals
        .iter()
        .map(|interval| interval.burned_fraction)
        .sum::<f64>();
    if !measured.is_finite() || measured <= 0.0 {
        // A flat quantized series is censored evidence, not proof of a zero
        // burn rate, so callers must keep learning rather than divide by it.
        return None;
    }
    let aggregate = measured / elapsed_hours;
    let exogenous = estimate_exogenous_burn(&intervals, aggregate);
    let governed = exogenous.map(|exogenous| (aggregate - exogenous).max(0.0));
    let worker_hours = intervals
        .iter()
        .map(|interval| interval.elapsed_hours * f64::from(interval.governed_workers_x2) / 2.0)
        .sum::<f64>();
    let per_worker = governed
        .filter(|_| worker_hours > 0.0)
        .map(|governed| governed * elapsed_hours / worker_hours);
    Some(AggregateBurnEstimate {
        aggregate_burn_per_hour: aggregate,
        exogenous_burn_per_hour: exogenous,
        governed_worker_burn_per_hour: governed,
        per_worker_burn_per_hour: per_worker,
    })
}

/// Estimate the exogenous component from zero-worker intervals when they
/// exist. Otherwise use a robust line intercept across worker-count bands.
/// With only one nonzero worker count the split is unidentifiable, so return
/// `None` instead of attributing interactive usage to governed workers.
fn estimate_exogenous_burn(intervals: &[BurnInterval], aggregate: f64) -> Option<f64> {
    let mut bands: std::collections::BTreeMap<u32, (f64, f64)> = std::collections::BTreeMap::new();
    for interval in intervals.iter().filter(|interval| interval.split_interval) {
        let entry = bands.entry(interval.governed_workers_x2).or_default();
        entry.0 += interval.elapsed_hours;
        entry.1 += interval.burned_fraction;
    }
    if let Some((hours, usage)) = bands.get(&0) {
        return Some((usage / hours).clamp(0.0, aggregate));
    }
    let means = bands
        .iter()
        .filter_map(|(workers_x2, (hours, usage))| {
            (*hours > 0.0).then_some((f64::from(*workers_x2) / 2.0, usage / hours))
        })
        .collect::<Vec<_>>();
    let mut slopes = Vec::new();
    for left in 0..means.len() {
        for right in (left + 1)..means.len() {
            let dx = means[right].0 - means[left].0;
            if dx != 0.0 {
                let slope = (means[right].1 - means[left].1) / dx;
                if slope.is_finite() && slope > 0.0 {
                    slopes.push(slope);
                }
            }
        }
    }
    if slopes.is_empty() {
        return None;
    }
    slopes.sort_by(f64::total_cmp);
    let slope = slopes[slopes.len() / 2];
    let mut intercepts = means
        .iter()
        .map(|(workers, rate)| rate - slope * workers)
        .filter(|value| value.is_finite())
        .collect::<Vec<_>>();
    intercepts.sort_by(f64::total_cmp);
    Some(intercepts[intercepts.len() / 2].clamp(0.0, aggregate))
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
    use crate::state::{AggregateBurnSample, WindowSample};
    use chrono::Duration;
    use std::collections::{BTreeMap, VecDeque};

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

    fn add_banked_burn_history(
        prior: &mut AccountState,
        now: DateTime<Utc>,
        resets_at: DateTime<Utc>,
        final_used_fraction: f64,
        exogenous_per_hour: f64,
        governed_per_worker_hour: f64,
    ) {
        let baseline =
            final_used_fraction - 25.0 * exogenous_per_hour - 12.0 * governed_per_worker_hour;
        let points = [
            (-25, baseline, 0),
            (-13, baseline + 12.0 * exogenous_per_hour, 0),
            (-12, baseline + 13.0 * exogenous_per_hour, 1),
            (
                -1,
                baseline + 24.0 * exogenous_per_hour + 11.0 * governed_per_worker_hour,
                1,
            ),
        ];
        prior.aggregate_burn_history.insert(
            "codex.secondary".to_owned(),
            points
                .into_iter()
                .map(
                    |(hours, used_fraction, governed_workers)| AggregateBurnSample {
                        observed_at: now + Duration::hours(hours),
                        used_fraction,
                        resets_at,
                        governed_workers,
                    },
                )
                .collect::<VecDeque<_>>(),
        );
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
            eligible_backlog_capacity: None,
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
            eligible_backlog_capacity: None,
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
            eligible_backlog_capacity: None,
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
            eligible_backlog_capacity: None,
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
            eligible_backlog_capacity: None,
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
            eligible_backlog_capacity: None,
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
            eligible_backlog_capacity: None,
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
            eligible_backlog_capacity: None,
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
                used_fraction: 0.30,
                resets_at: reset,
                duration_minutes: Some(10_080),
                reached: false,
            }],
            eligible_backlog_capacity: Some(20),
            reset_credits: Some(ResetCreditsSnapshot {
                available_count: 1,
                credits: None,
            }),
        };
        let mut prior = AccountState::default();
        add_banked_burn_history(&mut prior, now, reset, 0.30, 0.001, 0.01);
        prior.windows.insert(
            "codex.secondary".into(),
            WindowSample {
                observed_at: now - Duration::hours(1),
                used_fraction: 0.29,
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

        let no_backlog_signal = QuotaSnapshot {
            eligible_backlog_capacity: None,
            ..snapshot.clone()
        };
        let held = evaluate("test", &config, &no_backlog_signal, &prior, 1, now).unwrap();
        assert_eq!(held.desired_workers, 1);
        assert_eq!(
            held.banked_resets.unwrap().eligible_backlog_capacity,
            1,
            "without a backlog signal the current fleet size is the scale-up cap"
        );

        let one_worker_backlog = QuotaSnapshot {
            eligible_backlog_capacity: Some(1),
            ..snapshot
        };
        let capped = evaluate("test", &config, &one_worker_backlog, &prior, 1, now).unwrap();
        let capped_banked = capped.banked_resets.unwrap();
        assert_eq!(capped.desired_workers, 1);
        assert_eq!(capped_banked.eligible_backlog_capacity, 1);
        assert!(!capped_banked.feasible);
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
            eligible_backlog_capacity: Some(20),
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
        add_banked_burn_history(&mut prior, now, reset, 0.20, 0.001, 0.01);
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
    fn codex_credit_replay_warns_before_expiry_and_rebases_after_out_of_cycle_reset() {
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct ReplayFixture {
            samples: Vec<AggregateBurnSample>,
        }

        fn snapshot(
            sample: &AggregateBurnSample,
            reset_credits: Option<ResetCreditsSnapshot>,
        ) -> QuotaSnapshot {
            QuotaSnapshot {
                observed_at: sample.observed_at,
                fresh: true,
                windows: vec![QuotaWindow {
                    id: "codex.secondary".into(),
                    used_fraction: sample.used_fraction,
                    resets_at: sample.resets_at,
                    duration_minutes: Some(10_080),
                    reached: false,
                }],
                eligible_backlog_capacity: Some(12),
                reset_credits,
            }
        }

        let replay: ReplayFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/codex-reset-credit-replay.json"
        ))
        .unwrap();
        let first_poll = replay
            .samples
            .iter()
            .position(|sample| sample.observed_at.to_rfc3339() == "2026-10-02T21:13:00+00:00")
            .unwrap();
        let reset_poll = replay
            .samples
            .iter()
            .position(|sample| sample.observed_at.to_rfc3339() == "2026-10-07T03:28:00+00:00")
            .unwrap();
        let mut config = account();
        config.banked_resets.enabled = true;
        config.fleet.max_workers = 12;
        let expiry_8a8b = "2026-10-05T04:19:00Z".parse::<DateTime<Utc>>().unwrap();
        let mut prior = AccountState::default();
        for sample in &replay.samples[..first_poll] {
            prior.record(
                &snapshot(sample, None),
                sample.governed_workers,
                sample.governed_workers,
            );
        }

        let first_sample = &replay.samples[first_poll];
        let first_snapshot = snapshot(
            first_sample,
            Some(ResetCreditsSnapshot {
                available_count: 1,
                credits: Some(vec![ResetCredit {
                    id: "8a8b".into(),
                    reset_type: Some("weekly".into()),
                    status: "available".into(),
                    granted_at: None,
                    expires_at: Some(expiry_8a8b),
                    title: None,
                    description: None,
                }]),
            }),
        );
        let first = evaluate(
            "codex",
            &config,
            &first_snapshot,
            &prior,
            first_sample.governed_workers,
            first_sample.observed_at,
        )
        .unwrap();
        let first_banked = first.banked_resets.as_ref().unwrap();
        assert!(first_banked.aggregate_burn_per_hour.unwrap() > 0.004);
        assert!(first_banked.aggregate_burn_per_hour.unwrap() < 0.006);
        assert_eq!(first_banked.advisories.len(), 1);
        assert_eq!(first_banked.advisories[0].credit_id, "8a8b");
        assert_eq!(
            first_banked.advisories[0].severity,
            AdvisorySeverity::Infeasible
        );
        assert!(first_banked.advisories[0].slack_hours < 0.0);
        assert!(first_banked.advisories[0]
            .required_burn_per_hour
            .is_some_and(|pace| pace > 0.02));
        assert!(first_banked.advisories[0]
            .max_feasible_burn_per_hour
            .is_some_and(|pace| pace < 0.01));
        assert!(!first_banked.advisories[0].feasible);
        assert!(
            expiry_8a8b
                .signed_duration_since(first_sample.observed_at)
                .num_hours()
                >= 48
        );

        prior.record(
            &first_snapshot,
            first_sample.governed_workers,
            first.desired_workers,
        );
        let first_generation = first_banked
            .credit_deadlines
            .first()
            .map(|_| first_sample.resets_at)
            .unwrap();
        prior.record_credit_alerts(
            first_generation,
            first_banked.credit_deadlines.iter().map(|deadline| {
                (
                    deadline.credit_id.clone(),
                    deadline.severity.map_or(0, AdvisorySeverity::rank),
                )
            }),
        );
        let repeated = evaluate(
            "codex",
            &config,
            &first_snapshot,
            &prior,
            first_sample.governed_workers,
            first_sample.observed_at,
        )
        .unwrap();
        assert!(repeated.banked_resets.unwrap().advisories.is_empty());
        for sample in &replay.samples[(first_poll + 1)..reset_poll] {
            prior.record(
                &snapshot(sample, None),
                sample.governed_workers,
                sample.governed_workers,
            );
        }

        let reset_sample = &replay.samples[reset_poll];
        let future_expiry_one = "2026-10-22T20:35:00Z".parse::<DateTime<Utc>>().unwrap();
        let future_expiry_two = "2026-10-29T18:53:00Z".parse::<DateTime<Utc>>().unwrap();
        let reset_snapshot = snapshot(
            reset_sample,
            Some(ResetCreditsSnapshot {
                available_count: 2,
                credits: Some(vec![
                    ResetCredit {
                        id: "credit-oct-22".into(),
                        reset_type: Some("weekly".into()),
                        status: "available".into(),
                        granted_at: None,
                        expires_at: Some(future_expiry_one),
                        title: None,
                        description: None,
                    },
                    ResetCredit {
                        id: "credit-oct-29".into(),
                        reset_type: Some("weekly".into()),
                        status: "available".into(),
                        granted_at: None,
                        expires_at: Some(future_expiry_two),
                        title: None,
                        description: None,
                    },
                ]),
            }),
        );
        let rebased = evaluate(
            "codex",
            &config,
            &reset_snapshot,
            &prior,
            reset_sample.governed_workers,
            reset_sample.observed_at,
        )
        .unwrap();
        let reset_banked = rebased.banked_resets.unwrap();
        assert_ne!(first_generation, reset_sample.resets_at);
        assert_eq!(reset_banked.credit_deadlines[0].need, 0.99);
        assert_eq!(reset_banked.credit_deadlines[1].need, 1.99);
        let expected_slack = future_expiry_one
            .signed_duration_since(reset_sample.observed_at)
            .num_milliseconds() as f64
            / 3_600_000.0
            - 6.0
            - 0.99 / reset_banked.aggregate_burn_per_hour.unwrap();
        assert!(
            (reset_banked.credit_deadlines[0].slack_hours.unwrap() - expected_slack).abs() < 1e-9
        );
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
            eligible_backlog_capacity: None,
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
            eligible_backlog_capacity: Some(10),
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
                eligible_backlog_capacity: None,
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
            eligible_backlog_capacity: None,
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
            eligible_backlog_capacity: None,
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
                eligible_backlog_capacity: None,
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
                    eligible_backlog_capacity: None,
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
                    eligible_backlog_capacity: None,
                    reset_credits: None,
                };
                let snapshot_two = QuotaSnapshot {
                    observed_at: now,
                    fresh: true,
                    windows: vec![window_a, window_b],
                    eligible_backlog_capacity: None,
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
                    eligible_backlog_capacity: None,
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
                    eligible_backlog_capacity: None,
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
                        eligible_backlog_capacity: None,
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
                    eligible_backlog_capacity: None,
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
