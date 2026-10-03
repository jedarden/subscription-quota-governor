//! NEEDLE-native worker-count reconciliation (plan.md §22.9).
//!
//! NEEDLE owns worker creation and shutdown. This adapter only counts the
//! tmux sessions in NEEDLE's `needle-<adapter>-*` namespace and delegates
//! the requested change to `needle run` or `needle stop`.

use super::{new_process_group_command, wait_with_timeout, Actuator, Observer, CHILD_TIMEOUT};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
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
}

impl Observer for NeedleStatusObserver {
    fn current_workers(&self) -> Result<u32> {
        count_needle_heartbeats(
            &self.heartbeat_dir,
            &self.agent,
            self.stale_after_seconds,
            Utc::now(),
        )
    }
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
fn count_needle_heartbeats(
    heartbeat_dir: &Path,
    agent: &str,
    stale_after_seconds: u64,
    now: DateTime<Utc>,
) -> Result<u32> {
    let stale_after_seconds = i64::try_from(stale_after_seconds)
        .context("needle_status stale_after_seconds exceeds the supported range")?;
    let prefix = format!("{agent}-");
    let entries = match fs::read_dir(heartbeat_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to read NEEDLE heartbeat directory {}",
                    heartbeat_dir.display()
                )
            });
        }
    };

    let mut workers = 0_u32;
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
            workers = workers
                .checked_add(1)
                .context("NEEDLE heartbeat count exceeds u32::MAX")?;
        }
    }

    Ok(workers)
}

pub(super) struct NeedleRunActuator {
    pub(super) repo: PathBuf,
    pub(super) adapter: String,
}

impl Actuator for NeedleRunActuator {
    fn actuate(&self, desired: u32) -> Result<()> {
        reconcile_needle_workers("needle", "tmux", &self.repo, &self.adapter, desired)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TmuxSession {
    name: String,
    attached: bool,
}

fn reconcile_needle_workers(
    needle_program: &str,
    tmux_program: &str,
    repo: &Path,
    adapter: &str,
    desired: u32,
) -> Result<()> {
    check_needle_adapter_with_program(needle_program, adapter)?;
    let sessions = list_sessions(tmux_program)?;
    let prefix = session_prefix(adapter);
    let mut matching: Vec<_> = sessions
        .iter()
        .filter(|session| session.name.starts_with(&prefix))
        .cloned()
        .collect();
    matching.sort_by(|left, right| left.name.cmp(&right.name));

    match desired.cmp(&(matching.len() as u32)) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Greater => {
            let delta = desired - matching.len() as u32;
            let args = needle_run_args(repo, adapter, delta);
            run_needle(needle_program, &args)
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
            count_needle_heartbeats(dir.path(), "claude-print", 60, now).unwrap(),
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
        };
        let observer = crate::fleet::observer_for(&config);

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
            count_needle_heartbeats(dir.path(), "codex", 60, now).unwrap(),
            1
        );
    }

    #[test]
    fn needle_status_treats_a_missing_heartbeat_directory_as_zero_workers() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("missing");

        assert_eq!(
            count_needle_heartbeats(&missing, "codex", 60, Utc::now()).unwrap(),
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

        let error = count_needle_heartbeats(dir.path(), "codex", 60, Utc::now()).unwrap_err();
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
            2,
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
            1,
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(log).unwrap(),
            "stop --identifier needle-codex-alpha\n"
        );
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
            2,
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
}
