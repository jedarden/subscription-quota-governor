use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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
    /// Plan §28.13 (WP27): this instance's federation identity. Absent
    /// entirely means federation of one and is accepted unchanged; WP31
    /// adds `peers`, the staleness bounds and `repo_owners`.
    #[serde(default)]
    pub federation: Option<FederationConfig>,
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
    /// Plan §26.2: roster units and adopted stragglers this pool may retire,
    /// idle-only, once a prefixed replacement in the same pool runs. Empty
    /// (the default) leaves the §23 floor untouched.
    #[serde(default)]
    pub takeover: Vec<TakeoverTarget>,
    /// Plan §26.2: helper program that performs the idle-only retirement,
    /// invoked after each cycle's elastic reconcile. It runs outside this
    /// process because the retirement gate needs `/proc` and signal reach
    /// the governor's own hardening does not provide. Required when
    /// `takeover` is non-empty and meaningless otherwise; the takeover
    /// manifest it reads is part of the deployment, like the roster file it
    /// supersedes.
    #[serde(default)]
    pub takeover_helper: Option<Vec<String>>,
}

/// One worker this pool may take over (plan §26.2). Roster workers carry
/// their systemd unit; a straggler outside systemd (for example a
/// `needle-stable` tmux supervisor) is adopted by identity alone.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TakeoverTarget {
    /// NEEDLE worker identity: the tmux identifier and the heartbeat
    /// filename component. Unique across every pool (plan §26.3: every
    /// worker belongs to exactly one pool).
    pub worker_id: String,
    /// Agent (adapter) the worker runs, as it appears in the heartbeat
    /// identity `<agent>-<worker_id>`.
    pub agent: String,
    /// The systemd roster unit, when the worker has one. Absent for a
    /// straggler, which is retired by signalling its supervisor directly.
    #[serde(default)]
    pub unit: Option<String>,
}

/// Plan §28.13 (WP27): per-instance federation identity. Everything here is
/// read-only input to the peer record; no peer transport or trust decision
/// lives in this block yet.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FederationConfig {
    /// This host's id within the federation (§28.2). Peer records carry it
    /// verbatim, so it must be unique across the federation.
    pub host_id: String,
    /// Where the persisted random `instance_id` lives (§28.4). Defaults to
    /// a sibling of the state file.
    #[serde(default)]
    pub instance_id_file: Option<PathBuf>,
    /// Where the monotonic `record_seq` counter lives (§28.4). Defaults to
    /// a sibling of the state file.
    #[serde(default)]
    pub record_seq_file: Option<PathBuf>,
    /// The WP24 host-capacity adapter's state file, whose `last_evidence`
    /// carries the measured ceiling and launch reservations this instance
    /// publishes (§26.6, §28.4).
    #[serde(default)]
    pub host_capacity_state: Option<PathBuf>,
    /// Per account: the provider's non-secret account identifier whose
    /// truncated hash is that account's `account_fingerprint` (§28.4).
    /// Accounts without an entry fingerprint their configured account
    /// name, which matches today's label-based identity (§28.11).
    #[serde(default)]
    pub account_identities: BTreeMap<String, String>,
    /// The peers this instance pulls (§28.2/§28.3, WP28). Empty -- the
    /// default -- is a federation of one that behaves exactly as before.
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
    /// §28.7: a peer record whose age on this receiver's monotonic clock
    /// exceeds this many seconds makes the peer stale, freezing scale-up.
    /// Defaults to 120 s.
    #[serde(default = "default_peer_stale_after_seconds")]
    pub peer_stale_after_seconds: u64,
    /// §28.7: once a peer's age passes this many seconds it is counted as
    /// zero workers, `peer_expired` is emitted, and scale-up may resume.
    /// Defaults to 30 minutes.
    #[serde(default = "default_peer_expiry_seconds")]
    pub peer_expiry_seconds: u64,
}

/// One configured federation peer (§28.13: `peers: [{host_id, argv}]`).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PeerConfig {
    /// The peer's host id, as its own records must claim it (§28.2).
    pub host_id: String,
    /// The full §28.3 pull argv. Absent means the sanctioned default
    /// `[ssh, -o, ConnectTimeout=<n>, <host_id>, subgov, peer-state, --json]`
    /// (§22.10: bounded ConnectTimeout, key-based auth already on the host).
    #[serde(default)]
    pub argv: Option<Vec<String>>,
}

impl FederationConfig {
    pub fn instance_id_path(&self, config: &Config) -> PathBuf {
        self.instance_id_file
            .clone()
            .unwrap_or_else(|| config.state_path().with_file_name("federation-instance-id"))
    }

    pub fn record_seq_path(&self, config: &Config) -> PathBuf {
        self.record_seq_file
            .clone()
            .unwrap_or_else(|| config.state_path().with_file_name("federation-record-seq"))
    }

    /// §28.7 `peer_stale_after` as a `Duration`.
    pub fn peer_stale_after(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.peer_stale_after_seconds)
    }

    /// §28.7 `peer_expiry` as a `Duration`.
    pub fn peer_expiry(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.peer_expiry_seconds)
    }
}

/// One placement target within an account's fleet (plan.md §22.3).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// Defaults to the account's `max_workers` when absent; validated to
    /// never exceed it.
    #[serde(default)]
    pub max_workers: Option<u32>,
    /// Defaults to the account's `max_scale_up_per_cycle` when absent.
    #[serde(default)]
    pub max_scale_up_per_cycle: Option<u32>,
    /// Defaults to the account's `max_scale_down_per_cycle` when absent.
    #[serde(default)]
    pub max_scale_down_per_cycle: Option<u32>,
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
    Static {
        workers: u32,
    },
    File {
        path: PathBuf,
    },
    Command {
        argv: Vec<String>,
    },
    NeedleStatus {
        /// NEEDLE adapter name whose workers are counted on this host.
        agent: String,
        /// NEEDLE heartbeat directory; defaults to `~/.needle/state/heartbeats`.
        #[serde(default = "default_needle_heartbeat_dir")]
        heartbeat_dir: PathBuf,
        /// Ignore heartbeats older than this many seconds.
        #[serde(default = "default_needle_stale_after_seconds")]
        stale_after_seconds: u64,
        /// Plan §23.3: when set, only heartbeats whose worker id starts with
        /// this prefix are the governor's own; the rest are reported as
        /// unowned (roster) workers and never counted or stopped.
        #[serde(default)]
        identifier_prefix: Option<String>,
    },
}

fn default_needle_heartbeat_dir() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".needle/state/heartbeats"))
        .unwrap_or_else(|| PathBuf::from("~/.needle/state/heartbeats"))
}

fn default_needle_stale_after_seconds() -> u64 {
    60
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
        /// Run each cycle even when quota desired equals observed, for
        /// adapters that reconcile a separate measured host ceiling.
        #[serde(default)]
        reconcile_unchanged: bool,
    },
    NeedleRun {
        repo: PathBuf,
        adapter: String,
        /// Plan §23.3: launch workers as `<prefix><n>` and reconcile only
        /// sessions carrying the prefix. Absent = unscoped (v1 behaviour).
        #[serde(default)]
        identifier_prefix: Option<String>,
        /// Plan §26.5: seed each new worker with the ready workspace that
        /// has the fewest running workers, read from the WP23 demand-state
        /// file and the NEEDLE heartbeat directory. Configure both or
        /// neither; without them `repo` is the permanent single seed.
        #[serde(default)]
        seed_demand_state: Option<PathBuf>,
        #[serde(default)]
        seed_heartbeats: Option<PathBuf>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backpressure: Option<BackpressureConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zai_saturation: Option<ZaiSaturationConfig>,
    #[serde(default)]
    pub windows: BTreeMap<String, WindowPolicy>,
}

