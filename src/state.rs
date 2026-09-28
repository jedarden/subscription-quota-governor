use crate::model::QuotaSnapshot;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The only schema version this binary can write, and the newest it can
/// read. A file with no `schema_version` at all predates versioning but has
/// the same shape as version 1, so it defaults to current rather than an
/// unknown-legacy marker.
pub const STATE_SCHEMA_VERSION: u32 = 1;

fn current_schema_version() -> u32 {
    STATE_SCHEMA_VERSION
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct State {
    #[serde(default = "current_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub accounts: BTreeMap<String, AccountState>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            accounts: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AccountState {
    #[serde(default)]
    pub windows: BTreeMap<String, WindowSample>,
    #[serde(default)]
    pub last_target: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WindowSample {
    pub observed_at: DateTime<Utc>,
    pub used_fraction: f64,
    pub resets_at: DateTime<Utc>,
    pub workers: u32,
}

impl AccountState {
    pub fn record(&mut self, snapshot: &QuotaSnapshot, workers: u32, target: u32) {
        self.windows = snapshot
            .windows
            .iter()
            .map(|window| {
                (
                    window.id.clone(),
                    WindowSample {
                        observed_at: snapshot.observed_at,
                        used_fraction: window.used_fraction,
                        resets_at: window.resets_at,
                        workers,
                    },
                )
            })
            .collect();
        self.last_target = Some(target);
    }
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        let state: Self = match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse state {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read state {}", path.display()))
            }
        };
        if state.schema_version > STATE_SCHEMA_VERSION {
            bail!(
                "state {} has schema version {}, newer than the {} this binary supports; refusing to load and risk misreading it",
                path.display(),
                state.schema_version,
                STATE_SCHEMA_VERSION
            );
        }
        Ok(state)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create state directory {}", parent.display()))?;
        let temporary = temporary_path(path);
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .with_context(|| format!("failed to create {}", temporary.display()))?;
            serde_json::to_writer_pretty(&mut file, self)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, path)
                .with_context(|| format!("failed to install state {}", path.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

pub struct StateLock {
    _file: File,
}

impl StateLock {
    pub fn acquire(state_path: &Path) -> Result<Self> {
        let parent = state_path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let lock_path = PathBuf::from(format!("{}.lock", state_path.display()));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("failed to open lock {}", lock_path.display()))?;
        file.try_lock_exclusive()
            .with_context(|| format!("another governor owns {}", lock_path.display()))?;
        Ok(Self { _file: file })
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.json");
    path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_state_carries_current_schema_version() {
        let state = State::default();
        assert_eq!(state.schema_version, STATE_SCHEMA_VERSION);
    }

    #[test]
    fn missing_schema_version_defaults_to_current() {
        let state: State = serde_json::from_str("{}").unwrap();
        assert_eq!(state.schema_version, STATE_SCHEMA_VERSION);
    }

    #[test]
    fn save_then_load_round_trips_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        State::default().save(&path).unwrap();
        let loaded = State::load(&path).unwrap();
        assert_eq!(loaded.schema_version, STATE_SCHEMA_VERSION);
    }

    #[test]
    fn load_refuses_a_newer_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        fs::write(&path, r#"{"schema_version":999,"accounts":{}}"#).unwrap();
        let error = State::load(&path).unwrap_err();
        assert!(
            error.to_string().contains("schema version"),
            "unexpected error: {error}"
        );
    }
}
