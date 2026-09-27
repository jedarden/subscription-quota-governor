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
use subscription_governor::model::QuotaSnapshot;
use subscription_governor::source;
use subscription_governor::state::{PendingResetRedemption, State, StateLock};
use uuid::Uuid;

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
        let failures = run_cycle(&config, &mut state, &state_path, observe_only);
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

fn run_cycle(
    config: &Config,
    state: &mut State,
    state_path: &std::path::Path,
    observe_only: bool,
) -> usize {
    let mut failures = 0;
    for (name, account_config) in &config.accounts {
        let result = (|| -> Result<()> {
            let mut snapshot = source::collect(&account_config.source)?;
            let workers = fleet::current_workers(&account_config.fleet)?;
            let prior = state.accounts.get(name).cloned().unwrap_or_default();
            let mut decision =
                evaluate(name, account_config, &snapshot, &prior, workers, Utc::now())?;

            let pending = state
                .accounts
                .get(name)
                .and_then(|account| account.pending_reset_redemption.clone());
            let should_redeem = account_config.banked_resets.auto_redeem
                && !observe_only
                && (pending.is_some()
                    || decision
                        .banked_resets
                        .as_ref()
                        .is_some_and(|banked| banked.redeem_recommended));
            if should_redeem {
                let redemption = match pending {
                    Some(pending) => pending,
                    None => {
                        let pending = PendingResetRedemption {
                            idempotency_key: Uuid::new_v4().to_string(),
                            credit_id: earliest_available_credit_id(&snapshot),
                            started_at: Utc::now(),
                        };
                        state
                            .accounts
                            .entry(name.clone())
                            .or_default()
                            .pending_reset_redemption = Some(pending.clone());
                        // This write-ahead record must reach disk before the
                        // irreversible provider request is sent.
                        state.save(state_path)?;
                        pending
                    }
                };
                let outcome = source::consume_reset_credit(
                    &account_config.source,
                    &redemption.idempotency_key,
                    redemption.credit_id.as_deref(),
                )?;
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "event": "reset_redemption",
                        "account": name,
                        "outcome": outcome.as_str(),
                    }))?
                );
                if let source::ResetConsumeOutcome::Other(value) = &outcome {
                    bail!(
                        "Codex returned an unrecognized reset outcome {value:?}; retaining the pending idempotency key"
                    );
                }
                state
                    .accounts
                    .entry(name.clone())
                    .or_default()
                    .pending_reset_redemption = None;
                // Reconcile all definitive outcomes. Success requires an
                // authoritative read; failure outcomes can also mean the
                // pre-request balance was stale.
                snapshot = source::collect(&account_config.source)?;
                decision = evaluate(name, account_config, &snapshot, &prior, workers, Utc::now())?;
            }

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

fn earliest_available_credit_id(snapshot: &QuotaSnapshot) -> Option<String> {
    snapshot
        .reset_credits
        .as_ref()?
        .credits
        .as_deref()?
        .iter()
        .filter(|credit| credit.status == "available")
        .min_by_key(|credit| (credit.expires_at.is_none(), credit.expires_at))
        .map(|credit| credit.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use subscription_governor::model::{ResetCredit, ResetCreditsSnapshot};

    #[test]
    fn chooses_earliest_expiring_available_credit() {
        let expiration = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
        let snapshot = QuotaSnapshot {
            observed_at: Utc::now(),
            fresh: true,
            windows: Vec::new(),
            reset_credits: Some(ResetCreditsSnapshot {
                available_count: 3,
                credits: Some(vec![
                    reset_credit("no-expiry", "available", None),
                    reset_credit(
                        "redeemed",
                        "redeemed",
                        Some(expiration - chrono::Duration::days(1)),
                    ),
                    reset_credit("earliest", "available", Some(expiration)),
                    reset_credit(
                        "later",
                        "available",
                        Some(expiration + chrono::Duration::days(1)),
                    ),
                ]),
            }),
        };

        assert_eq!(
            earliest_available_credit_id(&snapshot).as_deref(),
            Some("earliest")
        );
    }

    fn reset_credit(
        id: &str,
        status: &str,
        expires_at: Option<chrono::DateTime<Utc>>,
    ) -> ResetCredit {
        ResetCredit {
            id: id.to_owned(),
            reset_type: Some("weekly".into()),
            status: status.to_owned(),
            granted_at: None,
            expires_at,
            title: None,
            description: None,
        }
    }
}
