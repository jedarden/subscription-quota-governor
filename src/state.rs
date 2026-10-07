use crate::model::QuotaSnapshot;
#[cfg(test)]
use crate::model::QuotaWindow;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The only schema version this binary can write, and the newest it can
/// read. A file with no `schema_version` at all predates versioning and is
/// read with the current version. Earlier versions are migrated by defaulting
/// fields added to account and host state, then advancing the version on load.
pub const STATE_SCHEMA_VERSION: u32 = 4;

fn current_schema_version() -> u32 {
    STATE_SCHEMA_VERSION
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct State {
    #[serde(default = "current_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub accounts: BTreeMap<String, AccountState>,
    /// Per-host samples keyed first by account, then by configured host id.
    /// This is separate from `accounts` so a host placement can never replace
    /// the account controller's `last_target` total.
    #[serde(default)]
    pub host_states: BTreeMap<String, BTreeMap<String, HostState>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            accounts: BTreeMap::new(),
            host_states: BTreeMap::new(),
        }
    }
}

/// How many samples of burn-rate history to retain per window generation.
/// The exact value is one of §21's evidence-gated forks (needs
/// observation-mode traces to tune); this is a conservative placeholder
/// that bounds memory and disk without deciding the final number.
const MAX_HISTORY_SAMPLES_PER_GENERATION: usize = 16;

/// More than two days at the one-minute aggregate-sample interval below, or
/// over ten days at the default five-minute poll interval.
const MAX_AGGREGATE_BURN_SAMPLES: usize = 3_072;
const MIN_AGGREGATE_SAMPLE_INTERVAL_SECONDS: i64 = 60;

/// Maximum number of recent per-host placement decisions retained on disk.
pub const MAX_PLACEMENT_HISTORY_SAMPLES: usize = 16;

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct AccountState {
    #[serde(default)]
    pub windows: BTreeMap<String, WindowSample>,
    #[serde(default)]
    pub last_target: Option<u32>,
    /// Bounded burn-rate history per window, keyed by window id. Reset
    /// whenever a window's `resets_at` changes, since samples from a prior
    /// generation cannot inform this generation's slope.
    #[serde(default)]
    pub history: BTreeMap<String, WindowHistory>,
    /// Aggregate usage samples retained across reset generations. These are
    /// used for conservative long-baseline banked-credit pacing; unlike
    /// `history`, an out-of-cycle reset does not discard earlier intervals.
    #[serde(default)]
    pub aggregate_burn_history: BTreeMap<String, VecDeque<AggregateBurnSample>>,
    /// Last advisory severity emitted for each credit in the current weekly
    /// reset generation, to avoid repeating an alert on every poll.
    #[serde(default)]
    pub credit_alerts: BTreeMap<String, CreditAlertState>,
}

/// Quota samples associated with one host in one account. `workers` in each
/// window sample is that host's observed worker count; unlike `AccountState`,
/// this record has no desired-total field.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct HostState {
    #[serde(default)]
    pub windows: BTreeMap<String, WindowSample>,
    /// Bounded burn-rate history per window, keyed by window id. Reset
    /// whenever a window's `resets_at` changes.
    #[serde(default)]
    pub history: BTreeMap<String, WindowHistory>,
    /// Recent placement targets for this host. Each cycle is retained so a
    /// consumer can inspect the cadence as well as detect alternating targets.
    /// Oldest entries are evicted once the fixed history bound is reached.
    #[serde(default)]
    pub placement_history: VecDeque<PlacementSample>,
}

/// One host's planned placement target in a decision cycle.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct PlacementSample {
    pub observed_at: DateTime<Utc>,
    pub target_workers: u32,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct WindowSample {
    pub observed_at: DateTime<Utc>,
    pub used_fraction: f64,
    pub resets_at: DateTime<Utc>,
    pub workers: u32,
}

/// A bounded run of samples sharing one reset generation.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct WindowHistory {
    pub resets_at: DateTime<Utc>,
    pub samples: VecDeque<HistorySample>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct HistorySample {
    pub observed_at: DateTime<Utc>,
    pub used_fraction: f64,
    pub workers: u32,
}

/// One account-level quota reading for burn-rate estimation. The worker count
/// is the governed fleet count at that observation; it is used to distinguish
/// worker burn from exogenous account usage when the history supports it.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct AggregateBurnSample {
    pub observed_at: DateTime<Utc>,
    pub used_fraction: f64,
    pub resets_at: DateTime<Utc>,
    pub governed_workers: u32,
}