/// Explicit opt-in search for the smallest fleet near the throughput plateau.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ZaiSaturationConfig {
    pub step_workers: u32,
    pub dwell_seconds: u64,
    pub minimum_window_seconds: u64,
    pub minimum_attempts: u64,
    pub plateau_fraction: f64,
    pub concurrency_fraction: f64,
    pub reprobe_seconds: u64,
}

impl Default for ZaiSaturationConfig {
    fn default() -> Self {
        Self {
            step_workers: 1,
            dwell_seconds: 600,
            minimum_window_seconds: 300,
            minimum_attempts: 30,
            plateau_fraction: 0.05,
            concurrency_fraction: 0.10,
            reprobe_seconds: 3600,
        }
    }
}

/// Policy for applying provider throttling feedback after quota evaluation (§24.4).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackpressureConfig {
    #[serde(default = "default_backpressure_hold_above")]
    pub hold_above: f64,
    #[serde(default = "default_backpressure_step_down_above")]
    pub step_down_above: f64,
    #[serde(default = "default_backpressure_recover_below")]
    pub recover_below: f64,
    #[serde(default = "default_backpressure_quiet_cycles")]
    pub quiet_cycles: u32,
    #[serde(default = "default_backpressure_signal_loss_release_seconds")]
    pub signal_loss_release_seconds: u64,
}

