use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use subscription_governor::config::Config;
use subscription_governor::controller::{evaluate, Decision};
use subscription_governor::fleet;
use subscription_governor::source;
use subscription_governor::state::{State, StateLock};

#[derive(Debug, Parser)]
#[command(name = "subgov", version, about)]
struct Cli {
    #[arg(short, long, default_value = "governor.yaml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Parse and validate the configuration.
    Check,
    /// Fetch and print one account's normalized quota snapshot.
    Snapshot { account: String },
    /// Evaluate accounts, optionally actuating fleet targets.
    Run {
        /// Perform one cycle and exit.
        #[arg(long)]
        once: bool,
        /// Print decisions and persist observations without changing targets.
        #[arg(long)]
        observe_only: bool,
    },
    /// Print each account's readiness classification from the last
    /// completed cycle, without inspecting credentials or live sources.
    Status,
}

/// Stable exit-code categories (plan.md §12). Each carries the underlying
/// error so the top-level handler can print it once, uniformly.
enum GovernorError {
    /// 2: CLI or configuration error.
    CliOrConfig(anyhow::Error),
    /// 3: state ownership or persistence failure.
    State(anyhow::Error),
    /// 4: one or more source/observer failures.
    Source(anyhow::Error),
    /// 5: actuation failure.
    Actuation(anyhow::Error),
}

impl GovernorError {
    fn exit_code(&self) -> u8 {
        match self {
            Self::CliOrConfig(_) => 2,
            Self::State(_) => 3,
            Self::Source(_) => 4,
            Self::Actuation(_) => 5,
        }
    }

    fn error(&self) -> &anyhow::Error {
        match self {
            Self::CliOrConfig(error)
            | Self::State(error)
            | Self::Source(error)
            | Self::Actuation(error) => error,
        }
    }
}

fn main() -> ExitCode {
    match run_cli() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "{}",
                json!({
                    "event": "fatal_error",
                    "time": Utc::now(),
                    "exit_code": error.exit_code(),
                    "error": format!("{:#}", error.error()),
                })
            );
            ExitCode::from(error.exit_code())
        }
    }
}

fn run_cli() -> Result<(), GovernorError> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config).map_err(GovernorError::CliOrConfig)?;
    match cli.command {
        Commands::Check => {
            println!(
                "configuration is valid ({} accounts)",
                config.accounts.len()
            );
            Ok(())
        }
        Commands::Snapshot { account } => snapshot(&config, &account),
        Commands::Run { once, observe_only } => run(config, once, observe_only),
        Commands::Status => status(&config),
    }
}

fn snapshot(config: &Config, account: &str) -> Result<(), GovernorError> {
    let account_config = config
        .accounts
        .get(account)
        .with_context(|| format!("unknown account {account}"))
        .map_err(GovernorError::CliOrConfig)?;
    (|| -> Result<()> {
        let snapshot = source::collect(&account_config.source)?;
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        Ok(())
    })()
    .map_err(GovernorError::Source)
}

/// Prints one JSONL line per configured account classifying it as
/// `healthy_learning`, `intentional_hold`, `provider_failure`,
/// `stale_drain`, or `actuation_failure` (plan.md §16 WP6), read from the
/// sidecar status file the run loop writes after every cycle. This never
/// contacts a source, observer, or actuator, so it never needs credentials
/// and never blocks on a live provider.
fn status(config: &Config) -> Result<(), GovernorError> {
    let path = status_path(&config.state_path());
    let recorded: BTreeMap<String, AccountReadiness> = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse status file {}", path.display()))
            .map_err(GovernorError::State)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(error) => {
            return Err(GovernorError::State(
                anyhow::Error::new(error)
                    .context(format!("failed to read status file {}", path.display())),
            ))
        }
    };
    for name in config.accounts.keys() {
        let line = match recorded.get(name) {
            Some(readiness) => json!({
                "event": "status",
                "account": name,
                "time": Utc::now(),
                "status": readiness,
            }),
            None => json!({
                "event": "status",
                "account": name,
                "time": Utc::now(),
                "status": {"state": "unknown", "reason": "no cycle has completed yet"},
            }),
        };
        println!(
            "{}",
            serde_json::to_string(&line).map_err(|error| GovernorError::State(error.into()))?
        );
    }
    Ok(())
}

