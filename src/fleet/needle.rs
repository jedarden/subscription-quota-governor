//! NEEDLE-native worker-count reconciliation (plan.md §22.9).
//!
//! NEEDLE owns worker creation and shutdown. This adapter only counts the
//! tmux sessions in NEEDLE's `needle-<adapter>-*` namespace and delegates
//! the requested change to `needle run` or `needle stop`.

use super::{new_process_group_command, wait_with_timeout, Actuator, Observer, CHILD_TIMEOUT};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_CHILD_OUTPUT_BYTES: u64 = 1024 * 1024;

pub(super) struct NeedleStatusObserver {
    pub(super) agent: String,
    pub(super) heartbeat_dir: PathBuf,
    pub(super) stale_after_seconds: u64,
    pub(super) identifier_prefix: Option<String>,
    /// Plan §26.2: worker ids this pool may retire. They are counted in
    /// their own bucket and never form the §23 unowned floor.
    pub(super) takeover: Vec<String>,
}

impl NeedleStatusObserver {
    fn counts(&self) -> Result<HeartbeatCounts> {
        count_needle_heartbeats(
            &self.heartbeat_dir,
            &self.agent,
            self.identifier_prefix.as_deref(),
            &self.takeover,
            self.stale_after_seconds,
            Utc::now(),
        )
    }
}

impl Observer for NeedleStatusObserver {
    fn current_workers(&self) -> Result<u32> {
        Ok(self.counts()?.owned)
    }

    fn unowned_workers(&self) -> Result<u32> {
        Ok(self.counts()?.unowned)
    }
}

/// Fresh heartbeats for one adapter, split by the governor's identifier
/// prefix (plan §23.3) and by the §26.2 takeover list. Without a prefix
/// every worker is `owned`, which is the v1 behaviour; a takeover-listed
/// worker is pending replacement and never forms the unowned floor.
#[derive(Debug, Default, PartialEq, Eq)]
struct HeartbeatCounts {
    owned: u32,
    unowned: u32,
    takeover: u32,
}

#[derive(Deserialize)]
struct NeedleHeartbeat {
    qualified_id: String,
    worker_id: String,
    #[serde(rename = "last_heartbeat", alias = "timestamp")]
    last_heartbeat: DateTime<Utc>,
}

/// Counts fresh NEEDLE heartbeats whose qualified filename belongs to `agent`.
/// NEEDLE names these files `{agent}-{worker_id}.json`; checking the JSON's
/// `worker_id` as well as `qualified_id` distinguishes adapters whose names
/// share a prefix (for example, `claude-print` and `claude-print-opus`).
/// `takeover` lists worker ids (plan §26.2) counted in their own bucket,
/// excluded from both `owned` and the unowned floor.
fn count_needle_heartbeats(
    heartbeat_dir: &Path,
    agent: &str,
    identifier_prefix: Option<&str>,
    takeover: &[String],
    stale_after_seconds: u64,
    now: DateTime<Utc>,
) -> Result<HeartbeatCounts> {
    let stale_after_seconds = i64::try_from(stale_after_seconds)
        .context("needle_status stale_after_seconds exceeds the supported range")?;
    let prefix = format!("{agent}-");
    let entries = match fs::read_dir(heartbeat_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HeartbeatCounts::default())
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to read NEEDLE heartbeat directory {}",
                    heartbeat_dir.display()
                )
            });
        }
    };

    let mut counts = HeartbeatCounts::default();
    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "failed to read an entry in NEEDLE heartbeat directory {}",
                heartbeat_dir.display()
            )
        })?;
        if !entry
            .file_type()
            .with_context(|| {
                format!(
                    "failed to inspect NEEDLE heartbeat {}",
                    entry.path().display()
                )
            })?
            .is_file()
        {
            continue;
        }

        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(qualified_id) = file_name.strip_suffix(".json") else {
            continue;
        };
        if !qualified_id.starts_with(&prefix) || qualified_id.len() == prefix.len() {
            continue;
        }

        let mut file = File::open(&path)
            .with_context(|| format!("failed to open NEEDLE heartbeat {}", path.display()))?;
        let bytes = super::read_bounded(&mut file, super::MAX_OBSERVER_BYTES)
            .with_context(|| format!("NEEDLE heartbeat {}", path.display()))?;
        let heartbeat: NeedleHeartbeat = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid NEEDLE heartbeat {}", path.display()))?;
        if heartbeat.qualified_id != format!("{agent}-{}", heartbeat.worker_id)
            || heartbeat.qualified_id != qualified_id
        {
            continue;
        }
        let age = now.signed_duration_since(heartbeat.last_heartbeat);
        let age_seconds = age.num_seconds();
        let is_fresh = age_seconds < 0
            || age_seconds < stale_after_seconds
            || (age_seconds == stale_after_seconds && age.subsec_nanos() == 0);
        if is_fresh {
            let owned = identifier_prefix
                .map(|prefix| crate::ownership::matches_worker_prefix(prefix, &heartbeat.worker_id))
                .unwrap_or(true);
            let slot = if owned {
                &mut counts.owned
            } else if takeover.iter().any(|id| id == &heartbeat.worker_id) {
                &mut counts.takeover
            } else {
                &mut counts.unowned
            };
            *slot = slot
                .checked_add(1)
                .context("NEEDLE heartbeat count exceeds u32::MAX")?;
        }
    }

    Ok(counts)
}

pub(super) struct NeedleRunActuator {
    pub(super) repo: PathBuf,
    pub(super) adapter: String,
    pub(super) identifier_prefix: Option<String>,
    pub(super) seed_demand_state: Option<PathBuf>,
    pub(super) seed_heartbeats: Option<PathBuf>,
}

impl Actuator for NeedleRunActuator {
    fn actuate(&self, desired: u32) -> Result<()> {
        self.actuate_with_exclusions(desired, &BTreeSet::new())
    }

