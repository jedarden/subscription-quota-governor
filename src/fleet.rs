use crate::config::{
    AccountConfig, ActuatorConfig, FleetConfig, HostConfig, ObserverReconciliation,
    WorkerObserverConfig,
};
use crate::placement::HostPlacement;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Maximum bytes read from an observer's file or command stdout (plan.md
/// §11.1: "bound file and stdout size"). A worker count is a bare integer or
/// a one-field JSON object, so this is generous headroom rather than a tight
/// budget.
const MAX_OBSERVER_BYTES: u64 = 4096;

/// Ceiling on how long an observer or actuator child process (and everything
/// it spawns) may run before it is killed (plan.md §11.1/§11.2: "apply a
/// command timeout and kill the complete child process group"). Matches the
/// default timeout already used for the source adapters' external calls.
const CHILD_TIMEOUT: Duration = Duration::from_secs(15);

/// How often a timed-out-or-not check polls `Child::try_wait`.
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Reports the fleet's current worker count for an account.
///
/// Implemented by the baseline `static`/`file`/`command` adapters and by
/// in-memory test doubles that stand in for them in controller-cycle and
/// integration tests.
pub trait Observer {
    fn current_workers(&self) -> Result<u32>;
}

/// Applies a desired worker count to an account's fleet.
///
/// Implemented by the baseline `none`/`target_file`/`command` adapters and by
/// in-memory test doubles that stand in for them in controller-cycle and
/// integration tests. The signature takes only the desired count: plan.md
/// §11.2's "pass no quota or credential data into actuator arguments" is
/// enforced structurally here, not by convention -- there is no parameter an
/// implementation could pass such data through even if it wanted to.
pub trait Actuator {
    fn actuate(&self, desired: u32) -> Result<()>;
}

/// The observable outcome of one actuation decision (plan.md §11.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActuationOutcome {
    /// Whether the actuator was actually invoked. `false` when `desired`
    /// equals `observed` (nothing to reconcile) or when the configured
    /// actuator is `none` (mutation is deliberately disabled), even though
    /// those two cases differ in whether there was something to do.
    pub actuated: bool,
}

/// Builds the concrete `Observer` described by an account's fleet config.
pub fn observer_for(config: &WorkerObserverConfig) -> Box<dyn Observer> {
    match config {
        WorkerObserverConfig::Static { workers } => Box::new(StaticObserver { workers: *workers }),
        WorkerObserverConfig::File { path } => Box::new(FileObserver { path: path.clone() }),
        WorkerObserverConfig::Command { argv } => Box::new(CommandObserver { argv: argv.clone() }),
    }
}

/// Builds the concrete `Actuator` described by an account's fleet config.
pub fn actuator_for(config: &ActuatorConfig) -> Box<dyn Actuator> {
    match config {
        ActuatorConfig::None => Box::new(NoneActuator),
        ActuatorConfig::TargetFile { path } => Box::new(TargetFileActuator { path: path.clone() }),
        ActuatorConfig::Command { argv } => Box::new(CommandActuator { argv: argv.clone() }),
    }
}

pub fn current_workers(config: &FleetConfig) -> Result<u32> {
    let observer_config = config
        .observer
        .as_ref()
        .context("fleet.observer is required when fleet.hosts is not configured")?;
    let observed = observer_for(observer_config).current_workers()?;
    reconcile_observed_range(observed, config)
}

/// Reads the worker count from each configured host's observer.
///
/// Host counts stay keyed by host id so callers can pass them directly to
/// [`crate::placement::place`]. Account-level minimum and maximum limits are
/// not applied to each host individually; those limits describe the aggregate
/// account, while each host may have a different ceiling.
pub fn current_host_workers(config: &AccountConfig) -> Result<BTreeMap<String, u32>> {
    let hosts = configured_hosts(config)?;
    hosts
        .iter()
        .map(|(host_id, host)| {
            let observed = observer_for(&host.observer)
                .current_workers()
                .with_context(|| format!("host {host_id} observer failed"))?;
            Ok((host_id.clone(), observed))
        })
        .collect()
}