fn run(config: Config, once: bool, observe_only: bool) -> Result<(), GovernorError> {
    let state_path = config.state_path();
    let _lock = StateLock::acquire(&state_path).map_err(GovernorError::State)?;
    let (mut state, quarantined) = State::load(&state_path).map_err(GovernorError::State)?;
    if let Some(quarantined) = quarantined {
        eprintln!(
            "{}",
            json!({
                "event": "state_quarantined",
                "time": Utc::now(),
                "state_path": state_path.display().to_string(),
                "quarantined_path": quarantined.quarantined_path.display().to_string(),
                "error": quarantined.error,
            })
        );
    }
    let shutdown = install_shutdown_flag().map_err(GovernorError::State)?;
    let interval = Duration::from_secs(config.poll_interval_seconds);
    let mut anchor = Instant::now();
    let status_path = status_path(&state_path);
    loop {
        let outcome = run_cycle(&config, &mut state, observe_only, &shutdown);
        state.save(&state_path).map_err(GovernorError::State)?;
        if let Err(error) = merge_status_report(&status_path, &outcome.statuses) {
            eprintln!(
                "{}",
                json!({"event": "status_write_error", "time": Utc::now(), "error": format!("{error:#}")})
            );
        }
        if once {
            if outcome.actuation_failures > 0 {
                return Err(GovernorError::Actuation(anyhow!(
                    "{} account(s) failed actuation",
                    outcome.actuation_failures
                )));
            }
            if outcome.observation_failures > 0 {
                return Err(GovernorError::Source(anyhow!(
                    "{} account(s) failed",
                    outcome.observation_failures
                )));
            }
            return Ok(());
        }
        if shutdown.load(Ordering::SeqCst) {
            shutdown_complete();
            return Ok(());
        }
        let (next_anchor, sleep_for) = advance_schedule(anchor, interval, Instant::now());
        anchor = next_anchor;
        interruptible_sleep(sleep_for + bounded_jitter(interval), &shutdown);
        if shutdown.load(Ordering::SeqCst) {
            shutdown_complete();
            return Ok(());
        }
    }
}

/// An account's readiness classification (plan.md §16 WP6 definition of
/// done): distinguishes healthy learning, an intentional hold at the
/// configured target, a provider/observer failure, a stale-data drain, and
/// an actuation failure -- all without exposing credentials or raw
/// provider errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReadinessState {
    HealthyLearning,
    IntentionalHold,
    ProviderFailure,
    StaleDrain,
    ActuationFailure,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AccountReadiness {
    state: ReadinessState,
    reason: String,
    updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_workers: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    desired_workers: Option<u32>,
}

fn readiness_for_decision(decision: &Decision) -> AccountReadiness {
    let base = AccountReadiness {
        state: ReadinessState::HealthyLearning,
        reason: String::new(),
        updated_at: Utc::now(),
        observed_at: Some(decision.observed_at),
        current_workers: Some(decision.current_workers),
        desired_workers: Some(decision.desired_workers),
    };
    if decision.stale {
        return AccountReadiness {
            state: ReadinessState::StaleDrain,
            reason: "stale_data_holding".to_owned(),
            ..base
        };
    }
    let intentional_hold = decision
        .windows
        .iter()
        .any(|window| window.reason == "target_reached")
        || decision
            .banked_resets
            .as_ref()
            .is_some_and(|banked| banked.manual_redemption_recommended);
    if intentional_hold {
        return AccountReadiness {
            state: ReadinessState::IntentionalHold,
            reason: "target_reached".to_owned(),
            ..base
        };
    }
    let binding = decision
        .binding_window
        .as_ref()
        .and_then(|id| decision.windows.iter().find(|window| &window.id == id))
        .or_else(|| {
            decision
                .windows
                .iter()
                .find(|window| window.desired_workers == decision.desired_workers)
        });
    let reason = binding
        .map(|window| window.reason.clone())
        .unwrap_or_else(|| "healthy".to_owned());
    AccountReadiness { reason, ..base }
}