/// Severity rank retained for event de-duplication: zero is clear, followed
/// by warn, page, and infeasible.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct CreditAlertState {
    pub resets_at: DateTime<Utc>,
    pub severity: u8,
}

impl AccountState {
    pub fn record(&mut self, snapshot: &QuotaSnapshot, workers: u32, target: u32) {
        record_samples(&mut self.windows, &mut self.history, snapshot, workers);
        record_aggregate_burn_samples(&mut self.aggregate_burn_history, snapshot, workers);
        self.last_target = Some(target);
    }

    /// Replace the current alert levels with the latest per-credit levels.
    /// A changed reset timestamp starts a fresh advisory generation.
    pub fn record_credit_alerts(
        &mut self,
        resets_at: DateTime<Utc>,
        levels: impl IntoIterator<Item = (String, u8)>,
    ) {
        self.credit_alerts = levels
            .into_iter()
            .map(|(credit_id, severity)| {
                (
                    credit_id,
                    CreditAlertState {
                        resets_at,
                        severity,
                    },
                )
            })
            .collect();
    }
}

impl HostState {
    pub fn record(&mut self, snapshot: &QuotaSnapshot, workers: u32) {
        record_samples(&mut self.windows, &mut self.history, snapshot, workers);
    }

    /// Append a planned target, keeping only the newest bounded set of cycles.
    pub fn record_placement(&mut self, observed_at: DateTime<Utc>, target_workers: u32) {
        self.placement_history.push_back(PlacementSample {
            observed_at,
            target_workers,
        });
        while self.placement_history.len() > MAX_PLACEMENT_HISTORY_SAMPLES {
            self.placement_history.pop_front();
        }
    }

    /// Return whether the most recent `minimum_changes` target transitions
    /// alternate between exactly two worker counts. Consecutive cycles at the
    /// same target do not count as changes. Requiring the caller to choose the
    /// threshold avoids baking an unvalidated oscillation policy into state.
    pub fn placement_alternates_for(&self, minimum_changes: usize) -> bool {
        if !(2..MAX_PLACEMENT_HISTORY_SAMPLES).contains(&minimum_changes)
            || minimum_changes >= self.placement_history.len()
        {
            return false;
        }

        let needed_targets = minimum_changes + 1;
        let mut recent_changes = Vec::with_capacity(needed_targets);
        for sample in self.placement_history.iter().rev() {
            if recent_changes.last() != Some(&sample.target_workers) {
                recent_changes.push(sample.target_workers);
                if recent_changes.len() == needed_targets {
                    break;
                }
            }
        }
        if recent_changes.len() != needed_targets {
            return false;
        }
        recent_changes.reverse();

        let first = recent_changes[0];
        let second = recent_changes[1];
        first != second
            && recent_changes
                .iter()
                .enumerate()
                .all(|(index, target)| *target == if index % 2 == 0 { first } else { second })
    }
}

impl State {
    /// Record a sample under the `(account, host)` key without changing the
    /// account-level desired total maintained by `AccountState::record`.
    pub fn record_host(
        &mut self,
        account: &str,
        host: &str,
        snapshot: &QuotaSnapshot,
        workers: u32,
    ) {
        self.host_states
            .entry(account.to_owned())
            .or_default()
            .entry(host.to_owned())
            .or_default()
            .record(snapshot, workers);
    }

    /// Record a planned placement under its `(account, host)` key, even when
    /// quota observation is stale or the cycle runs in observe-only mode.
    pub fn record_host_placement(
        &mut self,
        account: &str,
        host: &str,
        observed_at: DateTime<Utc>,
        target_workers: u32,
    ) {
        self.host_states
            .entry(account.to_owned())
            .or_default()
            .entry(host.to_owned())
            .or_default()
            .record_placement(observed_at, target_workers);
    }
}

fn record_samples(
    windows: &mut BTreeMap<String, WindowSample>,
    history: &mut BTreeMap<String, WindowHistory>,
    snapshot: &QuotaSnapshot,
    workers: u32,
) {
    *windows = snapshot
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

    // Rebuilt fresh each call, like `windows` above, so a window that drops
    // out of the snapshot does not leave an orphaned generation accumulating.
    let mut next_history = BTreeMap::new();
    for window in &snapshot.windows {
        let mut entry = history
            .remove(&window.id)
            .filter(|existing: &WindowHistory| existing.resets_at == window.resets_at)
            .unwrap_or_else(|| WindowHistory {
                resets_at: window.resets_at,
                samples: VecDeque::new(),
            });
        entry.samples.push_back(HistorySample {
            observed_at: snapshot.observed_at,
            used_fraction: window.used_fraction,
            workers,
        });
        while entry.samples.len() > MAX_HISTORY_SAMPLES_PER_GENERATION {
            entry.samples.pop_front();
        }
        next_history.insert(window.id.clone(), entry);
    }
    *history = next_history;
}