/// Dispatches each placement target to the actuator configured for that host.
///
/// The complete host-id mapping is validated before the first actuator runs,
/// preventing malformed or partial placement results from causing a partial
/// set of host mutations. Per-host `current` and `target` values are consumed
/// directly from placement; host actuators never receive account quota data.
pub fn actuate_host_targets(
    config: &AccountConfig,
    placements: &[HostPlacement],
) -> Result<BTreeMap<String, ActuationOutcome>> {
    let hosts = configured_hosts(config)?;
    let mut by_host = BTreeMap::new();
    for placement in placements {
        if !hosts.contains_key(&placement.host_id) {
            bail!("placement references unknown host {}", placement.host_id);
        }
        if by_host
            .insert(placement.host_id.as_str(), placement)
            .is_some()
        {
            bail!("placement contains duplicate host {}", placement.host_id);
        }
    }
    for host_id in hosts.keys() {
        if !by_host.contains_key(host_id.as_str()) {
            bail!("placement is missing configured host {host_id}");
        }
    }

    hosts
        .iter()
        .map(|(host_id, host)| {
            let placement = by_host[host_id.as_str()];
            let outcome = actuate_config(&host.actuator, placement.current, placement.target)
                .with_context(|| format!("host {host_id} actuator failed"))?;
            Ok((host_id.clone(), outcome))
        })
        .collect()
}

fn configured_hosts(config: &AccountConfig) -> Result<&BTreeMap<String, HostConfig>> {
    config
        .fleet
        .hosts
        .as_ref()
        .filter(|hosts| !hosts.is_empty())
        .context("fleet.hosts must contain at least one host")
}

/// Applies plan.md §11.1's range requirement: "reject counts outside the
/// configured fleet range unless a documented reconciliation mode is
/// selected." Layered on top of `Observer::current_workers` (rather than
/// inside each impl) so every observer -- baseline or an in-memory test
/// double -- gets the same policy uniformly.
fn reconcile_observed_range(observed: u32, config: &FleetConfig) -> Result<u32> {
    if observed >= config.min_workers && observed <= config.max_workers {
        return Ok(observed);
    }
    match config.observer_reconciliation {
        ObserverReconciliation::Strict => bail!(
            "observed worker count {observed} is outside the configured fleet range [{}, {}]",
            config.min_workers,
            config.max_workers
        ),
        ObserverReconciliation::Clamp => Ok(observed.clamp(config.min_workers, config.max_workers)),
    }
}

/// Actuates a fleet toward `desired`, applying plan.md §11.2's policy:
///
/// - skip invocation entirely when `desired == observed` (nothing to do);
/// - always report `actuated: false` for the `none` actuator, even when
///   `desired` differs from `observed`.
///
/// A caller that only wants to *observe* (e.g. `run --observe-only`) should
/// not call this function at all rather than rely on it to no-op, since that
/// is a runtime/CLI policy distinct from the fleet-level rules enforced
/// here.
///
/// On `Err`, no actuation happened (or a partial one failed) and the caller
/// must not treat this cycle's observation as authoritative -- retaining
/// whatever state it already holds is what lets the next cycle reconcile.
pub fn actuate(config: &FleetConfig, observed: u32, desired: u32) -> Result<ActuationOutcome> {
    actuate_config(&config.actuator, observed, desired)
}

fn actuate_config(
    actuator: &ActuatorConfig,
    observed: u32,
    desired: u32,
) -> Result<ActuationOutcome> {
    if desired == observed {
        return Ok(ActuationOutcome { actuated: false });
    }
    if matches!(actuator, ActuatorConfig::None) {
        return Ok(ActuationOutcome { actuated: false });
    }
    actuator_for(actuator).actuate(desired)?;
    Ok(ActuationOutcome { actuated: true })
}

struct StaticObserver {
    workers: u32,
}

impl Observer for StaticObserver {
    fn current_workers(&self) -> Result<u32> {
        Ok(self.workers)
    }
}

struct FileObserver {
    path: PathBuf,
}

impl Observer for FileObserver {
    fn current_workers(&self) -> Result<u32> {
        let mut file = File::open(&self.path)
            .with_context(|| format!("failed to read worker count {}", self.path.display()))?;
        let bytes = read_bounded(&mut file, MAX_OBSERVER_BYTES)
            .with_context(|| format!("worker count {}", self.path.display()))?;
        let text = String::from_utf8(bytes).context("worker count file was not UTF-8")?;
        parse_worker_count(&text)
    }
}

struct CommandObserver {
    argv: Vec<String>,
}

impl Observer for CommandObserver {
    fn current_workers(&self) -> Result<u32> {
        command_observer_current_workers(&self.argv, CHILD_TIMEOUT)
    }
}

