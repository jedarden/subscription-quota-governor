use crate::config::{ActuatorConfig, FleetConfig, WorkerObserverConfig};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn current_workers(config: &FleetConfig) -> Result<u32> {
    match &config.observer {
        WorkerObserverConfig::Static { workers } => Ok(*workers),
        WorkerObserverConfig::File { path } => {
            let text = fs::read_to_string(path)
                .with_context(|| format!("failed to read worker count {}", path.display()))?;
            parse_worker_count(&text)
        }
        WorkerObserverConfig::Command { argv } => {
            let output = Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(Stdio::null())
                .stderr(Stdio::inherit())
                .output()
                .with_context(|| format!("failed to execute worker observer {}", argv[0]))?;
            if !output.status.success() {
                bail!("worker observer {} exited with {}", argv[0], output.status);
            }
            parse_worker_count(
                &String::from_utf8(output.stdout).context("worker count was not UTF-8")?,
            )
        }
    }
}

pub fn actuate(config: &FleetConfig, desired: u32) -> Result<()> {
    match &config.actuator {
        ActuatorConfig::None => Ok(()),
        ActuatorConfig::TargetFile { path } => write_target(path, desired),
        ActuatorConfig::Command { argv } => {
            let rendered: Vec<String> = argv
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
