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
    #[serde(default)]
    pub banked_resets: BankedResetConfig,
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
    /// Required unless `hosts` is configured (plan.md §22.3: "observer/actuator
    /// here and hosts below are mutually exclusive").
    #[serde(default)]
    pub observer: Option<WorkerObserverConfig>,
    #[serde(default)]
    pub actuator: ActuatorConfig,
    /// How to handle an observed worker count outside `[min_workers,
    /// max_workers]` (plan.md §11.1: "reject counts outside the configured
    /// fleet range unless a documented reconciliation mode is selected").
    #[serde(default)]
    pub observer_reconciliation: ObserverReconciliation,
    /// Per-host placement (plan.md §22.2/§22.3). Optional and additive: an
    /// account with no `hosts` key behaves byte-identically to v0.1, using
    /// `observer`/`actuator` above directly for the account's single implicit
    /// host.
    #[serde(default)]
    pub hosts: Option<BTreeMap<String, HostConfig>>,
}

/// One placement target within an account's fleet (plan.md §22.3).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// Defaults to the account's `max_workers` when absent; validated to
    /// never exceed it.
    #[serde(default)]
    pub max_workers: Option<u32>,
    /// Required whenever `resource_source` is set (§22.3: "there is no safe
    /// default reserve").
    #[serde(default)]
    pub resource_reserve: Option<ResourceReserveConfig>,
    #[serde(default)]
    pub resource_source: Option<ResourceSourceConfig>,
    pub observer: WorkerObserverConfig,
    #[serde(default)]
    pub actuator: ActuatorConfig,
}

/// Headroom no placement may consume on a host (plan.md §22.3/§22.8).
/// Neither field has a safe default -- both are required whenever a host
/// declares a `resource_source` at all.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceReserveConfig {
    pub cpu_reserve_fraction: f64,
    pub mem_reserve_mb: u64,
}

/// Source for a host's §22.4 `ResourceSnapshot`. Reuses the generic
/// file/http/command transport from §7.4 verbatim -- no new transport type
/// (§22.5).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceSourceConfig {
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
}

/// The only two documented reconciliation modes for an observed worker count
/// outside `[min_workers, max_workers]` (plan.md §11.1).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObserverReconciliation {
    /// Fail the cycle for this account rather than act on an out-of-range
    /// observation.
    #[default]
    Strict,
    /// Pull the observed count back into range and proceed. For a fleet
    /// whose real state legitimately drifts outside the configured range
    /// (e.g. another reconciler is also touching it), this keeps the
    /// governor's decisions bounded instead of refusing to run.
    Clamp,
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

/// Policy for earned, one-shot quota resets such as Codex banked resets.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BankedResetConfig {
    /// Raise the account's worker floor while reset credits are available.
    #[serde(default)]
    pub enabled: bool,
    /// Never pace slower than this multiple of one full quota window per its
    /// advertised duration while a reset credit is available.
    #[serde(default = "default_minimum_pace_multiplier")]
    pub minimum_pace_multiplier: f64,
    /// Utilization at which the decision output asks a human to redeem.
    #[serde(default = "default_redeem_at_utilization")]
    pub redeem_at_utilization: f64,
    /// Finish deadline-driven consumption this far before credit expiry.
    #[serde(default = "default_deadline_safety_seconds")]
    pub deadline_safety_seconds: u64,
}

impl Default for BankedResetConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            minimum_pace_multiplier: default_minimum_pace_multiplier(),
            redeem_at_utilization: default_redeem_at_utilization(),
            deadline_safety_seconds: default_deadline_safety_seconds(),
        }
    }
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
            validate_banked_resets(account, name)?;
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
        if let Some(observer) = &mut self.observer {
            expand_observer_path(observer);
        }
        expand_actuator_path(&mut self.actuator);
        if let Some(hosts) = &mut self.hosts {
            for host in hosts.values_mut() {
                host.expand_paths();
            }
        }
    }
}

impl HostConfig {
    fn expand_paths(&mut self) {
        expand_observer_path(&mut self.observer);
        expand_actuator_path(&mut self.actuator);
        if let Some(ResourceSourceConfig::NormalizedFile { path }) = &mut self.resource_source {
            *path = expand_tilde(path);
        }
    }
}