/// The `CommandObserver` implementation, with the timeout as a parameter so
/// tests can use a short one instead of waiting out `CHILD_TIMEOUT`.
///
/// Reads stdout on a background thread so a size-bounded read (which must
/// keep consuming bytes to notice the bound was exceeded) and a wall-clock
/// timeout can be enforced at the same time: the main thread blocks on
/// `recv_timeout` instead of on the read itself, and kills the child's whole
/// process group if the deadline passes before the reader reports back.
fn command_observer_current_workers(argv: &[String], timeout: Duration) -> Result<u32> {
    let mut command = new_process_group_command(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start worker observer {}", argv[0]))?;
    let mut stdout = child
        .stdout
        .take()
        .context("worker observer stdout was not piped")?;

    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let _ = sender.send(read_bounded(&mut stdout, MAX_OBSERVER_BYTES));
    });

    let bytes = match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            kill_child_tree(&mut child);
            let _ = reader.join();
            bail!("worker observer {} timed out after {timeout:?}", argv[0]);
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!(
                "worker observer {} reader thread ended unexpectedly",
                argv[0]
            );
        }
    };
    let _ = reader.join();

    if bytes.is_err() {
        // The child may still be trying to write past the bound; kill its
        // whole process group rather than risk a grandchild blocking
        // forever on a full pipe buffer nobody is draining.
        kill_child_tree(&mut child);
    }
    let bytes = bytes.with_context(|| format!("worker observer {} stdout", argv[0]))?;

    let status = child
        .wait()
        .with_context(|| format!("failed to wait for worker observer {}", argv[0]))?;
    if !status.success() {
        bail!("worker observer {} exited with {}", argv[0], status);
    }
    let text = String::from_utf8(bytes).context("worker count was not UTF-8")?;
    parse_worker_count(&text)
}

struct NoneActuator;

impl Actuator for NoneActuator {
    fn actuate(&self, _desired: u32) -> Result<()> {
        Ok(())
    }
}

struct TargetFileActuator {
    path: PathBuf,
}

impl Actuator for TargetFileActuator {
    fn actuate(&self, desired: u32) -> Result<()> {
        write_target(&self.path, desired)
    }
}

struct CommandActuator {
    argv: Vec<String>,
}

impl Actuator for CommandActuator {
    fn actuate(&self, desired: u32) -> Result<()> {
        command_actuator_actuate(&self.argv, desired, CHILD_TIMEOUT)
    }
}

/// The non-secret environment variable an idempotency token is passed
/// through (plan.md §11.2). See [`idempotency_token`] for the contract.
const IDEMPOTENCY_TOKEN_ENV: &str = "SUBGOV_IDEMPOTENCY_TOKEN";

/// The `CommandActuator` implementation, with the timeout as a parameter so
/// tests can use a short one instead of waiting out `CHILD_TIMEOUT`.
fn command_actuator_actuate(argv: &[String], desired: u32, timeout: Duration) -> Result<()> {
    let rendered: Vec<String> = argv
        .iter()
        .map(|argument| argument.replace("{desired_workers}", &desired.to_string()))
        .collect();
    let mut command = new_process_group_command(&rendered[0]);
    command
        .args(&rendered[1..])
        .env(IDEMPOTENCY_TOKEN_ENV, idempotency_token(&rendered, desired))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to execute fleet actuator {}", rendered[0]))?;
    let status = wait_with_timeout(&mut child, timeout)
        .with_context(|| format!("fleet actuator {}", rendered[0]))?;
    if !status.success() {
        bail!("fleet actuator {} exited with {status}", rendered[0]);
    }
    Ok(())
}

/// A stable idempotency token for one command-actuator invocation (plan.md
/// §11.2: "optionally support an idempotency token through a non-secret
/// environment variable after its contract is specified").
///
/// Contract:
///
/// - passed to the child as `SUBGOV_IDEMPOTENCY_TOKEN`; using it is entirely
///   optional for the receiving script, which is free to ignore an
///   environment variable it does not recognize;
/// - a hex-encoded, deterministic hash of the rendered command and the
///   desired count -- *not* a random nonce, and *not* derived from the
///   process id, a path, or wall-clock time -- so it is stable across
///   process restarts, not just within one;
/// - identical for two invocations that represent the same logical
///   actuation: an account whose actuation failed or hung is retried
///   unchanged on the next reconciliation cycle (plan.md §11.2: "retain the
///   prior state on actuation failure so the next cycle reconciles"), so the
///   retry renders the same command against the same desired count and
///   produces the same token, letting a script de-duplicate the retried
///   partial actuation. It changes whenever the rendered command or the
///   desired count changes, since either means a genuinely different
///   actuation is being requested;
/// - carries no quota, credential, or account-identifying data -- the
///   `Actuator` trait passes none of that in to begin with -- so it is safe
///   to log.
fn idempotency_token(rendered_argv: &[String], desired: u32) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    rendered_argv.hash(&mut hasher);
    desired.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Waits for `child` to exit, polling rather than blocking so a deadline can
