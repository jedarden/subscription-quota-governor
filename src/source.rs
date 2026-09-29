use crate::config::SourceConfig;
use crate::model::{
    QuotaSnapshot, QuotaWindow, ResetCredit, ResetCreditsSnapshot, ResourceSnapshot,
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use fs2::FileExt;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;
use thiserror::Error;

const REFRESH_THRESHOLD_MILLIS: i64 = 300_000;
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Ceiling on how long a `command` source's child process -- and everything
/// it spawns, notably an SSH remote command per plan.md §22.10 -- may run
/// before it is killed. Mirrors `src/fleet.rs`'s `CHILD_TIMEOUT`: "The
/// command timeout in §7.4/§11.1 applies to the whole SSH round trip, not
/// just local execution." There is no separate, larger budget for the
/// network hop -- `ssh`'s own connection attempt, authentication, and the
/// remote command's runtime all come out of this one budget, same as local
/// execution.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
/// Maximum bytes read from a generic source's command stdout, file, or HTTP
/// body (plan.md §7.4: "Files and HTTP bodies have explicit maximum sizes in
/// v1."). A normalized quota snapshot is a small JSON document, so this is
/// generous headroom rather than a tight budget.
const MAX_GENERIC_SOURCE_BYTES: u64 = 1024 * 1024;
/// Maximum bytes for one Codex app-server stdout line -- one JSON-RPC frame
/// plus its terminating newline (plan.md §7.2: "Bound stdout frame size and
/// ignore unrelated frames without unbounded buffering"). A rate-limit
/// response is a small JSON document, so this is generous headroom rather
/// than a tight budget, matching [`MAX_GENERIC_SOURCE_BYTES`].
const MAX_CODEX_FRAME_BYTES: usize = 1024 * 1024;
/// Ceiling on how long a Codex session's push-notification cache may answer
/// a poll before an authoritative `account/rateLimits/read` is forced
/// (plan.md §7.2: "Consume rate-limit update notifications while
/// periodically reconciling with authoritative full reads"). Notifications
/// alone never reset this clock -- only a completed full read does -- so a
/// missed or dropped `account/rateLimits/updated` push can drift the
/// reported quota for at most this long. Three times the 300-second default
/// `poll_interval_seconds` (plan.md §8): long enough that a healthy
/// notification stream answers most polls from cache, short enough to bound
/// drift to a handful of poll cycles.
const CODEX_RECONCILE_INTERVAL: Duration = Duration::from_secs(900);

/// A 3xx response encountered where redirects are disabled. `Display`
/// includes only the response's own status code and the requested URL --
/// both operator-configured or protocol-level facts, never response
/// content -- so it is safe to chain into any source family's error.
#[derive(Debug, Error)]
#[error("refused a {status} redirect from {url} (redirects are disabled for this source)")]
pub struct RedirectRefused {
    status: u16,
    url: String,
}

/// A source read (command stdout, a snapshot file, or an HTTP body)
/// stopped early. `Display` never includes the bytes read -- only the
/// byte-count limit or a safe I/O error -- per plan.md §14 requirement 8.
#[derive(Debug, Error)]
pub enum ReadBoundedError {
    #[error("failed to read source output")]
    Io(#[source] std::io::Error),
    #[error("source output exceeded the {limit}-byte maximum")]
    TooLarge { limit: u64 },
}

/// Errors from the Claude Code / Anthropic OAuth native source (plan.md
/// §7.1). Every variant's `Display` is built only from a path, a field
/// name, a status code, or literal text -- never a token, credential, or
/// raw provider response body -- per plan.md §14 requirement 8: "Error
/// types carry safe classifications; raw response bodies remain local to
/// parsers and are discarded."
#[derive(Debug, Error)]
pub enum AnthropicSourceError {
    #[error("HTTP source timeout_seconds must be greater than zero")]
    InvalidTimeout,
    #[error("failed to open Claude Code credentials {path} for locking")]
    CredentialsUnopenable { path: PathBuf },
    #[error(
        "failed to lock Claude Code credentials {path} (a concurrent writer may be holding it)"
    )]
    Locked { path: PathBuf },
    #[error("failed to read or parse Claude Code credentials {path}")]
    CredentialsUnreadable { path: PathBuf },
    #[error("Claude Code credentials {path} are missing required field `{field}`")]
    MissingField { path: PathBuf, field: &'static str },
    #[error(
        "Claude Code credentials {path} were refreshed by another process during this poll; \
         skipping this cycle rather than overwriting a concurrent refresh"
    )]
    ConcurrentRefresh { path: PathBuf },
    #[error("Anthropic token refresh request failed")]
    RefreshRequestFailed(#[source] ureq::Error),
    #[error("Anthropic token refresh {0}")]
    RefreshRedirectRefused(#[source] RedirectRefused),
    #[error("Anthropic token refresh response is missing required field `{field}`")]
    RefreshResponseMissingField { field: &'static str },
    #[error("Anthropic token refresh response was not valid JSON")]
    RefreshResponseMalformed,
    #[error("failed to write refreshed Claude Code credentials {path}")]
    CredentialsWriteFailed { path: PathBuf },
    #[error("Anthropic usage request failed")]
    UsageRequestFailed(#[source] ureq::Error),
    #[error("Anthropic usage response {0}")]
    UsageRedirectRefused(#[source] RedirectRefused),
    #[error("Anthropic usage response was not valid JSON")]
    UsageResponseMalformed,
    #[error("Anthropic usage response did not contain a usable quota snapshot")]
    UsageEmpty,
}

/// Errors from the Codex app-server native source (plan.md §7.2). A
/// JSON-RPC error's numeric `code` (a small standard integer) is safe to
/// surface; its `message`/`data` fields are provider-controlled text and
/// are deliberately discarded rather than included in `Display`, per
/// plan.md §14 requirement 8.
#[derive(Debug, Error)]
pub enum CodexSourceError {
    #[error("failed to start {path} app-server")]
    SpawnFailed { path: PathBuf },
    #[error("Codex app-server has no stdin")]
    NoStdin,
    #[error("Codex app-server has no stdout")]
    NoStdout,
    #[error("failed to write to the Codex app-server")]
    WriteFailed(#[source] std::io::Error),
    #[error("timed out initializing the Codex app-server")]
    InitializeTimedOut,
    #[error("Codex app-server rejected initialization (code {code:?})")]
    InitializeRejected { code: Option<i64> },
    #[error("timed out reading Codex rate limits")]
    RateLimitsTimedOut,
    #[error("Codex app-server response channel closed unexpectedly")]
    ChannelClosed,
    #[error("Codex app-server rejected the rate-limit request (code {code:?})")]
    RateLimitsRejected { code: Option<i64> },
    #[error("Codex rate-limit response omitted its result")]
    ResultMissing,
    #[error("Codex rate-limit response contained no usable quota windows")]
    NoUsableWindows,
}

/// Errors from the generic `command`/`normalized_file`/`normalized_http`
/// sources (plan.md §7.4). This same transport and error set is shared by
/// the resource-source collectors (plan.md §22.5), so no variant name
/// describes a quota-specific concept. Every variant's `Display` is built
/// only from a path, a command name, a byte-count limit, a status code, or
/// literal text -- never the source's actual output, which is untrusted and
/// may carry arbitrary content -- per plan.md §14 requirement 8.
#[derive(Debug, Error)]
pub enum GenericSourceError {
    #[error("failed to read snapshot file {path}")]
    FileUnreadable { path: PathBuf },
    #[error("failed to read snapshot file {path}")]
    FileReadFailed {
        path: PathBuf,
        #[source]
        source: ReadBoundedError,
    },
    #[error("failed to start source command `{command}`")]
    CommandSpawnFailed { command: String },
    #[error("source command `{command}` stdout was not piped")]
    CommandStdoutMissing { command: String },
    #[error("failed to read source command `{command}` stdout")]
    CommandReadFailed {
        command: String,
        #[source]
        source: ReadBoundedError,
    },
    #[error("failed to wait for source command `{command}`")]
    CommandWaitFailed { command: String },
    #[error("source command `{command}` exited with a failure status")]
    CommandFailed { command: String },
    #[error("source command `{command}` timed out after {timeout:?} (plan.md §22.10: this bounds the whole SSH round trip too, not just local execution)")]
    CommandTimedOut { command: String, timeout: Duration },
    #[error("source command `{command}`'s output reader ended unexpectedly")]
    CommandReaderEnded { command: String },
    #[error("HTTP source timeout_seconds must be greater than zero")]
    InvalidTimeout,
    #[error("normalized HTTP source request failed")]
    HttpRequestFailed(#[source] ureq::Error),
    #[error("normalized HTTP source response {0}")]
    HttpRedirectRefused(#[source] RedirectRefused),
    #[error("failed to read normalized HTTP source response body")]
    HttpReadFailed(#[source] ReadBoundedError),
    #[error("source did not emit a valid normalized quota snapshot")]
    MalformedSnapshot,
    #[error("source did not emit a valid normalized resource snapshot")]
    MalformedResourceSnapshot,
    #[error(
        "resource sources only support command/normalized_file/normalized_http, \
         not this source type"
    )]
    UnsupportedResourceSourceType,
}

/// Collects one normalized quota snapshot for an account's configured
/// source.
///
/// This is the failure-isolation boundary described by plan.md §7.4: "a
/// source error affects only its account and cannot actuate its fleet." Every
/// branch either returns a validated `QuotaSnapshot` or an `Err` -- there is
/// no partial-success path -- so a caller that treats `Err` as "skip this
/// account's evaluation and actuation this cycle" (as `run_cycle` in
/// `main.rs` does, per-account and before any actuator call) can never end up
/// actuating a fleet from a failed poll.
pub fn collect(source: &SourceConfig) -> Result<QuotaSnapshot> {
    let snapshot = match source {
        SourceConfig::NormalizedFile { path } => collect_normalized_file(path)?,
        SourceConfig::NormalizedHttp {
            url,
            timeout_seconds,
        } => collect_normalized_http(url, *timeout_seconds)?,
        SourceConfig::Command { argv } => collect_command(argv)?,
        SourceConfig::AnthropicOauth {
            credentials_path,
            usage_url,
            token_url,
            timeout_seconds,
        } => collect_anthropic(credentials_path, usage_url, token_url, *timeout_seconds)?,
        SourceConfig::CodexAppServer {
            executable,
            timeout_seconds,
        } => collect_codex(executable, *timeout_seconds)?,
    };
    validate_snapshot(snapshot)
}

/// Reads a generic snapshot file's bytes under the shared §7.4 bound. Used
/// by both the quota (`normalized_file`) and resource (§22.5) collectors --
/// the two differ only in what they deserialize the bytes into.
fn read_generic_file_bytes(path: &Path) -> Result<Vec<u8>> {
    let mut file = File::open(path).map_err(|_| GenericSourceError::FileUnreadable {
        path: path.to_owned(),
    })?;
    read_bounded(&mut file, MAX_GENERIC_SOURCE_BYTES)
        .map_err(|source| GenericSourceError::FileReadFailed {
            path: path.to_owned(),
            source,
        })
        .map_err(Into::into)
}

fn collect_normalized_file(path: &Path) -> Result<QuotaSnapshot> {
    let bytes = read_generic_file_bytes(path)?;
    serde_json::from_slice(&bytes).map_err(|_| GenericSourceError::MalformedSnapshot.into())
}

/// Runs a generic source command (argv, never shell-interpreted) and reads
/// its stdout under the shared §7.4 bound, within `timeout`. Used by both
/// the quota (`command`) and resource (§22.5) collectors -- including a
/// cross-host `command` source whose `argv[0]` is `ssh` (§22.10): the child
/// is spawned as the leader of its own process group precisely so an SSH
/// invocation (and whatever it runs remotely) can be killed completely on
/// timeout, not just the local `ssh` process, and `timeout` bounds the
/// whole round trip -- connection, authentication, and the remote
/// command's runtime -- not just local startup.
///
/// Reads stdout on a background thread so the size bound (which must keep
/// consuming bytes to notice it was exceeded) and the wall-clock timeout
/// can be enforced at the same time: the main thread blocks on
/// `recv_timeout` instead of on the read itself. Mirrors
/// `src/fleet.rs`'s `command_observer_current_workers`.
fn read_generic_command_bytes(argv: &[String], timeout: Duration) -> Result<Vec<u8>> {
    let command = argv[0].clone();
    let mut child = new_process_group_command(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|_| GenericSourceError::CommandSpawnFailed {
            command: command.clone(),
        })?;
    let mut stdout =
        child
            .stdout
            .take()
            .ok_or_else(|| GenericSourceError::CommandStdoutMissing {
                command: command.clone(),
            })?;

    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let _ = sender.send(read_bounded(&mut stdout, MAX_GENERIC_SOURCE_BYTES));
    });

    let bytes = match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            kill_child_tree(&mut child);
            let _ = reader.join();
            return Err(GenericSourceError::CommandTimedOut { command, timeout }.into());
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = reader.join();
            return Err(GenericSourceError::CommandReaderEnded { command }.into());
        }
    };
    let _ = reader.join();

    if bytes.is_err() {
        // The child may still be trying to write past the bound; kill its
        // whole process group rather than risk a grandchild (e.g. an SSH
        // remote command) blocking forever on a full pipe buffer nobody is
        // draining.
        kill_child_tree(&mut child);
    }
    let bytes = bytes.map_err(|source| GenericSourceError::CommandReadFailed {
        command: command.clone(),
        source,
    })?;

    let status = child
        .wait()
        .map_err(|_| GenericSourceError::CommandWaitFailed {
            command: command.clone(),
        })?;
    if !status.success() {
        return Err(GenericSourceError::CommandFailed { command }.into());
    }
    Ok(bytes)
}

/// Spawns `program` as the leader of its own new process group, so
/// [`kill_child_tree`] can terminate it and everything it spawns -- an SSH
/// remote command in particular (plan.md §22.10) -- not just the immediate
/// child. Mirrors `src/fleet.rs`'s helper of the same name; kept as its own
/// copy here rather than a cross-module import so the account-source and
/// fleet-actuation code paths stay independent.
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

/// Kills `child`'s whole process group (or just `child` where process
/// groups aren't available) and reaps it. Best-effort, matching
/// `src/fleet.rs`'s helper of the same name: a child that has already
/// exited, or a signal that fails to reach every descendant, is not
/// treated as an error here -- the caller is already on a failure or
/// timeout path.
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

fn collect_command(argv: &[String]) -> Result<QuotaSnapshot> {
    let bytes = read_generic_command_bytes(argv, COMMAND_TIMEOUT)?;
    serde_json::from_slice(&bytes).map_err(|_| GenericSourceError::MalformedSnapshot.into())
}

/// Issues a generic, non-redirecting, finite-timeout HTTP GET and reads the
/// response body under the shared §7.4 bound. Used by both the quota
/// (`normalized_http`) and resource (§22.5) collectors.
fn read_generic_http_bytes(url: &str, timeout_seconds: u64) -> Result<Vec<u8>> {
    let agent = http_agent(timeout_seconds).map_err(|_| GenericSourceError::InvalidTimeout)?;
    let response = agent
        .get(url)
        .call()
        .map_err(GenericSourceError::HttpRequestFailed)?;
    let response = reject_redirect(response).map_err(GenericSourceError::HttpRedirectRefused)?;
    let mut reader = response.into_reader();
    read_bounded(&mut reader, MAX_GENERIC_SOURCE_BYTES)
        .map_err(GenericSourceError::HttpReadFailed)
        .map_err(Into::into)
}

fn collect_normalized_http(url: &str, timeout_seconds: u64) -> Result<QuotaSnapshot> {
    let bytes = read_generic_http_bytes(url, timeout_seconds)?;
    serde_json::from_slice(&bytes).map_err(|_| GenericSourceError::MalformedSnapshot.into())
}

/// Collects one normalized resource snapshot for a host's configured
/// resource source (plan.md §22.5). Reuses the same generic
/// command/normalized_file/normalized_http transport as [`collect`] --
/// size-bounded, non-shell, finite-timeout, per plan.md §7.4 -- differing
/// only in the target type and its own §22.4 validation rules. Resource
/// sources have no meaningful analog for the account-quota-specific
/// `anthropic_oauth` and `codex_app_server` source types, so those are
/// rejected rather than silently ignored.
pub fn collect_resource(source: &SourceConfig) -> Result<ResourceSnapshot> {
    let bytes = match source {
        SourceConfig::NormalizedFile { path } => read_generic_file_bytes(path)?,
        SourceConfig::NormalizedHttp {
            url,
            timeout_seconds,
        } => read_generic_http_bytes(url, *timeout_seconds)?,
        SourceConfig::Command { argv } => read_generic_command_bytes(argv, COMMAND_TIMEOUT)?,
        SourceConfig::AnthropicOauth { .. } | SourceConfig::CodexAppServer { .. } => {
            return Err(GenericSourceError::UnsupportedResourceSourceType.into());
        }
    };
    let snapshot: ResourceSnapshot = serde_json::from_slice(&bytes)
        .map_err(|_| GenericSourceError::MalformedResourceSnapshot)?;
    snapshot.validate()?;
    Ok(snapshot)
}

/// Reads at most `limit` bytes from `reader`, failing rather than silently
/// truncating if more data is available. Used to bound every generic
/// source's untrusted or unbounded input -- command stdout, a snapshot file,
/// or an HTTP response body -- per plan.md §7.4.
fn read_bounded(
    reader: &mut dyn Read,
    limit: u64,
) -> std::result::Result<Vec<u8>, ReadBoundedError> {
    let mut buffer = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut buffer)
        .map_err(ReadBoundedError::Io)?;
    if buffer.len() as u64 > limit {
        return Err(ReadBoundedError::TooLarge { limit });
    }
    Ok(buffer)
}

fn collect_anthropic(
    credentials_path: &Path,
    usage_url: &str,
    token_url: &str,
    timeout_seconds: u64,
) -> Result<QuotaSnapshot> {
    let access_token = obtain_access_token(credentials_path, token_url, timeout_seconds)?;

    let agent = http_agent(timeout_seconds).map_err(|_| AnthropicSourceError::InvalidTimeout)?;
    let response = agent
        .get(usage_url)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("User-Agent", "claude-code/2.1.114")
        .call()
        .map_err(AnthropicSourceError::UsageRequestFailed)?;
    let response = reject_redirect(response).map_err(AnthropicSourceError::UsageRedirectRefused)?;
    let payload: Value = response
        .into_json()
        .map_err(|_| AnthropicSourceError::UsageResponseMalformed)?;
    parse_anthropic_usage(&payload, Utc::now())
}

/// Reads the Claude Code OAuth credentials and returns a valid access token,
/// refreshing it first if it is near expiry.
///
/// Claude Code itself may be running concurrently and refresh the same file.
/// An OS advisory lock on the credentials file serializes against any other
/// locker (this process's other accounts sharing a path, another governor
/// instance, or a cooperating Claude Code process) for the whole read-decide
/// section. Because the lock alone cannot bind a non-cooperating writer, the
/// refresh result is also never written blindly: immediately before the
/// write, the file is re-read and the refresh token used for this refresh is
/// compared against what is currently on disk. A mismatch means some other
/// process already rotated it while the network round-trip was in flight, so
/// this refresh is discarded rather than clobbering a legitimate concurrent
/// rotation -- which would otherwise strand Claude Code on an
/// already-consumed refresh token.
fn obtain_access_token(
    credentials_path: &Path,
    token_url: &str,
    timeout_seconds: u64,
) -> Result<String> {
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(credentials_path)
        .map_err(|_| AnthropicSourceError::CredentialsUnopenable {
            path: credentials_path.to_owned(),
        })?;
    lock_exclusive_bounded(&lock_file, timeout_seconds).map_err(|_| {
        AnthropicSourceError::Locked {
            path: credentials_path.to_owned(),
        }
    })?;
    // Held until this function returns, so the whole read-decide(-refresh)
    // section below is one critical section under the lock.
    let _lock = lock_file;

    let credentials = read_json(credentials_path, "Claude Code credentials").map_err(|_| {
        AnthropicSourceError::CredentialsUnreadable {
            path: credentials_path.to_owned(),
        }
    })?;
    let oauth = credentials
        .get("claudeAiOauth")
        .and_then(Value::as_object)
        .ok_or_else(|| AnthropicSourceError::MissingField {
            path: credentials_path.to_owned(),
            field: "claudeAiOauth",
        })?;
    if let Some(token) = fresh_access_token(credentials_path, oauth)? {
        return Ok(token);
    }
    let refresh_token = required_refresh_token(credentials_path, oauth)?;
    let refreshed = refresh_anthropic(&refresh_token, token_url, timeout_seconds)?;
    apply_refreshed_credentials(credentials_path, &refresh_token, &refreshed)
}

fn lock_exclusive_bounded(file: &File, timeout_seconds: u64) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_seconds.max(1));
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    bail!("timed out waiting for an exclusive lock");
                }
                std::thread::sleep(LOCK_POLL_INTERVAL);
            }
            Err(error) => return Err(error).context("failed to acquire an exclusive lock"),
        }
    }
}

