use crate::config::{AccountConfig, StaleBehavior, Strategy};
use crate::model::QuotaSnapshot;
use crate::state::AccountState;
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Decision {
    pub account: String,
    pub observed_at: DateTime<Utc>,
    pub current_workers: u32,
    pub desired_workers: u32,
    pub stale: bool,
    pub windows: Vec<WindowDecision>,
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
        });
    }

    let mut decisions = Vec::new();
    for window in &snapshot.windows {
        let Some(policy) = config.utilization.policy_for(&window.id) else {
            continue;
        };
        let mut burn_per_worker = None;
        let (desired, reason) = if window.reached || window.used_fraction >= policy.target {
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
                                    (policy.target - window.used_fraction) / remaining_hours;
                                let ratio = required_rate / per_worker;
                                let rounded = ratio.round();
                                let workers = if (ratio - rounded).abs() < 1e-9 {
                                    rounded
                                } else {
                                    ratio.ceil()
                                }
                                .max(0.0) as u32;
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
            target_utilization: policy.target,
            resets_at: window.resets_at,
            desired_workers: desired,
            reason: reason.to_owned(),
            observed_burn_per_worker_hour: burn_per_worker,
        });
    }
    if decisions.is_empty() {
        bail!("account {account_name}: no enabled quota windows were observed");
    }
    let raw_desired = decisions
        .iter()
        .map(|decision| decision.desired_workers)
        .min()
        .unwrap_or(current_workers);
    Ok(Decision {
        account: account_name.to_owned(),
        observed_at: snapshot.observed_at,
        current_workers,
        desired_workers: apply_step_limits(raw_desired, current_workers, config),
        stale: false,
        windows: decisions,
    })
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
    use crate::model::QuotaWindow;
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
}