    fn actuate_with_exclusions(&self, desired: u32, excluded: &BTreeSet<PathBuf>) -> Result<()> {
        let seeds = match (&self.seed_demand_state, &self.seed_heartbeats) {
            (Some(demand_state), Some(heartbeats)) => Some(SeedSources {
                demand_state,
                heartbeats,
            }),
            _ => None,
        };
        reconcile_needle_workers(
            "needle",
            "tmux",
            &self.repo,
            &self.adapter,
            self.identifier_prefix.as_deref(),
            seeds.as_ref(),
            desired,
            excluded,
        )
    }
}

/// Plan §26.5 seed sources: the WP23 demand-state document naming ready
/// workspaces, and the NEEDLE heartbeat directory used to count running
/// workers per workspace. Configured as a pair or not at all; without them
/// the configured `repo` stays the permanent single seed.
struct SeedSources<'a> {
    demand_state: &'a Path,
    heartbeats: &'a Path,
}

/// A launchable ready job from the WP23 demand-state document.
#[derive(Deserialize)]
struct SeedReadyJob {
    workspace: PathBuf,
}

/// The demand-state document the WP23 adapters atomically publish every
/// cycle. `backlog_unavailable` is set instead of `ready` when the demand
/// query failed: unknown demand must never seed a launch (a failed query is
/// unknown, never zero). Unknown fields are ignored, so future publishers
/// stay readable.
#[derive(Deserialize)]
struct SeedDemandState {
    #[serde(default)]
    backlog_unavailable: Option<String>,
    #[serde(default)]
    ready: Vec<SeedReadyJob>,
}

/// The heartbeat fields the seed occupancy count relies on. NEEDLE refreshes
/// heartbeats while a worker runs, so freshness is the running test.
#[derive(Deserialize)]
struct SeedHeartbeat {
    workspace: PathBuf,
    #[serde(rename = "last_heartbeat", alias = "timestamp")]
    last_heartbeat: DateTime<Utc>,
}

/// Same freshness window the productive adapters apply when treating a
/// heartbeat as a running worker: `0 <= age < 60` seconds.
const SEED_STALE_AFTER_SECONDS: i64 = 60;

/// Reads the ready workspaces from a WP23 demand-state document. A missing,
/// failed or malformed query fails closed: seeding from no evidence would
/// either stack workers on the fallback repo or launch bare.
fn read_seed_demand(path: &Path) -> Result<Vec<PathBuf>> {
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read seed demand state {}", path.display()))?;
    let state: SeedDemandState = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid seed demand state {}", path.display()))?;
    if let Some(reason) = state.backlog_unavailable {
        bail!(
            "seed demand state {} is unavailable: {reason}",
            path.display()
        );
    }
    Ok(state.ready.into_iter().map(|job| job.workspace).collect())
}

pub(super) fn seedable_count(path: &Path, excluded: &BTreeSet<PathBuf>) -> Result<u32> {
    let ready = read_seed_demand(path)?;
    Ok(ready
        .iter()
        .filter(|path| !excluded.contains(*path))
        .count()
        .min(u32::MAX as usize) as u32)
}

/// Counts fresh NEEDLE heartbeats per workspace across every adapter, because
/// §26.5 occupancy is workers at a workspace, not this pool's claims. Like
/// the heartbeat observer, a malformed or unreadable heartbeat fails closed;
/// a missing directory counts as no running workers.
fn seed_occupancy(dir: &Path, now: DateTime<Utc>) -> Result<BTreeMap<PathBuf, u32>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to read NEEDLE heartbeat directory {}",
                    dir.display()
                )
            });
        }
    };

    let mut occupancy = BTreeMap::new();
    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "failed to read an entry in NEEDLE heartbeat directory {}",
                dir.display()
            )
        })?;
        if !entry
            .file_type()
            .with_context(|| {
                format!(
                    "failed to inspect NEEDLE heartbeat {}",
                    entry.path().display()
                )
            })?
            .is_file()
        {
            continue;
        }

        let path = entry.path();
        if path.extension().and_then(|name| name.to_str()) != Some("json") {
            continue;
        }
        let mut file = File::open(&path)
            .with_context(|| format!("failed to open NEEDLE heartbeat {}", path.display()))?;
        let bytes = super::read_bounded(&mut file, super::MAX_OBSERVER_BYTES)
            .with_context(|| format!("NEEDLE heartbeat {}", path.display()))?;
        let heartbeat: SeedHeartbeat = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid NEEDLE heartbeat {}", path.display()))?;
        let age = now
            .signed_duration_since(heartbeat.last_heartbeat)
            .num_seconds();
        if (0..SEED_STALE_AFTER_SECONDS).contains(&age) {
            *occupancy.entry(heartbeat.workspace).or_insert(0) += 1;
        }
    }

    Ok(occupancy)
}

