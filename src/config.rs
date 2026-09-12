use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_seconds: u64,
    #[serde(default)]
    pub state_path: Option<PathBuf>,
    pub accounts: BTreeMap<String, AccountConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    pub source: SourceConfig,
    pub fleet: FleetConfig,
    pub utilization: UtilizationConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceConfig {
    NormalizedFile {
        path: PathBuf,
    },
    NormalizedHttp {
        url: String,
        #[serde(default = "default_timeout")]
        timeout_seconds: u64,
    },
    Command {
        argv: Vec<String>,
    },
    AnthropicOauth {
        #[serde(default = "default_claude_credentials")]
        credentials_path: PathBuf,
        #[serde(default = "default_anthropic_usage_url")]
        usage_url: String,
        #[serde(default = "default_anthropic_token_url")]
        token_url: String,
        #[serde(default = "default_timeout")]
        timeout_seconds: u64,
    },
    CodexAppServer {
        #[serde(default = "default_codex_executable")]
        executable: PathBuf,
        #[serde(default = "default_timeout")]
        timeout_seconds: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FleetConfig {
    #[serde(default)]
    pub min_workers: u32,
    pub max_workers: u32,
    #[serde(default = "default_step")]
    pub bootstrap_workers: u32,
    #[serde(default = "default_step")]
    pub max_scale_up_per_cycle: u32,
    #[serde(default = "default_step")]
    pub max_scale_down_per_cycle: u32,
    pub observer: WorkerObserverConfig,
    #[serde(default)]
    pub actuator: ActuatorConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerObserverConfig {
    Static { workers: u32 },
    File { path: PathBuf },
    Command { argv: Vec<String> },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActuatorConfig {
    #[default]
    None,
    TargetFile {
        path: PathBuf,
    },
    Command {
        argv: Vec<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UtilizationConfig {
    #[serde(default)]
    pub target_utilization: Option<f64>,
    #[serde(default)]
    pub reserve_fraction: Option<f64>,
    #[serde(default)]
    pub strategy: Strategy,
    #[serde(default = "default_stale_after")]
    pub stale_after_seconds: u64,
    #[serde(default)]
    pub stale_behavior: StaleBehavior,
    #[serde(default = "default_min_sample")]
    pub minimum_sample_seconds: u64,
    #[serde(default)]
    pub windows: BTreeMap<String, WindowPolicy>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    #[default]
    LinearToReset,
    CeilingOnly,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleBehavior {
    #[default]
    Hold,
    MinWorkers,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WindowPolicy {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub target_utilization: Option<f64>,
    #[serde(default)]
    pub reserve_fraction: Option<f64>,
    #[serde(default)]
    pub strategy: Option<Strategy>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes =
            fs::read(path).with_context(|| format!("failed to read config {}", path.display()))?;
        let mut config: Self = serde_yaml::from_slice(&bytes)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        if let Some(state_path) = &config.state_path {
            config.state_path = Some(expand_tilde(state_path));
        }
        for account in config.accounts.values_mut() {
            account.source.expand_paths();
            account.fleet.expand_paths();
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported config version {}; expected 1", self.version);
        }
        if self.accounts.is_empty() {
            bail!("at least one account is required");
        }
        if self.poll_interval_seconds == 0 {
            bail!("poll_interval_seconds must be positive");
        }
        for (name, account) in &self.accounts {
            if account.fleet.max_workers < account.fleet.min_workers {
                bail!("account {name}: max_workers must be >= min_workers");
            }
            if account.fleet.bootstrap_workers > account.fleet.max_workers {
                bail!("account {name}: bootstrap_workers must be <= max_workers");
            }
            validate_target(
                account.utilization.target_utilization,
                account.utilization.reserve_fraction,
                &format!("account {name}"),
                true,
            )?;
            for (window, policy) in &account.utilization.windows {
                validate_target(
                    policy.target_utilization,
                    policy.reserve_fraction,
                    &format!("account {name} window {window}"),
                    false,
                )?;
            }
            validate_source(&account.source, name)?;
            validate_fleet(&account.fleet, name)?;
        }
        Ok(())
    }

    pub fn state_path(&self) -> PathBuf {
        self.state_path.clone().unwrap_or_else(|| {
            dirs::state_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("subscription-governor/state.json")
        })
    }
}

impl SourceConfig {
    fn expand_paths(&mut self) {
        match self {
            Self::NormalizedFile { path } => *path = expand_tilde(path),
            Self::AnthropicOauth {
                credentials_path, ..
            } => *credentials_path = expand_tilde(credentials_path),
            Self::CodexAppServer { executable, .. } => *executable = expand_tilde(executable),
            Self::Command { .. } | Self::NormalizedHttp { .. } => {}
        }
    }
}

impl FleetConfig {
    fn expand_paths(&mut self) {
        match &mut self.observer {
            WorkerObserverConfig::File { path } => *path = expand_tilde(path),
            WorkerObserverConfig::Static { .. } | WorkerObserverConfig::Command { .. } => {}
        }
        if let ActuatorConfig::TargetFile { path } = &mut self.actuator {
            *path = expand_tilde(path);
        }
    }
}

impl UtilizationConfig {
    pub fn policy_for(&self, id: &str) -> Option<ResolvedPolicy> {
        let override_policy = self.windows.get(id);
        if override_policy.and_then(|p| p.enabled) == Some(false) {
            return None;
        }
        let target = override_policy
            .and_then(|p| p.target_utilization)
            .or_else(|| override_policy.and_then(|p| p.reserve_fraction.map(|r| 1.0 - r)))
            .or(self.target_utilization)
            .or_else(|| self.reserve_fraction.map(|r| 1.0 - r))
            .expect("validated utilization target");
        Some(ResolvedPolicy {
            target,
            strategy: override_policy
                .and_then(|p| p.strategy.clone())
                .unwrap_or_else(|| self.strategy.clone()),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedPolicy {
    pub target: f64,
    pub strategy: Strategy,
}

fn validate_target(
    target: Option<f64>,
    reserve: Option<f64>,
    context: &str,
    required: bool,
) -> Result<()> {
    if target.is_some() && reserve.is_some() {
        bail!("{context}: set target_utilization or reserve_fraction, not both");
    }
    if required && target.is_none() && reserve.is_none() {
        bail!("{context}: target_utilization or reserve_fraction is required");
    }
    if let Some(value) = target {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) || value == 0.0 {
            bail!("{context}: target_utilization must be in (0, 1]");
        }
    }
    if let Some(value) = reserve {
        if !value.is_finite() || !(0.0..1.0).contains(&value) {
            bail!("{context}: reserve_fraction must be in [0, 1)");
        }
    }
    Ok(())
}

fn validate_source(source: &SourceConfig, account: &str) -> Result<()> {
    if let SourceConfig::Command { argv } = source {
        validate_argv(argv, &format!("account {account} source command"), false)?;
    }
    Ok(())
}

fn validate_fleet(fleet: &FleetConfig, account: &str) -> Result<()> {
    if let WorkerObserverConfig::Command { argv } = &fleet.observer {
        validate_argv(argv, &format!("account {account} observer command"), false)?;
    }
    if let ActuatorConfig::Command { argv } = &fleet.actuator {
        validate_argv(argv, &format!("account {account} actuator command"), true)?;
    }
    Ok(())
}

fn validate_argv(argv: &[String], context: &str, require_placeholder: bool) -> Result<()> {
    if argv.is_empty() || argv.iter().any(String::is_empty) {
        bail!("{context}: argv must contain non-empty arguments");
    }
    if require_placeholder && !argv.iter().any(|arg| arg.contains("{desired_workers}")) {
        bail!("{context}: argv must contain {{desired_workers}}");
    }
    Ok(())
}

pub fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = text.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

fn default_poll_interval() -> u64 {
    300
}
fn default_stale_after() -> u64 {
    900
}
fn default_min_sample() -> u64 {
    300
}
fn default_step() -> u32 {
    1
}
fn default_timeout() -> u64 {
    15
}
fn default_claude_credentials() -> PathBuf {
    PathBuf::from("~/.claude/.credentials.json")
}
fn default_anthropic_usage_url() -> String {
    "https://api.anthropic.com/api/oauth/usage".into()
}
fn default_anthropic_token_url() -> String {
    "https://platform.claude.com/v1/oauth/token".into()
}
fn default_codex_executable() -> PathBuf {
    PathBuf::from("codex")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_becomes_target() {
        let config = UtilizationConfig {
            target_utilization: None,
            reserve_fraction: Some(0.15),
            strategy: Strategy::LinearToReset,
            stale_after_seconds: 900,
            stale_behavior: StaleBehavior::Hold,
            minimum_sample_seconds: 300,
            windows: BTreeMap::new(),
        };
        assert_eq!(config.policy_for("weekly").unwrap().target, 0.85);
    }
}
