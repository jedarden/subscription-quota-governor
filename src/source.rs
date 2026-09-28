use crate::config::SourceConfig;
use crate::model::{QuotaSnapshot, QuotaWindow, ResetCredit, ResetCreditsSnapshot};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use fs2::FileExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const REFRESH_THRESHOLD_MILLIS: i64 = 300_000;
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Maximum bytes read from a generic source's command stdout, file, or HTTP
/// body (plan.md §7.4: "Files and HTTP bodies have explicit maximum sizes in
/// v1."). A normalized quota snapshot is a small JSON document, so this is
/// generous headroom rather than a tight budget.
const MAX_GENERIC_SOURCE_BYTES: u64 = 1024 * 1024;

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
        SourceConfig::NormalizedFile { path } => {
            let mut file = File::open(path)
                .with_context(|| format!("failed to read snapshot {}", path.display()))?;
            let bytes = read_bounded(&mut file, MAX_GENERIC_SOURCE_BYTES)
                .with_context(|| format!("snapshot {}", path.display()))?;
            serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse snapshot {}", path.display()))?
        }
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

fn collect_command(argv: &[String]) -> Result<QuotaSnapshot> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to start quota source {}", argv[0]))?;
    let mut stdout = child
        .stdout
        .take()
        .context("quota source stdout was not piped")?;
    let bytes = read_bounded(&mut stdout, MAX_GENERIC_SOURCE_BYTES);
    drop(stdout);
    if bytes.is_err() {
        // The child may still be trying to write past the bound; kill it
        // rather than risk it blocking forever on a full pipe buffer nobody
        // is draining.
        let _ = child.kill();
        let _ = child.wait();
    }
    let bytes = bytes.with_context(|| format!("quota source {} stdout", argv[0]))?;

    let status = child
        .wait()
        .with_context(|| format!("failed to wait for quota source {}", argv[0]))?;
    if !status.success() {
        bail!("quota source {} exited with {}", argv[0], status);
    }
    serde_json::from_slice(&bytes)
        .context("quota source did not emit a normalized snapshot")
}

fn collect_normalized_http(url: &str, timeout_seconds: u64) -> Result<QuotaSnapshot> {
    let agent = http_agent(timeout_seconds)?;
    let response = agent
        .get(url)
        .call()
        .map_err(|error| anyhow!("normalized HTTP quota request failed: {error}"))?;
    let mut reader = response.into_reader();
    let bytes = read_bounded(&mut reader, MAX_GENERIC_SOURCE_BYTES)
        .context("normalized HTTP quota response")?;
    serde_json::from_slice(&bytes)
        .context("normalized HTTP source returned invalid snapshot JSON")
}

/// Reads at most `limit` bytes from `reader`, failing rather than silently
/// truncating if more data is available. Used to bound every generic
/// source's untrusted or unbounded input -- command stdout, a snapshot file,
/// or an HTTP response body -- per plan.md §7.4.
fn read_bounded(reader: &mut dyn Read, limit: u64) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut buffer)
        .context("failed to read source output")?;
    if buffer.len() as u64 > limit {
        bail!("source output exceeds the {limit}-byte maximum");
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

    let response = http_agent(timeout_seconds)?
        .get(usage_url)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("User-Agent", "claude-code/2.1.114")
        .call()
        .map_err(|error| anyhow!("Anthropic usage request failed: {error}"))?;
    let payload: Value = response
        .into_json()
        .context("Anthropic usage endpoint returned invalid JSON")?;
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
        .with_context(|| {
            format!(
                "failed to open Claude Code credentials {} for locking",
                credentials_path.display()
            )
        })?;
    lock_exclusive_bounded(&lock_file, timeout_seconds).with_context(|| {
        format!(
            "failed to lock Claude Code credentials {} (a concurrent writer may be holding it)",
            credentials_path.display()
        )
    })?;
    // Held until this function returns, so the whole read-decide(-refresh)
    // section below is one critical section under the lock.
    let _lock = lock_file;

    let credentials = read_json(credentials_path, "Claude Code credentials")?;
    let oauth = credentials
        .get("claudeAiOauth")
        .and_then(Value::as_object)
        .context("Claude Code credentials are missing claudeAiOauth")?;
    if let Some(token) = fresh_access_token(oauth)? {
        return Ok(token);
    }
    let refresh_token = required_refresh_token(oauth)?;
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
fn fresh_access_token(oauth: &serde_json::Map<String, Value>) -> Result<Option<String>> {
    let expires_at = oauth
        .get("expiresAt")
        .and_then(Value::as_i64)
        .context("Claude Code credentials are missing expiresAt")?;
    if Utc::now().timestamp_millis() + REFRESH_THRESHOLD_MILLIS >= expires_at {
        return Ok(None);
    }
    let token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .context("Claude Code credentials are missing accessToken")?
        .to_owned();
    Ok(Some(token))
}

fn required_refresh_token(oauth: &serde_json::Map<String, Value>) -> Result<String> {
    oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .context("Claude Code credentials are missing refreshToken")
        .map(str::to_owned)
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
        .context("Anthropic token refresh omitted accessToken")?
        .to_owned();
    let new_refresh = refreshed
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .context("Anthropic token refresh omitted refreshToken")?
        .to_owned();
    let new_expiry = refreshed
        .get("expiresAt")
        .and_then(Value::as_i64)
        .context("Anthropic token refresh omitted expiresAt")?;

    let mut current = read_json(credentials_path, "Claude Code credentials")?;
    let current_oauth = current
        .get_mut("claudeAiOauth")
        .and_then(Value::as_object_mut)
        .context("Claude Code credentials are missing claudeAiOauth")?;
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
            _ => bail!(
                "Claude Code credentials at {} were refreshed by another process during this \
                 poll; skipping this cycle rather than overwriting a concurrent refresh",
                credentials_path.display()
            ),
        };
    }

    current_oauth.insert("accessToken".into(), Value::String(access.clone()));
    current_oauth.insert("refreshToken".into(), Value::String(new_refresh));
    current_oauth.insert("expiresAt".into(), Value::Number(new_expiry.into()));
    write_json_atomic(credentials_path, &current)?;
    Ok(access)
}