/// Orders scale-up seeds (plan §26.5): repeatedly take the ready workspace
/// with the fewest running workers, ties resolving to the lexicographically
/// smallest path so the choice is deterministic across cycles. Every pick
/// consumes one ready entry of that workspace and raises its occupancy, so a
/// batch spreads across workspaces instead of stacking on the first route.
/// The result never outlives the ready frontier; a caller must not launch
/// without a seed.
fn seed_order(ready: &[PathBuf], running: &BTreeMap<PathBuf, u32>) -> Vec<PathBuf> {
    let mut remaining: BTreeMap<PathBuf, u32> = BTreeMap::new();
    for route in ready {
        *remaining.entry(route.clone()).or_insert(0) += 1;
    }
    let mut occupancy = running.clone();
    let mut seeds = Vec::with_capacity(ready.len());
    for _ in 0..ready.len() {
        let next = remaining
            .iter()
            .filter(|(_, left)| **left > 0)
            .map(|(route, _)| route.clone())
            .min_by_key(|route| (occupancy.get(route).copied().unwrap_or(0), route.clone()));
        let Some(route) = next else { break };
        *remaining.get_mut(&route).expect("tracked ready route") -= 1;
        *occupancy.entry(route.clone()).or_insert(0) += 1;
        seeds.push(route);
    }
    seeds
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TmuxSession {
    name: String,
    attached: bool,
}

#[allow(clippy::too_many_arguments)] // The adapter's explicit programs, seed policy and exclusion set are independently testable.
fn reconcile_needle_workers(
    needle_program: &str,
    tmux_program: &str,
    repo: &Path,
    adapter: &str,
    identifier_prefix: Option<&str>,
    seeds: Option<&SeedSources>,
    desired: u32,
    excluded: &BTreeSet<PathBuf>,
) -> Result<()> {
    check_needle_adapter_with_program(needle_program, adapter)?;
    let sessions = list_sessions(tmux_program)?;
    let adapter_prefix = session_prefix(adapter);
    let mut matching: Vec<_> = sessions
        .iter()
        .filter(|session| match identifier_prefix {
            Some(prefix) => session
                .name
                .strip_prefix(&adapter_prefix)
                .is_some_and(|id| crate::ownership::matches_worker_prefix(prefix, id)),
            None => session.name.starts_with(&adapter_prefix),
        })
        .cloned()
        .collect();
    matching.sort_by(|left, right| left.name.cmp(&right.name));

    match desired.cmp(&(matching.len() as u32)) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Greater => {
            let delta = desired - matching.len() as u32;
            if seeds.is_none() && excluded.contains(repo) {
                return Ok(());
            }
            // Plan §26.5: with seeds configured, every scale-up worker is
            // launched with the ready workspace that has the fewest running
            // workers, read from the WP23 demand state and the NEEDLE
            // heartbeat directory; NEEDLE's own roaming takes over after the
            // seed. Never bare: fewer ready seeds than the delta launches
            // fewer workers and the next cycle reconciles the rest. Without
            // seeds the configured repo stays the permanent single seed.
            if identifier_prefix.is_none() && seeds.is_none() {
                return run_needle(needle_program, &needle_run_args(repo, adapter, delta));
            }
            let launches: Vec<PathBuf> = match seeds {
                Some(sources) => {
                    let ready: Vec<_> = read_seed_demand(sources.demand_state)?
                        .into_iter()
                        .filter(|path| !excluded.contains(path))
                        .collect();
                    let occupancy = seed_occupancy(sources.heartbeats, Utc::now())?;
                    seed_order(&ready, &occupancy)
                        .into_iter()
                        .take(delta as usize)
                        .collect()
                }
                None => vec![repo.to_path_buf(); delta as usize],
            };
            // One launch per worker: each seed differs, and a prefixed pool
            // needs distinct `<prefix><n>` identifiers so the worker is
            // recognisably ours (plan §23.3).
            let mut taken: Vec<String> = sessions.iter().map(|s| s.name.clone()).collect();
            let mut next = 1_u32;
            for route in launches {
                let mut args = needle_run_args(&route, adapter, 1);
                if let Some(identifier_prefix) = identifier_prefix {
                    let (identifier, session) = loop {
                        let identifier = format!("{identifier_prefix}{next}");
                        let session = format!("{}{identifier}", session_prefix(adapter));
                        next += 1;
                        if !taken.contains(&session) {
                            break (identifier, session);
                        }
                    };
                    args.extend([OsString::from("--identifier"), OsString::from(identifier)]);
                    taken.push(session);
                }
                run_needle(needle_program, &args)?;
            }
            Ok(())
        }
        std::cmp::Ordering::Less => {
            let count = (matching.len() as u32 - desired) as usize;
            let selected = stop_candidates(&matching, &sessions, count)?;
            for session in selected {
                // NEEDLE's identifier selector is a substring match. Passing
                // the full session name is exact unless a longer NEEDLE
                // session contains it, which stop_candidates rejects.
                run_needle(
                    needle_program,
                    &[
                        OsString::from("stop"),
                        OsString::from("--identifier"),
                        OsString::from(&session.name),
                    ],
                )?;
            }
            Ok(())
        }
    }
}

pub(super) fn check_needle_adapter(adapter: &str) -> Result<()> {
    check_needle_adapter_with_program("needle", adapter)
}

fn check_needle_adapter_with_program(needle_program: &str, adapter: &str) -> Result<()> {
    let output = capture_with_timeout(needle_program, &["test-agent", adapter], CHILD_TIMEOUT)
        .with_context(|| format!("failed to check NEEDLE adapter {adapter}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        bail!(
            "NEEDLE adapter {adapter} check exited with {}{}",
            output.status,
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let status = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Status:").map(str::trim));
    if status != Some("READY") {
        bail!(
            "NEEDLE adapter {adapter} is not ready (test-agent status: {})",
            status.unwrap_or("missing")
        );
    }

    let probe = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Probe:").map(str::trim))
        .and_then(|probe| probe.strip_prefix("exit "))
        .and_then(|probe| probe.split_whitespace().next())
        .and_then(|code| code.parse::<i32>().ok());
    match probe {
        Some(0) => Ok(()),
        Some(code) => bail!("NEEDLE adapter {adapter} probe exited with {code}"),
        None => bail!("NEEDLE adapter {adapter} did not report a probe exit code"),
    }
}

fn session_prefix(adapter: &str) -> String {
    // NEEDLE sanitizes periods when it constructs `needle-<agent>-<id>`.
    format!("needle-{}-", adapter.replace('.', "_"))
}

fn needle_run_args(repo: &Path, adapter: &str, count: u32) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("run"),
        OsString::from("-w"),
        repo.as_os_str().to_owned(),
        OsString::from("-a"),
        OsString::from(adapter),
    ];
    if count > 1 {
        args.extend([OsString::from("-c"), OsString::from(count.to_string())]);
    }
    args
}

