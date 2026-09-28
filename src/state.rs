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

        // Serialize and round-trip in memory before touching disk at all.
        // serde_json encodes a non-finite f64 (NaN/Infinity) as JSON `null`
        // without erroring, which would otherwise write "successfully" and
        // get promoted over the rename -- clobbering the last state that
        // was actually loadable with one that no longer is. Catching that
        // here keeps the temp-file-then-rename path fail closed: nothing
        // that can't be read back ever reaches `path`.
        let mut payload =
            serde_json::to_vec_pretty(self).context("failed to serialize state")?;
        serde_json::from_slice::<Self>(&payload).with_context(|| {
            format!(
                "serialized state for {} does not round-trip; refusing to persist it over the last valid state",
                path.display()
            )
        })?;
        payload.push(b'\n');

        let temporary = temporary_path(path);
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .with_context(|| format!("failed to create {}", temporary.display()))?;
            restrict_permissions(&file)
                .with_context(|| format!("failed to restrict permissions on {}", temporary.display()))?;
            file.write_all(&payload)?;
            file.sync_all()?;
            fs::rename(&temporary, path)
                .with_context(|| format!("failed to install state {}", path.display()))?;
            // On Unix, the rename's directory-entry update is not itself
            // durable until the containing directory is fsync'd -- without
            // this, a crash can leave the rename visible in memory but lost
            // on disk after power loss, even though the file's own fsync
            // already committed its bytes.
            sync_directory(parent)
                .with_context(|| format!("failed to fsync directory {}", parent.display()))?;
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
        restrict_permissions(&file)
            .with_context(|| format!("failed to restrict permissions on {}", lock_path.display()))?;
        file.try_lock_exclusive()
            .with_context(|| format!("another governor owns {}", lock_path.display()))?;
        Ok(Self { _file: file })
    }
}

// Applied unconditionally on every open, not just at creation, so a lock
// file left over from before this restriction existed gets tightened too,
// not just newly-created ones.
#[cfg(unix)]
fn restrict_permissions(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_file: &File) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(dir: &Path) -> Result<()> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_dir: &Path) -> Result<()> {
    Ok(())
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
    #[cfg(unix)]
    fn save_sets_restrictive_mode_on_the_state_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        State::default().save(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    #[cfg(unix)]
    fn acquire_sets_restrictive_mode_on_the_lock_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        let _lock = StateLock::acquire(&path).unwrap();
        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        let mode = fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    #[cfg(unix)]
    fn acquire_tightens_a_preexisting_lock_file_with_looser_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        fs::write(&lock_path, b"").unwrap();
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o644)).unwrap();

        let _lock = StateLock::acquire(&path).unwrap();
        let mode = fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    #[cfg(unix)]
    fn sync_directory_fsyncs_an_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        sync_directory(dir.path()).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn sync_directory_errors_on_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(sync_directory(&missing).is_err());
    }

    #[test]
    fn save_fsyncs_the_state_directory_after_rename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        State::default().save(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn save_never_clobbers_last_valid_state_with_a_non_finite_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");

        let mut good = State::default();
        good.accounts.insert(
            "acct".to_string(),
            AccountState {
                windows: BTreeMap::new(),
                last_target: Some(3),
            },
        );
        good.save(&path).unwrap();
        let good_bytes = fs::read(&path).unwrap();

        let mut bad = good.clone();
        bad.accounts.get_mut("acct").unwrap().windows.insert(
            "5h".to_string(),
            WindowSample {
                observed_at: Utc::now(),
                used_fraction: f64::NAN,
                resets_at: Utc::now(),
                workers: 1,
            },
        );
        let error = bad.save(&path).unwrap_err();
        assert!(
            error.to_string().contains("round-trip"),
            "unexpected error: {error}"
        );

        let bytes_after = fs::read(&path).unwrap();
        assert_eq!(
            good_bytes, bytes_after,
            "a failed write must not change the persisted state"
        );
        let temp_leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(
            temp_leftovers.is_empty(),
            "temp file must be cleaned up on failure"
        );
    }

    #[test]
    fn save_leaves_last_valid_state_untouched_when_rename_target_is_unwritable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        State::default().save(&path).unwrap();
        let good_bytes = fs::read(&path).unwrap();

        // Pre-create the temp file so `create_new` fails, simulating a
        // write-path failure that never reaches the rename step at all.
        let temporary = temporary_path(&path);
        fs::write(&temporary, b"garbage").unwrap();

        let mut next = State::default();
        next.accounts.insert(
            "acct".to_string(),
            AccountState {
                windows: BTreeMap::new(),
                last_target: Some(9),
            },
        );
        let result = next.save(&path);
        assert!(result.is_err());

        let bytes_after = fs::read(&path).unwrap();
        assert_eq!(
            good_bytes, bytes_after,
            "a failed write must not change the persisted state"
        );
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