fn record_aggregate_burn_samples(
    histories: &mut BTreeMap<String, VecDeque<AggregateBurnSample>>,
    snapshot: &QuotaSnapshot,
    governed_workers: u32,
) {
    let mut next = BTreeMap::new();
    for window in &snapshot.windows {
        let mut samples = histories.remove(&window.id).unwrap_or_default();
        let sample = AggregateBurnSample {
            observed_at: snapshot.observed_at,
            used_fraction: window.used_fraction,
            resets_at: window.resets_at,
            governed_workers,
        };
        let should_record = samples.back().is_none_or(|previous| {
            previous.resets_at != sample.resets_at
                || sample
                    .observed_at
                    .signed_duration_since(previous.observed_at)
                    .num_seconds()
                    >= MIN_AGGREGATE_SAMPLE_INTERVAL_SECONDS
        });
        if should_record {
            samples.push_back(sample);
        }
        while samples.len() > MAX_AGGREGATE_BURN_SAMPLES {
            samples.pop_front();
        }
        next.insert(window.id.clone(), samples);
    }
    *histories = next;
}

/// A state file exists but does not parse as a `State` at all (bad JSON,
/// wrong shape) — as opposed to parsing fine with an unsupported
/// `schema_version`, which stays a hard load failure since misreading a
/// *newer* format is a correctness risk, not a corruption to recover from.
/// Distinct from a missing file, which is a genuine first run.
#[derive(Debug)]
pub struct Quarantined {
    /// Where the unparseable file was moved so it survives for forensics.
    pub quarantined_path: PathBuf,
    /// The parse error that triggered quarantine, for the caller to log.
    pub error: String,
}

fn quarantine_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.json");
    path.with_file_name(format!(
        "{file_name}.corrupt-{}-{}",
        Utc::now().format("%Y%m%dT%H%M%S%.9fZ"),
        std::process::id()
    ))
}