/// Returns `Some(accessToken)` if it is not within the refresh threshold of
/// expiry, `None` if a refresh is needed.
fn fresh_access_token(
    credentials_path: &Path,
    oauth: &serde_json::Map<String, Value>,
) -> Result<Option<String>> {
    let expires_at = oauth
        .get("expiresAt")
        .and_then(Value::as_i64)
        .ok_or_else(|| AnthropicSourceError::MissingField {
            path: credentials_path.to_owned(),
            field: "expiresAt",
        })?;
    if Utc::now().timestamp_millis() + REFRESH_THRESHOLD_MILLIS >= expires_at {
        return Ok(None);
    }
    let token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AnthropicSourceError::MissingField {
            path: credentials_path.to_owned(),
            field: "accessToken",
        })?
        .to_owned();
    Ok(Some(token))
}

fn required_refresh_token(
    credentials_path: &Path,
    oauth: &serde_json::Map<String, Value>,
) -> Result<String> {
    oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            AnthropicSourceError::MissingField {
                path: credentials_path.to_owned(),
                field: "refreshToken",
            }
            .into()
        })
}

/// Applies a completed Anthropic token refresh to the credentials file,
/// unless the refresh token it was derived from is no longer the one on
/// disk -- in which case another process already rotated it concurrently and
/// this result is discarded instead of overwritten. Caller must hold the
/// credentials file lock.
fn apply_refreshed_credentials(
    credentials_path: &Path,
    expected_refresh_token: &str,
    refreshed: &Value,
) -> Result<String> {
    let access = refreshed
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(AnthropicSourceError::RefreshResponseMissingField {
            field: "accessToken",
        })?
        .to_owned();
    let new_refresh = refreshed
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(AnthropicSourceError::RefreshResponseMissingField {
            field: "refreshToken",
        })?
        .to_owned();
    let new_expiry = refreshed
        .get("expiresAt")
        .and_then(Value::as_i64)
        .ok_or(AnthropicSourceError::RefreshResponseMissingField { field: "expiresAt" })?;

    let mut current = read_json(credentials_path, "Claude Code credentials").map_err(|_| {
        AnthropicSourceError::CredentialsUnreadable {
            path: credentials_path.to_owned(),
        }
    })?;
    let current_oauth = current
        .get_mut("claudeAiOauth")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| AnthropicSourceError::MissingField {
            path: credentials_path.to_owned(),
            field: "claudeAiOauth",
        })?;
    let current_refresh_token = current_oauth.get("refreshToken").and_then(Value::as_str);
    if current_refresh_token != Some(expected_refresh_token) {
        let current_access = current_oauth
            .get("accessToken")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let current_expiry = current_oauth.get("expiresAt").and_then(Value::as_i64);
        return match (current_access, current_expiry) {
            (Some(access), Some(expiry))
                if !access.is_empty()
                    && Utc::now().timestamp_millis() + REFRESH_THRESHOLD_MILLIS < expiry =>
            {
                Ok(access)
            }
            _ => Err(AnthropicSourceError::ConcurrentRefresh {
                path: credentials_path.to_owned(),
            }
            .into()),
        };
    }

    current_oauth.insert("accessToken".into(), Value::String(access.clone()));
    current_oauth.insert("refreshToken".into(), Value::String(new_refresh));
    current_oauth.insert("expiresAt".into(), Value::Number(new_expiry.into()));
    write_json_atomic(credentials_path, &current).map_err(|_| {
        AnthropicSourceError::CredentialsWriteFailed {
            path: credentials_path.to_owned(),
        }
    })?;
    Ok(access)
}

