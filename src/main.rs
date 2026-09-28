use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use rand::Rng;
use serde_json::json;
use std::path::PathBuf;
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    match cli.command {
        Commands::Check => {
            println!(
                "configuration is valid ({} accounts)",
                config.accounts.len()
            );
            Ok(())
        }
        Commands::Snapshot { account } => {
            let account_config = config
                .accounts
                .get(&account)
                .with_context(|| format!("unknown account {account}"))?;
            let snapshot = source::collect(&account_config.source)?;
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
            Ok(())
        }
        Commands::Run { once, observe_only } => run(config, once, observe_only),
    }
}

fn run(config: Config, once: bool, observe_only: bool) -> Result<()> {
    let state_path = config.state_path();
    let _lock = StateLock::acquire(&state_path)?;
    let mut state = State::load(&state_path)?;
    let interval = Duration::from_secs(config.poll_interval_seconds);
    let mut anchor = Instant::now();
    loop {
        let failures = run_cycle(&config, &mut state, observe_only);
        state.save(&state_path)?;
        if once {
            if failures > 0 {
                bail!("{failures} account(s) failed");
            }
            return Ok(());
        }
        let (next_anchor, sleep_for) = advance_schedule(anchor, interval, Instant::now());
        anchor = next_anchor;
        thread::sleep(sleep_for + bounded_jitter(interval));
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

fn run_cycle(config: &Config, state: &mut State, observe_only: bool) -> usize {
    let mut failures = 0;
    for (name, account_config) in &config.accounts {
        let result = (|| -> Result<()> {
            let snapshot = source::collect(&account_config.source)?;
            let workers = fleet::current_workers(&account_config.fleet)?;
            let prior = state.accounts.get(name).cloned().unwrap_or_default();
            let decision = evaluate(name, account_config, &snapshot, &prior, workers, Utc::now())?;

            let changed = decision.desired_workers != workers;
            let has_actuator = !matches!(&account_config.fleet.actuator, ActuatorConfig::None);
            let actuated = changed && !observe_only && has_actuator;
            if actuated {
                fleet::actuate(&account_config.fleet, decision.desired_workers)?;
            }
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "event": "decision",
                    "observe_only": observe_only,
                    "actuated": actuated,
                    "decision": decision,
                }))?
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
        if let Err(error) = result {
            failures += 1;
            eprintln!(
                "{}",
                json!({"event": "account_error", "account": name, "error": format!("{error:#}")})
            );
        }
    }
    failures
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
