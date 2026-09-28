use crate::config::{ActuatorConfig, FleetConfig, WorkerObserverConfig};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
/// integration tests.
pub trait Actuator {
    fn actuate(&self, desired: u32) -> Result<()>;
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
    observer_for(&config.observer).current_workers()
}

pub fn actuate(config: &FleetConfig, desired: u32) -> Result<()> {
    actuator_for(&config.actuator).actuate(desired)
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
        let text = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read worker count {}", self.path.display()))?;
        parse_worker_count(&text)
    }
}

struct CommandObserver {
    argv: Vec<String>,
}

impl Observer for CommandObserver {
    fn current_workers(&self) -> Result<u32> {
        let output = Command::new(&self.argv[0])
            .args(&self.argv[1..])
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .with_context(|| format!("failed to execute worker observer {}", self.argv[0]))?;
        if !output.status.success() {
            bail!(
                "worker observer {} exited with {}",
                self.argv[0],
                output.status
            );
        }
        parse_worker_count(&String::from_utf8(output.stdout).context("worker count was not UTF-8")?)
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
}