impl Default for BackpressureConfig {
    fn default() -> Self {
        Self {
            hold_above: default_backpressure_hold_above(),
            step_down_above: default_backpressure_step_down_above(),
            recover_below: default_backpressure_recover_below(),
            quiet_cycles: default_backpressure_quiet_cycles(),
            signal_loss_release_seconds: default_backpressure_signal_loss_release_seconds(),
        }
    }
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
    /// Opt-in rolling measured worker-hour baseline for quantized windows.
    #[serde(default)]
    pub burn_rate_window_seconds: Option<u64>,
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
    /// Use all verified eligible work capacity while banked credits exist.
    /// Requires `enabled`; does not authorize redeeming a credit.
    #[serde(default)]
    pub full_speed: bool,
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
            full_speed: false,
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
        if let Some(federation) = &mut config.federation {
            for path in [
                &mut federation.instance_id_file,
                &mut federation.record_seq_file,
                &mut federation.host_capacity_state,
            ]
            .into_iter()
            .flatten()
            {
                *path = expand_tilde(path);
            }
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
        validate_duration(self.poll_interval_seconds, "poll_interval_seconds")?;
        let mut takeover_workers: BTreeMap<String, String> = BTreeMap::new();
        for (name, account) in &self.accounts {
            if account.fleet.max_workers < account.fleet.min_workers {
                bail!("account {name}: max_workers must be >= min_workers");
            }
            if account.fleet.bootstrap_workers > account.fleet.max_workers {
                bail!("account {name}: bootstrap_workers must be <= max_workers");
            }
            validate_duration(
                account.utilization.stale_after_seconds,
                &format!("account {name}: utilization.stale_after_seconds"),
            )?;
            validate_duration(
                account.utilization.minimum_sample_seconds,
                &format!("account {name}: utilization.minimum_sample_seconds"),
            )?;
            validate_target(
                account.utilization.target_utilization,
                account.utilization.reserve_fraction,
                &format!("account {name}"),
                true,
            )?;
            if let Some(policy) = &account.utilization.zai_saturation {
                if policy.step_workers == 0 || policy.minimum_attempts == 0 {
                    bail!("account {name}: utilization.zai_saturation step_workers and minimum_attempts must be positive");
                }
                for (field, value) in [
                    ("dwell_seconds", policy.dwell_seconds),
                    ("minimum_window_seconds", policy.minimum_window_seconds),
                    ("reprobe_seconds", policy.reprobe_seconds),
                ] {
                    validate_duration(
                        value,
                        &format!("account {name}: utilization.zai_saturation.{field}"),
                    )?;
                }
                for (field, value) in [
                    ("plateau_fraction", policy.plateau_fraction),
                    ("concurrency_fraction", policy.concurrency_fraction),
                ] {
                    if !value.is_finite() || !(0.0..1.0).contains(&value) {
                        bail!(
                            "account {name}: utilization.zai_saturation.{field} must be in [0, 1)"
                        );
                    }
                }
            }
            if let Some(backpressure) = &account.utilization.backpressure {
                validate_backpressure(backpressure, name)?;
            }
            for (window, policy) in &account.utilization.windows {
                if policy.burn_rate_window_seconds.is_some_and(|seconds| {
                    seconds < account.utilization.minimum_sample_seconds
                        || seconds > i64::MAX as u64
                }) {
                    bail!("account {name} window {window}: burn_rate_window_seconds must be >= minimum_sample_seconds and fit i64");
                }
                validate_target(
                    policy.target_utilization,
                    policy.reserve_fraction,
                    &format!("account {name} window {window}"),
                    false,
                )?;
            }
            validate_source(&account.source, name)?;
            validate_fleet(&account.fleet, name)?;
            validate_takeover(&account.fleet, name)?;
            for target in &account.fleet.takeover {
                if let Some(existing) = takeover_workers.get(&target.worker_id) {
                    bail!(
                        "account {name}: takeover worker {} is already listed by account {existing} (plan §26.3: every worker belongs to exactly one pool)",
                        target.worker_id
                    );
                }
                takeover_workers.insert(target.worker_id.clone(), name.clone());
            }
            validate_banked_resets(account, name)?;
        }
        if let Some(federation) = &self.federation {
            validate_federation(federation, self)?;
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

    /// The derived per-cycle readiness report, a sibling of the state file.
    /// Both the `status` command and the federation peer record read it.
    pub fn status_path(&self) -> PathBuf {
        self.state_path().with_file_name("status.json")
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
    match observer {
        WorkerObserverConfig::File { path } => *path = expand_tilde(path),
        WorkerObserverConfig::NeedleStatus { heartbeat_dir, .. } => {
            *heartbeat_dir = expand_tilde(heartbeat_dir)
        }
        WorkerObserverConfig::Static { .. } | WorkerObserverConfig::Command { .. } => {}
    }
}

fn expand_actuator_path(actuator: &mut ActuatorConfig) {
    match actuator {
        ActuatorConfig::TargetFile { path } => *path = expand_tilde(path),
        ActuatorConfig::NeedleRun {
            repo,
            seed_demand_state,
            seed_heartbeats,
            ..
        } => {
            *repo = expand_tilde(repo);
            if let Some(path) = seed_demand_state {
                *path = expand_tilde(path);
            }
            if let Some(path) = seed_heartbeats {
                *path = expand_tilde(path);
            }
        }
        ActuatorConfig::None | ActuatorConfig::Command { .. } => {}
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
            burn_rate_window_seconds: override_policy.and_then(|p| p.burn_rate_window_seconds),
            strategy: override_policy
                .and_then(|p| p.strategy.clone())
                .unwrap_or_else(|| self.strategy.clone()),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedPolicy {
    pub burn_rate_window_seconds: Option<u64>,
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

fn validate_backpressure(policy: &BackpressureConfig, account: &str) -> Result<()> {
    let thresholds = [
        ("recover_below", policy.recover_below),
        ("hold_above", policy.hold_above),
        ("step_down_above", policy.step_down_above),
    ];
    for (field, value) in thresholds {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            bail!("account {account}: utilization.backpressure.{field} must be in [0, 1]");
        }
    }
    if !(policy.recover_below < policy.hold_above && policy.hold_above < policy.step_down_above) {
        bail!("account {account}: utilization.backpressure thresholds must satisfy recover_below < hold_above < step_down_above");
    }
    if policy.quiet_cycles == 0 {
        bail!("account {account}: utilization.backpressure.quiet_cycles must be >= 1");
    }
    validate_duration(
        policy.signal_loss_release_seconds,
        &format!("account {account}: utilization.backpressure.signal_loss_release_seconds"),
    )
}

/// Plan §28.13 (WP27): the identity fields the local peer record needs, and
/// (WP28) the peer list and §28.7 staleness bounds. WP31 extends this with
/// repo_owners, `subgov peers`/`check`, and the state schema migration.
fn validate_federation(federation: &FederationConfig, config: &Config) -> Result<()> {
    if federation.host_id.trim().is_empty() || federation.host_id.chars().any(char::is_whitespace) {
        bail!("federation.host_id must be non-empty and free of whitespace");
    }
    for account in federation.account_identities.keys() {
        if !config.accounts.contains_key(account) {
            bail!("federation.account_identities names account {account}, which is not configured");
        }
    }
    for (account, identifier) in &federation.account_identities {
        if identifier.trim().is_empty() {
            bail!("federation.account_identities.{account} must be non-empty");
        }
    }
    let mut peer_hosts = BTreeSet::new();
    for peer in &federation.peers {
        if peer.host_id.trim().is_empty() || peer.host_id.chars().any(char::is_whitespace) {
            bail!("federation.peers[].host_id must be non-empty and free of whitespace");
        }
        if peer.host_id == federation.host_id {
            bail!(
                "federation.peers lists this instance's own host {}: a peer is another subgov on a different host (plan.md §28.2)",
                peer.host_id
            );
        }
        if !peer_hosts.insert(peer.host_id.clone()) {
            bail!(
                "federation.peers lists host {} more than once",
                peer.host_id
            );
        }
        if let Some(argv) = &peer.argv {
            validate_argv(
                argv,
                &format!("federation peer {} pull command", peer.host_id),
                false,
            )?;
        }
    }
    if federation.peer_stale_after_seconds == 0 {
        bail!("federation.peer_stale_after_seconds must be at least 1 second (plan.md §28.7)");
    }
    if federation.peer_expiry_seconds <= federation.peer_stale_after_seconds {
        bail!(
            "federation.peer_expiry_seconds ({}) must exceed peer_stale_after_seconds ({}): expiry is the recovery path out of a stale freeze (plan.md §28.7)",
            federation.peer_expiry_seconds, federation.peer_stale_after_seconds
        );
    }
    Ok(())
}

fn validate_source(source: &SourceConfig, account: &str) -> Result<()> {
    match source {
        SourceConfig::Command { argv } => {
            validate_argv(argv, &format!("account {account} source command"), false)?;
        }
        SourceConfig::NormalizedHttp {
            timeout_seconds, ..
        }
        | SourceConfig::AnthropicOauth {
            timeout_seconds, ..
        }
        | SourceConfig::CodexAppServer {
            timeout_seconds, ..
        } => {
            validate_duration(
                *timeout_seconds,
                &format!("account {account} source timeout_seconds"),
            )?;
        }
        SourceConfig::NormalizedFile { .. } => {}
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
    if let Some(WorkerObserverConfig::NeedleStatus {
        agent,
        heartbeat_dir,
        stale_after_seconds,
        ..
    }) = &fleet.observer
    {
        validate_needle_status(
            agent,
            heartbeat_dir,
            *stale_after_seconds,
            &format!("account {account} observer needle_status"),
        )?;
    }
    if let ActuatorConfig::Command { argv, .. } = &fleet.actuator {
        validate_argv(argv, &format!("account {account} actuator command"), true)?;
    }
    if let ActuatorConfig::NeedleRun {
        repo,
        adapter,
        identifier_prefix,
        seed_demand_state,
        seed_heartbeats,
    } = &fleet.actuator
    {
        validate_needle_run(
            repo,
            adapter,
            &format!("account {account} actuator needle_run"),
        )?;
        validate_needle_seeds(
            seed_demand_state.as_deref(),
            seed_heartbeats.as_deref(),
            &format!("account {account} actuator needle_run"),
        )?;
        validate_identifier_prefix(
            identifier_prefix.as_deref(),
            &format!("account {account} actuator needle_run"),
        )?;
        if let Some(WorkerObserverConfig::NeedleStatus {
            identifier_prefix: observer_prefix,
            ..
        }) = &fleet.observer
        {
            if observer_prefix != identifier_prefix {
                bail!(
                    "account {account}: needle_status observer and needle_run actuator must use the same identifier_prefix"
                );
            }
        }
    }
    if let Some(WorkerObserverConfig::NeedleStatus {
        identifier_prefix, ..
    }) = &fleet.observer
    {
        validate_identifier_prefix(
            identifier_prefix.as_deref(),
            &format!("account {account} observer needle_status"),
        )?;
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

/// Plan §26.2: a takeover list retires roster workers only through the
/// §23.3 prefix machinery -- a listed worker leaves the floor the moment a
/// prefixed replacement in the same pool runs, so both must be configured,
/// and the same pairing rules as §23.3 apply. The retirement itself runs in
/// the unsandboxed helper.
fn validate_takeover(fleet: &FleetConfig, account: &str) -> Result<()> {
    if fleet.takeover.is_empty() {
        if fleet.takeover_helper.is_some() {
            bail!("account {account}: fleet.takeover_helper is meaningless without fleet.takeover");
        }
        return Ok(());
    }
    if fleet.hosts.is_some() {
        bail!("account {account}: fleet.takeover applies to the account's single fleet, not per-host fleets");
    }
    let ActuatorConfig::NeedleRun {
        identifier_prefix: Some(actuator_prefix),
        ..
    } = &fleet.actuator
    else {
        bail!(
            "account {account}: fleet.takeover requires a needle_run actuator with an identifier_prefix, because a listed worker is retired only once a prefixed replacement in the same pool runs"
        );
    };
    let Some(WorkerObserverConfig::NeedleStatus {
        identifier_prefix: Some(observer_prefix),
        ..
    }) = &fleet.observer
    else {
        bail!(
            "account {account}: fleet.takeover requires a needle_status observer with an identifier_prefix, so listed workers leave the unowned floor (§26.2: the floor shrinks to zero)"
        );
    };
    if observer_prefix != actuator_prefix {
        bail!(
            "account {account}: needle_status observer and needle_run actuator must use the same identifier_prefix"
        );
    }
    let Some(helper) = &fleet.takeover_helper else {
        bail!(
            "account {account}: fleet.takeover requires fleet.takeover_helper to perform the idle-only retirement"
        );
    };
    validate_argv(helper, &format!("account {account} takeover_helper"), false)?;
    let mut listed = std::collections::BTreeSet::new();
    for target in &fleet.takeover {
        validate_takeover_identity(
            &target.worker_id,
            &format!("account {account} takeover worker_id"),
        )?;
        validate_takeover_identity(&target.agent, &format!("account {account} takeover agent"))?;
        if let Some(unit) = &target.unit {
            if unit.as_str() != format!("needle-worker@{}.service", target.worker_id) {
                bail!(
                    "account {account}: takeover unit {unit} does not match worker {}",
                    target.worker_id
                );
            }
        }
        if !listed.insert(target.worker_id.as_str()) {
            bail!(
                "account {account}: takeover lists worker {} more than once",
                target.worker_id
            );
        }
    }
    Ok(())
}

/// The identity charset shared with the deployed roster tooling
/// (zai-fleet.py's IDENTIFIER rule) and with NEEDLE's adapter names: a
/// worker id or agent appears in heartbeat filenames and tmux identifiers,
/// so it must be unambiguous in both.
fn validate_takeover_identity(value: &str, context: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        || !value.chars().next().unwrap().is_ascii_alphanumeric()
    {
        bail!("{context} must start with an ASCII letter or digit and contain only ASCII letters, digits, '.', '_' or '-'");
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
    if let WorkerObserverConfig::NeedleStatus {
        agent,
        heartbeat_dir,
        stale_after_seconds,
        ..
    } = &host.observer
    {
        validate_needle_status(
            agent,
            heartbeat_dir,
            *stale_after_seconds,
            &format!("account {account} host {host_name} observer needle_status"),
        )?;
    }
    if let ActuatorConfig::Command { argv, .. } = &host.actuator {
        validate_argv(
            argv,
            &format!("account {account} host {host_name} actuator command"),
            true,
        )?;
    }
    if let ActuatorConfig::NeedleRun {
        repo,
        adapter,
        seed_demand_state,
        seed_heartbeats,
        ..
    } = &host.actuator
    {
        validate_needle_run(
            repo,
            adapter,
            &format!("account {account} host {host_name} actuator needle_run"),
        )?;
        validate_needle_seeds(
            seed_demand_state.as_deref(),
            seed_heartbeats.as_deref(),
            &format!("account {account} host {host_name} actuator needle_run"),
        )?;
    }
    match &host.resource_source {
        Some(ResourceSourceConfig::Command { argv }) => {
            validate_argv(
                argv,
                &format!("account {account} host {host_name} resource_source command"),
                false,
            )?;
        }
        Some(ResourceSourceConfig::NormalizedHttp {
            timeout_seconds, ..
        }) => {
            validate_duration(
                *timeout_seconds,
                &format!("account {account} host {host_name} resource_source timeout_seconds"),
            )?;
        }
        Some(ResourceSourceConfig::NormalizedFile { .. }) | None => {}
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

/// plan.md §8: "Poll, staleness, sample, and transport durations are positive
/// and bounded." The upper bound only guards against a value that would
/// overflow once treated as an `i64` (chrono durations, deadline arithmetic)
/// -- it is not a guess at a realistic operational ceiling, so a
/// deliberately huge value like examples/all-three.yaml's 10-year
/// `stale_after_seconds` (used to effectively disable staleness) stays valid.
fn validate_duration(value: u64, context: &str) -> Result<()> {
    if value == 0 {
        bail!("{context} must be positive");
    }
    if value > i64::MAX as u64 {
        bail!("{context} is too large");
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

fn validate_needle_run(repo: &Path, adapter: &str, context: &str) -> Result<()> {
    if repo.as_os_str().is_empty() {
        bail!("{context}: repo must not be empty");
    }
    if adapter.is_empty()
        || !adapter
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("{context}: adapter must contain only ASCII letters, digits, '.', '_' or '-'");
    }
    Ok(())
}

/// Plan §26.5 seed selection is configured as a pair: ready workspaces come
/// from the WP23 demand state, running-worker occupancy from the heartbeat
/// directory. Half a pair would silently change what the other half means.
fn validate_needle_seeds(
    demand_state: Option<&Path>,
    heartbeats: Option<&Path>,
    context: &str,
) -> Result<()> {
    match (demand_state, heartbeats) {
        (None, None) => Ok(()),
        (Some(demand_state), Some(heartbeats)) => {
            for path in [demand_state, heartbeats] {
                if path.as_os_str().is_empty() {
                    bail!("{context}: seed_demand_state and seed_heartbeats must not be empty");
                }
            }
            Ok(())
        }
        _ => bail!("{context}: seed_demand_state and seed_heartbeats must be configured together"),
    }
}

fn validate_needle_status(
    agent: &str,
    heartbeat_dir: &Path,
    stale_after_seconds: u64,
    context: &str,
) -> Result<()> {
    if agent.is_empty()
        || !agent
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("{context}: agent must contain only ASCII letters, digits, '.', '_' or '-'");
    }
    if heartbeat_dir.as_os_str().is_empty() {
        bail!("{context}: heartbeat_dir must not be empty");
    }
    validate_duration(
        stale_after_seconds,
        &format!("{context} stale_after_seconds"),
    )
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
/// §28.7 `peer_stale_after` default: 120 s.
fn default_peer_stale_after_seconds() -> u64 {
    120
}
/// §28.7 `peer_expiry` default: 30 min.
fn default_peer_expiry_seconds() -> u64 {
    1_800
}
fn default_stale_after() -> u64 {
    900
}
fn default_min_sample() -> u64 {
    300
}
fn default_backpressure_hold_above() -> f64 {
    0.20
}
fn default_backpressure_step_down_above() -> f64 {
    0.50
}
fn default_backpressure_recover_below() -> f64 {
    0.10
}
fn default_backpressure_quiet_cycles() -> u32 {
    3
}
fn default_backpressure_signal_loss_release_seconds() -> u64 {
    1_800
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

/// Plan §23.3: an identifier prefix becomes part of a tmux session name and
/// a heartbeat filename, so it is restricted to a conservative character set
/// and must be non-empty when present.
fn validate_identifier_prefix(prefix: Option<&str>, context: &str) -> Result<()> {
    let Some(prefix) = prefix else {
        return Ok(());
    };
    if prefix.is_empty() || prefix.len() > 32 {
        bail!("{context}: identifier_prefix must be 1-32 characters");
    }
    if !prefix
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        bail!("{context}: identifier_prefix may contain only a-z, 0-9, '-' and '_'");
    }
    Ok(())
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
            backpressure: None,
            zai_saturation: None,
            windows: BTreeMap::new(),
        };
        assert_eq!(config.policy_for("weekly").unwrap().target, 0.85);
    }

    #[test]
    fn banked_resets_default_to_detection_without_actuation() {
        let policy = BankedResetConfig::default();
        assert!(!policy.enabled);
        assert!(!policy.full_speed);
        assert_eq!(policy.minimum_pace_multiplier, 2.0);
    }

    #[test]
    fn banked_reset_full_speed_requires_an_explicit_boolean() {
        let policy: BankedResetConfig = serde_yaml::from_str("enabled: true").unwrap();
        assert!(!policy.full_speed);
        let policy: BankedResetConfig =
            serde_yaml::from_str("enabled: true\nfull_speed: true").unwrap();
        assert!(policy.full_speed);
        assert!(serde_yaml::from_str::<BankedResetConfig>("full_speed: 1").is_err());
        assert!(serde_yaml::from_str::<BankedResetConfig>("full_speed: yes").is_err());
    }

    /// plan.md §28.7/§28.13 (WP28): the peer list and staleness bounds a
    /// federation block may carry, and their defaults.
    #[test]
    fn federation_peers_default_and_validate() {
        let base = |federation_yaml: &str| {
            format!(
                "version: 1\naccounts:\n  acct:\n    source: {{ type: normalized_file, path: /tmp/subgov-test-source.json }}\n    utilization: {{ reserve_fraction: 0.1 }}\n    fleet:\n      max_workers: 4\n      observer: {{ type: static, workers: 1 }}\n      actuator: {{ type: none }}\nfederation:\n{federation_yaml}"
            )
        };

        // A federation block with no peer keys is a federation of one, with
        // the §28.7 defaults: stale after 120 s, expiry after 30 min.
        let config: Config = serde_yaml::from_str(&base("  host_id: codinghome")).unwrap();
        config.validate().unwrap();
        let federation = config.federation.as_ref().unwrap();
        assert!(federation.peers.is_empty());
        assert_eq!(federation.peer_stale_after_seconds, 120);
        assert_eq!(federation.peer_expiry_seconds, 1_800);

        // One peer with an explicit argv and bounds passes.
        let config: Config = serde_yaml::from_str(&base(
            "  host_id: codinghome\n  peer_stale_after_seconds: 60\n  peer_expiry_seconds: 600\n  peers:\n    - host_id: gmktec\n      argv: [\"/usr/local/bin/fake-ssh\", \"gmktec\", \"subgov\", \"peer-state\", \"--json\"]",
        ))
        .unwrap();
        config.validate().unwrap();
        let federation = config.federation.as_ref().unwrap();
        assert_eq!(federation.peers.len(), 1);
        assert_eq!(federation.peers[0].host_id, "gmktec");
        assert_eq!(federation.peer_stale_after().as_secs(), 60);
        assert_eq!(federation.peer_expiry().as_secs(), 600);

        for (federation_yaml, expected) in [
            // §28.2: a peer is another subgov on a different host.
            (
                "  host_id: codinghome\n  peers:\n    - host_id: codinghome",
                "own host",
            ),
            (
                "  host_id: codinghome\n  peers:\n    - host_id: gmktec\n    - host_id: gmktec",
                "more than once",
            ),
            (
                "  host_id: codinghome\n  peers:\n    - host_id: \"gmtec labs\"",
                "free of whitespace",
            ),
            (
                "  host_id: codinghome\n  peers:\n    - host_id: gmktec\n      argv: []",
                "argv must contain non-empty arguments",
            ),
            // §28.7: expiry is the recovery path out of a freeze, so it must
            // come after the freeze starts, and the freeze must have a
            // non-zero width.
            (
                "  host_id: codinghome\n  peer_stale_after_seconds: 0",
                "at least 1 second",
            ),
            (
                "  host_id: codinghome\n  peer_stale_after_seconds: 120\n  peer_expiry_seconds: 120",
                "must exceed peer_stale_after_seconds",
            ),
        ] {
            let config: Config = serde_yaml::from_str(&base(federation_yaml)).unwrap();
            let err = config.validate().unwrap_err().to_string();
            assert!(err.contains(expected), "expected {expected:?}, got: {err}");
        }
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
        assert_eq!(hosts["codinghome"].max_scale_up_per_cycle, None);
        assert_eq!(hosts["codinghome"].max_scale_down_per_cycle, None);
    }

    #[test]
    fn host_step_limits_parse_independently_and_allow_zero() {
        let yaml = base_config(&indent(
            r#"max_workers: 8
max_scale_up_per_cycle: 4
max_scale_down_per_cycle: 5
hosts:
  codinghome:
    max_scale_up_per_cycle: 0
    observer: { type: static, workers: 1 }
  lab:
    max_scale_down_per_cycle: 2
    observer: { type: static, workers: 1 }"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        config.validate().unwrap();
        let hosts = config.accounts["acct"].fleet.hosts.as_ref().unwrap();
        assert_eq!(hosts["codinghome"].max_scale_up_per_cycle, Some(0));
        assert_eq!(hosts["codinghome"].max_scale_down_per_cycle, None);
        assert_eq!(hosts["lab"].max_scale_up_per_cycle, None);
        assert_eq!(hosts["lab"].max_scale_down_per_cycle, Some(2));
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
    fn needle_run_actuator_config_validates_and_expands_repo_path() {
        let yaml = base_config(&indent(
            r#"max_workers: 4
observer: { type: static, workers: 1 }
actuator: { type: needle_run, repo: "~/work/project", adapter: claude-print }"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        config.validate().unwrap();
        let Config { mut accounts, .. } = config;
        accounts.get_mut("acct").unwrap().fleet.expand_paths();
        let ActuatorConfig::NeedleRun { repo, adapter, .. } = &accounts["acct"].fleet.actuator
        else {
            panic!("expected needle_run actuator");
        };
        assert_eq!(repo, &expand_tilde(Path::new("~/work/project")));
        assert_eq!(adapter, "claude-print");
    }

    #[test]
    fn needle_run_rejects_adapter_names_that_cannot_be_safely_matched() {
        let yaml = base_config(&indent(
            r#"max_workers: 4
observer: { type: static, workers: 1 }
actuator: { type: needle_run, repo: /workspace, adapter: "agent *" }"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("adapter must contain only ASCII letters"),
            "{err}"
        );
    }

    #[test]
    fn needle_seed_sources_expand_and_must_be_configured_as_a_pair() {
        let yaml = base_config(&indent(
            r#"max_workers: 4
observer: { type: static, workers: 1 }
actuator:
  type: needle_run
  repo: /workspace
  adapter: codex
  seed_demand_state: "~/state/demand.json"
  seed_heartbeats: "~/.needle/state/heartbeats""#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        config.validate().unwrap();
        let Config { mut accounts, .. } = config;
        accounts.get_mut("acct").unwrap().fleet.expand_paths();
        let ActuatorConfig::NeedleRun {
            seed_demand_state,
            seed_heartbeats,
            ..
        } = &accounts["acct"].fleet.actuator
        else {
            panic!("expected needle_run actuator");
        };
        assert_eq!(
            seed_demand_state.as_deref(),
            Some(expand_tilde(Path::new("~/state/demand.json")).as_path())
        );
        assert!(seed_heartbeats.is_some());

        for partial in [
            r#"seed_demand_state: /state/demand.json"#,
            r#"seed_heartbeats: ~/.needle/state/heartbeats"#,
        ] {
            let yaml = base_config(&indent(&format!(
                r#"max_workers: 4
observer: {{ type: static, workers: 1 }}
actuator: {{ type: needle_run, repo: /workspace, adapter: codex, {partial} }}"#
            )));
            let config: Config = serde_yaml::from_str(&yaml).unwrap();
            let err = config.validate().unwrap_err().to_string();
            assert!(
                err.contains("must be configured together"),
                "{partial}: {err}"
            );
        }

        let yaml = base_config(&indent(
            r#"max_workers: 4
observer: { type: static, workers: 1 }
actuator:
  type: needle_run
  repo: /workspace
  adapter: codex
  seed_demand_state: ""
  seed_heartbeats: ~/.needle/state/heartbeats"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("must not be empty"), "{err}");
    }

    #[test]
    fn needle_status_config_defaults_and_expands_heartbeat_directory() {
        let yaml = base_config(&indent(
            r#"max_workers: 4
observer:
  type: needle_status
  agent: claude-print
  heartbeat_dir: ~/test-heartbeats"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        config.validate().unwrap();
        let Config { mut accounts, .. } = config;
        accounts.get_mut("acct").unwrap().fleet.expand_paths();
        let Some(WorkerObserverConfig::NeedleStatus {
            agent,
            heartbeat_dir,
            stale_after_seconds,
            ..
        }) = accounts["acct"].fleet.observer.as_ref()
        else {
            panic!("expected needle_status observer");
        };

        assert_eq!(agent, "claude-print");
        assert_eq!(heartbeat_dir, &expand_tilde(Path::new("~/test-heartbeats")));
        assert_eq!(*stale_after_seconds, 60);
    }

    #[test]
    fn needle_status_defaults_to_needles_heartbeat_directory() {
        let yaml = base_config(&indent(
            r#"max_workers: 4
observer: { type: needle_status, agent: claude-print }"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        config.validate().unwrap();
        let Some(WorkerObserverConfig::NeedleStatus { heartbeat_dir, .. }) =
            config.accounts["acct"].fleet.observer.as_ref()
        else {
            panic!("expected needle_status observer");
        };

        let expected = dirs::home_dir()
            .map(|home| home.join(".needle/state/heartbeats"))
            .unwrap_or_else(|| PathBuf::from("~/.needle/state/heartbeats"));
        assert_eq!(heartbeat_dir, &expected);
    }

    #[test]
    fn needle_status_rejects_invalid_agent_names() {
        let yaml = base_config(&indent(
            r#"max_workers: 4
observer: { type: needle_status, agent: "claude print" }"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("agent must contain only ASCII letters"),
            "{err}"
        );
    }

    #[test]
    fn needle_status_requires_a_positive_staleness_threshold() {
        let yaml = base_config(&indent(
            r#"max_workers: 4
observer: { type: needle_status, agent: claude-print, stale_after_seconds: 0 }"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("observer needle_status stale_after_seconds must be positive"),
            "{err}"
        );
    }

    #[test]
    fn needle_status_rejects_an_empty_heartbeat_directory() {
        let yaml = base_config(&indent(
            r#"max_workers: 4
observer: { type: needle_status, agent: claude-print, heartbeat_dir: "" }"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("observer needle_status: heartbeat_dir must not be empty"),
            "{err}"
        );
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

    #[test]
    fn validate_duration_rejects_zero() {
        let err = validate_duration(0, "some_duration")
            .unwrap_err()
            .to_string();
        assert!(err.contains("some_duration must be positive"), "{err}");
    }

    #[test]
    fn validate_duration_rejects_values_that_overflow_i64() {
        let err = validate_duration(i64::MAX as u64 + 1, "some_duration")
            .unwrap_err()
            .to_string();
        assert!(err.contains("some_duration is too large"), "{err}");
    }

    #[test]
    fn validate_duration_accepts_a_deliberately_huge_but_i64_safe_value() {
        // examples/all-three.yaml's stale_after_seconds: 315360000 (10 years),
        // used to effectively disable staleness -- must stay valid.
        validate_duration(315_360_000, "some_duration").unwrap();
        validate_duration(i64::MAX as u64, "some_duration").unwrap();
    }

    #[test]
    fn poll_interval_seconds_zero_is_rejected() {
        let mut yaml = base_config(&indent(
            "max_workers: 4\nobserver: { type: static, workers: 1 }\nactuator: { type: none }",
        ));
        yaml = yaml.replacen("version: 1", "version: 1\npoll_interval_seconds: 0", 1);
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("poll_interval_seconds must be positive"),
            "{err}"
        );
    }

    #[test]
    fn poll_interval_seconds_above_i64_max_is_rejected() {
        let mut yaml = base_config(&indent(
            "max_workers: 4\nobserver: { type: static, workers: 1 }\nactuator: { type: none }",
        ));
        yaml = yaml.replacen(
            "version: 1",
            &format!("version: 1\npoll_interval_seconds: {}", i64::MAX as u64 + 1),
            1,
        );
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("poll_interval_seconds is too large"), "{err}");
    }

    fn config_with_utilization(utilization_yaml: &str) -> String {
        format!(
            r#"
version: 1
accounts:
  acct:
    source:
      type: normalized_file
      path: /tmp/subgov-test-source.json
    fleet:
      max_workers: 4
      observer: {{ type: static, workers: 1 }}
      actuator: {{ type: none }}
    utilization:
{utilization_yaml}
"#
        )
    }

    #[test]
    fn rolling_burn_window_is_optional_and_validated() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../schema/governor-config.schema.json")).unwrap();
        let field = &schema["$defs"]["windowPolicy"]["properties"]["burn_rate_window_seconds"];
        assert_eq!(field["type"], "integer");
        assert_eq!(field["minimum"], 1);
        assert_eq!(field["maximum"].as_u64(), Some(i64::MAX as u64));
        assert!(schema["$defs"]["windowPolicy"].get("required").is_none());
        let yaml = config_with_utilization(&indent("reserve_fraction: 0.15"));
        let cfg: Config = serde_yaml::from_str(&yaml).unwrap();
        assert!(cfg.accounts["acct"]
            .utilization
            .policy_for("weekly")
            .unwrap()
            .burn_rate_window_seconds
            .is_none());
        for seconds in ["86400", "0", "18446744073709551615"] {
            let yaml = config_with_utilization(&indent(&format!("reserve_fraction: 0.15\nwindows:\n  weekly:\n    burn_rate_window_seconds: {seconds}")));
            let cfg: Config = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(cfg.validate().is_ok(), seconds == "86400");
        }
    }

    #[test]
    fn stale_after_seconds_zero_is_rejected() {
        let yaml =
            config_with_utilization(&indent("reserve_fraction: 0.1\nstale_after_seconds: 0"));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("utilization.stale_after_seconds must be positive"),
            "{err}"
        );
    }

    #[test]
    fn minimum_sample_seconds_zero_is_rejected() {
        let yaml =
            config_with_utilization(&indent("reserve_fraction: 0.1\nminimum_sample_seconds: 0"));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("utilization.minimum_sample_seconds must be positive"),
            "{err}"
        );
    }

    #[test]
    fn backpressure_policy_uses_plan_defaults() {
        let yaml = config_with_utilization(&indent("reserve_fraction: 0.1\nbackpressure: {}"));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        config.validate().unwrap();
        let policy = config.accounts["acct"]
            .utilization
            .backpressure
            .as_ref()
            .unwrap();
        assert_eq!(policy.hold_above, 0.20);
        assert_eq!(policy.step_down_above, 0.50);
        assert_eq!(policy.recover_below, 0.10);
        assert_eq!(policy.quiet_cycles, 3);
        assert_eq!(policy.signal_loss_release_seconds, 1_800);
    }

    #[test]
    fn backpressure_policy_rejects_invalid_threshold_order_and_recovery_settings() {
        for (extra, expected) in [
            ("recover_below: 0.20", "thresholds must satisfy"),
            ("step_down_above: 0.15", "thresholds must satisfy"),
            ("quiet_cycles: 0", "quiet_cycles must be >= 1"),
            (
                "signal_loss_release_seconds: 0",
                "signal_loss_release_seconds must be positive",
            ),
        ] {
            let yaml = config_with_utilization(&indent(&format!(
                "reserve_fraction: 0.1\nbackpressure:\n  {extra}"
            )));
            let config: Config = serde_yaml::from_str(&yaml).unwrap();
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(expected), "{extra}: {error}");
        }
    }

    #[test]
    fn saturation_policy_is_strict_opt_in_and_validates_settings() {
        let yaml = config_with_utilization(&indent("reserve_fraction: 0.1"));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        assert!(serde_json::to_value(&config.accounts["acct"].utilization)
            .unwrap()
            .get("zai_saturation")
            .is_none());
        let enabled = config_with_utilization(&indent("reserve_fraction: 0.1\nzai_saturation: {}"));
        let config: Config = serde_yaml::from_str(&enabled).unwrap();
        config.validate().unwrap();
        let policy = config.accounts["acct"]
            .utilization
            .zai_saturation
            .as_ref()
            .unwrap();
        assert_eq!(policy.step_workers, 1);
        assert_eq!(policy.dwell_seconds, 600);
        assert_eq!(policy.reprobe_seconds, 3600);
        for setting in [
            "step_workers: 0",
            "minimum_attempts: 0",
            "dwell_seconds: 0",
            "minimum_window_seconds: 0",
            "reprobe_seconds: 0",
            "plateau_fraction: 1",
            "concurrency_fraction: -0.1",
            "dwell_seconds: 9223372036854775808",
        ] {
            let yaml = config_with_utilization(&indent(&format!(
                "reserve_fraction: 0.1\nzai_saturation:\n  {setting}"
            )));
            let config: Config = serde_yaml::from_str(&yaml).unwrap();
            assert!(config.validate().is_err(), "{setting}");
        }
        let unknown = enabled.replace(
            "zai_saturation: {}",
            "zai_saturation: {unexpected_key: true}",
        );
        assert!(serde_yaml::from_str::<Config>(&unknown).is_err());
        let example: Config =
            serde_yaml::from_str(include_str!("../examples/zai-saturation.yaml")).unwrap();
        example.validate().unwrap();
    }

    #[test]
    fn absent_backpressure_policy_is_omitted_from_serialized_utilization() {
        let yaml = config_with_utilization(&indent("reserve_fraction: 0.1"));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let serialized = serde_json::to_value(&config.accounts["acct"].utilization).unwrap();
        assert!(serialized.get("backpressure").is_none());
    }

    /// examples/all-three.yaml relies on a deliberately huge stale_after_seconds
    /// (10 years) to effectively disable staleness -- tightening the bound must
    /// not break that documented pattern.
    #[test]
    fn a_deliberately_huge_stale_after_seconds_still_validates() {
        let yaml = config_with_utilization(&indent(
            "reserve_fraction: 0.1\nstale_after_seconds: 315360000",
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn source_timeout_seconds_zero_is_rejected() {
        let yaml = base_config(&indent(
            "max_workers: 4\nobserver: { type: static, workers: 1 }\nactuator: { type: none }",
        ))
        .replace(
            "type: normalized_file\n      path: /tmp/subgov-test-source.json",
            "type: normalized_http\n      url: https://example.invalid/usage\n      timeout_seconds: 0",
        );
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("source timeout_seconds must be positive"),
            "{err}"
        );
    }

    #[test]
    fn host_resource_source_timeout_seconds_zero_is_rejected() {
        let yaml = base_config(&indent(
            r#"max_workers: 8
hosts:
  codinghome:
    max_workers: 8
    resource_reserve:
      cpu_reserve_fraction: 0.25
      mem_reserve_mb: 4096
    resource_source:
      type: normalized_http
      url: https://example.invalid/resources
      timeout_seconds: 0
    observer: { type: static, workers: 1 }
    actuator: { type: none }"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("resource_source timeout_seconds must be positive"),
            "{err}"
        );
    }

    #[test]
    fn identifier_prefix_must_be_conservative_and_match_between_observer_and_actuator() {
        for bad in ["", "Sg-", "sg codex", "sg/x"] {
            assert!(validate_identifier_prefix(Some(bad), "t").is_err(), "{bad}");
        }
        assert!(validate_identifier_prefix(Some("sg-codex-"), "t").is_ok());
        assert!(validate_identifier_prefix(None, "t").is_ok());

        let yaml = base_config(&indent(
            r#"max_workers: 4
observer: { type: needle_status, agent: codex, identifier_prefix: sg-a- }
actuator: { type: needle_run, repo: /r, adapter: codex, identifier_prefix: sg-b- }"#,
        ));
        let config: Config = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("same identifier_prefix"), "{err}");
    }

    // --- Plan §26.2: the per-pool takeover list. ---

    fn takeover_fleet_yaml(extra: &str) -> String {
        indent(&format!(
            r#"max_workers: 18
observer:
  type: needle_status
  agent: claude-code-glm-5.3-flash
  identifier_prefix: sg-zai-
actuator:
  type: needle_run
  repo: /workspace
  adapter: claude-code-glm-5.3-flash
  identifier_prefix: sg-zai-
{extra}"#
        ))
    }

    fn takeover_config_with(fleet_yaml: &str) -> Config {
        serde_yaml::from_str(&format!(
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
        ))
        .unwrap()
    }

    #[test]
    fn takeover_list_parses_units_and_stragglers_and_validates() {
        let fleet = takeover_fleet_yaml(
            "takeover:\n  - worker_id: glm-front-herdr\n    agent: claude-code-glm-5.3-flash\n    unit: needle-worker@glm-front-herdr.service\n  - worker_id: glm-roam-25\n    agent: claude-code-glm-5.3\ntakeover_helper: [\"/usr/local/bin/zai-takeover\"]",
        );
        let config = takeover_config_with(&fleet);
        config.validate().unwrap();
        let takeover = &config.accounts["acct"].fleet.takeover;
        assert_eq!(takeover.len(), 2);
        assert_eq!(takeover[0].worker_id, "glm-front-herdr");
        assert_eq!(
            takeover[0].unit.as_deref(),
            Some("needle-worker@glm-front-herdr.service")
        );
        assert_eq!(takeover[1].worker_id, "glm-roam-25");
        assert_eq!(takeover[1].unit, None, "a straggler has no unit");

        // Without takeover, takeover fields default to empty and absent.
        let plain = takeover_config_with(&takeover_fleet_yaml(""));
        plain.validate().unwrap();
        assert!(plain.accounts["acct"].fleet.takeover.is_empty());
        assert!(plain.accounts["acct"].fleet.takeover_helper.is_none());
    }

    /// A fleet whose takeover wiring is valid: prefixed observer/actuator
    /// pair, one roster unit and one straggler, and a helper.
    fn valid_takeover_fleet() -> FleetConfig {
        FleetConfig {
            min_workers: 0,
            max_workers: 18,
            bootstrap_workers: 1,
            max_scale_up_per_cycle: 1,
            max_scale_down_per_cycle: 1,
            observer: Some(WorkerObserverConfig::NeedleStatus {
                agent: "claude-code-glm-5.3-flash".into(),
                heartbeat_dir: PathBuf::from("/hb"),
                stale_after_seconds: 60,
                identifier_prefix: Some("sg-zai-".into()),
            }),
            actuator: ActuatorConfig::NeedleRun {
                repo: PathBuf::from("/workspace"),
                adapter: "claude-code-glm-5.3-flash".into(),
                identifier_prefix: Some("sg-zai-".into()),
                seed_demand_state: None,
                seed_heartbeats: None,
            },
            observer_reconciliation: ObserverReconciliation::default(),
            hosts: None,
            takeover: vec![
                TakeoverTarget {
                    worker_id: "glm-front-herdr".into(),
                    agent: "claude-code-glm-5.3-flash".into(),
                    unit: Some("needle-worker@glm-front-herdr.service".into()),
                },
                TakeoverTarget {
                    worker_id: "glm-roam-25".into(),
                    agent: "claude-code-glm-5.3".into(),
                    unit: None,
                },
            ],
            takeover_helper: Some(vec!["/usr/local/bin/zai-takeover".into()]),
        }
    }

    fn account_with_fleet(fleet: FleetConfig) -> Config {
        let mut accounts = BTreeMap::new();
        accounts.insert(
            "acct".to_string(),
            AccountConfig {
                source: SourceConfig::NormalizedFile {
                    path: PathBuf::from("/tmp/subgov-test-source.json"),
                },
                fleet,
                utilization: UtilizationConfig {
                    target_utilization: None,
                    reserve_fraction: Some(0.1),
                    strategy: Strategy::LinearToReset,
                    stale_after_seconds: 900,
                    stale_behavior: StaleBehavior::default(),
                    minimum_sample_seconds: 60,
                    backpressure: None,
                    zai_saturation: None,
                    windows: BTreeMap::new(),
                },
                banked_resets: BankedResetConfig::default(),
            },
        );
        Config {
            version: 1,
            poll_interval_seconds: default_poll_interval(),
            state_path: None,
            accounts,
            federation: None,
        }
    }

    #[test]
    fn takeover_requires_the_prefix_pair_and_a_helper() {
        // The valid fleet passes.
        account_with_fleet(valid_takeover_fleet())
            .validate()
            .unwrap();

        let missing_helper = {
            let mut fleet = valid_takeover_fleet();
            fleet.takeover_helper = None;
            fleet
        };
        let err = account_with_fleet(missing_helper)
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires fleet.takeover_helper"), "{err}");

        let stray_helper = {
            let mut fleet = valid_takeover_fleet();
            fleet.takeover = Vec::new();
            fleet
        };
        let err = account_with_fleet(stray_helper)
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("meaningless without fleet.takeover"), "{err}");

        // A prefixed replacement is the retirement gate: without the
        // needle_run prefix there is no gate and takeover is rejected.
        for actuator in [
            ActuatorConfig::None,
            ActuatorConfig::NeedleRun {
                repo: PathBuf::from("/workspace"),
                adapter: "claude-code-glm-5.3-flash".into(),
                identifier_prefix: None,
                seed_demand_state: None,
                seed_heartbeats: None,
            },
        ] {
            let mut fleet = valid_takeover_fleet();
            fleet.actuator = actuator;
            fleet.observer = Some(WorkerObserverConfig::NeedleStatus {
                agent: "claude-code-glm-5.3-flash".into(),
                heartbeat_dir: PathBuf::from("/hb"),
                stale_after_seconds: 60,
                identifier_prefix: None,
            });
            let err = account_with_fleet(fleet)
                .validate()
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("requires a needle_run actuator with an identifier_prefix"),
                "{err}"
            );
        }

        // Floor accounting requires the needle_status observer with the same
        // prefix: a listed worker must leave the §23 unowned floor. (An
        // observer with a *different* prefix is already rejected by the
        // §23.3 pairing rule, so a non-needle_status observer is the case
        // this fleet-level check adds.)
        let mut fleet = valid_takeover_fleet();
        fleet.observer = Some(WorkerObserverConfig::Static { workers: 1 });
        let err = account_with_fleet(fleet)
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires a needle_status observer"), "{err}");

        let mut fleet = valid_takeover_fleet();
        if let ActuatorConfig::NeedleRun {
            identifier_prefix, ..
        } = &mut fleet.actuator
        {
            *identifier_prefix = Some("sg-codex-".into());
        }
        let err = account_with_fleet(fleet)
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("same identifier_prefix"), "{err}");
    }

    #[test]
    fn takeover_targets_validate_identities_units_and_uniqueness() {
        for (worker_id, agent, unit, expected) in [
            (
                "",
                "claude-code-glm-5.3-flash",
                None,
                "worker_id must start",
            ),
            (
                "glm front",
                "claude-code-glm-5.3-flash",
                None,
                "worker_id must start",
            ),
            ("glm-front-herdr", "", None, "agent must start"),
            (
                "glm-front-herdr",
                "claude-code-glm-5.3-flash",
                Some("other.service"),
                "does not match worker",
            ),
        ] {
            let mut fleet = valid_takeover_fleet();
            fleet.takeover = vec![TakeoverTarget {
                worker_id: worker_id.into(),
                agent: agent.into(),
                unit: unit.map(Into::into),
            }];
            let err = account_with_fleet(fleet)
                .validate()
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{worker_id}: {err}");
        }

        // A worker belongs to exactly one pool: duplicated within a pool is
        // rejected (plan §26.3).
        let mut fleet = valid_takeover_fleet();
        fleet.takeover.push(fleet.takeover[0].clone());
        let err = account_with_fleet(fleet)
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("more than once"), "{err}");
    }

    #[test]
    fn takeover_worker_id_is_unique_across_pools() {
        let mut first = account_with_fleet(valid_takeover_fleet());
        // A second, individually valid pool listing the same worker id.
        let mut second_fleet = valid_takeover_fleet();
        if let ActuatorConfig::NeedleRun {
            identifier_prefix, ..
        } = &mut second_fleet.actuator
        {
            *identifier_prefix = Some("sg-other-".into());
        }
        second_fleet.observer = Some(WorkerObserverConfig::NeedleStatus {
            agent: "claude-code-glm-5.3-flash".into(),
            heartbeat_dir: PathBuf::from("/hb"),
            stale_after_seconds: 60,
            identifier_prefix: Some("sg-other-".into()),
        });
        first.accounts.insert(
            "other".into(),
            AccountConfig {
                source: SourceConfig::NormalizedFile {
                    path: PathBuf::from("/tmp/subgov-test-source.json"),
                },
                fleet: second_fleet,
                utilization: first.accounts["acct"].utilization.clone(),
                banked_resets: BankedResetConfig::default(),
            },
        );
        let err = first.validate().unwrap_err().to_string();
        assert!(err.contains("already listed by account"), "{err}");
    }

    #[test]
    fn takeover_does_not_apply_to_per_host_fleets() {
        let mut fleet = valid_takeover_fleet();
        fleet.observer = None;
        fleet.actuator = ActuatorConfig::None;
        let mut hosts = BTreeMap::new();
        hosts.insert(
            "codinghome".to_string(),
            HostConfig {
                max_workers: None,
                max_scale_up_per_cycle: None,
                max_scale_down_per_cycle: None,
                resource_reserve: None,
                resource_source: None,
                observer: WorkerObserverConfig::Static { workers: 1 },
                actuator: ActuatorConfig::None,
            },
        );
        fleet.hosts = Some(hosts);
        let err = account_with_fleet(fleet)
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("single fleet"), "{err}");
    }
}
