//! NEEDLE-native worker-count reconciliation (plan.md §22.9).
//!
//! NEEDLE owns worker creation and shutdown. This adapter only counts the
//! tmux sessions in NEEDLE's `needle-<adapter>-*` namespace and delegates
//! the requested change to `needle run` or `needle stop`.

use super::{new_process_group_command, wait_with_timeout, Actuator, CHILD_TIMEOUT};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_TMUX_OUTPUT_BYTES: u64 = 1024 * 1024;

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
    let stdout = child.stdout.take().context("tmux stdout was not piped")?;
    let stderr = child.stderr.take().context("tmux stderr was not piped")?;
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
                        return Err(error).context("failed to read tmux output");
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                super::kill_child_tree(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                bail!("tmux list-sessions timed out after {timeout:?}");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                super::kill_child_tree(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                bail!("tmux output reader ended unexpectedly");
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
        let _ = sender.send((is_stdout, read_capped(&mut stream, MAX_TMUX_OUTPUT_BYTES)));
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
    use tempfile::TempDir;

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
        let needle = fake_program(
            &dir,
            "needle",
            &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
        );

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
        let needle = fake_program(
            &dir,
            "needle",
            &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
        );

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
}
