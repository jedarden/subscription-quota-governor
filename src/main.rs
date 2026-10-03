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
use subscription_governor::config::{Config, ResourceSourceConfig, SourceConfig};
use subscription_governor::controller::{evaluate, Decision};
use subscription_governor::fleet;
use subscription_governor::model::ResourceSnapshot;
use subscription_governor::placement::{self, HostPlacement};
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
    /// Read a no-secret inventory and reject overlapping controller ownership.
    Preflight {
        /// JSON inventory assembled from the read-only host inspection procedure.
        #[arg(short, long)]
        inventory: PathBuf,
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
    if let Commands::Preflight { inventory } = &cli.command {
        return subscription_governor::preflight::run(inventory)
            .map_err(GovernorError::CliOrConfig);
    }
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
        Commands::Preflight { .. } => unreachable!("preflight handled before config loading"),
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
    let mut scheduled_at: Option<Instant> = None;
    let status_path = status_path(&state_path);
    loop {
        let cycle_started_at = Instant::now();
        let (late, skipped_cycles) = schedule_drift(scheduled_at, cycle_started_at, interval);
        let outcome = run_cycle(&config, &mut state, observe_only, &shutdown);
        println!(
            "{}",
            serde_json::to_string(&json!({
                "event": "cycle_metrics",
                "time": Utc::now(),
                "duration_ms": cycle_started_at.elapsed().as_millis() as u64,
                "late": late,
                "skipped_cycles": skipped_cycles,
                "accounts_total": config.accounts.len(),
                "observation_failures": outcome.observation_failures,
                "actuation_failures": outcome.actuation_failures,
            }))
            .expect("cycle_metrics always serializes")
        );
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
        scheduled_at = Some(next_anchor);
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

/// The maximum delay `bounded_jitter` can add for `interval` -- 10%, capped
/// at 30s. Factored out so `schedule_drift`'s late threshold matches
/// `bounded_jitter`'s actual ceiling exactly, without depending on which
/// random value a given call produced.
fn max_bounded_jitter(interval: Duration) -> Duration {
    interval.mul_f64(0.1).min(Duration::from_secs(30))
}

/// A random delay up to 10% of `interval` (capped at 30s) so that many
/// accounts/instances on the same interval don't all poll their upstream
/// providers at the same moment. The exact range is a deliberately
/// conservative placeholder pending observation-mode evidence (plan.md §21).
fn bounded_jitter(interval: Duration) -> Duration {
    let max_jitter = max_bounded_jitter(interval);
    if max_jitter.is_zero() {
        return Duration::ZERO;
    }
    let millis = rand::thread_rng().gen_range(0..=max_jitter.as_millis() as u64);
    Duration::from_millis(millis)
}

/// How far a cycle's actual start lagged the schedule, for the §13 "loop
/// duration and skipped/late cycles" metric. `scheduled_at` is `None` for
/// the very first cycle, which has no schedule to be late against, and
/// whenever `interval` is zero (nothing to be late relative to). A cycle
/// counts as "late" only once the drift exceeds the maximum jitter
/// `bounded_jitter` could have added for this `interval` -- an ordinary
/// jittered wake is not a late cycle, only a cycle delayed by something else
/// (typically the previous cycle overrunning its interval). `skipped_cycles`
/// counts whole additional intervals that elapsed on top of that before this
/// cycle started (0 in the ordinary case).
fn schedule_drift(
    scheduled_at: Option<Instant>,
    started_at: Instant,
    interval: Duration,
) -> (bool, u64) {
    if interval.is_zero() {
        return (false, 0);
    }
    let Some(scheduled_at) = scheduled_at else {
        return (false, 0);
    };
    let drift = started_at.saturating_duration_since(scheduled_at);
    let late = drift > max_bounded_jitter(interval);
    let skipped_cycles = (drift.as_secs_f64() / interval.as_secs_f64()).floor() as u64;
    (late, skipped_cycles)
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

/// One account's §13 "planned metrics" line, emitted exactly once per
/// account per cycle regardless of success or failure -- unlike the
/// "decision" event (emitted only on success) and "account_error" event
/// (emitted only on failure), so a metrics consumer never has to correlate
/// two different event shapes just to count source/actuation success and
/// failure. Fields populated only as far as the cycle actually got are left
/// absent rather than defaulted, so e.g. a source failure never reports a
/// misleading `current_workers`.
#[derive(Debug, Serialize)]
struct AccountMetrics {
    event: &'static str,
    time: DateTime<Utc>,
    account: String,
    source_success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    sample_age_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_workers: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    desired_workers: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decision_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    binding_window: Option<String>,
    windows: Vec<WindowMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hosts: Option<Vec<HostMetrics>>,
    actuation_attempted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    actuation_succeeded: Option<bool>,
}

#[derive(Debug, Serialize)]
struct WindowMetrics {
    id: String,
    used_fraction: f64,
    target_utilization: f64,
    seconds_until_reset: u64,
}

#[derive(Debug, Serialize)]
struct HostMetrics {
    host_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_utilization: Option<HostResourceUtilization>,
    placed_workers: u32,
}

#[derive(Debug, Serialize)]
struct HostResourceUtilization {
    cpu_used_fraction: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_used_fraction: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
struct HostDecisionRecord {
    host_id: String,
    resource_snapshot: Option<ResourceSnapshot>,
    current_workers: u32,
    headroom: f64,
    fresh: bool,
    eligible: bool,
    target_workers: u32,
}

impl AccountMetrics {
    fn new(account: String, time: DateTime<Utc>) -> Self {
        Self {
            event: "metrics",
            time,
            account,
            source_success: false,
            sample_age_seconds: None,
            current_workers: None,
            desired_workers: None,
            decision_reason: None,
            binding_window: None,
            windows: Vec::new(),
            hosts: None,
            actuation_attempted: false,
            actuation_succeeded: None,
        }
    }

    /// Fills in every field a successfully computed `decision` makes
    /// available: desired workers, the account-level decision reason (the
    /// same derivation `readiness_for_decision` uses, for consistency with
    /// the status surface), the binding window, and one `WindowMetrics` per
    /// observed window with `seconds_until_reset` computed relative to
    /// `now` and clamped to zero the same way `evaluate`'s own
    /// `sample_age_seconds` is (a reset already in the past, per §9.3
    /// `reset_due`, is reported as zero seconds away, never negative).
    fn apply_decision(&mut self, decision: &Decision, decision_reason: String, now: DateTime<Utc>) {
        self.desired_workers = Some(decision.desired_workers);
        self.decision_reason = Some(decision_reason);
        self.binding_window = decision.binding_window.clone();
        self.windows = decision
            .windows
            .iter()
            .map(|window| WindowMetrics {
                id: window.id.clone(),
                used_fraction: window.used_fraction,
                target_utilization: window.target_utilization,
                seconds_until_reset: window
                    .resets_at
                    .signed_duration_since(now)
                    .num_seconds()
                    .max(0) as u64,
            })
            .collect();
    }

    fn apply_host_placements(&mut self, hosts: &[HostDecisionRecord]) {
        self.hosts = Some(
            hosts
                .iter()
                .map(|host| HostMetrics {
                    host_id: host.host_id.clone(),
                    resource_utilization: host.resource_snapshot.as_ref().map(|snapshot| {
                        HostResourceUtilization {
                            cpu_used_fraction: 1.0 - snapshot.cpu_available_fraction,
                            memory_used_fraction: (snapshot.mem_total_mb > 0).then(|| {
                                1.0 - snapshot.mem_available_mb as f64
                                    / snapshot.mem_total_mb as f64
                            }),
                        }
                    }),
                    placed_workers: host.target_workers,
                })
                .collect(),
        );
    }
}

fn resource_source_as_source(source: &ResourceSourceConfig) -> SourceConfig {
    match source {
        ResourceSourceConfig::NormalizedFile { path } => {
            SourceConfig::NormalizedFile { path: path.clone() }
        }
        ResourceSourceConfig::NormalizedHttp {
            url,
            timeout_seconds,
        } => SourceConfig::NormalizedHttp {
            url: url.clone(),
            timeout_seconds: *timeout_seconds,
        },
        ResourceSourceConfig::Command { argv } => SourceConfig::Command { argv: argv.clone() },
    }
}

fn collect_host_resources(
    account: &str,
    account_config: &subscription_governor::config::AccountConfig,
) -> BTreeMap<String, ResourceSnapshot> {
    let mut resources = BTreeMap::new();
    if let Some(hosts) = &account_config.fleet.hosts {
        for (host_id, host) in hosts {
            let Some(resource_source) = &host.resource_source else {
                continue;
            };
            let result = source::collect_resource(&resource_source_as_source(resource_source))
                .and_then(|snapshot| {
                    if snapshot.host_id == *host_id {
                        Ok(snapshot)
                    } else {
                        Err(anyhow!(
                            "resource snapshot host_id {} does not match configured host {host_id}",
                            snapshot.host_id
                        ))
                    }
                });
            match result {
                Ok(snapshot) => {
                    resources.insert(host_id.clone(), snapshot);
                }
                Err(_) => {
                    eprintln!(
                        "{}",
                        json!({
                            "event": "host_resource_error",
                            "account": account,
                            "host_id": host_id,
                            "time": Utc::now(),
                            "category": "resource_observation",
                        })
                    );
                }
            }
        }
    }
    resources
}

fn host_decision_records(
    placements: &[HostPlacement],
    resources: &BTreeMap<String, ResourceSnapshot>,
) -> Vec<HostDecisionRecord> {
    placements
        .iter()
        .map(|placement| HostDecisionRecord {
            host_id: placement.host_id.clone(),
            resource_snapshot: resources.get(&placement.host_id).cloned(),
            current_workers: placement.current,
            headroom: placement.headroom,
            fresh: placement.fresh,
            eligible: placement.eligible,
            target_workers: placement.target,
        })
        .collect()
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
        let now = Utc::now();
        let mut metrics = AccountMetrics::new(name.clone(), now);
        let mut host_current: Option<BTreeMap<String, u32>> = None;
        let mut host_placements: Option<Vec<HostPlacement>> = None;
        let mut host_records: Option<Vec<HostDecisionRecord>> = None;
        let mut host_actuation: Option<BTreeMap<String, fleet::ActuationOutcome>> = None;
        let result: Result<(), AccountFailure> = (|| {
            let snapshot =
                source::collect(&account_config.source).map_err(AccountFailure::Observation)?;
            metrics.source_success = true;
            metrics.sample_age_seconds = Some(
                now.signed_duration_since(snapshot.observed_at)
                    .num_seconds()
                    .max(0) as u64,
            );
            let workers = if account_config
                .fleet
                .hosts
                .as_ref()
                .is_some_and(|hosts| !hosts.is_empty())
            {
                let current = fleet::current_host_workers(account_config)
                    .map_err(AccountFailure::Observation)?;
                let total = fleet::total_host_workers(&account_config.fleet, &current)
                    .map_err(AccountFailure::Observation)?;
                host_current = Some(current);
                total
            } else {
                fleet::current_workers(&account_config.fleet)
                    .map_err(AccountFailure::Observation)?
            };
            metrics.current_workers = Some(workers);
            let prior = state.accounts.get(name).cloned().unwrap_or_default();
            let decision = evaluate(name, account_config, &snapshot, &prior, workers, now)
                .map_err(AccountFailure::Observation)?;
            let readiness = readiness_for_decision(&decision);
            metrics.apply_decision(&decision, readiness.reason.clone(), now);

            if let Some(current) = &host_current {
                let resources = collect_host_resources(name, account_config);
                let placements = placement::place(
                    name,
                    decision.desired_workers,
                    account_config,
                    current,
                    &resources,
                    now,
                )
                .map_err(AccountFailure::Observation)?;
                let records = host_decision_records(&placements, &resources);
                metrics.apply_host_placements(&records);
                host_records = Some(records);
                host_placements = Some(placements);
            }

            metrics.actuation_attempted = !observe_only;
            let actuated = if observe_only {
                false
            } else if let Some(placements) = &host_placements {
                let outcomes = fleet::actuate_host_targets(account_config, placements)
                    .map_err(AccountFailure::Actuation)?;
                metrics.actuation_succeeded = Some(true);
                let any_actuated = outcomes.values().any(|outcome| outcome.actuated);
                host_actuation = Some(outcomes);
                any_actuated
            } else {
                let actuation =
                    fleet::actuate(&account_config.fleet, workers, decision.desired_workers)
                        .map_err(AccountFailure::Actuation)?;
                metrics.actuation_succeeded = Some(true);
                actuation.actuated
            };
            let mut event = json!({
                "event": "decision",
                "observe_only": observe_only,
                "actuated": actuated,
                "decision": decision,
            });
            if let Some(hosts) = &host_records {
                event["hosts"] = serde_json::to_value(hosts)
                    .map_err(|error| AccountFailure::Observation(error.into()))?;
            }
            println!(
                "{}",
                serde_json::to_string(&event)
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
                if let (Some(current), Some(placements)) = (&host_current, &host_placements) {
                    for placement in placements {
                        let sample_workers = if host_actuation
                            .as_ref()
                            .and_then(|outcomes| outcomes.get(&placement.host_id))
                            .is_some_and(|outcome| outcome.actuated)
                        {
                            placement.target
                        } else {
                            current[&placement.host_id]
                        };
                        state.record_host(name, &placement.host_id, &snapshot, sample_workers);
                    }
                }
            }
            outcome.statuses.insert(name.clone(), readiness);
            Ok(())
        })();
        if let Err(failure) = result {
            match failure.category() {
                "actuation" => {
                    outcome.actuation_failures += 1;
                    metrics.actuation_succeeded = Some(false);
                }
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
        println!(
            "{}",
            serde_json::to_string(&metrics).expect("AccountMetrics always serializes")
        );
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

    #[test]
    fn schedule_drift_is_never_late_for_the_first_cycle() {
        let interval = Duration::from_secs(10);
        let started_at = Instant::now();
        assert_eq!(schedule_drift(None, started_at, interval), (false, 0));
    }

    #[test]
    fn schedule_drift_is_never_late_for_a_zero_interval() {
        let scheduled_at = Instant::now();
        let started_at = scheduled_at + Duration::from_secs(999);
        assert_eq!(
            schedule_drift(Some(scheduled_at), started_at, Duration::ZERO),
            (false, 0)
        );
    }

    #[test]
    fn schedule_drift_is_not_late_within_the_jitter_window() {
        let interval = Duration::from_secs(10);
        let scheduled_at = Instant::now();
        // max_bounded_jitter(10s) == 1s; landing exactly at that ceiling is
        // still an ordinary jittered wake, not lateness.
        let started_at = scheduled_at + max_bounded_jitter(interval);
        assert_eq!(
            schedule_drift(Some(scheduled_at), started_at, interval),
            (false, 0)
        );
    }

    #[test]
    fn schedule_drift_flags_late_beyond_the_jitter_window_without_skipping_a_whole_interval() {
        let interval = Duration::from_secs(10);
        let scheduled_at = Instant::now();
        let started_at = scheduled_at + max_bounded_jitter(interval) + Duration::from_millis(1);
        let (late, skipped_cycles) = schedule_drift(Some(scheduled_at), started_at, interval);
        assert!(late);
        assert_eq!(skipped_cycles, 0);
    }

    #[test]
    fn schedule_drift_counts_whole_skipped_intervals() {
        let interval = Duration::from_secs(10);
        let scheduled_at = Instant::now();
        let started_at = scheduled_at + Duration::from_secs(25);
        let (late, skipped_cycles) = schedule_drift(Some(scheduled_at), started_at, interval);
        assert!(late);
        assert_eq!(skipped_cycles, 2);
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

    // The test above uses AccountState::default() as the "prior" it expects
    // back, which can't distinguish "genuinely left untouched" from "reset to
    // something that happens to look like default" -- a real regression
    // (e.g. record() running before the actuate() error is checked) could
    // slip through if the incoming snapshot/decision ever produced a
    // default-shaped AccountState by coincidence. Use a prior with concrete,
    // non-default history/windows/last_target -- values a buggy cycle would
    // visibly clobber with this cycle's own (unactuated) observation -- so
    // an equality failure here can only mean the state was actually mutated.
    #[test]
    fn run_cycle_does_not_advance_a_richly_populated_prior_state_on_actuation_failure() {
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

        let prior = subscription_governor::state::AccountState {
            last_target: Some(7),
            windows: BTreeMap::from([(
                "five_hour".to_string(),
                subscription_governor::state::WindowSample {
                    observed_at: Utc::now() - chrono::Duration::hours(1),
                    used_fraction: 0.42,
                    resets_at: Utc::now() + chrono::Duration::hours(1),
                    workers: 2,
                },
            )]),
            ..Default::default()
        };

        let mut state = State::default();
        state.accounts.insert("a".to_string(), prior.clone());
        let shutdown = AtomicBool::new(false);
        let outcome = run_cycle(&config, &mut state, false, &shutdown);

        assert_eq!(outcome.actuation_failures, 1);
        assert_eq!(
            state.accounts.get("a"),
            Some(&prior),
            "a failed actuation must never overwrite the account's prior recorded state"
        );
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

#[cfg(test)]
mod metrics_tests {
    use super::*;
    use subscription_governor::controller::WindowDecision;

    fn window(id: &str, resets_at: DateTime<Utc>) -> WindowDecision {
        WindowDecision {
            id: id.to_owned(),
            used_fraction: 0.42,
            target_utilization: 0.9,
            resets_at,
            desired_workers: 2,
            reason: "paced_to_reset".to_owned(),
            observed_burn_per_worker_hour: Some(0.01),
        }
    }

    fn decision(windows: Vec<WindowDecision>) -> Decision {
        Decision {
            account: "a".to_owned(),
            observed_at: Utc::now(),
            current_workers: 1,
            desired_workers: 3,
            stale: false,
            windows,
            binding_window: Some("weekly".to_owned()),
            banked_resets: None,
        }
    }

    #[test]
    fn a_fresh_metrics_line_reports_only_the_event_and_account_before_any_step_succeeds() {
        let now = Utc::now();
        let metrics = AccountMetrics::new("a".to_owned(), now);
        let value = serde_json::to_value(&metrics).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object["event"], "metrics");
        assert_eq!(object["account"], "a");
        assert_eq!(object["source_success"], false);
        assert_eq!(object["actuation_attempted"], false);
        assert!(object["windows"].as_array().unwrap().is_empty());
        assert!(
            !object.contains_key("hosts"),
            "single-host metrics retain their existing JSON shape"
        );
        for absent in [
            "sample_age_seconds",
            "current_workers",
            "desired_workers",
            "decision_reason",
            "binding_window",
            "actuation_succeeded",
        ] {
            assert!(
                !object.contains_key(absent),
                "unpopulated field {absent:?} must be omitted, not printed as null"
            );
        }
    }

    #[test]
    fn apply_decision_fills_desired_workers_reason_and_binding_window() {
        let now = Utc::now();
        let mut metrics = AccountMetrics::new("a".to_owned(), now);
        let d = decision(vec![window("weekly", now + chrono::Duration::hours(2))]);
        metrics.apply_decision(&d, "paced_to_reset".to_owned(), now);
        assert_eq!(metrics.desired_workers, Some(3));
        assert_eq!(metrics.decision_reason.as_deref(), Some("paced_to_reset"));
        assert_eq!(metrics.binding_window.as_deref(), Some("weekly"));
        assert_eq!(metrics.windows.len(), 1);
        assert_eq!(metrics.windows[0].id, "weekly");
        assert_eq!(metrics.windows[0].seconds_until_reset, 2 * 3600);
    }

    #[test]
    fn apply_decision_clamps_seconds_until_reset_to_zero_for_a_reset_already_in_the_past() {
        let now = Utc::now();
        let mut metrics = AccountMetrics::new("a".to_owned(), now);
        let d = decision(vec![window(
            "five_hour",
            now - chrono::Duration::minutes(5),
        )]);
        metrics.apply_decision(&d, "reset_due".to_owned(), now);
        assert_eq!(metrics.windows[0].seconds_until_reset, 0);
    }
}