fn readiness_for_failure(failure: &AccountFailure) -> AccountReadiness {
    let (state, reason) = match failure {
        AccountFailure::Observation(_) => {
            (ReadinessState::ProviderFailure, "source_or_observer_error")
        }
        AccountFailure::Actuation(_) => (ReadinessState::ActuationFailure, "actuator_error"),
    };
    AccountReadiness {
        state,
        reason: reason.to_owned(),
        updated_at: Utc::now(),
        observed_at: None,
        current_workers: None,
        desired_workers: None,
    }
}

fn status_path(state_path: &Path) -> PathBuf {
    state_path.with_file_name("status.json")
}

/// Reads the existing status file (tolerating a missing or corrupt file by
/// starting empty, since this is a derived, best-effort surface rather than
/// durable state), applies `updates` on top, and atomically rewrites it.
fn merge_status_report(path: &Path, updates: &BTreeMap<String, AccountReadiness>) -> Result<()> {
    let mut recorded: BTreeMap<String, AccountReadiness> = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    for (name, readiness) in updates {
        recorded.insert(name.clone(), readiness.clone());
    }
    write_status_report(path, &recorded)
}

fn write_status_report(path: &Path, statuses: &BTreeMap<String, AccountReadiness>) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create status directory {}", parent.display()))?;
    let temporary = temporary_status_path(path);
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        serde_json::to_writer_pretty(&mut file, statuses)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to install status {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn temporary_status_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("status.json");
    path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()))
}

/// Installs a SIGTERM/SIGINT handler that flips a shared flag rather than
/// terminating the process immediately, so the run loop can finish or
/// abandon the in-flight cycle and persist valid state before exiting.
fn install_shutdown_flag() -> Result<Arc<AtomicBool>> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&shutdown);
    ctrlc::set_handler(move || {
        handler_flag.store(true, Ordering::SeqCst);
    })
    .context("failed to install SIGINT/SIGTERM handler")?;
    Ok(shutdown)
}

fn shutdown_complete() {
    eprintln!(
        "{}",
        json!({"event": "shutdown", "time": Utc::now(), "reason": "signal"})
    );
}

/// Sleeps for `duration`, but returns promptly (within one `STEP`) once
/// `shutdown` is set instead of blocking for the full duration.
fn interruptible_sleep(duration: Duration, shutdown: &AtomicBool) {
    const STEP: Duration = Duration::from_millis(200);
    let mut remaining = duration;
    while remaining > Duration::ZERO {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        let slice = remaining.min(STEP);
        thread::sleep(slice);
        remaining -= slice;
    }
}

/// Advances a monotonic-clock schedule by one interval from `anchor`.
///
/// Anchoring to the prior anchor plus the interval (rather than to `now`
/// after each cycle finishes) keeps the long-run cadence at exactly
/// `interval` regardless of how long each cycle took, so per-cycle work
/// never accumulates as schedule drift. If a cycle overran the interval,
/// the anchor resyncs to `now` so a slow cycle doesn't trigger a burst of
/// back-to-back catch-up cycles trying to make up for lost time.
fn advance_schedule(anchor: Instant, interval: Duration, now: Instant) -> (Instant, Duration) {
    let next_anchor = anchor + interval;
    if next_anchor > now {
        (next_anchor, next_anchor - now)
    } else {
        (now, Duration::ZERO)
    }
}

/// A random delay up to 10% of `interval` (capped at 30s) so that many
/// accounts/instances on the same interval don't all poll their upstream
/// providers at the same moment. The exact range is a deliberately
/// conservative placeholder pending observation-mode evidence (plan.md §21).
fn bounded_jitter(interval: Duration) -> Duration {
    let max_jitter = interval.mul_f64(0.1).min(Duration::from_secs(30));
    if max_jitter.is_zero() {
        return Duration::ZERO;
    }
    let millis = rand::thread_rng().gen_range(0..=max_jitter.as_millis() as u64);
    Duration::from_millis(millis)
}

