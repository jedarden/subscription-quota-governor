use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use rand::Rng;
use serde_json::json;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use subscription_governor::config::{ActuatorConfig, Config};
use subscription_governor::controller::evaluate;
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

fn run(config: Config, once: bool, observe_only: bool) -> Result<(), GovernorError> {
    let state_path = config.state_path();
    let _lock = StateLock::acquire(&state_path).map_err(GovernorError::State)?;
    let mut state = State::load(&state_path).map_err(GovernorError::State)?;
    let shutdown = install_shutdown_flag().map_err(GovernorError::State)?;
    let interval = Duration::from_secs(config.poll_interval_seconds);
    let mut anchor = Instant::now();
    loop {
        let outcome = run_cycle(&config, &mut state, observe_only, &shutdown);
        state.save(&state_path).map_err(GovernorError::State)?;
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
/// (exit 5).
#[derive(Debug, Default)]
struct CycleOutcome {
    observation_failures: usize,
    actuation_failures: usize,
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

            let changed = decision.desired_workers != workers;
            let has_actuator = !matches!(&account_config.fleet.actuator, ActuatorConfig::None);
            let actuated = changed && !observe_only && has_actuator;
            if actuated {
                fleet::actuate(&account_config.fleet, decision.desired_workers)
                    .map_err(AccountFailure::Actuation)?;
            }
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
            Ok(())
        })();
        if let Err(failure) = result {
            match failure.category() {
                "actuation" => outcome.actuation_failures += 1,
                _ => outcome.observation_failures += 1,
            }
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
        AccountConfig, BankedResetConfig, FleetConfig, SourceConfig, StaleBehavior, Strategy,
        UtilizationConfig, WorkerObserverConfig,
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
                observer: WorkerObserverConfig::Static { workers: 1 },
                actuator: ActuatorConfig::None,
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
        AccountConfig, BankedResetConfig, FleetConfig, SourceConfig, StaleBehavior, Strategy,
        UtilizationConfig, WorkerObserverConfig,
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
                observer: WorkerObserverConfig::Static { workers: 0 },
                actuator: ActuatorConfig::Command {
                    argv: vec!["/nonexistent/subgov-test-actuator".to_string()],
                },
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
}