fn expand_observer_path(observer: &mut WorkerObserverConfig) {
    if let WorkerObserverConfig::File { path } = observer {
        *path = expand_tilde(path);
    }
}

fn expand_actuator_path(actuator: &mut ActuatorConfig) {
    if let ActuatorConfig::TargetFile { path } = actuator {
        *path = expand_tilde(path);
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
    match (&fleet.observer, &fleet.hosts) {
        (None, None) => bail!(
            "account {account}: fleet.observer is required when fleet.hosts is not configured"
        ),
        (Some(_), Some(_)) => {
            bail!("account {account}: fleet.observer and fleet.hosts are mutually exclusive")
        }
        _ => {}
    }
    if fleet.hosts.is_some() && !matches!(fleet.actuator, ActuatorConfig::None) {
        bail!("account {account}: fleet.actuator and fleet.hosts are mutually exclusive");
    }
    if let Some(WorkerObserverConfig::Command { argv }) = &fleet.observer {
        validate_argv(argv, &format!("account {account} observer command"), false)?;
    }
    if let ActuatorConfig::Command { argv } = &fleet.actuator {
        validate_argv(argv, &format!("account {account} actuator command"), true)?;
    }
    if let Some(hosts) = &fleet.hosts {
        if hosts.is_empty() {
            bail!("account {account}: fleet.hosts requires at least one host when present");
        }
        for (host_name, host) in hosts {
            validate_host(host, account, host_name, fleet.max_workers)?;
        }
    }
    Ok(())
}

fn validate_host(
    host: &HostConfig,
    account: &str,
    host_name: &str,
    account_max_workers: u32,
) -> Result<()> {
    if host_name.is_empty() {
        bail!("account {account}: host keys must be non-empty");
    }
    if let Some(host_max_workers) = host.max_workers {
        if host_max_workers > account_max_workers {
            bail!(
                "account {account} host {host_name}: max_workers ({host_max_workers}) must not exceed the account's max_workers ({account_max_workers})"
            );
        }
    }
    if host.resource_source.is_some() && host.resource_reserve.is_none() {
        bail!(
            "account {account} host {host_name}: resource_reserve is required when resource_source is set"
        );
    }
    if let WorkerObserverConfig::Command { argv } = &host.observer {
        validate_argv(
            argv,
            &format!("account {account} host {host_name} observer command"),
            false,
        )?;
    }
    if let ActuatorConfig::Command { argv } = &host.actuator {
        validate_argv(
            argv,
            &format!("account {account} host {host_name} actuator command"),
            true,
        )?;
    }
    if let Some(ResourceSourceConfig::Command { argv }) = &host.resource_source {
        validate_argv(
            argv,
            &format!("account {account} host {host_name} resource_source command"),
            false,
        )?;
    }
    Ok(())
}

fn validate_banked_resets(account: &AccountConfig, name: &str) -> Result<()> {
    let policy = &account.banked_resets;
    if !policy.minimum_pace_multiplier.is_finite() || policy.minimum_pace_multiplier < 1.0 {
        bail!("account {name}: banked_resets.minimum_pace_multiplier must be finite and >= 1");
    }
    if !policy.redeem_at_utilization.is_finite()
        || !(0.0..=1.0).contains(&policy.redeem_at_utilization)
        || policy.redeem_at_utilization == 0.0
    {
        bail!("account {name}: banked_resets.redeem_at_utilization must be in (0, 1]");
    }
    if policy.deadline_safety_seconds > i64::MAX as u64 {
        bail!("account {name}: banked_resets.deadline_safety_seconds is too large");
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
fn default_minimum_pace_multiplier() -> f64 {
    2.0
}
fn default_redeem_at_utilization() -> f64 {
    1.0
}
fn default_deadline_safety_seconds() -> u64 {
    6 * 60 * 60
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

    #[test]
    fn banked_resets_default_to_detection_without_actuation() {
        let policy = BankedResetConfig::default();
        assert!(!policy.enabled);
        assert_eq!(policy.minimum_pace_multiplier, 2.0);
    }

    fn base_config(fleet_yaml: &str) -> String {
        format!(
            r#"
version: 1
accounts:
  acct:
    source:
      type: normalized_file
      path: /tmp/subgov-test-source.json
    utilization:
      reserve_fraction: 0.1
    fleet:
{fleet_yaml}
"#
        )
    }

    fn indent(yaml: &str) -> String {
        yaml.lines()
            .map(|line| format!("      {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// plan.md §22.2: an account with no `hosts` key behaves exactly as it
    /// does in v0.1.
    #[test]
    fn implicit_single_host_config_is_unaffected_by_hosts_support() {
        let yaml = base_config(&indent(
            "max_workers: 4\nobserver: { type: static, workers: 1 }\nactuator: { type: none }",
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        config.validate().unwrap();
        let fleet = &config.accounts["acct"].fleet;
        assert!(fleet.hosts.is_none());
        assert!(fleet.observer.is_some());
    }

    /// plan.md §22.3's two-host example (a local host and an SSH-reached one).
    #[test]
    fn fleet_hosts_parses_the_plan_example() {
        let yaml = base_config(&indent(
            r#"max_workers: 8
hosts:
  codinghome:
    max_workers: 6
    resource_reserve:
      cpu_reserve_fraction: 0.25
      mem_reserve_mb: 4096
    resource_source:
      type: command
      argv: ["/usr/local/bin/resource-probe"]
    observer:
      type: command
      argv: ["/usr/local/bin/count-ai-workers", "acct"]
    actuator:
      type: command
      argv: ["/usr/local/bin/set-ai-worker-target", "acct", "{desired_workers}"]
  lab:
    max_workers: 8
    resource_reserve:
      cpu_reserve_fraction: 0.30
      mem_reserve_mb: 8192
    resource_source:
      type: command
      argv: ["ssh", "lab.tailnet", "resource-probe"]
    observer:
      type: command
      argv: ["ssh", "lab.tailnet", "needle-worker-count", "acct"]
    actuator:
      type: command
      argv: ["ssh", "lab.tailnet", "needle-set-target", "acct", "{desired_workers}"]"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        config.validate().unwrap();
        let fleet = &config.accounts["acct"].fleet;
        assert!(fleet.observer.is_none());
        let hosts = fleet.hosts.as_ref().unwrap();
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts["codinghome"].max_workers, Some(6));
    }

    #[test]
    fn fleet_observer_and_hosts_are_mutually_exclusive() {
        let yaml = base_config(&indent(
            r#"max_workers: 8
observer: { type: static, workers: 1 }
hosts:
  codinghome:
    max_workers: 8
    observer: { type: static, workers: 1 }
    actuator: { type: none }"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn fleet_actuator_and_hosts_are_mutually_exclusive() {
        let yaml = base_config(&indent(
            r#"max_workers: 8
actuator: { type: target_file, path: /tmp/subgov-test-target }
hosts:
  codinghome:
    max_workers: 8
    observer: { type: static, workers: 1 }
    actuator: { type: none }"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn fleet_requires_observer_when_hosts_is_absent() {
        let yaml = base_config(&indent("max_workers: 8"));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("fleet.observer is required"), "{err}");
    }

    #[test]
    fn fleet_hosts_requires_at_least_one_host_when_present() {
        let yaml = base_config(&indent("max_workers: 8\nhosts: {}"));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("at least one host"), "{err}");
    }

    #[test]
    fn host_resource_source_requires_resource_reserve() {
        let yaml = base_config(&indent(
            r#"max_workers: 8
hosts:
  codinghome:
    max_workers: 8
    resource_source:
      type: command
      argv: ["/usr/local/bin/resource-probe"]
    observer: { type: static, workers: 1 }
    actuator: { type: none }"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("resource_reserve is required"), "{err}");
    }

    #[test]
    fn host_max_workers_must_not_exceed_account_max_workers() {
        let yaml = base_config(&indent(
            r#"max_workers: 4
hosts:
  codinghome:
    max_workers: 6
    observer: { type: static, workers: 1 }
    actuator: { type: none }"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("must not exceed"), "{err}");
    }

    #[test]
    fn host_actuator_command_requires_desired_workers_placeholder() {
        let yaml = base_config(&indent(
            r#"max_workers: 8
hosts:
  codinghome:
    max_workers: 8
    observer: { type: static, workers: 1 }
    actuator:
      type: command
      argv: ["/usr/local/bin/set-target", "acct"]"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("{desired_workers}"), "{err}");
    }
}