/// Per-cycle failure counts, split by the exit-code category (plan.md §12)
/// they map to: source/observer failures (exit 4) vs. actuation failures
/// (exit 5); and each processed account's readiness classification for the
/// status surface (plan.md §16 WP6).
#[derive(Debug, Default)]
struct CycleOutcome {
    observation_failures: usize,
    actuation_failures: usize,
    statuses: BTreeMap<String, AccountReadiness>,
}

/// Why one account's cycle failed, carrying enough to both count it under
/// the right exit-code category and report it accurately.
enum AccountFailure {
    /// Quota source, worker observer, or controller evaluation failed.
    Observation(anyhow::Error),
    /// The fleet actuator failed to apply the desired worker count.
    Actuation(anyhow::Error),
}

impl AccountFailure {
    fn category(&self) -> &'static str {
        match self {
            Self::Observation(_) => "observation",
            Self::Actuation(_) => "actuation",
        }
    }

    fn error(&self) -> &anyhow::Error {
        match self {
            Self::Observation(error) | Self::Actuation(error) => error,
        }
    }
}

fn run_cycle(
    config: &Config,
    state: &mut State,
    observe_only: bool,
    shutdown: &AtomicBool,
) -> CycleOutcome {
    let mut outcome = CycleOutcome::default();
    for (name, account_config) in &config.accounts {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let result: Result<(), AccountFailure> = (|| {
            let snapshot =
                source::collect(&account_config.source).map_err(AccountFailure::Observation)?;
            let workers = fleet::current_workers(&account_config.fleet)
                .map_err(AccountFailure::Observation)?;
            let prior = state.accounts.get(name).cloned().unwrap_or_default();
            let decision = evaluate(name, account_config, &snapshot, &prior, workers, Utc::now())
                .map_err(AccountFailure::Observation)?;

            let actuated = if observe_only {
                false
            } else {
                fleet::actuate(&account_config.fleet, workers, decision.desired_workers)
                    .map_err(AccountFailure::Actuation)?
                    .actuated
            };
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "event": "decision",
                    "observe_only": observe_only,
                    "actuated": actuated,
                    "decision": decision,
                }))
                .map_err(|error| AccountFailure::Observation(error.into()))?
            );
            let account_state = state.accounts.entry(name.clone()).or_default();
            if decision.stale {
                account_state.last_target = Some(decision.desired_workers);
            } else {
                let sample_workers = if actuated {
                    decision.desired_workers
                } else {
                    workers
                };
                account_state.record(&snapshot, sample_workers, decision.desired_workers);
            }
            outcome
                .statuses
                .insert(name.clone(), readiness_for_decision(&decision));
            Ok(())
        })();
        if let Err(failure) = result {
            match failure.category() {
                "actuation" => outcome.actuation_failures += 1,
                _ => outcome.observation_failures += 1,
            }
            outcome
                .statuses
                .insert(name.clone(), readiness_for_failure(&failure));
            eprintln!(
                "{}",
                json!({
                    "event": "account_error",
                    "account": name,
                    "category": failure.category(),
                    "time": Utc::now(),
                    "error": format!("{:#}", failure.error()),
                })
            );
        }
    }
    outcome
}

#[cfg(test)]
mod scheduling_tests {
    use super::*;

    #[test]
    fn schedule_does_not_drift_when_a_cycle_is_fast() {
        let interval = Duration::from_secs(10);
        let anchor = Instant::now();
        let now = anchor + Duration::from_millis(50);
        let (next_anchor, sleep_for) = advance_schedule(anchor, interval, now);
        assert_eq!(next_anchor, anchor + interval);
        assert_eq!(sleep_for, interval - Duration::from_millis(50));
    }

    #[test]
    fn schedule_resyncs_after_a_slow_cycle_without_bursting() {
        let interval = Duration::from_secs(10);
        let anchor = Instant::now();
        let now = anchor + Duration::from_secs(25);
        let (next_anchor, sleep_for) = advance_schedule(anchor, interval, now);
        assert_eq!(next_anchor, now);
        assert_eq!(sleep_for, Duration::ZERO);
    }

    #[test]
    fn jitter_never_exceeds_ten_percent_of_interval_or_the_cap() {
        let interval = Duration::from_secs(300);
        for _ in 0..1000 {
            let jitter = bounded_jitter(interval);
            assert!(jitter <= Duration::from_secs(30));
        }
    }