fn refresh_anthropic(refresh_token: &str, token_url: &str, timeout_seconds: u64) -> Result<Value> {
    let agent = http_agent(timeout_seconds).map_err(|_| AnthropicSourceError::InvalidTimeout)?;
    let response = agent
        .post(token_url)
        .set("Content-Type", "application/json")
        .set("User-Agent", "claude-code/2.1.114")
        .send_json(json!({
            "grantType": "refresh_token",
            "refreshToken": refresh_token
        }))
        .map_err(AnthropicSourceError::RefreshRequestFailed)?;
    reject_redirect(response)
        .map_err(AnthropicSourceError::RefreshRedirectRefused)?
        .into_json()
        .map_err(|_| AnthropicSourceError::RefreshResponseMalformed.into())
}

pub fn parse_anthropic_usage(payload: &Value, observed_at: DateTime<Utc>) -> Result<QuotaSnapshot> {
    let mut windows = BTreeMap::new();
    for id in ["five_hour", "seven_day", "weekly_scoped"] {
        if let Some(window) = payload.get(id) {
            if let Some(parsed) = parse_anthropic_window(id, window) {
                windows.insert(id.to_owned(), parsed);
            }
        }
    }
    if let Some(limits) = payload.get("limits").and_then(Value::as_array) {
        for limit in limits {
            let Some(id) = limit.get("kind").and_then(Value::as_str) else {
                continue;
            };
            if let Some(parsed) = parse_anthropic_limit(id, limit) {
                windows.insert(id.to_owned(), parsed);
            }
        }
    }
    if windows.is_empty() {
        bail!("Anthropic usage response contained no usable quota windows");
    }
    Ok(QuotaSnapshot {
        observed_at,
        fresh: true,
        windows: windows.into_values().collect(),
        reset_credits: None,
    })
}

fn parse_anthropic_window(id: &str, value: &Value) -> Option<QuotaWindow> {
    if value.is_null() || value.get("is_active").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let used = value.get("utilization")?.as_f64()? / 100.0;
    let resets_at = parse_timestamp(value.get("resets_at")?.as_str()?)?;
    Some(QuotaWindow {
        id: id.to_owned(),
        used_fraction: used,
        resets_at,
        duration_minutes: None,
        reached: used >= 1.0,
    })
}

fn parse_anthropic_limit(id: &str, value: &Value) -> Option<QuotaWindow> {
    if value.get("is_active").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let used = value.get("percent")?.as_f64()? / 100.0;
    let resets_at = parse_timestamp(value.get("resets_at")?.as_str()?)?;
    Some(QuotaWindow {
        id: id.to_owned(),
        used_fraction: used,
        resets_at,
        duration_minutes: None,
        reached: used >= 1.0,
    })
}

fn collect_codex(executable: &Path, timeout_seconds: u64) -> Result<QuotaSnapshot> {
    let (result, observed_at) = with_codex_session(executable, timeout_seconds)?;
    parse_codex_rate_limits(&result, observed_at)
}

/// Every live, handshaked Codex app-server session, keyed by executable
/// path and kept alive for the lifetime of this process (plan.md §7.2:
/// "Replace per-poll process startup with a supervised long-lived
/// session"). A single global map is safe here because `run_cycle` in
/// `main.rs` polls accounts one at a time on one thread; the `Mutex` exists
/// for soundness (interior mutability of a `static`) and test-thread
/// safety, not to guard real contention.
static CODEX_SESSIONS: OnceLock<Mutex<HashMap<PathBuf, CodexSession>>> = OnceLock::new();

fn codex_sessions() -> &'static Mutex<HashMap<PathBuf, CodexSession>> {
    CODEX_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Terminates and forgets every live supervised Codex session. Available for
/// a caller's orderly shutdown path, so a kept-alive app-server child does
/// not outlive the governor process, and for test isolation, since sessions
/// are cached process-wide rather than per-call.
pub fn shutdown_codex_sessions() {
    if let Some(sessions) = CODEX_SESSIONS.get() {
        let mut guard = sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.clear();
    }
}

/// Sends `account/rateLimits/read` on the executable's existing session if
/// one is live, transparently spawning and handshaking a fresh session
/// first if there is none yet or the existing one just failed. This is the
/// "supervised" half of §7.2: one broken session (the app-server exited,
/// the pipe broke, a request timed out) causes one respawn on the next
/// poll that actually needs to talk to it (a poll answered entirely from
/// the notification cache -- see [`codex_session_read_rate_limits`] --
/// never gets a chance to notice), not a permanent failure for every poll
/// after it.
///
/// Returns the rate-limit result alongside the wall-clock time it was
/// actually obtained, which may predate this call by up to
/// [`CODEX_RECONCILE_INTERVAL`] when the answer came from the cache.
fn with_codex_session(executable: &Path, timeout_seconds: u64) -> Result<(Value, DateTime<Utc>)> {
    let sessions = codex_sessions();
    let mut guard = sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(session) = guard.get_mut(executable) {
        match codex_session_read_rate_limits(session, timeout_seconds) {
            Ok(value) => return Ok(value),
            Err(_) => {
                // The cached session is no longer usable; drop it (its Drop
                // impl kills the child) and fall through to respawn.
                guard.remove(executable);
            }
        }
    }

    let mut session = spawn_and_handshake_codex(executable, timeout_seconds)?;
    let value = codex_session_read_rate_limits(&mut session, timeout_seconds)?;
    guard.insert(executable.to_owned(), session);
    Ok(value)
}

/// A live Codex app-server child past the `initialize`/`initialized`
/// handshake, ready for repeated `account/rateLimits/read` requests across
/// polls. `next_id` continues incrementing across calls so JSON-RPC ids stay
/// unique for the life of the session, not just within one poll.
///
/// `last_result` and `last_reconciled_at` implement plan.md §7.2's push
/// notification cache: `last_result` holds the most recent rate-limit
/// payload from either an `account/rateLimits/updated` notification or a
/// full `account/rateLimits/read`, and `last_reconciled_at` records when the
/// last *full read* completed -- notifications update `last_result` without
/// moving this clock, so [`CODEX_RECONCILE_INTERVAL`] bounds how long the
/// session may answer polls from the cache alone.
struct CodexSession {
    child: ChildGuard,
    stdin: ChildStdin,
    receiver: mpsc::Receiver<Value>,
    reader: Option<JoinHandle<()>>,
    next_id: i64,
    last_result: Option<Value>,
    last_reconciled_at: std::time::Instant,
    /// Wall-clock time `last_result` was actually obtained -- the moment a
    /// full read's response or a notification was received, never the
    /// moment a *cached* answer is later handed back to a poll. Reported as
    /// the returned snapshot's `observed_at` (plan.md §9.1's freshness gate
    /// compares `now - observed_at`), so a poll served from an
    /// increasingly-old cache is honestly reported as increasingly old,
    /// rather than stamped with the current time and appearing artificially
    /// fresh.
    last_observed_at: DateTime<Utc>,
}

impl Drop for CodexSession {
    fn drop(&mut self) {
        // Explicit kill before joining: this Drop runs before the compiler's
        // automatic field drops, so without this the reader thread could be
        // joined while the child (and thus its stdout) is still alive.
        self.child.terminate();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn spawn_and_handshake_codex(executable: &Path, timeout_seconds: u64) -> Result<CodexSession> {
    let child = Command::new(executable)
        .args(["app-server", "--listen", "stdio://"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| CodexSourceError::SpawnFailed {
            path: executable.to_owned(),
        })?;
    let mut child = ChildGuard(child);
    let stdin = child.0.stdin.take().ok_or(CodexSourceError::NoStdin)?;
    let stdout = child.0.stdout.take().ok_or(CodexSourceError::NoStdout)?;
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_bounded_codex_frame(&mut reader, MAX_CODEX_FRAME_BYTES) {
                Ok(Some(CodexFrame::Line(line))) => {
                    // A line that isn't valid JSON (or, further downstream
                    // in receive_codex_response/drain_pending_codex_notifications,
                    // an unrecognized notification or a mismatched id) is an
                    // unrelated frame -- ignore it and keep reading rather
                    // than treating it as fatal.
                    if let Ok(value) = serde_json::from_str::<Value>(&line) {
                        if sender.send(value).is_err() {
                            return;
                        }
                    }
                }
                Ok(Some(CodexFrame::Oversized)) => {
                    // Already discarded without retaining its bytes; treat
                    // it like any other unrelated frame and keep reading.
                }
                Ok(None) => return, // EOF: the app-server closed its stdout.
                Err(_) => return,
            }
        }
    });

