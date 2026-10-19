//! Read-only cached metrics. This module deliberately does not use source or fleet adapters.
use crate::config::{AccountConfig, ActuatorConfig, Config, SourceConfig, WorkerObserverConfig};
use crate::model::{
    same_reset_generation, BackpressureSnapshot, QuotaSnapshot, SaturationSnapshot,
};
use crate::state::{AccountState, AggregateBurnSample, WindowSample};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

mod ui;

#[derive(Debug, Serialize)]
pub struct Frame {
    pub observed_at: DateTime<Utc>,
    pub accounts: Vec<AccountCard>,
}

#[derive(Debug, Serialize)]
pub struct AccountCard {
    pub account: String,
    pub cache_status: String,
    pub readiness: String,
    pub reason: String,
    pub readiness_updated_at: Option<DateTime<Utc>>,
    pub controller_observed_at: Option<DateTime<Utc>>,
    pub controller_observed_workers: Option<u32>,
    pub target_workers: Option<u32>,
    pub windows: Vec<WindowCard>,
    pub cached_signals: Vec<String>,
    pub live_workers: Option<LiveWorkers>,
    pub pressure: Option<PressureCard>,
}

/// Full-proxy interval observations; worker counts are not concurrent requests.
#[derive(Debug, Serialize)]
pub struct PressureCard {
    pub observed_at: Option<DateTime<Utc>>,
    pub fresh: bool,
    pub saturation: Option<SaturationSnapshot>,
    pub backpressure: Option<BackpressureSnapshot>,
    pub minimum_attempts: u64,
    pub minimum_window_seconds: u64,
    pub concurrency_threshold: f64,
}

#[derive(Debug, Default, Serialize)]
pub struct LiveWorkers {
    pub live: u32,
    pub executing: u32,
    pub idle: u32,
    pub other: u32,
    pub stale_or_unverified: u32,
}

fn expand(path: &str) -> PathBuf {
    path.strip_prefix("~/")
        .and_then(|p| dirs::home_dir().map(|home| home.join(p)))
        .unwrap_or_else(|| PathBuf::from(path))
}

fn option_path(argv: &[String], flag: &str) -> Option<PathBuf> {
    argv.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| expand(&pair[1]))
}