    #[test]
    fn jitter_is_bounded_for_a_short_interval() {
        let interval = Duration::from_secs(1);
        for _ in 0..1000 {
            let jitter = bounded_jitter(interval);
            assert!(jitter <= Duration::from_millis(100));
        }
    }

    #[test]
    fn jitter_is_zero_for_a_zero_interval() {
        assert_eq!(bounded_jitter(Duration::ZERO), Duration::ZERO);
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use std::collections::BTreeMap;
    use subscription_governor::config::{
        AccountConfig, ActuatorConfig, BankedResetConfig, FleetConfig, ObserverReconciliation,
        SourceConfig, StaleBehavior, Strategy, UtilizationConfig, WorkerObserverConfig,
    };

    fn account_config(path: PathBuf) -> AccountConfig {
        AccountConfig {
            source: SourceConfig::NormalizedFile { path },
            fleet: FleetConfig {
                min_workers: 0,
                max_workers: 4,
                bootstrap_workers: 1,
                max_scale_up_per_cycle: 1,
                max_scale_down_per_cycle: 1,
                observer: Some(WorkerObserverConfig::Static { workers: 1 }),
                actuator: ActuatorConfig::None,
                observer_reconciliation: ObserverReconciliation::default(),
                hosts: None,
            },
            utilization: UtilizationConfig {
                target_utilization: Some(0.9),
                reserve_fraction: None,
                strategy: Strategy::CeilingOnly,
                stale_after_seconds: 900,
                stale_behavior: StaleBehavior::Hold,
                minimum_sample_seconds: 60,
                windows: BTreeMap::new(),
            },
            banked_resets: BankedResetConfig::default(),
        }
    }

    // Every account source points at a file that does not exist, so an
    // account that is actually attempted always fails collection. This lets
    // the tests below distinguish "abandoned" (never attempted, no failure)
    // from "attempted" (failure recorded) without needing a working fixture.
    fn config_with_unreachable_sources() -> Config {
        let mut accounts = BTreeMap::new();
        accounts.insert(
            "a".to_string(),
            account_config(PathBuf::from("/nonexistent/subgov-test-a.json")),
        );
        accounts.insert(
            "b".to_string(),
            account_config(PathBuf::from("/nonexistent/subgov-test-b.json")),
        );
        Config {
            version: 1,
            poll_interval_seconds: 300,
            state_path: None,
            accounts,
        }
    }

    #[test]
    fn run_cycle_abandons_every_account_when_shutdown_is_already_requested() {
        let config = config_with_unreachable_sources();
        let mut state = State::default();
        let shutdown = AtomicBool::new(true);
        let outcome = run_cycle(&config, &mut state, true, &shutdown);
        assert_eq!(
            outcome.observation_failures, 0,
            "no account should have been attempted"
        );
        assert_eq!(outcome.actuation_failures, 0);
    }

    #[test]
    fn run_cycle_attempts_every_account_when_not_shutting_down() {
        let config = config_with_unreachable_sources();
        let mut state = State::default();
        let shutdown = AtomicBool::new(false);
        let outcome = run_cycle(&config, &mut state, true, &shutdown);
        assert_eq!(outcome.observation_failures, 2);
        assert_eq!(outcome.actuation_failures, 0);
    }

    #[test]
    fn interruptible_sleep_returns_promptly_when_already_shutting_down() {
        let shutdown = AtomicBool::new(true);
        let start = Instant::now();
        interruptible_sleep(Duration::from_secs(5), &shutdown);
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn interruptible_sleep_stops_within_one_step_of_a_late_signal() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            flag.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        interruptible_sleep(Duration::from_secs(5), &shutdown);
        assert!(start.elapsed() < Duration::from_millis(400));
    }
}

#[cfg(test)]
mod exit_code_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use subscription_governor::config::{
        AccountConfig, ActuatorConfig, BankedResetConfig, FleetConfig, ObserverReconciliation,
        SourceConfig, StaleBehavior, Strategy, UtilizationConfig, WorkerObserverConfig,
    };