    let mut session = CodexSession {
        child,
        stdin,
        receiver,
        reader: Some(reader),
        next_id: 1,
        last_result: None,
        last_reconciled_at: std::time::Instant::now(),
        // Never read before last_result is populated; the placeholder value
        // is never observed.
        last_observed_at: Utc::now(),
    };

    let initialize_id = session.next_id;
    session.next_id += 1;
    let initialize = json!({
        "id": initialize_id,
        "method": "initialize",
        "params": {"clientInfo": {"name": "subscription-governor", "version": "0.1.0"}}
    });
    writeln!(session.stdin, "{initialize}").map_err(CodexSourceError::WriteFailed)?;
    session
        .stdin
        .flush()
        .map_err(CodexSourceError::WriteFailed)?;
    let initialized = receive_codex_response(&mut session, initialize_id, timeout_seconds)
        .map_err(|_| CodexSourceError::InitializeTimedOut)?;
    if let Some(error) = initialized.get("error") {
        let code = error.get("code").and_then(Value::as_i64);
        // `session` drops here (its own Drop terminates the half-
        // initialized child and joins the reader).
        return Err(CodexSourceError::InitializeRejected { code }.into());
    }

    writeln!(
        session.stdin,
        "{}",
        json!({"method": "initialized", "params": {}})
    )
    .map_err(CodexSourceError::WriteFailed)?;
    session
        .stdin
        .flush()
        .map_err(CodexSourceError::WriteFailed)?;

    Ok(session)
}

/// Answers one poll's rate-limit request, preferring the session's
/// push-notification cache over the network (plan.md §7.2). Drains any
/// notifications that arrived since the last poll first, so a push that
/// landed while this session was otherwise idle is picked up promptly; only
/// falls through to a full `account/rateLimits/read` round trip when there
/// is no cached result yet or [`CODEX_RECONCILE_INTERVAL`] has elapsed since
/// the last one.
fn codex_session_read_rate_limits(
    session: &mut CodexSession,
    timeout_seconds: u64,
) -> Result<(Value, DateTime<Utc>)> {
    drain_pending_codex_notifications(session);
    if codex_cache_is_usable(
        &session.last_result,
        session.last_reconciled_at,
        std::time::Instant::now(),
    ) {
        let result = session
            .last_result
            .clone()
            .expect("codex_cache_is_usable only returns true when last_result is Some");
        return Ok((result, session.last_observed_at));
    }

    let id = session.next_id;
    session.next_id += 1;
    writeln!(
        session.stdin,
        "{}",
        json!({"id": id, "method": "account/rateLimits/read"})
    )
    .map_err(CodexSourceError::WriteFailed)?;
    session
        .stdin
        .flush()
        .map_err(CodexSourceError::WriteFailed)?;
    let response = receive_codex_response(session, id, timeout_seconds)
        .map_err(|_| CodexSourceError::RateLimitsTimedOut)?;
    if let Some(error) = response.get("error") {
        let code = error.get("code").and_then(Value::as_i64);
        return Err(CodexSourceError::RateLimitsRejected { code }.into());
    }
    let result = response
        .get("result")
        .cloned()
        .ok_or(CodexSourceError::ResultMissing)?;
    let observed_at = Utc::now();
    session.last_result = Some(result.clone());
    session.last_reconciled_at = std::time::Instant::now();
    session.last_observed_at = observed_at;
    Ok((result, observed_at))
}

/// Whether a session's cached rate-limit result is fresh enough to answer a
/// poll without a network round trip: there must be a cached result at all,
/// and [`CODEX_RECONCILE_INTERVAL`] must not yet have elapsed since the last
/// *full read* (`last_reconciled_at`) -- a notification updates the cached
/// value but deliberately never this clock, so an uninterrupted stream of
/// pushes can never indefinitely postpone reconciliation. Takes its inputs
/// by value rather than `&CodexSession` so it can be unit-tested without
/// constructing a full session (which needs a real child process).
fn codex_cache_is_usable(
    last_result: &Option<Value>,
    last_reconciled_at: std::time::Instant,
    now: std::time::Instant,
) -> bool {
    last_result.is_some()
        && now.saturating_duration_since(last_reconciled_at) < CODEX_RECONCILE_INTERVAL
}

/// Drains every message currently waiting on the session's channel without
/// blocking, capturing the latest `account/rateLimits/updated` push into the
/// cache and discarding anything else -- a stray late response or an
/// unrelated notification. Called before deciding whether a poll can be
/// answered from the cache, so a push that arrived while this session was
/// idle between polls (the reader thread keeps running regardless) is
/// applied promptly rather than sitting unseen until the next full read.
fn drain_pending_codex_notifications(session: &mut CodexSession) {
    while let Ok(value) = session.receiver.try_recv() {
        capture_codex_notification(session, &value);
    }
}

/// Updates the session's push-notification cache if `value` is a
/// well-formed `account/rateLimits/updated` notification. Per JSON-RPC,
/// only requests and responses carry an `id`; a notification does not, so
/// that alone distinguishes it from a stray/late response. Its `params` is
/// assumed to carry the same result-shaped payload as
/// `account/rateLimits/read`'s `result` (plan.md §7.2 documents both
/// methods on the same protocol; no other shape is specified), so it can
/// feed the same [`parse_codex_rate_limits`] parser unchanged. Anything else
/// -- an unrelated method, a malformed notification -- is silently ignored,
/// matching how any other unrelated frame is already handled: one bad
/// message must never fail an otherwise-healthy session.
fn capture_codex_notification(session: &mut CodexSession, value: &Value) {
    if value.get("id").is_some() {
        return;
    }
    if value.get("method").and_then(Value::as_str) != Some("account/rateLimits/updated") {
        return;
    }
    if let Some(params) = value.get("params") {
        session.last_result = Some(params.clone());
        session.last_observed_at = Utc::now();
    }
}

struct ChildGuard(Child);

impl ChildGuard {
    fn terminate(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// One outcome of [`read_bounded_codex_frame`]: either a complete line
/// (parsed as JSON or discarded as garbage by the caller) or a frame that
/// was discarded here because it exceeded the size bound.
enum CodexFrame {
    Line(String),
    Oversized,
}

/// Reads one newline-terminated line (the newline itself stripped) from
/// `reader`, up to `limit` bytes. A line that reaches `limit` bytes without
/// a newline is discarded -- its already-read bytes are dropped and the
/// remainder up to the next newline is drained in small fixed-size chunks
/// rather than accumulated -- and reported as [`CodexFrame::Oversized`]
/// instead of growing the buffer without bound (plan.md §7.2: "Bound stdout
/// frame size ... without unbounded buffering"). Returns `Ok(None)` at true
/// EOF (no bytes read at all).
fn read_bounded_codex_frame(
    reader: &mut impl BufRead,
    limit: usize,
) -> std::io::Result<Option<CodexFrame>> {
    let mut buffer = Vec::new();
    let read = reader
        .by_ref()
        .take(limit as u64)
        .read_until(b'\n', &mut buffer)?;
    if read == 0 {
        return Ok(None);
    }
    if buffer.last() == Some(&b'\n') {
        buffer.pop();
        return Ok(Some(CodexFrame::Line(
            String::from_utf8_lossy(&buffer).into_owned(),
        )));
    }
    // `limit` bytes were consumed without finding a newline: either a
    // too-long frame or the stream ended mid-line (e.g. the process died
    // while writing). Either way, resynchronize on the next newline so a
    // later, well-formed frame is not itself treated as a continuation of
    // this discarded one.
    drain_until_newline(reader)?;
    Ok(Some(CodexFrame::Oversized))
}

/// Discards bytes in small fixed-size chunks until a newline is consumed or
/// EOF is reached, without ever holding more than one chunk in memory.
fn drain_until_newline(reader: &mut impl BufRead) -> std::io::Result<()> {
    const CHUNK: u64 = 4096;
    let mut sink = Vec::new();
    loop {
        sink.clear();
        let read = reader.by_ref().take(CHUNK).read_until(b'\n', &mut sink)?;
        if read == 0 || sink.last() == Some(&b'\n') {
            return Ok(());
        }
    }
}

/// Waits for the JSON-RPC response whose `id` matches, meanwhile capturing
/// any `account/rateLimits/updated` notification interleaved ahead of it
/// into the session's cache (via [`capture_codex_notification`]) instead of
/// silently discarding it -- everything else non-matching (a stray response
/// to an abandoned request, an unrelated notification) is still discarded,
/// same as before this session gained a cache.
fn receive_codex_response(
    session: &mut CodexSession,
    id: i64,
    timeout_seconds: u64,
) -> Result<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_seconds);
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .context("response deadline elapsed")?;
        let value = session
            .receiver
            .recv_timeout(remaining)
            .context("app-server response channel closed")?;
        if value.get("id").and_then(Value::as_i64) == Some(id) {
            return Ok(value);
        }
        capture_codex_notification(session, &value);
    }
}

pub fn parse_codex_rate_limits(
    result: &Value,
    observed_at: DateTime<Utc>,
) -> Result<QuotaSnapshot> {
    let mut windows = Vec::new();
    if let Some(by_id) = result.get("rateLimitsByLimitId").and_then(Value::as_object) {
        for (limit_id, limit) in by_id {
            parse_codex_bucket(limit_id, limit, &mut windows);
        }
    } else if let Some(limit) = result.get("rateLimits") {
        let limit_id = limit
            .get("limitId")
            .and_then(Value::as_str)
            .unwrap_or("default");
        parse_codex_bucket(limit_id, limit, &mut windows);
    }
    if windows.is_empty() {
        return Err(CodexSourceError::NoUsableWindows.into());
    }
    Ok(QuotaSnapshot {
        observed_at,
        fresh: true,
        windows,
        reset_credits: parse_reset_credits(result),
    })
}

fn parse_reset_credits(result: &Value) -> Option<ResetCreditsSnapshot> {
    let summary = result.get("rateLimitResetCredits")?.as_object()?;
    let available_count = summary.get("availableCount")?.as_u64()?;
    let credits = match summary.get("credits") {
        None | Some(Value::Null) => None,
        Some(Value::Array(rows)) => Some(
            rows.iter()
                .filter_map(|row| {
                    let id = row.get("id")?.as_str()?.trim();
                    if id.is_empty() {
                        return None;
                    }
                    let timestamp = |field: &str| {
                        row.get(field)
                            .and_then(Value::as_i64)
                            .and_then(|value| Utc.timestamp_opt(value, 0).single())
                    };
                    Some(ResetCredit {
                        id: id.to_owned(),
                        reset_type: row
                            .get("resetType")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        status: row
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_owned(),
                        granted_at: timestamp("grantedAt"),
                        expires_at: timestamp("expiresAt"),
                        title: row.get("title").and_then(Value::as_str).map(str::to_owned),
                        description: row
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    })
                })
                .collect(),
        ),
        Some(_) => None,
    };
    Some(ResetCreditsSnapshot {
        available_count,
        credits,
    })
}