/// Local process verification is deliberately independent of the controller's
/// last worker observation. No helper commands or process environments are read.
fn live_workers(account: &AccountConfig, now: DateTime<Utc>) -> (Option<LiveWorkers>, Vec<String>) {
    let argv = match &account.fleet.observer {
        Some(WorkerObserverConfig::Command { argv }) => Some(argv.as_slice()),
        _ => match &account.fleet.actuator {
            ActuatorConfig::Command { argv } => Some(argv.as_slice()),
            _ => None,
        },
    };
    let mut identities = Vec::<(String, String)>::new();
    let mut prefix = None;
    let mut heartbeat_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".needle/state/heartbeats");
    let mut stale_after = 60;
    let mut notes = Vec::new();
    let mut configured = false;
    if let Some(WorkerObserverConfig::NeedleStatus {
        agent,
        identifier_prefix,
        heartbeat_dir: dir,
        stale_after_seconds,
    }) = &account.fleet.observer
    {
        prefix = Some((agent.clone(), identifier_prefix.clone().unwrap_or_default()));
        heartbeat_dir = dir.clone();
        stale_after = *stale_after_seconds;
        configured = true;
    }
    if let Some(argv) = argv {
        if let Some(settings) = option_path(argv, "--settings").and_then(|p| read_json(&p)) {
            if let (Some(agent), Some(p)) =
                (settings["agent"].as_str(), settings["prefix"].as_str())
            {
                prefix = Some((agent.to_owned(), p.to_owned()));
                configured = true;
            }
            if let Some(dir) = settings["heartbeat_dir"].as_str() {
                heartbeat_dir = expand(dir);
            }
            if let Some(adopted) = settings["adopted_workers"].as_array() {
                for worker in adopted {
                    if let (Some(a), Some(w)) =
                        (worker["agent"].as_str(), worker["worker_id"].as_str())
                    {
                        identities.push((a.into(), w.into()));
                    }
                }
            }
            if let Some(work) = settings["work"].as_array() {
                let holds = work
                    .iter()
                    .filter_map(|entry| entry["external_hold"].as_str())
                    .filter(|hold| !hold.trim().is_empty())
                    .collect::<Vec<_>>();
                if !holds.is_empty() {
                    let native = holds
                        .iter()
                        .filter(|hold| hold.contains("internal_artifact"))
                        .count();
                    notes.push(format!(
                        "Reviewed work holds: {} native-policy, {} external (cached)",
                        native,
                        holds.len() - native
                    ));
                }
            }
            for (field, label) in [
                ("actuator_state", "Pending retirements"),
                ("demand_state", "Eligible backlog"),
            ] {
                if let Some(state) = settings[field].as_str().and_then(|p| read_json(&expand(p))) {
                    if field == "actuator_state" {
                        if let Some(retirees) = state["pending_retirees"].as_array() {
                            notes.push(format!("{label}: {} (cached)", retirees.len()));
                        }
                    } else if let Some(capacity) = state["eligible_backlog_capacity"].as_u64() {
                        notes.push(format!("{label}: {capacity} workers (cached demand)"));
                    }
                }
            }
        }
        if let Some(roster) = option_path(argv, "--roster").and_then(|p| read_json(&p)) {
            if let Some(workers) = roster["workers"].as_array() {
                configured = true;
                for worker in workers {
                    if let (Some(a), Some(w)) =
                        (worker["agent"].as_str(), worker["worker_id"].as_str())
                    {
                        identities.push((a.into(), w.into()));
                    }
                }
            }
            if let Some(state) = option_path(argv, "--state").and_then(|p| read_json(&p)) {
                if let Some(retirees) = state["pending_retirees"].as_array() {
                    notes.push(format!("Pending retirements: {} (cached)", retirees.len()));
                }
            }
        }
    }
    if !configured {
        return (None, notes);
    }
    let Ok(entries) = fs::read_dir(heartbeat_dir) else {
        return (None, notes);
    };
    let mut counts = LiveWorkers::default();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let Some(hb) = read_json(&entry.path()) else {
            continue;
        };
        let (Some(worker), Some(agent), Some(qualified)) = (
            hb["worker_id"].as_str(),
            hb["model"].as_str(),
            hb["qualified_id"].as_str(),
        ) else {
            continue;
        };
        if !identities.iter().any(|(a, w)| a == agent && w == worker)
            && !prefix
                .as_ref()
                .is_some_and(|(a, p)| a == agent && worker.starts_with(p))
        {
            continue;
        }
        let age = timestamp(hb.get("last_heartbeat").or_else(|| hb.get("timestamp")))
            .map(|t| age_seconds(now, t));
        let pid = hb["pid"].as_u64();
        let command = pid.and_then(|pid| fs::read(format!("/proc/{pid}/cmdline")).ok());
        let verified = command.as_ref().is_some_and(|bytes| {
            let args: Vec<_> = bytes
                .split(|b| *b == 0)
                .filter_map(|s| std::str::from_utf8(s).ok())
                .filter(|s| !s.is_empty())
                .collect();
            args.first().is_some_and(|p| {
                Path::new(p)
                    .file_name()
                    .is_some_and(|n| n == "needle" || n == "needle-stable")
            }) && args.contains(&"run")
                && args.windows(2).any(|p| p == ["--agent", agent])
                && args.windows(2).any(|p| p == ["--identifier", worker])
        });
        if qualified != format!("{agent}-{worker}")
            || entry.path().file_stem().is_none_or(|s| s != qualified)
            || !age.is_some_and(|s| s >= 0 && (s as u64) < stale_after)
            || !verified
        {
            counts.stale_or_unverified += 1;
            continue;
        }
        counts.live += 1;
        if hb["state"].as_str() == Some("EXECUTING") {
            counts.executing += 1;
        } else if hb["is_idle"].as_bool() == Some(true)
            && hb["current_task"].is_null()
            && hb["current_bead"].is_null()
        {
            counts.idle += 1;
        } else {
            counts.other += 1;
        }
    }
    (Some(counts), notes)
}