    #[test]
    fn exit_codes_match_the_plan_md_section_12_table() {
        assert_eq!(GovernorError::CliOrConfig(anyhow!("x")).exit_code(), 2);
        assert_eq!(GovernorError::State(anyhow!("x")).exit_code(), 3);
        assert_eq!(GovernorError::Source(anyhow!("x")).exit_code(), 4);
        assert_eq!(GovernorError::Actuation(anyhow!("x")).exit_code(), 5);
    }

    // A valid snapshot with used_fraction below the ceiling-only target, an
    // observer reporting 0 current workers, and an actuator pointed at a
    // binary that does not exist: the controller decides to scale up from 0
    // (a real, non-`none` actuator is invoked), and that invocation fails,
    // isolating an actuation failure from an observation failure.
    fn account_config_with_failing_actuator(path: PathBuf) -> AccountConfig {
        AccountConfig {
            source: SourceConfig::NormalizedFile { path },
            fleet: FleetConfig {
                min_workers: 0,
                max_workers: 3,
                bootstrap_workers: 1,
                max_scale_up_per_cycle: 10,
                max_scale_down_per_cycle: 10,
                observer: Some(WorkerObserverConfig::Static { workers: 0 }),
                actuator: ActuatorConfig::Command {
                    argv: vec!["/nonexistent/subgov-test-actuator".to_string()],
                },
                observer_reconciliation: ObserverReconciliation::default(),
                hosts: None,
            },
            utilization: UtilizationConfig {
                target_utilization: Some(0.9),
                reserve_fraction: None,
                strategy: Strategy::CeilingOnly,
                stale_after_seconds: 900,
                stale_behavior: StaleBehavior::Hold,
                minimum_sample_seconds: 60,
                windows: BTreeMap::new(),
            },
            banked_resets: BankedResetConfig::default(),
        }
    }

    #[test]
    fn run_cycle_counts_an_actuation_failure_separately_from_an_observation_failure() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot_path = dir.path().join("snapshot.json");
        let snapshot = json!({
            "observed_at": Utc::now(),
            "fresh": true,
            "windows": [
                {"id": "five_hour", "used_fraction": 0.1, "resets_at": Utc::now() + chrono::Duration::hours(2)},
            ],
        });
        fs::write(&snapshot_path, snapshot.to_string()).unwrap();