fn parse_codex_bucket(limit_id: &str, bucket: &Value, output: &mut Vec<QuotaWindow>) {
    for name in ["primary", "secondary"] {
        let Some(window) = bucket.get(name).filter(|value| !value.is_null()) else {
            continue;
        };
        let (Some(used_percent), Some(reset_epoch)) = (
            window.get("usedPercent").and_then(Value::as_f64),
            window.get("resetsAt").and_then(Value::as_i64),
        ) else {
            continue;
        };
        let Some(resets_at) = Utc.timestamp_opt(reset_epoch, 0).single() else {
            continue;
        };
        output.push(QuotaWindow {
            id: format!("{limit_id}.{name}"),
            used_fraction: used_percent / 100.0,
            resets_at,
            duration_minutes: window.get("windowDurationMins").and_then(Value::as_u64),
            reached: used_percent >= 100.0,
        });
    }
}

fn validate_snapshot(snapshot: QuotaSnapshot) -> Result<QuotaSnapshot> {
    if snapshot.windows.is_empty() {
        bail!("quota snapshot has no windows");
    }
    // Tracks ids already seen so far, to catch a duplicate on a later
    // window (plan.md §6.2: "id is stable within one source and account...
    // Duplicate IDs ... fail the snapshot"). schema/quota-snapshot.schema.json
    // documents this exact rule as a known gap it cannot mechanically
    // enforce (JSON Schema has no portable keyword for per-property
    // array-item uniqueness), so this check is this repo's sole enforcement
    // of it.
    let mut seen_ids = HashSet::with_capacity(snapshot.windows.len());
    for (index, window) in snapshot.windows.iter().enumerate() {
        // window.id is source-supplied content (arbitrary for a generic
        // command/file/http source) and is deliberately never echoed into
        // an error message -- per plan.md §14 requirement 8, an index is
        // used instead of the untrusted id text.
        if window.id.is_empty() {
            bail!("quota snapshot window {index} has an empty id");
        }
        if !window.used_fraction.is_finite() || !(0.0..=1.0).contains(&window.used_fraction) {
            bail!("quota snapshot window {index} used_fraction must be in [0, 1]");
        }
        if !seen_ids.insert(window.id.as_str()) {
            bail!("quota snapshot window {index} has a duplicate id (already used by an earlier window)");
        }
    }
    if let Some(reset_credits) = &snapshot.reset_credits {
        for credit in reset_credits.credits.as_deref().unwrap_or_default() {
            if credit.id.trim().is_empty() {
                bail!("quota snapshot has a reset credit with an empty id");
            }
        }
    }
    Ok(snapshot)
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

/// Builds an HTTP client with a mandatory, finite overall-request timeout
/// (connect through reading the full response body) -- plan.md §7.4: "HTTP
/// requests have mandatory finite timeouts." `timeout_seconds: 0` is refused
/// rather than silently producing a request with no effective deadline.
///
/// Redirects are always disabled (`.redirects(0)`). Plan.md §7.4 requires
/// this outright for credential-bearing native (Anthropic) requests, and
/// permits either "disabled" or "same-origin-only" for `normalized_http`;
/// this uses the same agent for both and picks the stricter option
/// uniformly rather than hand-rolling same-origin redirect following. With
/// `redirects(0)` a 3xx response comes back as `Ok` (per ureq) rather than
/// an error, so callers must still reject it explicitly via
/// [`reject_redirect`].
fn http_agent(timeout_seconds: u64) -> Result<ureq::Agent> {
    if timeout_seconds == 0 {
        bail!("HTTP source timeout_seconds must be greater than zero");
    }
    Ok(ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(timeout_seconds))
        .redirects(0)
        .build())
}

/// Turns a 3xx response from a `redirects(0)` agent into an explicit error
/// instead of letting it fall through to JSON parsing with a confusing
/// message.
fn reject_redirect(
    response: ureq::Response,
) -> std::result::Result<ureq::Response, RedirectRefused> {
    if (300..400).contains(&response.status()) {
        return Err(RedirectRefused {
            status: response.status(),
            url: response.get_url().to_owned(),
        });
    }
    Ok(response)
}

fn read_json(path: &Path, label: &str) -> Result<Value> {
    let bytes =
        fs::read(path).with_context(|| format!("failed to read {label} at {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {label}"))
}

