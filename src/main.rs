use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;
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
    loop {
        let failures = run_cycle(&config, &mut state, observe_only);
        state.save(&state_path)?;
        if once {
            if failures > 0 {
                bail!("{failures} account(s) failed");
            }
            return Ok(());
        }
        thread::sleep(Duration::from_secs(config.poll_interval_seconds));
    }
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
