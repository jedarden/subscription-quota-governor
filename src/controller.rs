use crate::config::{AccountConfig, StaleBehavior, Strategy};
use crate::model::QuotaSnapshot;
use crate::state::AccountState;
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
        return Ok(Decision {
            account: account_name.to_owned(),
            observed_at: snapshot.observed_at,
            current_workers,
            desired_workers: apply_step_limits(raw, current_workers, config),
            stale: true,
            windows: Vec::new(),
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
                            let elapsed_hours = snapshot
                                .observed_at
                                .signed_duration_since(sample.observed_at)
                                .num_milliseconds()
                                as f64
                                / 3_600_000.0;
                            let per_worker = (window.used_fraction - sample.used_fraction)
                                / elapsed_hours
                                / f64::from(sample.workers);
                            if per_worker > 0.0 && per_worker.is_finite() {
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
                            } else {
                                (current_workers, "no_observed_burn")
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
    let ordinary_desired = decisions
        .iter()
        .map(|decision| decision.desired_workers)
        .min()
        .unwrap_or(current_workers);
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
    if bounded > current {
        bounded.min(current.saturating_add(fleet.max_scale_up_per_cycle))
    } else {
        bounded.max(current.saturating_sub(fleet.max_scale_down_per_cycle))
    }
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
                observer: WorkerObserverConfig::Static { workers: 4 },
                actuator: ActuatorConfig::None,
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
}