#[derive(Debug, Serialize)]
pub struct WindowCard {
    pub id: String,
    pub observed_at: DateTime<Utc>,
    pub fresh: bool,
    pub used_percent: f64,
    pub target_percent: f64,
    pub configured_target_percent: f64,
    pub headroom_percentage_points: f64,
    pub reserve_percent: f64,
    pub resets_at: DateTime<Utc>,
    pub consumption_percentage_points_per_hour: Option<f64>,
    pub rate_status: String,
    pub baseline_seconds: Option<i64>,
    pub lookback_seconds: u64,
    pub expected_remaining_percent_at_reset: Option<f64>,
    /// Conditional time at the measured pace, before applying any future reset.
    pub projected_empty_at: Option<DateTime<Utc>>,
    pub projection_assumption: String,
    pub history: Vec<HistoryPoint>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HistoryPoint {
    pub observed_at: DateTime<Utc>,
    pub used_percent: f64,
}

/// Strip control characters, including complete ANSI CSI/OSC sequences, before rendering.
fn clean(value: &str) -> String {
    let mut result = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    let mut escape = false;
                    for c in chars.by_ref() {
                        if c == '\u{7}' || (escape && c == '\\') {
                            break;
                        }
                        escape = c == '\u{1b}';
                    }
                }
                _ => {}
            }
        } else if !c.is_control() {
            result.push(if c.is_ascii() { c } else { '?' });
        }
    }
    result.chars().take(200).collect()
}

fn read_json(path: &Path) -> Option<Value> {
    // Bound cache reads; damaged or unexpectedly large caches stay unknown.
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 16 * 1024 * 1024 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value?.as_str()?.parse().ok()
}

