use crate::config::{ActuatorConfig, FleetConfig, ObserverReconciliation, WorkerObserverConfig};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Maximum bytes read from an observer's file or command stdout (plan.md
/// §11.1: "bound file and stdout size"). A worker count is a bare integer or
/// a one-field JSON object, so this is generous headroom rather than a tight
/// budget.
const MAX_OBSERVER_BYTES: u64 = 4096;

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
    let observed = observer_for(&config.observer).current_workers()?;
    reconcile_observed_range(observed, config)
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
    if desired == observed {
        return Ok(ActuationOutcome { actuated: false });
    }
    if matches!(config.actuator, ActuatorConfig::None) {
        return Ok(ActuationOutcome { actuated: false });
    }
    actuator_for(&config.actuator).actuate(desired)?;
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
        let mut child = Command::new(&self.argv[0])
            .args(&self.argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to start worker observer {}", self.argv[0]))?;
        let mut stdout = child
            .stdout
            .take()
            .context("worker observer stdout was not piped")?;
        let bytes = read_bounded(&mut stdout, MAX_OBSERVER_BYTES);
        drop(stdout);
        if bytes.is_err() {
            // The child may still be trying to write past the bound; kill it
            // rather than risk it blocking forever on a full pipe buffer
            // nobody is draining.
            let _ = child.kill();
            let _ = child.wait();
        }
        let bytes = bytes.with_context(|| format!("worker observer {} stdout", self.argv[0]))?;

        let status = child
            .wait()
            .with_context(|| format!("failed to wait for worker observer {}", self.argv[0]))?;
        if !status.success() {
            bail!("worker observer {} exited with {}", self.argv[0], status);
        }
        let text = String::from_utf8(bytes).context("worker count was not UTF-8")?;
        parse_worker_count(&text)
    }
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
        let rendered: Vec<String> = self
            .argv
            .iter()
            .map(|argument| argument.replace("{desired_workers}", &desired.to_string()))
            .collect();
        let status = Command::new(&rendered[0])
            .args(&rendered[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("failed to execute fleet actuator {}", rendered[0]))?;
        if !status.success() {
            bail!("fleet actuator {} exited with {status}", rendered[0]);
        }
        Ok(())
    }
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
    use crate::config::{ActuatorConfig, WorkerObserverConfig};

    struct RecordingActuator {
        calls: std::cell::RefCell<Vec<u32>>,
    }

    impl Actuator for RecordingActuator {
        fn actuate(&self, desired: u32) -> Result<()> {
            self.calls.borrow_mut().push(desired);
            Ok(())
        }
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
            observer: WorkerObserverConfig::Static { workers },
            actuator: ActuatorConfig::None,
            observer_reconciliation,
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