fn stop_candidates<'a>(
    matching: &'a [TmuxSession],
    all_sessions: &[TmuxSession],
    count: usize,
) -> Result<Vec<&'a TmuxSession>> {
    let detached: Vec<_> = matching
        .iter()
        .filter(|session| !session.attached)
        .collect();
    if detached.len() < count {
        bail!(
            "cannot stop {count} NEEDLE workers: only {} matching sessions are detached",
            detached.len()
        );
    }

    let selected = &detached[..count];
    for session in selected {
        let ambiguous = all_sessions.iter().any(|other| {
            other.name != session.name
                && other.name.starts_with("needle-")
                && other.name.contains(&session.name)
        });
        if ambiguous {
            bail!(
                "cannot stop NEEDLE session {} by identifier because another session contains its name",
                session.name
            );
        }
    }
    Ok(selected.to_vec())
}

fn list_sessions(tmux_program: &str) -> Result<Vec<TmuxSession>> {
    let output = capture_with_timeout(
        tmux_program,
        &[
            "list-sessions",
            "-F",
            "#{session_name}\t#{session_attached}",
        ],
        CHILD_TIMEOUT,
    )
    .with_context(|| format!("failed to list tmux sessions using {tmux_program}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("no server running")
            || lower.contains("failed to connect to server")
            || lower.contains("error connecting to")
        {
            return Ok(Vec::new());
        }
        bail!(
            "tmux list-sessions exited with {}: {}",
            output.status,
            stderr.trim()
        );
    }

    parse_sessions(&String::from_utf8_lossy(&output.stdout))
}

fn parse_sessions(text: &str) -> Result<Vec<TmuxSession>> {
    text.lines()
        .map(|line| {
            let (name, attached) = line
                .split_once('\t')
                .with_context(|| format!("malformed tmux session row {line:?}"))?;
            let attached = match attached {
                "0" => false,
                "1" => true,
                _ => bail!("invalid tmux attached flag {attached:?} for session {name:?}"),
            };
            if name.is_empty() {
                bail!("tmux returned a session with an empty name");
            }
            Ok(TmuxSession {
                name: name.to_string(),
                attached,
            })
        })
        .collect()
}

fn run_needle(program: &str, args: &[OsString]) -> Result<()> {
    let mut command = new_process_group_command(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let rendered: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to execute {program} {}", rendered.join(" ")))?;
    let status = wait_with_timeout(&mut child, CHILD_TIMEOUT)
        .with_context(|| format!("NEEDLE command {}", rendered.join(" ")))?;
    if !status.success() {
        bail!("NEEDLE command {} exited with {status}", rendered.join(" "));
    }
    Ok(())
}

fn capture_with_timeout(program: &str, args: &[&str], timeout: Duration) -> Result<Output> {
    let mut command = new_process_group_command(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to execute {program}"))?;
    let stdout = child
        .stdout
        .take()
        .context("command stdout was not piped")?;
    let stderr = child
        .stderr
        .take()
        .context("command stderr was not piped")?;
    let (sender, receiver) = mpsc::channel();
    let stdout_reader = spawn_capped_reader(stdout, true, sender.clone());
    let stderr_reader = spawn_capped_reader(stderr, false, sender);

    let deadline = Instant::now() + timeout;
    let mut captured = [None, None];
    for _ in 0..2 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok((is_stdout, result)) => {
                let index = if is_stdout { 0 } else { 1 };
                match result {
                    Ok(bytes) => captured[index] = Some(bytes),
                    Err(error) => {
                        super::kill_child_tree(&mut child);
                        let _ = stdout_reader.join();
                        let _ = stderr_reader.join();
                        return Err(error).context("failed to read command output");
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                super::kill_child_tree(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                bail!("command {program} timed out after {timeout:?}");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                super::kill_child_tree(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                bail!("command output reader ended unexpectedly");
            }
        }
    }
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
    let status = wait_with_timeout(&mut child, timeout)?;
    Ok(Output {
        status,
        stdout: captured[0].take().context("tmux stdout was not captured")?,
        stderr: captured[1].take().context("tmux stderr was not captured")?,
    })
}

fn spawn_capped_reader<R: Read + Send + 'static>(
    mut stream: R,
    is_stdout: bool,
    sender: mpsc::Sender<(bool, Result<Vec<u8>>)>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let _ = sender.send((is_stdout, read_capped(&mut stream, MAX_CHILD_OUTPUT_BYTES)));
    })
}

fn read_capped(reader: &mut impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .context("failed to read command output")?;
    if bytes.len() as u64 > limit {
        bail!("command output exceeded the {limit}-byte limit");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;
    use tempfile::TempDir;

    fn write_heartbeat(dir: &Path, agent: &str, worker_id: &str, timestamp: DateTime<Utc>) {
        let qualified_id = format!("{agent}-{worker_id}");
        fs::write(
            dir.join(format!("{qualified_id}.json")),
            serde_json::json!({
                "qualified_id": qualified_id,
                "worker_id": worker_id,
                "last_heartbeat": timestamp.to_rfc3339(),
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn needle_status_counts_fresh_heartbeats_for_only_the_configured_agent() {
        let dir = TempDir::new().unwrap();
        let now = Utc::now();
        write_heartbeat(dir.path(), "claude-print", "worker-1", now);
        write_heartbeat(dir.path(), "claude-print", "worker-2", now);
        write_heartbeat(dir.path(), "claude-print-opus", "worker-3", now);
        fs::write(dir.path().join("other-agent-worker-4.json"), "not json").unwrap();

        assert_eq!(
            count_needle_heartbeats(dir.path(), "claude-print", None, &[], 60, now)
                .unwrap()
                .owned,
            2
        );
    }

    #[test]
    fn needle_status_observer_factory_reports_the_current_worker_count() {
        let dir = TempDir::new().unwrap();
        write_heartbeat(dir.path(), "codex", "worker-1", Utc::now());
        let config = crate::config::WorkerObserverConfig::NeedleStatus {
            agent: "codex".to_string(),
            heartbeat_dir: dir.path().to_path_buf(),
            stale_after_seconds: 60,
            identifier_prefix: None,
        };
        let observer = crate::fleet::observer_for(&config, &[]);

        assert_eq!(observer.current_workers().unwrap(), 1);
    }

    #[test]
    fn needle_status_excludes_stale_heartbeats_and_includes_the_boundary() {
        let dir = TempDir::new().unwrap();
        let now = Utc::now();
        write_heartbeat(
            dir.path(),
            "codex",
            "boundary",
            now - ChronoDuration::seconds(60),
        );
        write_heartbeat(
            dir.path(),
            "codex",
            "stale",
            now - ChronoDuration::milliseconds(60_500),
        );

        assert_eq!(
            count_needle_heartbeats(dir.path(), "codex", None, &[], 60, now)
                .unwrap()
                .owned,
            1
        );
    }

    #[test]
    fn needle_status_treats_a_missing_heartbeat_directory_as_zero_workers() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("missing");

        assert_eq!(
            count_needle_heartbeats(&missing, "codex", None, &[], 60, Utc::now())
                .unwrap()
                .owned,
            0
        );
    }

    #[test]
    fn needle_status_fails_closed_on_a_malformed_matching_heartbeat() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("codex-worker-1.json"),
            serde_json::json!({ "qualified_id": "codex-worker-1", "worker_id": "worker-1" })
                .to_string(),
        )
        .unwrap();

        let error =
            count_needle_heartbeats(dir.path(), "codex", None, &[], 60, Utc::now()).unwrap_err();
        assert!(error.to_string().contains("invalid NEEDLE heartbeat"));
    }

    #[cfg(unix)]
    fn fake_program(dir: &TempDir, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.path().join(name);
        std::fs::write(&path, body).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    fn ready_test_agent_script(log_path: &Path) -> String {
        format!(
            "#!/bin/sh\nif [ \"$1\" = test-agent ]; then\n  printf 'Probe: exit 0 (0ms)\\nStatus:  READY\\n'\n  exit 0\nfi\nprintf '%s\\n' \"$*\" >> '{}'\n",
            log_path.display()
        )
    }

    #[test]
    fn run_command_targets_the_repo_adapter_and_only_the_needed_delta() {
        let one = needle_run_args(Path::new("/repos/one project"), "claude-print", 1);
        assert_eq!(
            one,
            ["run", "-w", "/repos/one project", "-a", "claude-print"].map(OsString::from)
        );

        let several = needle_run_args(Path::new("/repos/project"), "codex", 3);
        assert_eq!(
            several,
            ["run", "-w", "/repos/project", "-a", "codex", "-c", "3"].map(OsString::from)
        );
    }

    #[test]
    fn session_prefix_follows_needles_adapter_session_name_format() {
        assert_eq!(session_prefix("claude.print"), "needle-claude_print-");
    }

    #[test]
    fn parses_tmux_session_names_and_attachment_state() {
        assert_eq!(
            parse_sessions("needle-codex-alpha\t0\nother\t1\n").unwrap(),
            vec![
                TmuxSession {
                    name: "needle-codex-alpha".to_string(),
                    attached: false,
                },
                TmuxSession {
                    name: "other".to_string(),
                    attached: true,
                }
            ]
        );
    }

    #[test]
    fn scale_down_only_selects_detached_sessions_in_the_adapter_pattern() {
        let all = parse_sessions(
            "needle-codex-alpha\t0\nneedle-codex-bravo\t1\nneedle-claude-alpha\t0\n",
        )
        .unwrap();
        let matching = all
            .iter()
            .filter(|session| session.name.starts_with(&session_prefix("codex")))
            .cloned()
            .collect::<Vec<_>>();
        let selected = stop_candidates(&matching, &all, 1).unwrap();
        assert_eq!(selected[0].name, "needle-codex-alpha");
    }

    #[test]
    fn scale_down_fails_without_stopping_attached_sessions() {
        let all = parse_sessions("needle-codex-alpha\t1\n").unwrap();
        let err = stop_candidates(&all, &all, 1).unwrap_err().to_string();
        assert!(
            err.contains("only 0 matching sessions are detached"),
            "{err}"
        );
    }

    #[test]
    fn scale_down_fails_when_needles_substring_selector_is_ambiguous() {
        let all = parse_sessions("needle-codex-alpha\t0\nneedle-codex-alpha-extra\t0\n").unwrap();
        let err = stop_candidates(&all, &all, 1).unwrap_err().to_string();
        assert!(err.contains("another session contains its name"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn scale_up_invokes_needle_with_repo_adapter_and_delta() {
        let dir = TempDir::new().unwrap();
        let tmux = fake_program(
            &dir,
            "tmux",
            "#!/bin/sh\necho 'no server running on test socket' >&2\nexit 1\n",
        );
        let log = dir.path().join("needle.log");
        let needle = fake_program(&dir, "needle", &ready_test_agent_script(&log));

        reconcile_needle_workers(
            needle.to_str().unwrap(),
            tmux.to_str().unwrap(),
            Path::new("/repo/project"),
            "codex",
            None,
            None,
            2,
            &BTreeSet::new(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(log).unwrap(),
            "run -w /repo/project -a codex -c 2\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scale_down_invokes_needle_stop_only_for_a_detached_matching_session() {
        let dir = TempDir::new().unwrap();
        let tmux = fake_program(
            &dir,
            "tmux",
            "#!/bin/sh\nprintf 'needle-codex-alpha\\t0\\nneedle-codex-bravo\\t1\\nneedle-claude-alpha\\t0\\n'\n",
        );
        let log = dir.path().join("needle.log");
        let needle = fake_program(&dir, "needle", &ready_test_agent_script(&log));

        reconcile_needle_workers(
            needle.to_str().unwrap(),
            tmux.to_str().unwrap(),
            Path::new("/repo/project"),
            "codex",
            None,
            None,
            1,
            &BTreeSet::new(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(log).unwrap(),
            "stop --identifier needle-codex-alpha\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn prefixed_scale_down_never_stops_a_session_outside_the_prefix() {
        let dir = TempDir::new().unwrap();
        // Two governor sessions and one same-adapter session it did not launch.
        let tmux = fake_program(
            &dir,
            "tmux",
            "#!/bin/sh\nprintf 'needle-codex-sg-codex-1\\t0\\nneedle-codex-sg-codex-2\\t0\\nneedle-codex-roster-a\\t0\\n'\n",
        );
        let log = dir.path().join("needle.log");
        let needle = fake_program(&dir, "needle", &ready_test_agent_script(&log));

        reconcile_needle_workers(
            needle.to_str().unwrap(),
            tmux.to_str().unwrap(),
            Path::new("/repo/project"),
            "codex",
            Some("sg-codex-"),
            None,
            0,
            &BTreeSet::new(),
        )
        .unwrap();

        let logged = fs::read_to_string(&log).unwrap();
        assert!(logged.contains("needle-codex-sg-codex-1"), "{logged}");
        assert!(logged.contains("needle-codex-sg-codex-2"), "{logged}");
        assert!(!logged.contains("roster-a"), "{logged}");
    }

    #[cfg(unix)]
    #[test]
    fn needle_adapter_check_requires_ready_and_a_successful_probe() {
        let dir = TempDir::new().unwrap();
        let ready = fake_program(
            &dir,
            "needle-ready",
            "#!/bin/sh\nprintf 'Probe: exit 0 (0ms)\\nStatus:  READY\\n'\n",
        );
        check_needle_adapter_with_program(ready.to_str().unwrap(), "codex").unwrap();

        let failed_probe = fake_program(
            &dir,
            "needle-failed-probe",
            "#!/bin/sh\nprintf 'Probe: exit 127 (0ms)\\nStatus:  READY\\n'\n",
        );
        let error = check_needle_adapter_with_program(failed_probe.to_str().unwrap(), "codex")
            .unwrap_err()
            .to_string();
        assert!(error.contains("probe exited with 127"), "{error}");

        let warning = fake_program(
            &dir,
            "needle-warning",
            "#!/bin/sh\nprintf 'Probe: exit 0 (0ms)\\nStatus:  WARNING\\n'\n",
        );
        let error = check_needle_adapter_with_program(warning.to_str().unwrap(), "codex")
            .unwrap_err()
            .to_string();
        assert!(error.contains("status: WARNING"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn needle_adapter_failure_prevents_session_inspection_and_actuation() {
        let dir = TempDir::new().unwrap();
        let needle_log = dir.path().join("needle.log");
        let needle = fake_program(
            &dir,
            "needle",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nif [ \"$1\" = test-agent ]; then exit 2; fi\n",
                needle_log.display()
            ),
        );
        let tmux_log = dir.path().join("tmux.log");
        let tmux = fake_program(
            &dir,
            "tmux",
            &format!("#!/bin/sh\ntouch '{}'\n", tmux_log.display()),
        );

        let error = reconcile_needle_workers(
            needle.to_str().unwrap(),
            tmux.to_str().unwrap(),
            Path::new("/repo/project"),
            "codex",
            None,
            None,
            2,
            &BTreeSet::new(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("check exited"), "{error}");
        assert_eq!(
            std::fs::read_to_string(needle_log).unwrap(),
            "test-agent codex\n"
        );
        assert!(
            !tmux_log.exists(),
            "tmux must not be inspected after a failed check"
        );
    }

    #[test]
    fn identifier_prefix_splits_owned_from_roster_heartbeats() {
        let dir = TempDir::new().unwrap();
        let now = Utc::now();
        write_heartbeat(dir.path(), "codex", "sg-codex-1", now);
        write_heartbeat(dir.path(), "codex", "sg-codex-2", now);
        write_heartbeat(dir.path(), "codex", "luna-roster-01", now);
        write_heartbeat(dir.path(), "codex", "luna-roster-02", now);
        write_heartbeat(dir.path(), "codex", "luna-roster-03", now);

        // One listed worker leaves the floor; the other two unlisted roster
        // workers keep it (plan §26.2).
        let roster = vec!["luna-roster-01".to_string()];
        let counts =
            count_needle_heartbeats(dir.path(), "codex", Some("sg-codex-"), &roster, 60, now)
                .unwrap();
        assert_eq!(
            counts,
            HeartbeatCounts {
                owned: 2,
                unowned: 2,
                takeover: 1,
            }
        );

        let unscoped = count_needle_heartbeats(dir.path(), "codex", None, &[], 60, now).unwrap();
        assert_eq!(
            unscoped,
            HeartbeatCounts {
                owned: 5,
                unowned: 0,
                takeover: 0,
            }
        );
    }

    #[test]
    fn prefixed_scale_up_launches_one_uniquely_identified_worker_each() {
        let dir = TempDir::new().unwrap();
        let tmux = fake_program(
            &dir,
            "tmux",
            "#!/bin/sh\nprintf 'needle-codex-sg-codex-1\\t0\\nneedle-codex-roster-a\\t0\\n'\n",
        );
        let log = dir.path().join("needle.log");
        let needle = fake_program(&dir, "needle", &ready_test_agent_script(&log));

        reconcile_needle_workers(
            needle.to_str().unwrap(),
            tmux.to_str().unwrap(),
            Path::new("/repo/project"),
            "codex",
            Some("sg-codex-"),
            None,
            3,
            &BTreeSet::new(),
        )
        .unwrap();

        let logged = fs::read_to_string(&log).unwrap();
        assert!(logged.contains("--identifier sg-codex-2"), "{logged}");
        assert!(logged.contains("--identifier sg-codex-3"), "{logged}");
        assert!(!logged.contains("--identifier sg-codex-1\n"), "{logged}");
        assert_eq!(logged.matches("run -w").count(), 2, "{logged}");
    }

    fn write_demand(path: &Path, ready: &[&Path]) {
        let document = serde_json::json!({
            "observed_at": Utc::now().to_rfc3339(),
            "ready": ready.iter().map(|route| serde_json::json!({
                "id": "j1", "workspace": route.display().to_string(),
            })).collect::<Vec<_>>(),
        });
        fs::write(path, document.to_string()).unwrap();
    }

    fn write_seed_heartbeat(dir: &Path, name: &str, workspace: &Path, timestamp: DateTime<Utc>) {
        fs::write(
            dir.join(format!("{name}.json")),
            serde_json::json!({
                "qualified_id": name,
                "worker_id": "worker",
                "workspace": workspace.display().to_string(),
                "last_heartbeat": timestamp.to_rfc3339(),
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn excluded_workspace_is_never_used_as_a_launch_seed() {
        let dir = TempDir::new().unwrap();
        let contested = dir.path().join("contested");
        let allowed = dir.path().join("allowed");
        fs::create_dir_all(&contested).unwrap();
        fs::create_dir_all(&allowed).unwrap();
        fs::create_dir_all(dir.path().join("heartbeats")).unwrap();
        let demand_path = dir.path().join("demand.json");
        write_demand(&demand_path, &[&contested, &allowed]);
        let tmux = fake_program(
            &dir,
            "tmux",
            "#!/bin/sh\necho 'no server running on test socket' >&2\nexit 1\n",
        );
        let log = dir.path().join("needle.log");
        let needle = fake_program(&dir, "needle", &ready_test_agent_script(&log));
        let seeds = SeedSources {
            demand_state: &demand_path,
            heartbeats: &dir.path().join("heartbeats"),
        };
        reconcile_needle_workers(
            needle.to_str().unwrap(),
            tmux.to_str().unwrap(),
            &contested,
            "codex",
            Some("sg-codex-"),
            Some(&seeds),
            2,
            &BTreeSet::from([contested.clone()]),
        )
        .unwrap();
        let logged = fs::read_to_string(&log).unwrap();
        assert_eq!(logged.matches("run -w").count(), 1, "{logged}");
        assert!(logged.contains(&allowed.display().to_string()), "{logged}");
        assert!(
            !logged.contains(&contested.display().to_string()),
            "{logged}"
        );
    }

    #[test]
    fn seed_order_spreads_a_batch_across_ready_workspaces() {
        let first = PathBuf::from("/repos/a");
        let second = PathBuf::from("/repos/b");
        let expected = [first.clone(), second.clone(), first.clone()];
        let seeds = seed_order(&[first.clone(), first, second], &BTreeMap::new());
        assert_eq!(seeds, expected);
    }

    #[test]
    fn seed_order_prefers_the_least_occupied_ready_workspace() {
        let busy = PathBuf::from("/repos/a");
        let idle = PathBuf::from("/repos/b");
        let running = BTreeMap::from([(busy.clone(), 2), (idle.clone(), 0)]);
        let expected = [idle.clone(), busy.clone()];
        let seeds = seed_order(&[busy, idle], &running);
        assert_eq!(seeds, expected);
    }

    #[test]
    fn seed_order_ties_resolve_to_the_smallest_path_deterministically() {
        let first = PathBuf::from("/repos/a");
        let second = PathBuf::from("/repos/b");
        let running = BTreeMap::from([(second.clone(), 0), (first.clone(), 0)]);
        let seeds = seed_order(&[second.clone(), first.clone()], &running);
        assert_eq!(seeds, [first, second]);
    }

    #[test]
    fn seed_order_never_outlives_the_ready_frontier() {
        let route = PathBuf::from("/repos/a");
        let running = BTreeMap::from([(route.clone(), 9)]);
        assert!(seed_order(&[], &running).is_empty());
        assert_eq!(seed_order(std::slice::from_ref(&route), &running), [route]);
    }

    #[test]
    #[cfg(unix)]
    fn seed_scale_up_launches_each_worker_at_the_least_occupied_ready_workspace() {
        let dir = TempDir::new().unwrap();
        let first = dir.path().join("a-repo");
        let second = dir.path().join("b-repo");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::create_dir_all(dir.path().join("heartbeats")).unwrap();
        write_demand(&dir.path().join("demand.json"), &[&first, &second]);
        // One live worker at `first` pushes the first launch to `second`.
        write_seed_heartbeat(
            &dir.path().join("heartbeats"),
            "codex-luna-roster-1",
            &first,
            Utc::now(),
        );
        let tmux = fake_program(
            &dir,
            "tmux",
            "#!/bin/sh\necho 'no server running on test socket' >&2\nexit 1\n",
        );
        let log = dir.path().join("needle.log");
        let needle = fake_program(&dir, "needle", &ready_test_agent_script(&log));
        let seeds = SeedSources {
            demand_state: &dir.path().join("demand.json"),
            heartbeats: &dir.path().join("heartbeats"),
        };

        reconcile_needle_workers(
            needle.to_str().unwrap(),
            tmux.to_str().unwrap(),
            Path::new("/repo/project"),
            "codex",
            Some("sg-codex-"),
            Some(&seeds),
            2,
            &BTreeSet::new(),
        )
        .unwrap();

        let logged = fs::read_to_string(&log).unwrap();
        let first_line = logged.lines().next().unwrap();
        assert!(
            first_line.contains(&second.display().to_string()),
            "{logged}"
        );
        assert!(first_line.contains("--identifier sg-codex-1"), "{logged}");
        assert!(logged.contains(&first.display().to_string()), "{logged}");
        assert!(!logged.contains("/repo/project"), "{logged}");
        assert_eq!(logged.matches("run -w").count(), 2, "{logged}");
    }

    #[test]
    #[cfg(unix)]
    fn seed_scale_up_never_launches_more_than_the_ready_frontier() {
        let dir = TempDir::new().unwrap();
        let route = dir.path().join("a-repo");
        fs::create_dir_all(&route).unwrap();
        write_demand(&dir.path().join("demand.json"), &[&route]);
        let tmux = fake_program(
            &dir,
            "tmux",
            "#!/bin/sh\necho 'no server running on test socket' >&2\nexit 1\n",
        );
        let log = dir.path().join("needle.log");
        let needle = fake_program(&dir, "needle", &ready_test_agent_script(&log));
        let seeds = SeedSources {
            demand_state: &dir.path().join("demand.json"),
            heartbeats: &dir.path().join("heartbeats"),
        };

        reconcile_needle_workers(
            needle.to_str().unwrap(),
            tmux.to_str().unwrap(),
            Path::new("/repo/project"),
            "codex",
            None,
            Some(&seeds),
            3,
            &BTreeSet::new(),
        )
        .unwrap();

        let logged = fs::read_to_string(&log).unwrap();
        assert_eq!(logged.matches("run -w").count(), 1, "{logged}");
        assert!(logged.contains(&route.display().to_string()), "{logged}");
    }

    #[test]
    #[cfg(unix)]
    fn seed_scale_up_without_ready_work_launches_nothing() {
        let dir = TempDir::new().unwrap();
        write_demand(&dir.path().join("demand.json"), &[]);
        let tmux = fake_program(
            &dir,
            "tmux",
            "#!/bin/sh\necho 'no server running on test socket' >&2\nexit 1\n",
        );
        let log = dir.path().join("needle.log");
        let needle = fake_program(&dir, "needle", &ready_test_agent_script(&log));
        let seeds = SeedSources {
            demand_state: &dir.path().join("demand.json"),
            heartbeats: &dir.path().join("heartbeats"),
        };

        reconcile_needle_workers(
            needle.to_str().unwrap(),
            tmux.to_str().unwrap(),
            Path::new("/repo/project"),
            "codex",
            Some("sg-codex-"),
            Some(&seeds),
            2,
            &BTreeSet::new(),
        )
        .unwrap();

        assert!(!log.exists(), "a seedless launch would be bare");
    }

    #[test]
    #[cfg(unix)]
    fn seed_scale_up_fails_closed_without_a_demand_state_document() {
        let dir = TempDir::new().unwrap();
        let tmux = fake_program(
            &dir,
            "tmux",
            "#!/bin/sh\necho 'no server running on test socket' >&2\nexit 1\n",
        );
        let log = dir.path().join("needle.log");
        let needle = fake_program(&dir, "needle", &ready_test_agent_script(&log));
        let seeds = SeedSources {
            demand_state: &dir.path().join("missing.json"),
            heartbeats: &dir.path().join("heartbeats"),
        };

        let error = reconcile_needle_workers(
            needle.to_str().unwrap(),
            tmux.to_str().unwrap(),
            Path::new("/repo/project"),
            "codex",
            None,
            Some(&seeds),
            2,
            &BTreeSet::new(),
        )
        .unwrap_err()
        .to_string();

        assert!(
            error.contains("failed to read seed demand state"),
            "{error}"
        );
        assert!(!log.exists(), "unknown demand must not launch");
    }

    #[test]
    #[cfg(unix)]
    fn seed_scale_up_fails_closed_on_an_unavailable_demand_query() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("demand.json"),
            serde_json::json!({
                "observed_at": Utc::now().to_rfc3339(),
                "backlog_unavailable": "ValueError",
            })
            .to_string(),
        )
        .unwrap();
        let tmux = fake_program(
            &dir,
            "tmux",
            "#!/bin/sh\necho 'no server running on test socket' >&2\nexit 1\n",
        );
        let log = dir.path().join("needle.log");
        let needle = fake_program(&dir, "needle", &ready_test_agent_script(&log));
        let seeds = SeedSources {
            demand_state: &dir.path().join("demand.json"),
            heartbeats: &dir.path().join("heartbeats"),
        };

        let error = reconcile_needle_workers(
            needle.to_str().unwrap(),
            tmux.to_str().unwrap(),
            Path::new("/repo/project"),
            "codex",
            None,
            Some(&seeds),
            2,
            &BTreeSet::new(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("unavailable"), "{error}");
        assert!(!log.exists(), "unknown demand must not launch");
    }

    #[test]
    fn stale_heartbeats_do_not_count_as_running_workers() {
        let dir = TempDir::new().unwrap();
        let heartbeats = dir.path().join("heartbeats");
        fs::create_dir_all(&heartbeats).unwrap();
        let route = dir.path().join("a-repo");
        write_seed_heartbeat(
            &heartbeats,
            "codex-gone",
            &route,
            Utc::now() - ChronoDuration::seconds(61),
        );
        fs::write(heartbeats.join("notes.txt"), "not a heartbeat").unwrap();

        assert!(seed_occupancy(&heartbeats, Utc::now()).unwrap().is_empty());
    }

    #[test]
    fn seed_occupancy_counts_fresh_workers_across_adapters() {
        let dir = TempDir::new().unwrap();
        let heartbeats = dir.path().join("heartbeats");
        fs::create_dir_all(&heartbeats).unwrap();
        let first = dir.path().join("a-repo");
        let second = dir.path().join("b-repo");
        write_seed_heartbeat(&heartbeats, "codex-luna-1", &first, Utc::now());
        write_seed_heartbeat(&heartbeats, "claude-code-glm-2", &first, Utc::now());
        write_seed_heartbeat(&heartbeats, "codex-luna-3", &second, Utc::now());

        let occupancy = seed_occupancy(&heartbeats, Utc::now()).unwrap();
        assert_eq!(occupancy.get(&first), Some(&2));
        assert_eq!(occupancy.get(&second), Some(&1));
    }

    #[test]
    fn seed_occupancy_treats_a_missing_directory_as_no_running_workers() {
        let dir = TempDir::new().unwrap();
        assert!(seed_occupancy(&dir.path().join("missing"), Utc::now())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn seed_occupancy_fails_closed_on_a_malformed_heartbeat() {
        let dir = TempDir::new().unwrap();
        let heartbeats = dir.path().join("heartbeats");
        fs::create_dir_all(&heartbeats).unwrap();
        fs::write(heartbeats.join("codex-worker-1.json"), "not json").unwrap();

        let error = seed_occupancy(&heartbeats, Utc::now())
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid NEEDLE heartbeat"), "{error}");
    }
}