fn refresh_anthropic(refresh_token: &str, token_url: &str, timeout_seconds: u64) -> Result<Value> {
    let response = http_agent(timeout_seconds)?
        .post(token_url)
        .set("Content-Type", "application/json")
        .set("User-Agent", "claude-code/2.1.114")
        .send_json(json!({
            "grantType": "refresh_token",
            "refreshToken": refresh_token
        }))
        .map_err(|error| anyhow!("Anthropic token refresh failed: {error}"))?;
    response
        .into_json()
        .context("Anthropic token refresh returned invalid JSON")
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
    let result = read_codex_rate_limits(executable, timeout_seconds)?;
    parse_codex_rate_limits(&result, Utc::now())
}

fn read_codex_rate_limits(executable: &Path, timeout_seconds: u64) -> Result<Value> {
    let child = Command::new(executable)
        .args(["app-server", "--listen", "stdio://"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start {} app-server", executable.display()))?;
    let mut child = ChildGuard(child);
    let mut stdin = child
        .0
        .stdin
        .take()
        .context("Codex app-server has no stdin")?;
    let stdout = child
        .0
        .stdout
        .take()
        .context("Codex app-server has no stdout")?;
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if let Ok(value) = serde_json::from_str::<Value>(&line) {
                        if sender.send(value).is_err() {
                            return;
                        }
                    }
                }
                Err(_) => return,
            }
        }
    });

    let initialize = json!({
        "id": 1,
        "method": "initialize",
        "params": {"clientInfo": {"name": "subscription-governor", "version": "0.1.0"}}
    });
    writeln!(stdin, "{initialize}").context("failed to initialize Codex app-server")?;
    stdin.flush()?;
    let initialized = receive_response(&receiver, 1, timeout_seconds)
        .context("timed out initializing Codex app-server")?;
    if let Some(error) = initialized.get("error") {
        child.terminate();
        let _ = reader.join();
        bail!("Codex app-server rejected initialization: {error}");
    }

    writeln!(stdin, "{}", json!({"method": "initialized", "params": {}}))?;
    writeln!(
        stdin,
        "{}",
        json!({"id": 2, "method": "account/rateLimits/read"})
    )?;
    stdin.flush()?;
    let response = receive_response(&receiver, 2, timeout_seconds);
    drop(stdin);
    child.terminate();
    let _ = reader.join();
    let response = response.context("timed out reading Codex rate limits")?;
    if let Some(error) = response.get("error") {
        bail!("Codex app-server rejected the rate-limit request: {error}");
    }
    response
        .get("result")
        .cloned()
        .context("Codex rate-limit response omitted result")
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

fn receive_response(
    receiver: &mpsc::Receiver<Value>,
    id: i64,
    timeout_seconds: u64,
) -> Result<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_seconds);
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .context("response deadline elapsed")?;
        let value = receiver
            .recv_timeout(remaining)
            .context("app-server response channel closed")?;
        if value.get("id").and_then(Value::as_i64) == Some(id) {
            return Ok(value);
        }
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
        bail!("Codex rate-limit response contained no usable quota windows");
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
    for window in &snapshot.windows {
        if window.id.is_empty() {
            bail!("quota snapshot has an empty window id");
        }
        if !window.used_fraction.is_finite() || !(0.0..=1.0).contains(&window.used_fraction) {
            bail!("quota window {} used_fraction must be in [0, 1]", window.id);
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
fn http_agent(timeout_seconds: u64) -> Result<ureq::Agent> {
    if timeout_seconds == 0 {
        bail!("HTTP source timeout_seconds must be greater than zero");
    }
    Ok(ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(timeout_seconds))
        .build())
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
                format!("failed to fsync {} after credentials rename", parent.display())
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
        assert_eq!(oauth.get("accessToken").unwrap().as_str(), Some("new-access"));
        assert_eq!(oauth.get("refreshToken").unwrap().as_str(), Some("refresh-b"));
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
}