/// be enforced, and kills its whole process group if it doesn't exit in
/// time (plan.md §11.1/§11.2).
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .context("failed to poll child process status")?
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            kill_child_tree(child);
            bail!("timed out after {timeout:?} waiting for the child to exit");
        }
        thread::sleep(CHILD_POLL_INTERVAL);
    }
}

/// Spawns `program` as the leader of its own new process group, so
/// [`kill_child_tree`] can terminate it and everything it spawns, not just
/// the immediate child (plan.md §11.1/§11.2: "kill the complete child
/// process group").
#[cfg(unix)]
fn new_process_group_command(program: &str) -> Command {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(program);
    command.process_group(0);
    command
}

#[cfg(not(unix))]
fn new_process_group_command(program: &str) -> Command {
    Command::new(program)
}

/// Kills `child`'s whole process group (or just `child` where process groups
/// aren't available) and reaps it. Best-effort: a child that has already
/// exited, or a signal that fails to reach every descendant, is not treated
/// as an error here -- the caller is already on a failure or timeout path.
#[cfg(unix)]
fn kill_child_tree(child: &mut Child) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(not(unix))]
fn kill_child_tree(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Parses a `file`/`command` observer's output: one unsigned decimal integer
/// or `{"current_workers": N}`, with leading/trailing whitespace allowed
/// (plan.md §11.1). Trailing non-whitespace data is rejected: `str::parse`
/// requires the whole trimmed string to be the integer, and
/// `serde_json::from_str` errors on trailing characters after the JSON
/// value, so neither branch accepts e.g. `"5 6"`, `"5\ngarbage"`, or a
/// concatenated second document.
fn parse_worker_count(text: &str) -> Result<u32> {
    let trimmed = text.trim();
    if let Ok(value) = trimmed.parse() {
        return Ok(value);
    }
    let value: Value = serde_json::from_str(trimmed)
        .context("worker observer must print an integer or {\"current_workers\": N}")?;
    value
        .get("current_workers")
        .and_then(Value::as_u64)
        .and_then(|count| u32::try_from(count).ok())
        .context("worker observer JSON omitted a valid current_workers")
}

/// Reads at most `limit` bytes from `reader`, failing rather than silently
/// truncating if more data is available. Bounds an observer's untrusted or
/// unbounded input -- command stdout or a worker-count file -- per plan.md
/// §11.1.
fn read_bounded(reader: &mut dyn Read, limit: u64) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut buffer)
        .context("failed to read observer output")?;
    if buffer.len() as u64 > limit {
        bail!("observer output exceeds the {limit}-byte maximum");
    }
    Ok(buffer)
}

fn write_target(path: &Path, desired: u32) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temporary = temporary_path(path);
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        writeln!(file, "{desired}")?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("failed to update target {}", path.display()))
}