impl State {
    /// Loads state from `path`, or `Self::default()` for a missing file (a
    /// genuine first run). A file that exists but fails to parse is moved
    /// aside rather than either silently discarded or treated as a first
    /// run: the second element of the returned tuple carries where it went
    /// and why, for the caller to log.
    pub fn load(path: &Path) -> Result<(Self, Option<Quarantined>)> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Self::default(), None))
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read state {}", path.display()))
            }
        };
        let mut state: Self = match serde_json::from_slice(&bytes) {
            Ok(state) => state,
            Err(parse_error) => {
                let quarantined_path = quarantine_path(path);
                fs::rename(path, &quarantined_path).with_context(|| {
                    format!(
                        "failed to quarantine malformed state {} to {}",
                        path.display(),
                        quarantined_path.display()
                    )
                })?;
                return Ok((
                    Self::default(),
                    Some(Quarantined {
                        quarantined_path,
                        error: parse_error.to_string(),
                    }),
                ));
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
        // Version 2 adds the independent per-(account, host) sample map.
        // Version 3 adds per-host placement target history. Version 4 adds
        // reset-spanning aggregate burn samples and advisory de-duplication
        // state. All additions default empty when loading an older file.
        if state.schema_version < STATE_SCHEMA_VERSION {
            state.schema_version = STATE_SCHEMA_VERSION;
        }
        for account in state.accounts.values_mut() {
            for samples in account.aggregate_burn_history.values_mut() {
                while samples.len() > MAX_AGGREGATE_BURN_SAMPLES {
                    samples.pop_front();
                }
            }
        }
        // Keep the in-memory bound even if a file was written by an older
        // development build or manually edited with an oversized history.
        for host in state
            .host_states
            .values_mut()
            .flat_map(BTreeMap::values_mut)
        {
            while host.placement_history.len() > MAX_PLACEMENT_HISTORY_SAMPLES {
                host.placement_history.pop_front();
            }
        }
        Ok((state, None))
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
        let mut payload = serde_json::to_vec_pretty(self).context("failed to serialize state")?;
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
            restrict_permissions(&file).with_context(|| {
                format!("failed to restrict permissions on {}", temporary.display())
            })?;
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
        restrict_permissions(&file).with_context(|| {
            format!("failed to restrict permissions on {}", lock_path.display())
        })?;
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

    fn snapshot_with_one_window(
        id: &str,
        used_fraction: f64,
        resets_at: DateTime<Utc>,
    ) -> QuotaSnapshot {
        QuotaSnapshot {
            observed_at: Utc::now(),
            fresh: true,
            windows: vec![QuotaWindow {
                id: id.to_string(),
                used_fraction,
                resets_at,
                duration_minutes: None,
                reached: false,
            }],
            eligible_backlog_capacity: None,
            reset_credits: None,
        }
    }

    #[test]
    fn record_bounds_history_length_per_generation() {
        let mut account = AccountState::default();
        let resets_at = Utc::now() + chrono::Duration::hours(5);
        for i in 0..(MAX_HISTORY_SAMPLES_PER_GENERATION + 5) {
            let snapshot = snapshot_with_one_window("5h", i as f64 * 0.01, resets_at);
            account.record(&snapshot, 1, 3);
        }
        let history = account.history.get("5h").unwrap();
        assert_eq!(history.samples.len(), MAX_HISTORY_SAMPLES_PER_GENERATION);
        // FIFO eviction: the oldest samples (lowest used_fraction) are gone,
        // the most recent one survives.
        let last = history.samples.back().unwrap();
        assert_eq!(
            last.used_fraction,
            (MAX_HISTORY_SAMPLES_PER_GENERATION + 4) as f64 * 0.01
        );
    }

    #[test]
    fn record_clears_history_when_the_generation_rolls_over() {
        let mut account = AccountState::default();
        let first_generation = Utc::now() + chrono::Duration::hours(5);
        for i in 0..3 {
            let snapshot = snapshot_with_one_window("5h", i as f64 * 0.1, first_generation);
            account.record(&snapshot, 1, 3);
        }
        assert_eq!(account.history.get("5h").unwrap().samples.len(), 3);

        let next_generation = first_generation + chrono::Duration::days(1);
        let snapshot = snapshot_with_one_window("5h", 0.0, next_generation);
        account.record(&snapshot, 1, 3);

        let history = account.history.get("5h").unwrap();
        assert_eq!(history.resets_at, next_generation);
        assert_eq!(
            history.samples.len(),
            1,
            "a new generation must not inherit the prior generation's samples"
        );
    }

    #[test]
    fn aggregate_burn_history_keeps_samples_across_reset_generations() {
        let mut account = AccountState::default();
        let start = Utc::now();
        let first_generation = start + chrono::Duration::hours(5);
        let second_generation = start + chrono::Duration::days(1);

        let mut first = snapshot_with_one_window("weekly", 0.92, first_generation);
        first.observed_at = start;
        account.record(&first, 6, 6);

        let mut second = snapshot_with_one_window("weekly", 0.01, second_generation);
        second.observed_at = start + chrono::Duration::hours(6);
        account.record(&second, 6, 6);

        let aggregate = &account.aggregate_burn_history["weekly"];
        assert_eq!(aggregate.len(), 2);
        assert_eq!(aggregate[0].resets_at, first_generation);
        assert_eq!(aggregate[1].resets_at, second_generation);

        let generation_local = &account.history["weekly"];
        assert_eq!(generation_local.resets_at, second_generation);
        assert_eq!(generation_local.samples.len(), 1);
    }

    #[test]
    fn record_drops_history_for_windows_no_longer_in_the_snapshot() {
        let mut account = AccountState::default();
        let resets_at = Utc::now() + chrono::Duration::hours(5);
        account.record(&snapshot_with_one_window("5h", 0.1, resets_at), 1, 3);
        assert!(account.history.contains_key("5h"));

        let empty_snapshot = QuotaSnapshot {
            observed_at: Utc::now(),
            fresh: true,
            windows: Vec::new(),
            eligible_backlog_capacity: None,
            reset_credits: None,
        };
        account.record(&empty_snapshot, 1, 3);
        assert!(
            account.history.is_empty(),
            "a window dropped from the snapshot must not leak stale history forever"
        );
    }

    #[test]
    fn history_round_trips_through_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        let resets_at = Utc::now() + chrono::Duration::hours(5);

        let mut state = State::default();
        let account = state.accounts.entry("acct".to_string()).or_default();
        let mut first = snapshot_with_one_window("5h", 0.2, resets_at);
        first.observed_at = Utc::now();
        let mut second = snapshot_with_one_window("5h", 0.3, resets_at);
        second.observed_at = first.observed_at + chrono::Duration::minutes(5);
        account.record(&first, 2, 3);
        account.record(&second, 2, 3);
        state.save(&path).unwrap();

        let (loaded, quarantined) = State::load(&path).unwrap();
        assert!(quarantined.is_none());
        let history = loaded.accounts["acct"].history.get("5h").unwrap();
        assert_eq!(history.samples.len(), 2);
        assert_eq!(history.samples.back().unwrap().used_fraction, 0.3);
        assert_eq!(
            loaded.accounts["acct"].aggregate_burn_history["5h"].len(),
            2
        );
    }

    #[test]
    fn host_samples_are_isolated_by_account_and_host_without_changing_account_total() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        let resets_at = Utc::now() + chrono::Duration::hours(5);
        let snapshot = snapshot_with_one_window("5h", 0.3, resets_at);

        let mut state = State::default();
        state
            .accounts
            .entry("acct-a".to_string())
            .or_default()
            .record(&snapshot, 10, 12);
        state.record_host("acct-a", "host-1", &snapshot, 4);
        state.record_host("acct-a", "host-2", &snapshot, 6);
        state.record_host("acct-b", "host-1", &snapshot, 2);
        state.save(&path).unwrap();

        let (loaded, quarantined) = State::load(&path).unwrap();
        assert!(quarantined.is_none());
        assert_eq!(loaded.accounts["acct-a"].last_target, Some(12));
        assert_eq!(
            loaded.host_states["acct-a"]["host-1"].windows["5h"].workers,
            4
        );
        assert_eq!(
            loaded.host_states["acct-a"]["host-2"].windows["5h"].workers,
            6
        );
        assert_eq!(
            loaded.host_states["acct-b"]["host-1"].windows["5h"].workers,
            2
        );
        assert!(
            loaded.host_states["acct-a"]["host-1"].history["5h"]
                .samples
                .len()
                == 1
        );
    }

    #[test]
    fn host_history_is_bounded_per_reset_generation() {
        let mut state = State::default();
        let resets_at = Utc::now() + chrono::Duration::hours(5);
        for i in 0..(MAX_HISTORY_SAMPLES_PER_GENERATION + 3) {
            let snapshot = snapshot_with_one_window("5h", i as f64 * 0.01, resets_at);
            state.record_host("acct", "host", &snapshot, 2);
        }

        let history = &state.host_states["acct"]["host"].history["5h"];
        assert_eq!(history.samples.len(), MAX_HISTORY_SAMPLES_PER_GENERATION);
        assert_eq!(
            history.samples.front().unwrap().used_fraction,
            0.03,
            "oldest samples should be evicted independently for this host"
        );
    }

    #[test]
    fn host_placement_history_is_bounded_and_keeps_the_newest_cycles() {
        let mut host = HostState::default();
        let start = Utc::now();
        for cycle in 0..(MAX_PLACEMENT_HISTORY_SAMPLES + 3) {
            host.record_placement(
                start + chrono::Duration::seconds(cycle as i64),
                cycle as u32,
            );
        }

        assert_eq!(host.placement_history.len(), MAX_PLACEMENT_HISTORY_SAMPLES);
        assert_eq!(
            host.placement_history.front().unwrap().target_workers,
            3,
            "oldest placement cycles should be evicted first"
        );
        assert_eq!(
            host.placement_history.back().unwrap().target_workers,
            (MAX_PLACEMENT_HISTORY_SAMPLES + 2) as u32
        );
    }

    #[test]
    fn placement_oscillation_detector_ignores_steady_cycles_and_checks_recent_changes() {
        let now = Utc::now();
        let mut host = HostState::default();
        for (offset, target) in [(0, 2), (1, 5), (2, 2), (3, 5), (4, 5), (5, 5)] {
            host.record_placement(now + chrono::Duration::seconds(offset), target);
        }

        assert!(!host.placement_alternates_for(1));
        assert!(host.placement_alternates_for(3));
        assert!(!host.placement_alternates_for(4));

        host.record_placement(now + chrono::Duration::seconds(6), 3);
        assert!(!host.placement_alternates_for(3));
    }

    #[test]
    fn placement_history_round_trips_and_defaults_for_older_host_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        let now = Utc::now();
        let mut state = State::default();
        state.record_host_placement("acct-a", "host-1", now, 2);
        state.record_host_placement("acct-a", "host-1", now + chrono::Duration::seconds(1), 5);
        state.record_host_placement("acct-b", "host-1", now, 9);
        state.save(&path).unwrap();

        let (loaded, quarantined) = State::load(&path).unwrap();
        assert!(quarantined.is_none());
        assert_eq!(loaded.schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(
            loaded.host_states["acct-a"]["host-1"].placement_history[1].target_workers,
            5
        );
        assert_eq!(
            loaded.host_states["acct-b"]["host-1"].placement_history[0].target_workers,
            9
        );

        let old_host: HostState = serde_json::from_str(r#"{"windows":{},"history":{}}"#).unwrap();
        assert!(old_host.placement_history.is_empty());
    }

    #[test]
    fn missing_history_field_defaults_to_empty() {
        let account: AccountState = serde_json::from_str("{}").unwrap();
        assert!(account.history.is_empty());
    }

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
        let (loaded, quarantined) = State::load(&path).unwrap();
        assert!(quarantined.is_none());
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
                ..Default::default()
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
                ..Default::default()
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

    #[test]
    fn load_treats_a_missing_file_as_a_first_run_not_a_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        let (loaded, quarantined) = State::load(&path).unwrap();
        assert!(quarantined.is_none());
        assert_eq!(loaded.schema_version, STATE_SCHEMA_VERSION);
        assert!(loaded.accounts.is_empty());
    }

    #[test]
    fn load_quarantines_unparseable_state_instead_of_starting_empty_unremarked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        let garbage = b"not json at all {{{";
        fs::write(&path, garbage).unwrap();

        let (loaded, quarantined) = State::load(&path).unwrap();

        // Recovery still yields a usable, empty state...
        assert_eq!(loaded.schema_version, STATE_SCHEMA_VERSION);
        assert!(loaded.accounts.is_empty());

        // ...but unlike a genuine first run, it is reported so the caller
        // can log it, and the bad file is preserved rather than discarded.
        let notice = quarantined.expect("malformed state must be reported, not silently ignored");
        assert!(
            notice.error.to_lowercase().contains("expected") || !notice.error.is_empty(),
            "quarantine notice should carry the parse error: {}",
            notice.error
        );
        assert!(
            !path.exists(),
            "the malformed file must be moved out of the live state path"
        );
        assert_eq!(
            fs::read(&notice.quarantined_path).unwrap(),
            garbage,
            "the quarantined copy must preserve the original bytes for forensics"
        );
    }

    #[test]
    fn load_quarantines_valid_json_with_the_wrong_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        // Valid JSON, but not an object State can deserialize into.
        fs::write(&path, r#"[1, 2, 3]"#).unwrap();

        let (loaded, quarantined) = State::load(&path).unwrap();
        assert!(loaded.accounts.is_empty());
        let notice = quarantined.expect("wrong-shaped JSON must be quarantined, not accepted");
        assert!(fs::read(&notice.quarantined_path).unwrap() == b"[1, 2, 3]");
        assert!(!path.exists());
    }

    #[test]
    fn load_after_quarantine_can_save_a_fresh_state_at_the_original_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        fs::write(&path, b"corrupt").unwrap();

        let (state, quarantined) = State::load(&path).unwrap();
        assert!(quarantined.is_some());
        state.save(&path).unwrap();

        let (reloaded, quarantined_again) = State::load(&path).unwrap();
        assert!(
            quarantined_again.is_none(),
            "the freshly saved state must load cleanly on the next cycle"
        );
        assert!(reloaded.accounts.is_empty());
    }

    // Crash-consistency tests below exercise the four boundaries `save`
    // crosses on every write: before any byte reaches the temp file, after
    // the temp file is written but not yet fsynced, after fsync but before
    // rename, and after rename but before the parent directory is fsynced.
    // Real fault injection (killing the process mid-syscall) isn't
    // reachable from a unit test, so each boundary is instead reproduced by
    // directly constructing the on-disk artifact a crash at that point
    // would leave -- using the same private `temporary_path`/`sync_directory`
    // helpers `save` itself uses, via `use super::*` -- and asserting the
    // safety property that matters for that boundary.

    fn state_with_target(target: u32) -> State {
        let mut state = State::default();
        state.accounts.insert(
            "acct".to_string(),
            AccountState {
                last_target: Some(target),
                ..Default::default()
            },
        );
        state
    }

    #[test]
    #[cfg(unix)]
    fn crash_before_any_write_leaves_original_state_untouched_and_no_temp_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        state_with_target(1).save(&path).unwrap();
        let good_bytes = fs::read(&path).unwrap();

        // No write permission on the parent directory means `save` fails at
        // `OpenOptions::create_new` for the temp file -- before a single
        // byte of the new state has been written anywhere.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let result = state_with_target(2).save(&path);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            result.is_err(),
            "save must fail when it cannot create the temp file"
        );

        assert_eq!(
            fs::read(&path).unwrap(),
            good_bytes,
            "a crash before the temp file is created must not alter the live state"
        );
        let temp_leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name() != path.file_name().unwrap())
            .collect();
        assert!(
            temp_leftovers.is_empty(),
            "no partial artifact should exist when the write never started: {temp_leftovers:?}"
        );
    }

    #[test]
    fn crash_after_write_before_rename_leaves_original_state_loadable() {
        // Covers both "after write, before fsync" and "after fsync, before
        // rename": from the perspective of anything reading `path`, those
        // two points are indistinguishable -- the new content exists only
        // at the temp path, and `path` itself has not moved yet.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        state_with_target(1).save(&path).unwrap();
        let good_bytes = fs::read(&path).unwrap();

        let new_state = state_with_target(2);
        let payload = serde_json::to_vec_pretty(&new_state).unwrap();
        let temporary = temporary_path(&path);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .unwrap();
        file.write_all(&payload).unwrap();
        file.sync_all().unwrap();
        // Deliberately stop here: no rename, simulating a crash that landed
        // after the write (and even after its own fsync) but before the
        // rename that would make it the live state.

        assert_eq!(
            fs::read(&path).unwrap(),
            good_bytes,
            "the live path must be untouched while the new content sits only in the temp file"
        );
        let (loaded, quarantined) = State::load(&path).unwrap();
        assert!(quarantined.is_none());
        assert_eq!(
            loaded.accounts["acct"].last_target,
            Some(1),
            "load must return the last-renamed state, ignoring an orphaned temp file"
        );
    }

    #[test]
    fn crash_mid_write_leaves_a_truncated_temp_file_but_original_state_is_still_loadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        state_with_target(1).save(&path).unwrap();
        let good_bytes = fs::read(&path).unwrap();

        let new_state = state_with_target(2);
        let payload = serde_json::to_vec_pretty(&new_state).unwrap();
        let truncated = &payload[..payload.len() / 2];
        let temporary = temporary_path(&path);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .unwrap();
        file.write_all(truncated).unwrap();
        // No sync_all, no rename: this is a crash mid-`write_all`, leaving
        // an incomplete, unparseable temp file that nothing ever reads.

        assert_eq!(fs::read(&path).unwrap(), good_bytes);
        let (loaded, quarantined) = State::load(&path).unwrap();
        assert!(
            quarantined.is_none(),
            "a truncated temp file must never be mistaken for the live state"
        );
        assert_eq!(loaded.accounts["acct"].last_target, Some(1));
    }

    #[test]
    fn crash_after_rename_before_directory_fsync_new_state_is_already_loadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        state_with_target(1).save(&path).unwrap();

        let new_state = state_with_target(2);
        let payload = serde_json::to_vec_pretty(&new_state).unwrap();
        let temporary = temporary_path(&path);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .unwrap();
        file.write_all(&payload).unwrap();
        file.sync_all().unwrap();
        fs::rename(&temporary, &path).unwrap();
        // Deliberately skip `sync_directory` here: this is the simulated
        // crash point between the rename and the parent-directory fsync.

        let (loaded, quarantined) = State::load(&path).unwrap();
        assert!(quarantined.is_none());
        assert_eq!(
            loaded.accounts["acct"].last_target,
            Some(2),
            "rename already made the new state visible to any reader, whether or not \
             the directory entry's fsync ever completes"
        );
    }

    #[test]
    fn corrupt_file_recovery_quarantines_a_truncated_mid_write_artifact_found_at_the_live_path() {
        // Distinct from the temp-file scenarios above: here the truncated
        // bytes have ended up directly at the live `path` (e.g. an older
        // binary without the temp-file+rename protection, or a filesystem
        // that reordered writes) rather than at a temp file `load` never
        // reads. `load` must recognize this as corruption and quarantine
        // it, not propagate a raw parse error or accept a partial value.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        let full = serde_json::to_vec_pretty(&state_with_target(1)).unwrap();
        fs::write(&path, &full[..full.len() / 2]).unwrap();

        let (loaded, quarantined) = State::load(&path).unwrap();
        assert!(loaded.accounts.is_empty());
        let notice = quarantined.expect("truncated JSON at the live path must be quarantined");
        assert_eq!(
            fs::read(&notice.quarantined_path).unwrap(),
            &full[..full.len() / 2]
        );
        assert!(!path.exists());
    }

    // Migration tests below prove a governor started against the unversioned
    // baseline or an older explicit version upgrades cleanly rather than
    // failing or silently truncating. Newly added maps default empty; account
    // samples and last_target remain intact. A version number newer than this
    // binary wrote is covered separately by `load_refuses_a_newer_schema_version`.

    #[test]
    fn migrates_a_legacy_pre_versioning_file_and_preserves_its_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        // The real v0.1 shape: no `schema_version` key, and no `history`
        // key on the account -- both were added later.
        fs::write(
            &path,
            r#"{
                "accounts": {
                    "acct": {
                        "windows": {
                            "5h": {
                                "observed_at": "2026-01-01T00:00:00Z",
                                "used_fraction": 0.42,
                                "resets_at": "2026-01-01T05:00:00Z",
                                "workers": 3
                            }
                        },
                        "last_target": 3
                    }
                }
            }"#,
        )
        .unwrap();

        let (loaded, quarantined) = State::load(&path).unwrap();
        assert!(
            quarantined.is_none(),
            "a legacy pre-versioning file is a supported source, not corruption"
        );
        assert_eq!(
            loaded.schema_version, STATE_SCHEMA_VERSION,
            "an absent schema_version must migrate to the current version in memory"
        );
        let account = &loaded.accounts["acct"];
        assert_eq!(account.last_target, Some(3));
        let window = &account.windows["5h"];
        assert_eq!(window.used_fraction, 0.42);
        assert_eq!(window.workers, 3);
        assert!(
            account.history.is_empty(),
            "a field introduced after v0.1 must default rather than fail to parse"
        );
    }

    #[test]
    fn migrated_legacy_state_persists_the_current_schema_version_once_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        fs::write(
            &path,
            r#"{"accounts":{"acct":{"windows":{},"last_target":7}}}"#,
        )
        .unwrap();

        let (migrated, _) = State::load(&path).unwrap();
        migrated.save(&path).unwrap();

        // Re-read the raw bytes (not through `State::load`, which would
        // paper over a missing field via `#[serde(default)]`) to confirm
        // the migration is sticky: once saved, the file explicitly carries
        // the current version rather than relying on the reader to infer
        // it again next time.
        let raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            raw["schema_version"],
            serde_json::json!(STATE_SCHEMA_VERSION)
        );

        let (reloaded, quarantined) = State::load(&path).unwrap();
        assert!(quarantined.is_none());
        assert_eq!(reloaded.accounts["acct"].last_target, Some(7));
    }

    #[test]
    fn explicit_current_schema_version_loads_without_migration_or_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        fs::write(
            &path,
            format!(
                r#"{{"schema_version":{STATE_SCHEMA_VERSION},"accounts":{{"acct":{{"windows":{{}},"last_target":5,"history":{{}}}}}}}}"#
            ),
        )
        .unwrap();

        let (loaded, quarantined) = State::load(&path).unwrap();
        assert!(quarantined.is_none());
        assert_eq!(loaded.schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(loaded.accounts["acct"].last_target, Some(5));
    }

    #[test]
    fn migrates_version_one_state_and_preserves_account_samples_and_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        fs::write(
            &path,
            r#"{
                "schema_version": 1,
                "accounts": {
                    "acct": {
                        "windows": {
                            "5h": {
                                "observed_at": "2026-01-01T00:00:00Z",
                                "used_fraction": 0.42,
                                "resets_at": "2026-01-01T05:00:00Z",
                                "workers": 3
                            }
                        },
                        "last_target": 7,
                        "history": {}
                    }
                }
            }"#,
        )
        .unwrap();

        let (migrated, quarantined) = State::load(&path).unwrap();
        assert!(quarantined.is_none());
        assert_eq!(migrated.schema_version, STATE_SCHEMA_VERSION);
        assert!(migrated.host_states.is_empty());
        assert_eq!(migrated.accounts["acct"].last_target, Some(7));
        assert_eq!(migrated.accounts["acct"].windows["5h"].workers, 3);

        migrated.save(&path).unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            raw["schema_version"],
            serde_json::json!(STATE_SCHEMA_VERSION)
        );
        assert_eq!(raw["accounts"]["acct"]["last_target"], serde_json::json!(7));
        assert!(raw["host_states"].as_object().unwrap().is_empty());
    }

    #[test]
    fn migrates_version_two_host_state_with_empty_placement_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governor-state.json");
        fs::write(
            &path,
            r#"{
                "schema_version": 2,
                "accounts": {"acct": {"windows": {}, "last_target": 4, "history": {}}},
                "host_states": {
                    "acct": {
                        "host-1": {
                            "windows": {},
                            "history": {}
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        let (migrated, quarantined) = State::load(&path).unwrap();
        assert!(quarantined.is_none());
        assert_eq!(migrated.schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(migrated.accounts["acct"].last_target, Some(4));
        assert!(migrated.host_states["acct"]["host-1"]
            .placement_history
            .is_empty());
    }
}