        let mut accounts = BTreeMap::new();
        accounts.insert(
            "a".to_string(),
            account_config_with_failing_actuator(snapshot_path),
        );
        let config = Config {
            version: 1,
            poll_interval_seconds: 300,
            state_path: None,
            accounts,
        };
        let mut state = State::default();
        let shutdown = AtomicBool::new(false);
        let outcome = run_cycle(&config, &mut state, false, &shutdown);
        assert_eq!(outcome.actuation_failures, 1);
        assert_eq!(outcome.observation_failures, 0);
    }

    // plan.md §11.2: "retain the prior state on actuation failure so the
    // next cycle reconciles." A failed actuation must not let this cycle's
    // (unactuated) observation overwrite the account's recorded state,
    // since the next cycle needs the old state to know what actually
    // happened last time, not what this cycle merely intended.
    #[test]
    fn run_cycle_retains_prior_account_state_on_actuation_failure() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot_path = dir.path().join("snapshot.json");
        let snapshot = json!({
            "observed_at": Utc::now(),
            "fresh": true,
            "windows": [
                {"id": "five_hour", "used_fraction": 0.1, "resets_at": Utc::now() + chrono::Duration::hours(2)},
            ],
        });
        fs::write(&snapshot_path, snapshot.to_string()).unwrap();

        let mut accounts = BTreeMap::new();
        accounts.insert(
            "a".to_string(),
            account_config_with_failing_actuator(snapshot_path),
        );
        let config = Config {
            version: 1,
            poll_interval_seconds: 300,
            state_path: None,
            accounts,
        };
        let mut state = State::default();
        let prior = subscription_governor::state::AccountState::default();
        state.accounts.insert("a".to_string(), prior.clone());
        let shutdown = AtomicBool::new(false);
        let outcome = run_cycle(&config, &mut state, false, &shutdown);
        assert_eq!(outcome.actuation_failures, 1);
        assert_eq!(state.accounts.get("a"), Some(&prior));
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use subscription_governor::controller::{BankedResetDecision, WindowDecision};

    fn decision(stale: bool, windows: Vec<WindowDecision>) -> Decision {
        let binding_window = windows.first().map(|window| window.id.clone());
        Decision {
            account: "a".to_owned(),
            observed_at: Utc::now(),
            current_workers: 1,
            desired_workers: 1,
            stale,
            windows,
            binding_window,
            banked_resets: None,
        }
    }

    fn window(reason: &str, desired_workers: u32) -> WindowDecision {
        WindowDecision {
            id: "five_hour".to_owned(),
            used_fraction: 0.5,
            target_utilization: 0.9,
            resets_at: Utc::now(),
            desired_workers,
            reason: reason.to_owned(),
            observed_burn_per_worker_hour: None,
        }
    }

    #[test]
    fn stale_decision_is_a_stale_drain() {
        let readiness = readiness_for_decision(&decision(true, Vec::new()));
        assert_eq!(readiness.state, ReadinessState::StaleDrain);
    }

    #[test]
    fn target_reached_window_is_an_intentional_hold() {
        let readiness = readiness_for_decision(&decision(false, vec![window("target_reached", 0)]));
        assert_eq!(readiness.state, ReadinessState::IntentionalHold);
    }

    #[test]
    fn manual_redemption_recommended_is_an_intentional_hold() {
        let mut d = decision(false, vec![window("paced_to_reset", 0)]);
        d.banked_resets = Some(BankedResetDecision {
            available_count: 1,
            governing_window: "weekly".to_owned(),
            minimum_pace_multiplier: 2.0,
            required_burn_per_hour: 0.1,
            desired_workers: 0,
            manual_redemption_recommended: true,
            deadline_missed: false,
            reason: "weekly_window_awaiting_manual_redemption".to_owned(),
            known_expirations: Vec::new(),
        });
        let readiness = readiness_for_decision(&d);
        assert_eq!(readiness.state, ReadinessState::IntentionalHold);
    }

    #[test]
    fn an_ordinary_decision_is_healthy_learning_with_the_binding_reason() {
        let readiness = readiness_for_decision(&decision(false, vec![window("paced_to_reset", 2)]));
        assert_eq!(readiness.state, ReadinessState::HealthyLearning);
        assert_eq!(readiness.reason, "paced_to_reset");
    }

    #[test]
    fn observation_failure_is_a_provider_failure() {
        let readiness = readiness_for_failure(&AccountFailure::Observation(anyhow!("boom")));
        assert_eq!(readiness.state, ReadinessState::ProviderFailure);
    }

    #[test]
    fn actuation_failure_is_an_actuation_failure() {
        let readiness = readiness_for_failure(&AccountFailure::Actuation(anyhow!("boom")));
        assert_eq!(readiness.state, ReadinessState::ActuationFailure);
    }

    #[test]
    fn merge_status_report_updates_only_the_given_accounts_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.json");

        let mut first = BTreeMap::new();
        first.insert(
            "a".to_owned(),
            readiness_for_failure(&AccountFailure::Observation(anyhow!("x"))),
        );
        first.insert(
            "b".to_owned(),
            readiness_for_decision(&decision(true, Vec::new())),
        );
        merge_status_report(&path, &first).unwrap();

        let mut second = BTreeMap::new();
        second.insert(
            "a".to_owned(),
            readiness_for_decision(&decision(false, vec![window("below_ceiling", 3)])),
        );
        merge_status_report(&path, &second).unwrap();

        let bytes = fs::read(&path).unwrap();
        let recorded: BTreeMap<String, AccountReadiness> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            recorded.len(),
            2,
            "account b must survive an update that only touches a"
        );
        assert_eq!(recorded["a"].state, ReadinessState::HealthyLearning);
        assert_eq!(recorded["b"].state, ReadinessState::StaleDrain);
    }

    #[test]
    fn status_path_is_a_sibling_of_the_state_file() {
        let path = status_path(Path::new("/var/lib/subscription-governor/state.json"));
        assert_eq!(
            path,
            PathBuf::from("/var/lib/subscription-governor/status.json")
        );
    }
}