fn temporary_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("target");
    path.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AccountConfig, ActuatorConfig, BankedResetConfig, FleetConfig, HostConfig,
        ObserverReconciliation, SourceConfig, Strategy, UtilizationConfig, WorkerObserverConfig,
    };

    struct RecordingActuator {
        calls: std::cell::RefCell<Vec<u32>>,
    }

    impl Actuator for RecordingActuator {
        fn actuate(&self, desired: u32) -> Result<()> {
            self.calls.borrow_mut().push(desired);
            Ok(())
        }
    }

    fn host(observer: WorkerObserverConfig, actuator: ActuatorConfig) -> HostConfig {
        HostConfig {
            max_workers: None,
            resource_reserve: None,
            resource_source: None,
            observer,
            actuator,
        }
    }

    fn account_with_hosts(hosts: BTreeMap<String, HostConfig>) -> AccountConfig {
        AccountConfig {
            source: SourceConfig::NormalizedFile {
                path: PathBuf::from("unused"),
            },
            fleet: FleetConfig {
                min_workers: 0,
                max_workers: 10,
                bootstrap_workers: 1,
                max_scale_up_per_cycle: 1,
                max_scale_down_per_cycle: 1,
                observer: None,
                actuator: ActuatorConfig::None,
                observer_reconciliation: ObserverReconciliation::default(),
                hosts: Some(hosts),
            },
            utilization: UtilizationConfig {
                target_utilization: Some(0.85),
                reserve_fraction: None,
                strategy: Strategy::LinearToReset,
                stale_after_seconds: 300,
                stale_behavior: Default::default(),
                minimum_sample_seconds: 60,
                windows: BTreeMap::new(),
            },
            banked_resets: BankedResetConfig::default(),
        }
    }

    fn placement(host_id: &str, current: u32, target: u32) -> HostPlacement {
        HostPlacement {
            host_id: host_id.into(),
            current,
            target,
            ceiling: 10,
            headroom: 1.0,
            fresh: true,
            eligible: true,
        }
    }

    #[test]
    fn current_host_workers_uses_each_hosts_configured_observer() {
        let account = account_with_hosts(BTreeMap::from([
            (
                "east".into(),
                host(
                    WorkerObserverConfig::Static { workers: 2 },
                    ActuatorConfig::None,
                ),
            ),
            (
                "west".into(),
                host(
                    WorkerObserverConfig::Static { workers: 7 },
                    ActuatorConfig::None,
                ),
            ),
        ]));

        assert_eq!(
            current_host_workers(&account).unwrap(),
            BTreeMap::from([("east".into(), 2), ("west".into(), 7)])
        );
    }

    #[test]
    fn actuate_host_targets_routes_each_placement_to_the_matching_host() {
        let dir = std::env::temp_dir().join(format!("subgov-host-actuate-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let east_target = dir.join("east-target");
        let west_target = dir.join("west-target");
        let account = account_with_hosts(BTreeMap::from([
            (
                "east".into(),
                host(
                    WorkerObserverConfig::Static { workers: 1 },
                    ActuatorConfig::TargetFile {
                        path: east_target.clone(),
                    },
                ),
            ),
            (
                "west".into(),
                host(
                    WorkerObserverConfig::Static { workers: 4 },
                    ActuatorConfig::TargetFile {
                        path: west_target.clone(),
                    },
                ),
            ),
        ]));

        // Reverse host order to show dispatch follows each placement's id,
        // not the order in which placements were produced.
        let outcomes = actuate_host_targets(
            &account,
            &[placement("west", 4, 6), placement("east", 1, 3)],
        )
        .unwrap();

        assert!(outcomes["east"].actuated);
        assert!(outcomes["west"].actuated);
        assert_eq!(fs::read_to_string(east_target).unwrap().trim(), "3");
        assert_eq!(fs::read_to_string(west_target).unwrap().trim(), "6");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn actuate_host_targets_rejects_incomplete_placement_before_actuating() {
        let dir = std::env::temp_dir().join(format!("subgov-host-missing-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let east_target = dir.join("east-target");
        let west_target = dir.join("west-target");
        let account = account_with_hosts(BTreeMap::from([
            (
                "east".into(),
                host(
                    WorkerObserverConfig::Static { workers: 1 },
                    ActuatorConfig::TargetFile {
                        path: east_target.clone(),
                    },
                ),
            ),
            (
                "west".into(),
                host(
                    WorkerObserverConfig::Static { workers: 4 },
                    ActuatorConfig::TargetFile {
                        path: west_target.clone(),
                    },
                ),
            ),
        ]));

        let error = actuate_host_targets(&account, &[placement("east", 1, 3)]).unwrap_err();

        assert!(error.to_string().contains("missing configured host west"));
        assert!(!east_target.exists());
        assert!(!west_target.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn static_observer_reports_configured_workers() {
        let observer = observer_for(&WorkerObserverConfig::Static { workers: 7 });
        assert_eq!(observer.current_workers().unwrap(), 7);
    }

    #[test]
    fn none_actuator_reports_no_error_and_no_state_change() {
        let actuator = actuator_for(&ActuatorConfig::None);
        assert!(actuator.actuate(5).is_ok());
    }

    #[test]
    fn in_memory_actuator_double_records_desired_workers() {
        let actuator = RecordingActuator {
            calls: std::cell::RefCell::new(Vec::new()),
        };
        actuator.actuate(3).unwrap();
        actuator.actuate(9).unwrap();
        assert_eq!(*actuator.calls.borrow(), vec![3, 9]);
    }

    #[test]
    fn target_file_actuator_writes_desired_workers() {
        let dir = std::env::temp_dir().join(format!("subgov-fleet-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("target");
        let actuator = actuator_for(&ActuatorConfig::TargetFile { path: path.clone() });
        actuator.actuate(4).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        assert_eq!(written.trim(), "4");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_worker_count_accepts_a_bare_integer_with_surrounding_whitespace() {
        assert_eq!(parse_worker_count("5").unwrap(), 5);
        assert_eq!(parse_worker_count(" 5\n").unwrap(), 5);
        assert_eq!(parse_worker_count("\t5\t\n").unwrap(), 5);
    }

    #[test]
    fn parse_worker_count_accepts_the_documented_json_shape() {
        assert_eq!(parse_worker_count(r#"{"current_workers": 5}"#).unwrap(), 5);
        assert_eq!(
            parse_worker_count(" \n{\"current_workers\": 5}\n").unwrap(),
            5
        );
    }

    #[test]
    fn parse_worker_count_rejects_trailing_non_whitespace_after_an_integer() {
        assert!(parse_worker_count("5 6").is_err());
        assert!(parse_worker_count("5garbage").is_err());
        assert!(parse_worker_count("5\nextra").is_err());
    }

    #[test]
    fn parse_worker_count_rejects_trailing_non_whitespace_after_json() {
        assert!(parse_worker_count(r#"{"current_workers": 5} extra"#).is_err());
        assert!(parse_worker_count(r#"{"current_workers": 5}{"current_workers": 6}"#).is_err());
    }

    #[test]
    fn parse_worker_count_rejects_empty_and_malformed_input() {
        assert!(parse_worker_count("").is_err());
        assert!(parse_worker_count("-1").is_err());
        assert!(parse_worker_count("5.5").is_err());
        assert!(parse_worker_count("not a number").is_err());
    }

    #[test]
    fn file_observer_rejects_a_file_over_the_size_bound() {
        let dir =
            std::env::temp_dir().join(format!("subgov-fleet-oversize-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("workers");
        fs::write(&path, vec![b'1'; (MAX_OBSERVER_BYTES + 1) as usize]).unwrap();
        let observer = observer_for(&WorkerObserverConfig::File { path: path.clone() });
        assert!(observer.current_workers().is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_observer_accepts_a_file_at_exactly_the_size_bound() {
        let dir = std::env::temp_dir().join(format!("subgov-fleet-atsize-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("workers");
        let mut contents = vec![b'0'; (MAX_OBSERVER_BYTES - 1) as usize];
        contents.push(b'5');
        fs::write(&path, &contents).unwrap();
        let observer = observer_for(&WorkerObserverConfig::File { path: path.clone() });
        assert_eq!(observer.current_workers().unwrap(), 5);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn command_observer_succeeds_for_small_output() {
        let observer = observer_for(&WorkerObserverConfig::Command {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo 3".to_string(),
            ],
        });
        assert_eq!(observer.current_workers().unwrap(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn command_observer_rejects_oversized_stdout_and_reaps_the_child() {
        let observer = observer_for(&WorkerObserverConfig::Command {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "yes | head -c 20000".to_string(),
            ],
        });
        assert!(observer.current_workers().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn command_observer_succeeds_well_within_a_short_timeout() {
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo 4".to_string(),
        ];
        assert_eq!(
            command_observer_current_workers(&argv, Duration::from_millis(500)).unwrap(),
            4
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_observer_times_out_on_a_hanging_command() {
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 5".to_string(),
        ];
        let start = Instant::now();
        let result = command_observer_current_workers(&argv, Duration::from_millis(100));
        assert!(result.is_err());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "should time out around 100ms, not wait for the 5s sleep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_observer_timeout_kills_the_whole_process_group() {
        let dir = std::env::temp_dir().join(format!("subgov-fleet-pgroup-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("grandchild-ran");
        // The direct child backgrounds a grandchild that (if it survives)
        // writes `marker` after 300ms, then blocks for 5s itself. A 100ms
        // timeout must kill the *group*: if only the direct child died, the
        // backgrounded grandchild would still write the marker.
        let script = format!("(sleep 0.3; touch {}) & sleep 5", marker.display());
        let argv = vec!["/bin/sh".to_string(), "-c".to_string(), script];
        let result = command_observer_current_workers(&argv, Duration::from_millis(100));
        assert!(result.is_err());
        thread::sleep(Duration::from_millis(700));
        assert!(
            !marker.exists(),
            "grandchild should have been killed along with the rest of the process group"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn command_actuator_succeeds_well_within_a_short_timeout() {
        let dir =
            std::env::temp_dir().join(format!("subgov-fleet-actuator-to-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("target");
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("echo {{desired_workers}} > {}", path.display()),
        ];
        command_actuator_actuate(&argv, 7, Duration::from_millis(500)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap().trim(), "7");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn command_actuator_times_out_on_a_hanging_command() {
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 5".to_string(),
        ];
        let start = Instant::now();
        let result = command_actuator_actuate(&argv, 3, Duration::from_millis(100));
        assert!(result.is_err());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "should time out around 100ms, not wait for the 5s sleep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_actuator_timeout_kills_the_whole_process_group() {
        let dir =
            std::env::temp_dir().join(format!("subgov-fleet-act-pgroup-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("grandchild-ran");
        let script = format!("(sleep 0.3; touch {}) & sleep 5", marker.display());
        let argv = vec!["/bin/sh".to_string(), "-c".to_string(), script];
        let result = command_actuator_actuate(&argv, 3, Duration::from_millis(100));
        assert!(result.is_err());
        thread::sleep(Duration::from_millis(700));
        assert!(
            !marker.exists(),
            "grandchild should have been killed along with the rest of the process group"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn idempotency_token_is_stable_for_the_same_command_and_desired_count() {
        let argv = vec!["/usr/bin/true".to_string(), "arg".to_string()];
        assert_eq!(idempotency_token(&argv, 5), idempotency_token(&argv, 5));
    }

    #[test]
    fn idempotency_token_changes_when_the_desired_count_changes() {
        let argv = vec!["/usr/bin/true".to_string()];
        assert_ne!(idempotency_token(&argv, 5), idempotency_token(&argv, 6));
    }

    #[test]
    fn idempotency_token_changes_when_the_command_changes() {
        assert_ne!(
            idempotency_token(&["/usr/bin/true".to_string()], 5),
            idempotency_token(&["/usr/bin/false".to_string()], 5)
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_actuator_passes_the_idempotency_token_as_a_non_secret_env_var() {
        let dir = std::env::temp_dir().join(format!("subgov-fleet-idem-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("token");
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("echo \"$SUBGOV_IDEMPOTENCY_TOKEN\" > {}", marker.display()),
        ];
        command_actuator_actuate(&argv, 7, Duration::from_millis(500)).unwrap();
        let recorded = fs::read_to_string(&marker).unwrap();
        assert_eq!(recorded.trim(), idempotency_token(&argv, 7));
        let _ = fs::remove_dir_all(&dir);
    }

    // --- WP4 definition of done: "a hung or failed external helper is
    // bounded, isolated to its account, and cannot be reported as successful
    // actuation." ---

    #[cfg(unix)]
    #[test]
    fn a_hung_actuation_is_bounded_and_never_reported_as_successful() {
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 5".to_string(),
        ];
        let start = Instant::now();
        let result = command_actuator_actuate(&argv, 4, Duration::from_millis(100));
        assert!(
            result.is_err(),
            "a hung actuation must never be reported as success"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "a hung actuation must be bounded by the timeout, not the hang"
        );
    }

    // plan.md §11.2: "retain the prior state on actuation failure so the
    // next cycle reconciles." A hung/failed attempt is retried unchanged
    // (same rendered command, same desired count) once the prior state is
    // retained -- this exercises that retry end to end (not just the pure
    // idempotency_token function) to prove the token wiring survives the
    // hang-and-kill path, not only the happy path.
    #[cfg(unix)]
    #[test]
    fn retrying_an_unchanged_desired_count_after_a_hung_actuation_reuses_the_same_idempotency_token(
    ) {
        let dir =
            std::env::temp_dir().join(format!("subgov-fleet-idem-retry-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let done_marker = dir.join("attempted");
        let token_marker = dir.join("token");
        // Always records the token first. The first invocation then hangs
        // (nothing has attempted yet); the second -- the identical argv and
        // desired count a retry after retained state produces -- finds the
        // marker from the first attempt and exits immediately instead.
        let script = format!(
            "echo \"$SUBGOV_IDEMPOTENCY_TOKEN\" > {token}; if [ -f {done} ]; then exit 0; else touch {done}; sleep 5; fi",
            token = token_marker.display(),
            done = done_marker.display(),
        );
        let argv = vec!["/bin/sh".to_string(), "-c".to_string(), script];

        let first = command_actuator_actuate(&argv, 6, Duration::from_millis(100));
        assert!(
            first.is_err(),
            "the hung first attempt must never be reported as success"
        );
        let first_token = fs::read_to_string(&token_marker).unwrap();

        let second = command_actuator_actuate(&argv, 6, Duration::from_millis(500));
        assert!(
            second.is_ok(),
            "the retry must succeed once idempotently completed"
        );
        let second_token = fs::read_to_string(&token_marker).unwrap();

        assert_eq!(
            first_token.trim(),
            second_token.trim(),
            "an unchanged retry must see the same idempotency token as the hung attempt"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // Exercises isolation at the fleet-primitive level: the actuator
    // functions carry no shared/global state, so a hung actuation for one
    // account cannot bleed into another's. (Per-account isolation *within a
    // cycle* -- one account's failure not aborting the others -- is a
    // main::run_cycle property and is covered by that module's own tests;
    // this is the fleet-level guarantee that makes it possible.)
    #[cfg(unix)]
    #[test]
    fn a_hung_actuation_does_not_affect_an_independent_actuation() {
        let dir =
            std::env::temp_dir().join(format!("subgov-fleet-isolation-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target_path = dir.join("target");

        let hung_argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 5".to_string(),
        ];
        let hung_result = command_actuator_actuate(&hung_argv, 4, Duration::from_millis(100));
        assert!(hung_result.is_err());

        let working_config = fleet_config_with_actuator(ActuatorConfig::TargetFile {
            path: target_path.clone(),
        });
        let outcome = actuate(&working_config, 3, 8).unwrap();
        assert!(
            outcome.actuated,
            "an independent account's actuation must succeed even after another hung"
        );
        assert_eq!(fs::read_to_string(&target_path).unwrap().trim(), "8");
        let _ = fs::remove_dir_all(&dir);
    }

    fn fleet_config_with(
        min_workers: u32,
        max_workers: u32,
        observer_reconciliation: ObserverReconciliation,
        workers: u32,
    ) -> FleetConfig {
        FleetConfig {
            min_workers,
            max_workers,
            bootstrap_workers: min_workers,
            max_scale_up_per_cycle: 1,
            max_scale_down_per_cycle: 1,
            observer: Some(WorkerObserverConfig::Static { workers }),
            actuator: ActuatorConfig::None,
            observer_reconciliation,
            hosts: None,
        }
    }

    #[test]
    fn current_workers_accepts_an_in_range_observation() {
        let config = fleet_config_with(1, 10, ObserverReconciliation::Strict, 5);
        assert_eq!(current_workers(&config).unwrap(), 5);
    }

    #[test]
    fn current_workers_rejects_an_out_of_range_observation_by_default() {
        let config = fleet_config_with(1, 10, ObserverReconciliation::Strict, 11);
        assert!(current_workers(&config).is_err());
    }

    #[test]
    fn current_workers_clamps_an_out_of_range_observation_in_clamp_mode() {
        let over = fleet_config_with(1, 10, ObserverReconciliation::Clamp, 11);
        assert_eq!(current_workers(&over).unwrap(), 10);
        let under = fleet_config_with(1, 10, ObserverReconciliation::Clamp, 0);
        assert_eq!(current_workers(&under).unwrap(), 1);
    }

    fn fleet_config_with_actuator(actuator: ActuatorConfig) -> FleetConfig {
        let mut config = fleet_config_with(1, 10, ObserverReconciliation::Strict, 5);
        config.actuator = actuator;
        config
    }

    #[test]
    fn actuate_skips_invocation_when_desired_equals_observed() {
        // A command actuator pointed at a binary that does not exist:
        // if `actuate` invoked it, this would fail to spawn and return Err.
        // Returning Ok with actuated:false proves the invocation was
        // skipped, not merely that it happened to succeed.
        let config = fleet_config_with_actuator(ActuatorConfig::Command {
            argv: vec!["/nonexistent/subgov-test-actuator".to_string()],
        });
        let outcome = actuate(&config, 5, 5).unwrap();
        assert!(!outcome.actuated);
    }

    #[test]
    fn actuate_reports_not_actuated_for_none_even_when_desired_differs() {
        let config = fleet_config_with_actuator(ActuatorConfig::None);
        let outcome = actuate(&config, 3, 8).unwrap();
        assert!(!outcome.actuated);
    }

    #[test]
    fn actuate_invokes_and_reports_actuated_when_desired_differs_and_actuator_is_not_none() {
        let dir = std::env::temp_dir().join(format!("subgov-fleet-actuate-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("target");
        let config = fleet_config_with_actuator(ActuatorConfig::TargetFile { path: path.clone() });
        let outcome = actuate(&config, 3, 8).unwrap();
        assert!(outcome.actuated);
        assert_eq!(fs::read_to_string(&path).unwrap().trim(), "8");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn actuate_propagates_a_real_actuation_failure() {
        let config = fleet_config_with_actuator(ActuatorConfig::Command {
            argv: vec!["/nonexistent/subgov-test-actuator".to_string()],
        });
        assert!(actuate(&config, 3, 8).is_err());
    }
}