fn fraction(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn valid_sample(sample: &WindowSample) -> bool {
    fraction(sample.used_fraction)
}

fn age_seconds(now: DateTime<Utc>, then: DateTime<Utc>) -> i64 {
    now.signed_duration_since(then).num_seconds()
}

fn compatible_history(
    state: &AccountState,
    id: &str,
    current: &WindowSample,
    now: DateTime<Utc>,
    lookback: u64,
    stale_after: u64,
) -> Vec<AggregateBurnSample> {
    let mut samples = state
        .aggregate_burn_history
        .get(id)
        .map(|s| s.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    if let Some(history) = state.history.get(id) {
        samples.extend(history.samples.iter().map(|s| AggregateBurnSample {
            observed_at: s.observed_at,
            used_fraction: s.used_fraction,
            resets_at: history.resets_at,
            governed_workers: s.workers,
        }));
    }
    samples.push(AggregateBurnSample {
        observed_at: current.observed_at,
        used_fraction: current.used_fraction,
        resets_at: current.resets_at,
        governed_workers: current.workers,
    });
    samples.sort_by_key(|s| s.observed_at);
    samples.dedup_by(|a, b| {
        a.observed_at == b.observed_at
            && a.used_fraction == b.used_fraction
            && same_reset_generation(a.resets_at, b.resets_at)
    });
    let mut compatible = Vec::new();
    let mut previous: Option<&AggregateBurnSample> = None;
    for sample in &samples {
        if sample.observed_at > current.observed_at {
            continue;
        }
        if age_seconds(now, sample.observed_at) < 0
            || age_seconds(current.observed_at, sample.observed_at) as u64 > lookback
            || !fraction(sample.used_fraction)
            || !same_reset_generation(sample.resets_at, current.resets_at)
        {
            compatible.clear();
            previous = None;
            continue;
        }
        if let Some(prev) = previous {
            let gap = age_seconds(sample.observed_at, prev.observed_at);
            if gap <= 0 || gap as u64 > stale_after || sample.used_fraction < prev.used_fraction {
                compatible.clear();
            }
        }
        compatible.push(sample.clone());
        previous = Some(sample);
    }
    compatible
}

fn measured_rate(
    state: &AccountState,
    id: &str,
    current: &WindowSample,
    now: DateTime<Utc>,
    lookback: u64,
    stale_after: u64,
    minimum: u64,
) -> (Option<f64>, Option<i64>, &'static str) {
    let samples = compatible_history(state, id, current, now, lookback, stale_after);
    let Some(base) = samples.first() else {
        return (None, None, "unknown: no compatible history");
    };
    let seconds = age_seconds(current.observed_at, base.observed_at);
    if seconds <= 0 || (seconds as u64) < minimum {
        return (None, None, "unknown: insufficient baseline");
    }
    let rate = (current.used_fraction - base.used_fraction) * 100.0 * 3600.0 / seconds as f64;
    if !rate.is_finite() || rate < 0.0 {
        return (None, None, "unknown: invalid history");
    }
    (
        Some(rate),
        Some(seconds),
        if rate == 0.0 {
            "measured flat (quantized; burn unknown)"
        } else {
            "measured account consumption"
        },
    )
}

fn pressure_card(
    snapshot: Option<&QuotaSnapshot>,
    account: &AccountConfig,
    now: DateTime<Utc>,
    policy: &crate::config::ZaiSaturationConfig,
) -> PressureCard {
    let fresh = snapshot.is_some_and(|s| {
        let age = age_seconds(now, s.observed_at);
        s.fresh && age >= 0 && age as u64 <= account.utilization.stale_after_seconds
    });
    let mut saturation = snapshot
        .and_then(|s| s.saturation.as_ref())
        .filter(|s| s.validate().is_ok() && s.window_seconds > 0)
        .cloned();
    if let Some(signal) = saturation.as_mut() {
        if !fresh
            || signal.attempts < policy.minimum_attempts
            || signal.window_seconds < policy.minimum_window_seconds
        {
            signal.upstream_concurrency_fraction = None;
        }
    }
    let mut backpressure = snapshot
        .and_then(|s| s.backpressure.as_ref())
        .filter(|s| s.validate().is_ok() && s.window_seconds > 0)
        .cloned();
    if let Some(signal) = backpressure.as_mut() {
        if !fresh
            || signal.attempts < policy.minimum_attempts
            || signal.window_seconds < policy.minimum_window_seconds
        {
            signal.upstream_429_fraction = None;
        }
    }
    PressureCard {
        observed_at: snapshot.map(|s| s.observed_at),
        fresh,
        saturation,
        backpressure,
        minimum_attempts: policy.minimum_attempts,
        minimum_window_seconds: policy.minimum_window_seconds,
        concurrency_threshold: policy.concurrency_fraction,
    }
}

fn window_card(
    id: &str,
    sample: &WindowSample,
    state: &AccountState,
    account: &AccountConfig,
    now: DateTime<Utc>,
    source_fresh: bool,
    snapshot: Option<&QuotaSnapshot>,
) -> Option<WindowCard> {
    if !valid_sample(sample) {
        return None;
    }
    let policy = account.utilization.policy_for(id)?;
    let target = if account.banked_resets.enabled
        && snapshot.is_some_and(|s| {
            s.reset_credits
                .as_ref()
                .is_some_and(|c| c.available_count > 0)
                && s.windows
                    .iter()
                    .any(|w| w.id == id && w.duration_minutes.is_some_and(|m| m >= 6 * 24 * 60))
        }) {
        policy
            .target
            .max(account.banked_resets.redeem_at_utilization)
    } else {
        policy.target
    };
    let lookback = policy.burn_rate_window_seconds.unwrap_or(3600);
    let age = age_seconds(now, sample.observed_at);
    let fresh = source_fresh
        && age >= 0
        && age as u64 <= account.utilization.stale_after_seconds
        && sample.resets_at > now;
    let (rate, baseline, status) = if fresh {
        measured_rate(
            state,
            id,
            sample,
            now,
            lookback,
            account.utilization.stale_after_seconds,
            account.utilization.minimum_sample_seconds,
        )
    } else {
        (None, None, "unknown: stale or expired quota")
    };
    // Request/rolling saturation quotas can replenish within a window; a linear
    // exhaustion extrapolation would claim evidence the cached counters do not supply.
    let linear = account.utilization.zai_saturation.is_none()
        && !id.to_ascii_lowercase().contains("rolling")
        && !id.to_ascii_lowercase().contains("request");
    let projected = if linear {
        rate.filter(|r| *r > 0.0).map(|r| {
            (100.0
                - sample.used_fraction * 100.0
                - r * age_seconds(sample.resets_at, sample.observed_at) as f64 / 3600.0)
                .clamp(0.0, 100.0)
        })
    } else {
        None
    };
    let projected_empty_at = if linear {
        rate.filter(|r| *r > 0.0).and_then(|r| {
            let seconds = ((1.0 - sample.used_fraction) * 100.0 / r * 3600.0).ceil();
            if !seconds.is_finite() || seconds < 0.0 || seconds >= i64::MAX as f64 {
                return None;
            }
            sample
                .observed_at
                .checked_add_signed(chrono::Duration::try_seconds(seconds as i64)?)
        })
    } else {
        None
    };
    Some(WindowCard {
        id: clean(id),
        observed_at: sample.observed_at,
        fresh,
        used_percent: sample.used_fraction * 100.0,
        target_percent: target * 100.0,
        configured_target_percent: policy.target * 100.0,
        headroom_percentage_points: (target - sample.used_fraction) * 100.0,
        reserve_percent: (1.0 - target) * 100.0,
        resets_at: sample.resets_at,
        consumption_percentage_points_per_hour: rate,
        rate_status: status.into(),
        baseline_seconds: baseline,
        lookback_seconds: lookback,
        expected_remaining_percent_at_reset: projected,
        projected_empty_at,
        projection_assumption: if linear {
            "same account pace from sample to reset; includes interactive usage"
        } else {
            "unknown: rolling/request quota; linear projection not valid"
        }
        .into(),
        history: compatible_history(
            state,
            id,
            sample,
            now,
            lookback,
            account.utilization.stale_after_seconds,
        )
        .into_iter()
        .map(|point| HistoryPoint {
            observed_at: point.observed_at,
            used_percent: point.used_fraction * 100.0,
        })
        .collect(),
    })
}

pub fn collect(configs: &[PathBuf], now: DateTime<Utc>) -> Frame {
    let mut accounts = Vec::new();
    for (index, path) in configs.iter().enumerate() {
        let config = match Config::load(path) {
            Ok(config) => config,
            Err(_) => {
                accounts.push(AccountCard {
                    account: format!("config {}", index + 1),
                    cache_status: "unknown: configuration unavailable or invalid".into(),
                    readiness: "unknown".into(),
                    reason: "configuration unavailable".into(),
                    readiness_updated_at: None,
                    controller_observed_at: None,
                    controller_observed_workers: None,
                    target_workers: None,
                    windows: Vec::new(),
                    cached_signals: Vec::new(),
                    live_workers: None,
                    pressure: None,
                });
                continue;
            }
        };
        let state_json = read_json(&config.state_path());
        let status_json = read_json(&config.state_path().with_file_name("status.json"));
        for (name, account) in &config.accounts {
            let state = state_json
                .as_ref()
                .and_then(|v| v.get("accounts"))
                .and_then(|v| v.get(name))
                .and_then(|v| serde_json::from_value::<AccountState>(v.clone()).ok());
            let status = status_json.as_ref().and_then(|v| v.get(name));
            let snapshot = match &account.source {
                SourceConfig::NormalizedFile { path } => {
                    read_json(path).and_then(|v| serde_json::from_value::<QuotaSnapshot>(v).ok())
                }
                _ => None,
            };
            let empty = AccountState::default();
            let cached = state.as_ref().unwrap_or(&empty);
            let mut windows: BTreeMap<String, (WindowSample, bool)> = cached
                .windows
                .iter()
                .map(|(id, s)| (id.clone(), (s.clone(), true)))
                .collect();
            if let Some(snapshot) = &snapshot {
                for window in &snapshot.windows {
                    if windows
                        .get(&window.id)
                        .is_none_or(|(s, _)| s.observed_at <= snapshot.observed_at)
                    {
                        windows.insert(
                            window.id.clone(),
                            (
                                WindowSample {
                                    observed_at: snapshot.observed_at,
                                    used_fraction: window.used_fraction,
                                    resets_at: window.resets_at,
                                    workers: 0,
                                },
                                snapshot.fresh,
                            ),
                        );
                    }
                }
            }
            let latest = cached.windows.values().max_by_key(|s| s.observed_at);
            let (live, live_notes) = live_workers(account, now);
            let status_workers = status
                .and_then(|s| s.get("current_workers"))
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok());
            let mut card = AccountCard {
                account: clean(name),
                cache_status: if state.is_some() {
                    "cached state"
                } else {
                    "state missing or malformed"
                }
                .into(),
                readiness: clean(
                    status
                        .and_then(|s| s.get("state"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                ),
                reason: clean(
                    status
                        .and_then(|s| s.get("reason"))
                        .and_then(Value::as_str)
                        .unwrap_or("no cached readiness"),
                ),
                readiness_updated_at: timestamp(status.and_then(|s| s.get("updated_at"))),
                controller_observed_at: if status_workers.is_some() {
                    timestamp(status.and_then(|s| s.get("updated_at")))
                } else {
                    latest.map(|s| s.observed_at)
                },
                controller_observed_workers: status_workers.or_else(|| latest.map(|s| s.workers)),
                target_workers: status
                    .and_then(|s| s.get("desired_workers"))
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .or(cached.last_target),
                windows: windows
                    .iter()
                    .filter_map(|(id, (s, f))| {
                        window_card(id, s, cached, account, now, *f, snapshot.as_ref())
                    })
                    .collect(),
                cached_signals: live_notes,
                live_workers: live,
                pressure: account
                    .utilization
                    .zai_saturation
                    .as_ref()
                    .map(|policy| pressure_card(snapshot.as_ref(), account, now, policy)),
            };
            if let Some(snapshot) = &snapshot {
                let age = age_seconds(now, snapshot.observed_at);
                let label = if snapshot.fresh
                    && age >= 0
                    && age as u64 <= account.utilization.stale_after_seconds
                {
                    "cached"
                } else {
                    "STALE cached"
                };
                if let Some(credits) = &snapshot.reset_credits {
                    let deadline = credits
                        .credits
                        .as_ref()
                        .and_then(|c| c.iter().filter_map(|c| c.expires_at).min());
                    card.cached_signals.push(format!(
                        "{label} banked credits: {}; earliest expiry: {}",
                        credits.available_count,
                        deadline
                            .map(|d| d.to_rfc3339())
                            .unwrap_or_else(|| "unknown".into())
                    ));
                    if account.banked_resets.enabled {
                        let hold = snapshot.windows.iter().any(|w| {
                            fraction(w.used_fraction)
                                && w.used_fraction >= account.banked_resets.redeem_at_utilization
                        }) || cached.history.values().any(|h| {
                            h.max_used_fraction >= account.banked_resets.redeem_at_utilization
                                && snapshot
                                    .windows
                                    .iter()
                                    .any(|w| same_reset_generation(w.resets_at, h.resets_at))
                        });
                        card.cached_signals.push(format!(
                            "Banked policy: {} | eligible capacity {} | redemption hold {}",
                            if account.banked_resets.full_speed {
                                "full speed capped by demand"
                            } else {
                                "minimum pace floor"
                            },
                            snapshot
                                .eligible_backlog_capacity
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| "unknown".into()),
                            if hold {
                                "threshold reached (cached)"
                            } else {
                                "not observed"
                            }
                        ));
                    }
                }
                if let Some(signal) = snapshot
                    .saturation
                    .as_ref()
                    .filter(|s| s.validate().is_ok() && s.window_seconds > 0)
                {
                    card.cached_signals.push(format!(
                        "{label} throughput: {} completed / {}s ({} req/s); attempts {}",
                        signal.completed_requests,
                        signal.window_seconds,
                        if signal.window_seconds > 0 {
                            format!(
                                "{:.3}",
                                signal.completed_requests as f64 / signal.window_seconds as f64
                            )
                        } else {
                            "unknown".into()
                        },
                        signal.attempts
                    ));
                }
                if let Some(signal) = snapshot
                    .backpressure
                    .as_ref()
                    .filter(|s| s.validate().is_ok() && s.window_seconds > 0)
                {
                    card.cached_signals.push(format!(
                        "{label} upstream 429: {}; {} attempts / {}s",
                        signal
                            .upstream_429_fraction
                            .filter(|v| fraction(*v))
                            .map(|v| format!("{:.1}%", v * 100.0))
                            .unwrap_or_else(|| "unknown".into()),
                        signal.attempts,
                        signal.window_seconds
                    ));
                }
            }
            if account.utilization.zai_saturation.is_some() {
                if let Some(pressure) = &card.pressure {
                    card.cached_signals.push(format!(
                        "{}provider 1302 concurrency rejection rate: {} (full-proxy upstream attempts; not active agent count)",
                        if pressure.fresh { "cached " } else { "STALE cached " },
                        pressure.saturation.as_ref()
                            .and_then(|s| s.upstream_concurrency_fraction)
                            .map(|rate| format!("{:.1}%", rate * 100.0))
                            .unwrap_or_else(|| "unknown".into())
                    ));
                }
                card.cached_signals.push(format!(
                    "controller backpressure {:?}; saturation workers {} at {}",
                    cached.backpressure.mode,
                    cached
                        .zai_saturation
                        .as_ref()
                        .and_then(|s| s.observed_workers)
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "unknown".into()),
                    cached
                        .zai_saturation
                        .as_ref()
                        .and_then(|s| s.last_sample_at)
                        .map(|d| d.to_rfc3339())
                        .unwrap_or_else(|| "unknown".into())
                ));
            }
            card.cached_signals.push(format!(
                "Policy: {:?} | agents {}..{} | step +{}/-{} | smallest window target and safety holds",
                account.utilization.strategy,
                account.fleet.min_workers,
                account.fleet.max_workers,
                account.fleet.max_scale_up_per_cycle,
                account.fleet.max_scale_down_per_cycle,
            ));
            accounts.push(card);
        }
    }
    Frame {
        observed_at: now,
        accounts,
    }
}

fn number(value: Option<u32>) -> String {
    value.map(|v| v.to_string()).unwrap_or_else(|| "?".into())
}
fn time(value: Option<DateTime<Utc>>) -> String {
    value
        .map(|v| v.format("%m-%d %H:%M:%SZ").to_string())
        .unwrap_or_else(|| "unknown".into())
}

pub fn render(frame: &Frame, width: usize) -> String {
    let mut lines = vec![format!("SUBGOV  {}  | cached metrics | q / Ctrl-C quit | j/k scroll",time(Some(frame.observed_at))),
        "Account consumption includes interactive use; worker counts do not imply causal attribution.".into()];
    for account in &frame.accounts {
        lines.push(format!(
            "--- {} | {} ---",
            account.account, account.cache_status
        ));
        lines.push(format!(
            "Workers: controller-observed {} @ {} -> target {}",
            number(account.controller_observed_workers),
            time(account.controller_observed_at),
            number(account.target_workers)
        ));
        lines.push(match &account.live_workers {
            Some(live) => format!(
                "Live now: {} verified | {} EXECUTING | {} idle | {} other | {} stale/unverified",
                live.live, live.executing, live.idle, live.other, live.stale_or_unverified
            ),
            None => {
                "Live now: unknown (local ownership / heartbeat / process evidence unavailable)"
                    .into()
            }
        });
        lines.push(format!(
            "Readiness: {} / {} | cycle {} ({}s ago)",
            account.readiness,
            account.reason,
            time(account.readiness_updated_at),
            account
                .readiness_updated_at
                .map(|t| age_seconds(frame.observed_at, t).to_string())
                .unwrap_or_else(|| "?".into())
        ));
        if account.windows.is_empty() {
            lines.push("Quota: unknown (missing, malformed or disabled cached windows)".into());
        }
        for w in &account.windows {
            lines.push(format!(
                "{} [{}{}] {:.1}% used / {:.1}% left | target {:.1}% | reserve {:.1}% | {}",
                w.id,
                "#".repeat((w.used_percent / 10.0).floor() as usize),
                "-".repeat(10usize.saturating_sub((w.used_percent / 10.0).floor() as usize)),
                w.used_percent,
                100.0 - w.used_percent,
                w.target_percent,
                w.reserve_percent,
                if w.fresh { "fresh" } else { "STALE" }
            ));
            lines.push(format!(
                "  Sample {} ({}s ago) | reset {} ({}h {:02}m left)",
                time(Some(w.observed_at)),
                age_seconds(frame.observed_at, w.observed_at),
                time(Some(w.resets_at)),
                age_seconds(w.resets_at, frame.observed_at).max(0) / 3600,
                age_seconds(w.resets_at, frame.observed_at).max(0) / 60 % 60,
            ));
            if (w.target_percent - w.configured_target_percent).abs() > f64::EPSILON {
                lines.push(format!(
                    "  Banked override: target {:.1}% (configured {:.1}%)",
                    w.target_percent, w.configured_target_percent
                ));
            }
            lines.push(format!(
                "  Pace: {} | {} | baseline {}m / lookback {}m",
                w.consumption_percentage_points_per_hour
                    .map(|r| format!("{r:.2} pp/hour"))
                    .unwrap_or_else(|| "unknown".into()),
                w.rate_status,
                w.baseline_seconds
                    .map(|s| (s / 60).to_string())
                    .unwrap_or_else(|| "?".into()),
                w.lookback_seconds / 60
            ));
            lines.push(format!(
                "  Remaining at reset: {} | {}",
                w.expected_remaining_percent_at_reset
                    .map(|v| format!("{v:.1}%"))
                    .unwrap_or_else(|| "unknown".into()),
                w.projection_assumption
            ));
            lines.push(format!(
                "  Empty at measured pace: {}{}",
                time(w.projected_empty_at),
                if w.projected_empty_at
                    .is_some_and(|empty| empty > w.resets_at)
                {
                    " (reset occurs first; conditional, ignores future resets)"
                } else {
                    " (estimate)"
                }
            ));
        }
        lines.extend(account.cached_signals.iter().cloned());
    }
    lines.push("Read-only cached telemetry. Projections assume measured pace continues; a new cycle does not refresh quota.".into());
    lines
        .into_iter()
        .map(|line| line.chars().take(width.max(1)).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

pub fn run(configs: &[PathBuf], interval_seconds: u64, once: bool, json: bool) -> Result<()> {
    let interactive = !once && !json && io::stdout().is_terminal() && io::stdin().is_terminal();
    if !interactive {
        let frame = collect(configs, Utc::now());
        if json {
            println!("{}", serde_json::to_string_pretty(&frame)?);
        } else {
            print!("{}", render(&frame, 120));
        }
        return Ok(());
    }
    let quit = Arc::new(AtomicBool::new(false));
    let signal = quit.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))
        .context("cannot install dashboard interrupt handler")?;
    let mut terminal = ratatui::try_init().context("cannot initialize dashboard terminal")?;
    let _guard = TerminalGuard;
    let mut state = ui::UiState::default();
    let mut data = collect(configs, Utc::now());
    let interval = Duration::from_secs(interval_seconds.max(1));
    let mut next_refresh = Instant::now() + interval;
    let mut redraw = true;
    while !quit.load(Ordering::SeqCst) {
        if !state.paused && Instant::now() >= next_refresh {
            data = collect(configs, Utc::now());
            next_refresh = Instant::now() + interval;
            redraw = true;
        }
        if redraw {
            terminal.draw(|frame| ui::draw(frame, &data, &mut state))?;
            redraw = false;
        }
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        match event::read()? {
            Event::Resize(_, _) => redraw = true,
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let count = data.accounts.len();
                let selected_before = state.selected;
                match key.code {
                    KeyCode::Char('q' | 'Q') => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Tab | KeyCode::Right => {
                        if count > 0 {
                            state.selected = (state.selected + 1) % count;
                        }
                    }
                    KeyCode::BackTab | KeyCode::Left => {
                        if count > 0 {
                            state.selected = (state.selected + count - 1) % count;
                        }
                    }
                    KeyCode::Char(number @ '1'..='9') => {
                        let index = (number as u8 - b'1') as usize;
                        if index < count {
                            state.selected = index;
                        }
                    }
                    KeyCode::Enter | KeyCode::Char('d' | 'D') => {
                        state.details = !state.details;
                        state.scroll = 0;
                    }
                    KeyCode::Esc => {
                        state.help = false;
                        state.details = false;
                        state.scroll = 0;
                    }
                    KeyCode::Char('o' | 'O' | '0') => {
                        state.details = false;
                        state.scroll = 0;
                    }
                    KeyCode::Char('?' | 'h') => state.help = !state.help,
                    KeyCode::Char(' ' | 'p' | 'P') => {
                        state.paused = !state.paused;
                        if !state.paused {
                            next_refresh = Instant::now();
                        }
                    }
                    KeyCode::Char('r' | 'R') => {
                        data = collect(configs, Utc::now());
                        next_refresh = Instant::now() + interval;
                    }
                    KeyCode::Down if !state.details => {
                        if count > 0 {
                            state.selected = (state.selected + 1) % count;
                        }
                    }
                    KeyCode::Up if !state.details => {
                        if count > 0 {
                            state.selected = (state.selected + count - 1) % count;
                        }
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        state.scroll = state.scroll.saturating_add(1)
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        state.scroll = state.scroll.saturating_sub(1)
                    }
                    KeyCode::PageDown => state.scroll = state.scroll.saturating_add(10),
                    KeyCode::PageUp => state.scroll = state.scroll.saturating_sub(10),
                    KeyCode::Home => state.scroll = 0,
                    _ => continue,
                }
                if state.selected != selected_before {
                    state.scroll = 0;
                }
                redraw = true;
            }
            _ => {}
        }
    }
    Ok(())
}