/// Writes `value` to `path` via a same-directory temporary file and rename,
/// preserving the original file's owner and permission bits when it already
/// exists (falling back to `0o600` for a brand-new file), and fsyncing the
/// parent directory after the rename so the new directory entry survives a
/// crash. This is used for the Claude Code credentials file, which Claude
/// Code itself owns and may read or write concurrently -- refreshing it must
/// not silently narrow its permissions or leave the rename un-durable.
fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let parent = path.parent().context("credentials path has no parent")?;
    let temporary = parent.join(format!(
        ".subscription-governor-credentials-{}.tmp",
        std::process::id()
    ));

    #[cfg(unix)]
    let original_metadata = fs::metadata(path).ok();

    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            let mode = original_metadata
                .as_ref()
                .map(|metadata| metadata.mode() & 0o777)
                .unwrap_or(0o600);
            options.mode(mode);
        }
        let mut file = options
            .open(&temporary)
            .context("failed to create temporary credentials file")?;
        serde_json::to_writer_pretty(&mut file, value)
            .context("failed to serialize refreshed credentials")?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);

        #[cfg(unix)]
        if let Some(metadata) = &original_metadata {
            std::os::unix::fs::chown(&temporary, Some(metadata.uid()), Some(metadata.gid()))
                .context("failed to preserve credentials file owner")?;
        }

        fs::rename(&temporary, path).context("failed to install refreshed credentials")?;

        #[cfg(unix)]
        {
            let parent_dir = File::open(parent).with_context(|| {
                format!(
                    "failed to open {} to fsync the refreshed credentials rename",
                    parent.display()
                )
            })?;
            parent_dir.sync_all().with_context(|| {
                format!(
                    "failed to fsync {} after credentials rename",
                    parent.display()
                )
            })?;
        }

        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed() -> DateTime<Utc> {
        "2026-09-12T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn parses_all_codex_limit_ids() {
        let result = json!({
            "rateLimitsByLimitId": {
                "codex": {
                    "primary": {"usedPercent": 30, "windowDurationMins": 300, "resetsAt": 1800000000},
                    "secondary": {"usedPercent": 40, "windowDurationMins": 10080, "resetsAt": 1800100000}
                },
                "codex_other": {
                    "primary": {"usedPercent": 10, "windowDurationMins": 300, "resetsAt": 1800000000}
                }
            }
        });
        let snapshot = parse_codex_rate_limits(&result, observed()).unwrap();
        assert_eq!(snapshot.windows.len(), 3);
        assert!(snapshot
            .windows
            .iter()
            .any(|window| window.id == "codex.secondary"));
    }

    #[test]
    fn parses_codex_reset_credit_balance_and_details() {
        let result = json!({
            "rateLimits": {
                "limitId": "codex",
                "secondary": {"usedPercent": 40, "windowDurationMins": 10080, "resetsAt": 1800100000}
            },
            "rateLimitResetCredits": {
                "availableCount": 3,
                "credits": [{
                    "id": "credit-1",
                    "resetType": "weekly",
                    "status": "available",
                    "grantedAt": 1799000000,
                    "expiresAt": 1800200000,
                    "title": "Reset",
                    "description": "One rate-limit reset"
                }]
            }
        });
        let snapshot = parse_codex_rate_limits(&result, observed()).unwrap();
        let credits = snapshot.reset_credits.unwrap();
        assert_eq!(credits.available_count, 3);
        let detail = &credits.credits.unwrap()[0];
        assert_eq!(detail.id, "credit-1");
        assert_eq!(detail.status, "available");
        assert_eq!(detail.expires_at.unwrap().timestamp(), 1_800_200_000);
    }

    #[test]
    fn codex_cache_is_usable_requires_a_cached_result() {
        let now = std::time::Instant::now();
        assert!(
            !codex_cache_is_usable(&None, now, now),
            "no cached result yet must never be reported usable, regardless of timing"
        );
    }

    #[test]
    fn codex_cache_is_usable_accepts_a_recent_full_read() {
        let now = std::time::Instant::now();
        let one_second_ago = now - Duration::from_secs(1);
        assert!(codex_cache_is_usable(
            &Some(json!({"rateLimitsByLimitId": {}})),
            one_second_ago,
            now
        ));
    }

    #[test]
    fn codex_cache_is_usable_expires_once_the_reconcile_interval_elapses() {
        let now = std::time::Instant::now();
        let just_past_the_interval = now - CODEX_RECONCILE_INTERVAL - Duration::from_secs(1);
        assert!(
            !codex_cache_is_usable(
                &Some(json!({"rateLimitsByLimitId": {}})),
                just_past_the_interval,
                now
            ),
            "a cache older than CODEX_RECONCILE_INTERVAL must force a fresh full read, so a \
             notification stream alone can never indefinitely postpone reconciliation"
        );
    }

    #[test]
    fn parses_legacy_and_generic_anthropic_windows() {
        let payload = json!({
            "five_hour": {"utilization": 20, "resets_at": "2026-09-12T14:00:00Z"},
            "seven_day": null,
            "limits": [{"kind": "weekly_scoped", "percent": 50, "resets_at": "2026-09-15T00:00:00Z"}]
        });
        let snapshot = parse_anthropic_usage(&payload, observed()).unwrap();
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[0].used_fraction, 0.2);
    }

    fn write_credentials(path: &Path, access: &str, refresh: &str, expires_at_millis: i64) {
        let document = json!({
            "claudeAiOauth": {
                "accessToken": access,
                "refreshToken": refresh,
                "expiresAt": expires_at_millis,
                "unrelatedField": "preserved",
            }
        });
        fs::write(path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    }

    #[test]
    fn obtain_access_token_returns_cached_token_without_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let far_future = Utc::now().timestamp_millis() + 3_600_000;
        write_credentials(&path, "still-valid", "unused-refresh", far_future);

        // token_url is intentionally unreachable: a fresh token must never
        // trigger a network refresh.
        let token = obtain_access_token(&path, "http://127.0.0.1:0/unreachable", 5).unwrap();
        assert_eq!(token, "still-valid");
    }

    #[test]
    fn obtain_access_token_times_out_when_another_process_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let far_future = Utc::now().timestamp_millis() + 3_600_000;
        write_credentials(&path, "still-valid", "unused-refresh", far_future);

        // Simulate a concurrently-running Claude Code process mid-write by
        // holding the OS advisory lock from a separate file descriptor.
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        holder.lock_exclusive().unwrap();

        let started = std::time::Instant::now();
        let result = obtain_access_token(&path, "http://127.0.0.1:0/unreachable", 1);
        assert!(result.is_err(), "expected a lock-contention error");
        assert!(started.elapsed() < Duration::from_secs(3));

        holder.unlock().unwrap();
    }

    #[test]
    fn apply_refreshed_credentials_writes_when_refresh_token_still_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        write_credentials(&path, "old-access", "refresh-a", 1_000);

        let refreshed = json!({
            "accessToken": "new-access",
            "refreshToken": "refresh-b",
            "expiresAt": Utc::now().timestamp_millis() + 3_600_000,
        });
        let token = apply_refreshed_credentials(&path, "refresh-a", &refreshed).unwrap();
        assert_eq!(token, "new-access");

        let on_disk = read_json(&path, "test credentials").unwrap();
        let oauth = on_disk.get("claudeAiOauth").unwrap();
        assert_eq!(
            oauth.get("accessToken").unwrap().as_str(),
            Some("new-access")
        );
        assert_eq!(
            oauth.get("refreshToken").unwrap().as_str(),
            Some("refresh-b")
        );
        // Unknown fields survive the refresh write.
        assert_eq!(
            oauth.get("unrelatedField").unwrap().as_str(),
            Some("preserved")
        );
    }

    #[cfg(unix)]
    #[test]
    fn refresh_write_preserves_original_file_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        write_credentials(&path, "old-access", "refresh-a", 1_000);
        // Claude Code (or an operator) may have set a mode other than our
        // own default; the refresh write must not narrow or widen it.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        let refreshed = json!({
            "accessToken": "new-access",
            "refreshToken": "refresh-b",
            "expiresAt": Utc::now().timestamp_millis() + 3_600_000,
        });
        apply_refreshed_credentials(&path, "refresh-a", &refreshed).unwrap();

        let mode = fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[cfg(unix)]
    #[test]
    fn refresh_write_preserves_original_owner() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        write_credentials(&path, "old-access", "refresh-a", 1_000);
        let original = fs::metadata(&path).unwrap();

        let refreshed = json!({
            "accessToken": "new-access",
            "refreshToken": "refresh-b",
            "expiresAt": Utc::now().timestamp_millis() + 3_600_000,
        });
        apply_refreshed_credentials(&path, "refresh-a", &refreshed).unwrap();

        let after = fs::metadata(&path).unwrap();
        assert_eq!(after.uid(), original.uid());
        assert_eq!(after.gid(), original.gid());
    }

    #[cfg(unix)]
    #[test]
    fn refresh_write_fsyncs_parent_directory_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        write_credentials(&path, "old-access", "refresh-a", 1_000);

        let refreshed = json!({
            "accessToken": "new-access",
            "refreshToken": "refresh-b",
            "expiresAt": Utc::now().timestamp_millis() + 3_600_000,
        });
        // Durability fsync of the parent directory happens as part of the
        // write path; a successful result proves it did not error even
        // though a directory fd (not a regular file) is being synced.
        apply_refreshed_credentials(&path, "refresh-a", &refreshed).unwrap();
    }

    #[test]
    fn apply_refreshed_credentials_discards_stale_refresh_without_overwriting_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        // Simulates Claude Code having already refreshed concurrently: the
        // refresh token on disk ("refresh-c") no longer matches the one our
        // (now-stale) in-flight refresh was derived from ("refresh-a").
        let claude_code_expiry = Utc::now().timestamp_millis() + 3_600_000;
        write_credentials(&path, "claude-code-access", "refresh-c", claude_code_expiry);

        let our_refreshed = json!({
            "accessToken": "our-access",
            "refreshToken": "refresh-b",
            "expiresAt": Utc::now().timestamp_millis() + 7_200_000,
        });
        let token = apply_refreshed_credentials(&path, "refresh-a", &our_refreshed).unwrap();
        // Must defer to Claude Code's fresher, concurrently-written token
        // rather than clobbering it with our own.
        assert_eq!(token, "claude-code-access");

        let on_disk = read_json(&path, "test credentials").unwrap();
        let oauth = on_disk.get("claudeAiOauth").unwrap();
        assert_eq!(
            oauth.get("refreshToken").unwrap().as_str(),
            Some("refresh-c"),
            "a concurrent rotation must never be overwritten"
        );
    }

    #[test]
    fn apply_refreshed_credentials_errors_without_writing_when_concurrent_state_is_also_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        write_credentials(&path, "stale-access", "refresh-c", 1_000);
        let before = fs::read(&path).unwrap();

        let our_refreshed = json!({
            "accessToken": "our-access",
            "refreshToken": "refresh-b",
            "expiresAt": Utc::now().timestamp_millis() + 7_200_000,
        });
        let result = apply_refreshed_credentials(&path, "refresh-a", &our_refreshed);
        assert!(result.is_err());

        let after = fs::read(&path).unwrap();
        assert_eq!(before, after, "file must be left untouched on discard");
    }

    /// Binds an ephemeral loopback port and immediately releases it, so a
    /// request against it fails with connection-refused deterministically
    /// without a mock server.
    fn unreachable_url(path: &str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}{path}")
    }

    #[test]
    fn collect_anthropic_fails_closed_when_credentials_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let source = SourceConfig::AnthropicOauth {
            credentials_path: dir.path().join("missing.json"),
            usage_url: unreachable_url("/usage"),
            token_url: unreachable_url("/token"),
            timeout_seconds: 1,
        };
        assert!(
            collect(&source).is_err(),
            "a missing credentials file must surface as Err, never a fabricated snapshot"
        );
    }

    #[test]
    fn collect_anthropic_fails_closed_when_credentials_are_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        fs::write(&path, b"{\"not\": \"oauth\"}").unwrap();
        let source = SourceConfig::AnthropicOauth {
            credentials_path: path,
            usage_url: unreachable_url("/usage"),
            token_url: unreachable_url("/token"),
            timeout_seconds: 1,
        };
        assert!(collect(&source).is_err());
    }

    #[test]
    fn collect_anthropic_fails_closed_and_leaves_credentials_untouched_when_refresh_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        // Already past the refresh threshold, so a refresh is required.
        write_credentials(&path, "expired-access", "refresh-a", 1_000);
        let before = fs::read(&path).unwrap();

        let source = SourceConfig::AnthropicOauth {
            credentials_path: path.clone(),
            usage_url: unreachable_url("/usage"),
            token_url: unreachable_url("/token"),
            timeout_seconds: 1,
        };
        assert!(collect(&source).is_err());

        let after = fs::read(&path).unwrap();
        assert_eq!(
            before, after,
            "a failed token refresh must not modify the credentials file"
        );
    }

    #[test]
    fn collect_anthropic_fails_closed_when_usage_endpoint_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let far_future = Utc::now().timestamp_millis() + 3_600_000;
        write_credentials(&path, "still-valid", "unused-refresh", far_future);

        let source = SourceConfig::AnthropicOauth {
            credentials_path: path,
            usage_url: unreachable_url("/usage"),
            token_url: unreachable_url("/token"),
            timeout_seconds: 1,
        };
        // The token is fresh (no refresh needed), isolating the failure to
        // the usage request itself -- this must still surface as Err, not an
        // empty or partial snapshot that could be mistaken for a real one.
        assert!(collect(&source).is_err());
    }

    /// Serves one HTTP/1.1 response over an ephemeral loopback port, with a
    /// caller-chosen status line and body, optionally delayed before being
    /// written. Used by the Anthropic mock-HTTP tests below (plan.md §7.1)
    /// to exercise `collect_anthropic`'s success, refresh, timeout,
    /// status-failure, and malformed-JSON paths against a live endpoint
    /// rather than only a connection-refused failure.
    fn spawn_anthropic_mock(
        status_line: &str,
        body: Vec<u8>,
        delay: Duration,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let status_line = status_line.to_owned();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                let header = format!(
                    "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        (format!("http://127.0.0.1:{port}/"), handle)
    }

    fn valid_anthropic_usage_body() -> Vec<u8> {
        json!({
            "five_hour": {"utilization": 42, "resets_at": "2026-09-13T00:00:00Z"},
            "seven_day": {"utilization": 55, "resets_at": "2026-09-19T00:00:00Z"}
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn collect_anthropic_succeeds_against_a_mock_usage_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let far_future = Utc::now().timestamp_millis() + 3_600_000;
        write_credentials(&path, "still-valid", "unused-refresh", far_future);

        let (usage_url, handle) = spawn_anthropic_mock(
            "HTTP/1.1 200 OK",
            valid_anthropic_usage_body(),
            Duration::ZERO,
        );

        let source = SourceConfig::AnthropicOauth {
            credentials_path: path,
            usage_url,
            // Intentionally unreachable: a fresh token must never trigger a
            // network refresh.
            token_url: unreachable_url("/token"),
            timeout_seconds: 5,
        };
        let snapshot = collect(&source).unwrap();
        assert_eq!(snapshot.windows.len(), 2);
        let by_id = |id: &str| {
            snapshot
                .windows
                .iter()
                .find(|window| window.id == id)
                .unwrap()
        };
        assert_eq!(by_id("five_hour").used_fraction, 0.42);
        assert_eq!(by_id("seven_day").used_fraction, 0.55);
        handle.join().unwrap();
    }

    #[test]
    fn collect_anthropic_refreshes_against_a_mock_token_endpoint_then_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        // Already past the refresh threshold, so a refresh is required
        // before the usage request can be made.
        write_credentials(&path, "expired-access", "refresh-a", 1_000);

        let new_expiry = Utc::now().timestamp_millis() + 3_600_000;
        let refresh_body = json!({
            "accessToken": "refreshed-access",
            "refreshToken": "refreshed-refresh",
            "expiresAt": new_expiry,
        })
        .to_string()
        .into_bytes();
        let (token_url, token_handle) =
            spawn_anthropic_mock("HTTP/1.1 200 OK", refresh_body, Duration::ZERO);
        let (usage_url, usage_handle) = spawn_anthropic_mock(
            "HTTP/1.1 200 OK",
            valid_anthropic_usage_body(),
            Duration::ZERO,
        );

        let source = SourceConfig::AnthropicOauth {
            credentials_path: path.clone(),
            usage_url,
            token_url,
            timeout_seconds: 5,
        };
        let snapshot = collect(&source).unwrap();
        assert_eq!(snapshot.windows.len(), 2);
        token_handle.join().unwrap();
        usage_handle.join().unwrap();

        let on_disk = read_json(&path, "test credentials").unwrap();
        let oauth = on_disk.get("claudeAiOauth").unwrap();
        assert_eq!(
            oauth.get("accessToken").unwrap().as_str(),
            Some("refreshed-access"),
            "the refreshed token from the mock token endpoint must be persisted"
        );
    }

    #[test]
    fn collect_anthropic_fails_closed_when_the_usage_endpoint_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let far_future = Utc::now().timestamp_millis() + 3_600_000;
        write_credentials(&path, "still-valid", "unused-refresh", far_future);

        // The mock delays its response well past the agent's own timeout, so
        // this exercises a genuine timeout rather than connection-refused.
        let (usage_url, handle) = spawn_anthropic_mock(
            "HTTP/1.1 200 OK",
            valid_anthropic_usage_body(),
            Duration::from_millis(1_500),
        );

        let source = SourceConfig::AnthropicOauth {
            credentials_path: path,
            usage_url,
            token_url: unreachable_url("/token"),
            timeout_seconds: 1,
        };
        let started = std::time::Instant::now();
        let result = collect(&source);
        assert!(result.is_err(), "a hung usage endpoint must surface as Err");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the agent's own timeout must bound the wait, not the server's delay"
        );
        handle.join().unwrap();
    }

    #[test]
    fn collect_anthropic_fails_closed_when_the_usage_endpoint_returns_a_failure_status() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let far_future = Utc::now().timestamp_millis() + 3_600_000;
        write_credentials(&path, "still-valid", "unused-refresh", far_future);

        let (usage_url, handle) = spawn_anthropic_mock(
            "HTTP/1.1 401 Unauthorized",
            br#"{"error":"invalid_token"}"#.to_vec(),
            Duration::ZERO,
        );

        let source = SourceConfig::AnthropicOauth {
            credentials_path: path,
            usage_url,
            token_url: unreachable_url("/token"),
            timeout_seconds: 5,
        };
        assert!(collect(&source).is_err());
        handle.join().unwrap();
    }

    #[test]
    fn collect_anthropic_fails_closed_when_the_usage_endpoint_returns_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let far_future = Utc::now().timestamp_millis() + 3_600_000;
        write_credentials(&path, "still-valid", "unused-refresh", far_future);

        let (usage_url, handle) = spawn_anthropic_mock(
            "HTTP/1.1 200 OK",
            b"not valid json {".to_vec(),
            Duration::ZERO,
        );

        let source = SourceConfig::AnthropicOauth {
            credentials_path: path,
            usage_url,
            token_url: unreachable_url("/token"),
            timeout_seconds: 5,
        };
        let error = collect(&source).unwrap_err();
        assert!(
            format!("{error:#}").contains("Anthropic usage response was not valid JSON"),
            "expected the malformed-JSON error variant: {error:#}"
        );
        handle.join().unwrap();
    }

    #[test]
    fn read_bounded_accepts_data_at_exactly_the_limit() {
        let data = vec![7u8; 10];
        let mut cursor = std::io::Cursor::new(data.clone());
        let bytes = read_bounded(&mut cursor, 10).unwrap();
        assert_eq!(bytes, data);
    }

    #[test]
    fn read_bounded_rejects_data_over_the_limit() {
        let mut cursor = std::io::Cursor::new(vec![7u8; 11]);
        assert!(read_bounded(&mut cursor, 10).is_err());
    }

    #[test]
    fn collect_rejects_a_snapshot_with_duplicate_window_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("duplicate.json");
        fs::write(
            &path,
            r#"{
                "observed_at": "2026-09-28T12:00:00Z",
                "windows": [
                    {"id": "weekly", "used_fraction": 0.2, "resets_at": "2026-09-30T00:00:00Z"},
                    {"id": "weekly", "used_fraction": 0.9, "resets_at": "2026-10-05T00:00:00Z"}
                ]
            }"#,
        )
        .unwrap();

        let source = SourceConfig::NormalizedFile { path };
        let error = collect(&source).unwrap_err();
        assert!(
            format!("{error:#}").contains("duplicate"),
            "expected a duplicate-id rejection: {error:#}"
        );
    }

    #[test]
    fn collect_accepts_distinct_window_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("distinct.json");
        fs::write(
            &path,
            r#"{
                "observed_at": "2026-09-28T12:00:00Z",
                "windows": [
                    {"id": "five_hour", "used_fraction": 0.2, "resets_at": "2026-09-28T17:00:00Z"},
                    {"id": "weekly", "used_fraction": 0.9, "resets_at": "2026-10-05T00:00:00Z"}
                ]
            }"#,
        )
        .unwrap();

        let source = SourceConfig::NormalizedFile { path };
        assert!(collect(&source).is_ok());
    }

    #[test]
    fn duplicate_window_id_error_never_leaks_the_id_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("duplicate.json");
        fs::write(
            &path,
            r#"{
                "observed_at": "2026-09-28T12:00:00Z",
                "windows": [
                    {"id": "sk-super-secret-token", "used_fraction": 0.2, "resets_at": "2026-09-30T00:00:00Z"},
                    {"id": "sk-super-secret-token", "used_fraction": 0.5, "resets_at": "2026-10-05T00:00:00Z"}
                ]
            }"#,
        )
        .unwrap();

        let source = SourceConfig::NormalizedFile { path };
        let error = collect(&source).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(
            !rendered.contains("sk-super-secret-token"),
            "the duplicate window id must never leak into the error text: {rendered}"
        );
    }

    #[test]
    fn collect_normalized_file_rejects_oversized_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.json");
        fs::write(&path, vec![b'0'; (MAX_GENERIC_SOURCE_BYTES + 10) as usize]).unwrap();

        let source = SourceConfig::NormalizedFile { path };
        assert!(collect(&source).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn collect_command_succeeds_for_small_output() {
        let source = SourceConfig::Command {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                r#"printf '{"observed_at":"2026-09-12T12:00:00Z","windows":[{"id":"w","used_fraction":0.1,"resets_at":"2026-09-13T00:00:00Z"}]}'"#
                    .to_string(),
            ],
        };
        let snapshot = collect(&source).unwrap();
        assert_eq!(snapshot.windows.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn collect_command_rejects_oversized_stdout_and_reaps_the_child() {
        let source = SourceConfig::Command {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "yes | head -c 2000000".to_string(),
            ],
        };
        assert!(collect(&source).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn read_generic_command_bytes_succeeds_well_within_a_short_timeout() {
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo hi".to_string(),
        ];
        let bytes = read_generic_command_bytes(&argv, Duration::from_millis(500)).unwrap();
        assert_eq!(bytes, b"hi\n");
    }

    #[cfg(unix)]
    #[test]
    fn read_generic_command_bytes_times_out_on_a_hanging_command() {
        // Plan.md §22.10: this is the exact class of hang an unreachable
        // SSH host (or one stuck at a password prompt, since only key-based
        // auth is sanctioned) would produce -- the command timeout must
        // bound it rather than waiting forever.
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 5".to_string(),
        ];
        let start = std::time::Instant::now();
        let result = read_generic_command_bytes(&argv, Duration::from_millis(100));
        assert!(result.is_err());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "should time out around 100ms, not wait for the 5s sleep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_generic_command_bytes_timeout_kills_the_whole_process_group() {
        // Mirrors src/fleet.rs's command_observer_timeout_kills_the_whole_process_group:
        // an SSH invocation's remote session is a grandchild of the local
        // `ssh` process (and may itself fork further), so killing only the
        // direct child on timeout would leak it -- plan.md §22.10 requires
        // the whole round trip to be bounded, not just the local exec step.
        let dir = std::env::temp_dir().join(format!("subgov-source-pgroup-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("grandchild-ran");
        let script = format!("(sleep 0.3; touch {}) & sleep 5", marker.display());
        let argv = vec!["/bin/sh".to_string(), "-c".to_string(), script];

        let result = read_generic_command_bytes(&argv, Duration::from_millis(100));
        assert!(result.is_err());
        std::thread::sleep(Duration::from_millis(700));
        assert!(
            !marker.exists(),
            "the grandchild should have been killed along with the rest of the process group"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn command_timeout_error_names_the_command_but_not_a_secret_argument() {
        // `sleep 5` alone genuinely hangs for the full duration (unlike
        // `sleep 5 <non-numeric>`, which GNU sleep rejects and exits
        // immediately, defeating the point of this test); the secret-shaped
        // value sits in the script text without affecting sleep's timing,
        // so this actually exercises the CommandTimedOut path.
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "SECRET=sk-super-secret-token; sleep 5".to_string(),
        ];
        let error = read_generic_command_bytes(&argv, Duration::from_millis(100)).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("/bin/sh"),
            "expected the command name to survive: {rendered}"
        );
        assert!(
            !rendered.contains("sk-super-secret-token"),
            "an argv element must never leak into the timeout error text: {rendered}"
        );
    }

    /// Serves `body` once over raw HTTP/1.1 on an ephemeral loopback port,
    /// with no request-size mocking library required, and returns the raw
    /// request bytes the client sent so tests can inspect exactly what went
    /// over the wire.
    fn spawn_http_server(body: Vec<u8>) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut request = Vec::new();
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                if let Ok(n) = stream.read(&mut buf) {
                    request.extend_from_slice(&buf[..n]);
                }
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
            }
            request
        });
        (format!("http://127.0.0.1:{port}/"), handle)
    }

    #[test]
    fn collect_normalized_http_parses_small_body() {
        let body = br#"{"observed_at":"2026-09-12T12:00:00Z","windows":[{"id":"w","used_fraction":0.1,"resets_at":"2026-09-13T00:00:00Z"}]}"#.to_vec();
        let (url, handle) = spawn_http_server(body);

        let source = SourceConfig::NormalizedHttp {
            url,
            timeout_seconds: 5,
        };
        let snapshot = collect(&source).unwrap();
        assert_eq!(snapshot.windows.len(), 1);
        handle.join().unwrap();
    }

    #[test]
    fn collect_normalized_http_rejects_oversized_body() {
        let body = vec![b'a'; (MAX_GENERIC_SOURCE_BYTES + 10) as usize];
        let (url, handle) = spawn_http_server(body);

        let source = SourceConfig::NormalizedHttp {
            url,
            timeout_seconds: 5,
        };
        assert!(collect(&source).is_err());
        handle.join().unwrap();
    }

    #[test]
    fn collect_normalized_http_sends_no_credential_headers() {
        let body = br#"{"observed_at":"2026-09-12T12:00:00Z","windows":[{"id":"w","used_fraction":0.1,"resets_at":"2026-09-13T00:00:00Z"}]}"#.to_vec();
        let (url, handle) = spawn_http_server(body);

        let source = SourceConfig::NormalizedHttp {
            url,
            timeout_seconds: 5,
        };
        collect(&source).unwrap();

        let request = String::from_utf8_lossy(&handle.join().unwrap()).to_lowercase();
        assert!(
            !request.contains("authorization:"),
            "a generic normalized_http source must never auto-attach a credential header: {request}"
        );
        assert!(!request.contains("cookie:"));
    }

    #[test]
    fn http_agent_rejects_a_zero_timeout() {
        assert!(http_agent(0).is_err());
    }

    #[test]
    fn http_agent_accepts_a_positive_timeout() {
        assert!(http_agent(1).is_ok());
    }

    #[test]
    fn collect_normalized_http_rejects_zero_timeout_without_dialing_out() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();

        let source = SourceConfig::NormalizedHttp {
            url: format!("http://127.0.0.1:{port}/"),
            timeout_seconds: 0,
        };
        assert!(collect(&source).is_err());

        // Give a stray connection attempt a brief window to arrive, then
        // prove none did: the zero-timeout rejection happens before dialing
        // out at all, not by racing an immediate connect-timeout.
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            listener.accept().is_err(),
            "a zero-timeout source must never attempt a connection"
        );
    }

    #[test]
    fn collect_normalized_http_refuses_a_redirect_instead_of_following_it() {
        // The redirect target is a second server; if it ever receives a
        // connection, the client followed the redirect instead of refusing
        // it as plan.md §7.4 requires.
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let target_port = target.local_addr().unwrap().port();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let source = SourceConfig::NormalizedHttp {
            url: format!("http://127.0.0.1:{port}/"),
            timeout_seconds: 5,
        };
        assert!(
            collect(&source).is_err(),
            "a redirect response must be refused, not treated as success"
        );
        handle.join().unwrap();

        std::thread::sleep(Duration::from_millis(100));
        assert!(
            target.accept().is_err(),
            "the redirect target must never be contacted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn collect_command_error_never_leaks_a_malformed_field_value() {
        // used_fraction is typed f64; a string here forces serde's
        // invalid-type path, which (unless discarded) would otherwise echo
        // the offending value straight into the error text.
        let source = SourceConfig::Command {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                r#"printf '{"observed_at":"2026-09-12T12:00:00Z","windows":[{"id":"w","used_fraction":"sk-super-secret-token","resets_at":"2026-09-13T00:00:00Z"}]}'"#
                    .to_string(),
            ],
        };
        let error = collect(&source).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(
            !rendered.contains("sk-super-secret-token"),
            "a malformed field value must never leak into the error text: {rendered}"
        );
    }

    #[test]
    fn collect_normalized_http_error_never_leaks_a_malformed_field_value() {
        let body = br#"{"observed_at":"2026-09-12T12:00:00Z","windows":[{"id":"w","used_fraction":"sk-super-secret-token","resets_at":"2026-09-13T00:00:00Z"}]}"#.to_vec();
        let (url, handle) = spawn_http_server(body);

        let source = SourceConfig::NormalizedHttp {
            url,
            timeout_seconds: 5,
        };
        let error = collect(&source).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(
            !rendered.contains("sk-super-secret-token"),
            "a malformed field value must never leak into the error text: {rendered}"
        );
        handle.join().unwrap();
    }

    #[test]
    fn codex_source_error_display_carries_only_a_safe_code() {
        let error = CodexSourceError::RateLimitsRejected { code: Some(-32000) };
        let text = error.to_string();
        assert!(text.contains("-32000"));

        let error = CodexSourceError::InitializeRejected { code: None };
        assert!(!error.to_string().is_empty());
    }

    fn frame_line(value: Option<CodexFrame>) -> String {
        match value {
            Some(CodexFrame::Line(line)) => line,
            other => panic!(
                "expected CodexFrame::Line, got {}",
                match other {
                    Some(CodexFrame::Oversized) => "Oversized",
                    None => "None (EOF)",
                    _ => unreachable!(),
                }
            ),
        }
    }

    #[test]
    fn read_bounded_codex_frame_returns_an_ordinary_line_without_its_newline() {
        let mut cursor = std::io::Cursor::new(b"{\"id\":1}\n".to_vec());
        let frame = read_bounded_codex_frame(&mut cursor, 4096).unwrap();
        assert_eq!(frame_line(frame), r#"{"id":1}"#);
    }

    #[test]
    fn read_bounded_codex_frame_handles_multiple_lines_in_sequence() {
        let mut cursor = std::io::Cursor::new(b"first\nsecond\n".to_vec());
        let first = read_bounded_codex_frame(&mut cursor, 4096).unwrap();
        assert_eq!(frame_line(first), "first");
        let second = read_bounded_codex_frame(&mut cursor, 4096).unwrap();
        assert_eq!(frame_line(second), "second");
    }

    #[test]
    fn read_bounded_codex_frame_returns_none_at_true_eof() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        let frame = read_bounded_codex_frame(&mut cursor, 4096).unwrap();
        assert!(frame.is_none());
    }

    #[test]
    fn read_bounded_codex_frame_discards_a_line_over_the_limit_instead_of_buffering_it() {
        // Well over the limit; a naive unbounded reader would accumulate all
        // of this before ever returning.
        let oversized_content = "a".repeat(50_000);
        let mut input = oversized_content.clone().into_bytes();
        input.push(b'\n');
        let mut cursor = std::io::Cursor::new(input);

        let frame = read_bounded_codex_frame(&mut cursor, 1024).unwrap();
        assert!(
            matches!(frame, Some(CodexFrame::Oversized)),
            "a line far exceeding the limit must be reported as Oversized, not buffered whole"
        );
    }

    #[test]
    fn read_bounded_codex_frame_resyncs_on_the_next_line_after_discarding_an_oversized_one() {
        let oversized = "x".repeat(10_000);
        let mut input = oversized.into_bytes();
        input.push(b'\n');
        input.extend_from_slice(b"a well-formed follow-up line\n");
        let mut cursor = std::io::Cursor::new(input);

        let first = read_bounded_codex_frame(&mut cursor, 1024).unwrap();
        assert!(matches!(first, Some(CodexFrame::Oversized)));

        let second = read_bounded_codex_frame(&mut cursor, 1024).unwrap();
        assert_eq!(
            frame_line(second),
            "a well-formed follow-up line",
            "the frame after a discarded oversized one must parse cleanly, proving the reader \
             resynchronized on the next newline rather than losing frame boundaries"
        );
    }

    #[test]
    fn read_bounded_codex_frame_discards_an_oversized_final_line_with_no_trailing_newline() {
        // The stream ends mid-line (no `\n` at all) past the limit -- e.g.
        // the process died while writing a line that was already too long.
        let input = "y".repeat(5_000).into_bytes();
        let mut cursor = std::io::Cursor::new(input);

        let frame = read_bounded_codex_frame(&mut cursor, 1024).unwrap();
        assert!(matches!(frame, Some(CodexFrame::Oversized)));
        // Nothing left to read afterward.
        assert!(read_bounded_codex_frame(&mut cursor, 1024)
            .unwrap()
            .is_none());
    }

    #[test]
    fn drain_until_newline_only_ever_holds_one_small_chunk_at_a_time() {
        // Exercises the multi-chunk path directly: several chunk-widths of
        // data before the terminating newline.
        let mut input = vec![b'z'; 4096 * 3 + 100];
        input.push(b'\n');
        input.extend_from_slice(b"next\n");
        let mut cursor = std::io::Cursor::new(input);

        drain_until_newline(&mut cursor).unwrap();
        let frame = read_bounded_codex_frame(&mut cursor, 4096).unwrap();
        assert_eq!(frame_line(frame), "next");
    }

    fn resource_snapshot_json() -> &'static str {
        r#"{
            "observed_at": "2026-09-28T12:00:00Z",
            "fresh": true,
            "host_id": "lab",
            "cpu_available_fraction": 0.42,
            "mem_available_mb": 12288,
            "mem_total_mb": 65536
        }"#
    }

    #[test]
    fn collect_resource_parses_a_valid_file_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resource.json");
        fs::write(&path, resource_snapshot_json()).unwrap();

        let source = SourceConfig::NormalizedFile { path };
        let snapshot = collect_resource(&source).unwrap();
        assert_eq!(snapshot.host_id, "lab");
        assert_eq!(snapshot.mem_total_mb, 65536);
    }

    #[test]
    fn collect_resource_rejects_oversized_file_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.json");
        fs::write(&path, vec![b'0'; (MAX_GENERIC_SOURCE_BYTES + 10) as usize]).unwrap();

        let source = SourceConfig::NormalizedFile { path };
        assert!(collect_resource(&source).is_err());
    }

    #[test]
    fn collect_resource_rejects_a_snapshot_failing_model_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resource.json");
        // mem_available_mb > mem_total_mb: valid JSON, fails
        // ResourceSnapshot::validate.
        fs::write(
            &path,
            r#"{
                "observed_at": "2026-09-28T12:00:00Z",
                "host_id": "lab",
                "cpu_available_fraction": 0.42,
                "mem_available_mb": 100,
                "mem_total_mb": 50
            }"#,
        )
        .unwrap();

        let source = SourceConfig::NormalizedFile { path };
        assert!(collect_resource(&source).is_err());
    }

    #[test]
    fn collect_resource_rejects_an_out_of_range_cpu_fraction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resource.json");
        fs::write(
            &path,
            r#"{
                "observed_at": "2026-09-28T12:00:00Z",
                "host_id": "lab",
                "cpu_available_fraction": 1.5,
                "mem_available_mb": 100,
                "mem_total_mb": 200
            }"#,
        )
        .unwrap();

        let source = SourceConfig::NormalizedFile { path };
        assert!(collect_resource(&source).is_err());
    }

    #[test]
    fn collect_resource_rejects_a_missing_required_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resource.json");
        fs::write(
            &path,
            r#"{
                "observed_at": "2026-09-28T12:00:00Z",
                "host_id": "lab",
                "cpu_available_fraction": 0.42,
                "mem_available_mb": 100
            }"#,
        )
        .unwrap();

        let source = SourceConfig::NormalizedFile { path };
        assert!(collect_resource(&source).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn collect_resource_succeeds_for_a_command_source() {
        let source = SourceConfig::Command {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("printf '%s' '{}'", resource_snapshot_json()),
            ],
        };
        let snapshot = collect_resource(&source).unwrap();
        assert_eq!(snapshot.host_id, "lab");
    }

    #[cfg(unix)]
    #[test]
    fn collect_resource_rejects_oversized_command_stdout_and_reaps_the_child() {
        let source = SourceConfig::Command {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "yes | head -c 2000000".to_string(),
            ],
        };
        assert!(collect_resource(&source).is_err());
    }

    #[test]
    fn collect_resource_parses_a_valid_http_snapshot() {
        let (url, handle) = spawn_http_server(resource_snapshot_json().as_bytes().to_vec());

        let source = SourceConfig::NormalizedHttp {
            url,
            timeout_seconds: 5,
        };
        let snapshot = collect_resource(&source).unwrap();
        assert_eq!(snapshot.host_id, "lab");
        handle.join().unwrap();
    }

    #[test]
    fn collect_resource_refuses_a_redirect_instead_of_following_it() {
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let target_port = target.local_addr().unwrap().port();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let source = SourceConfig::NormalizedHttp {
            url: format!("http://127.0.0.1:{port}/"),
            timeout_seconds: 5,
        };
        assert!(
            collect_resource(&source).is_err(),
            "a redirect response must be refused, not treated as success"
        );
        handle.join().unwrap();

        std::thread::sleep(Duration::from_millis(100));
        assert!(
            target.accept().is_err(),
            "the redirect target must never be contacted"
        );
    }

    #[test]
    fn collect_resource_rejects_anthropic_oauth_source_type() {
        let dir = tempfile::tempdir().unwrap();
        let source = SourceConfig::AnthropicOauth {
            credentials_path: dir.path().join("unused.json"),
            usage_url: unreachable_url("/usage"),
            token_url: unreachable_url("/token"),
            timeout_seconds: 1,
        };
        let error = collect_resource(&source).unwrap_err();
        assert!(
            error.to_string().contains("resource sources only support"),
            "expected the unsupported-source-type error, got: {error}"
        );
    }

    #[test]
    fn collect_resource_rejects_codex_app_server_source_type() {
        let source = SourceConfig::CodexAppServer {
            executable: PathBuf::from("/does/not/matter"),
            timeout_seconds: 1,
        };
        let error = collect_resource(&source).unwrap_err();
        assert!(
            error.to_string().contains("resource sources only support"),
            "expected the unsupported-source-type error, got: {error}"
        );
    }
}
