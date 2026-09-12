use crate::config::SourceConfig;
use crate::model::{QuotaSnapshot, QuotaWindow};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const REFRESH_THRESHOLD_MILLIS: i64 = 300_000;

pub fn collect(source: &SourceConfig) -> Result<QuotaSnapshot> {
    let snapshot = match source {
        SourceConfig::NormalizedFile { path } => {
            let bytes = fs::read(path)
                .with_context(|| format!("failed to read snapshot {}", path.display()))?;
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
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("failed to execute quota source {}", argv[0]))?;
    if !output.status.success() {
        bail!("quota source {} exited with {}", argv[0], output.status);
    }
    serde_json::from_slice(&output.stdout)
        .context("quota source did not emit a normalized snapshot")
}

fn collect_normalized_http(url: &str, timeout_seconds: u64) -> Result<QuotaSnapshot> {
    let agent = http_agent(timeout_seconds);
    let response = agent
        .get(url)
        .call()
        .map_err(|error| anyhow!("normalized HTTP quota request failed: {error}"))?;
    response
        .into_json()
        .context("normalized HTTP source returned invalid snapshot JSON")
}

fn collect_anthropic(
    credentials_path: &Path,
    usage_url: &str,
    token_url: &str,
    timeout_seconds: u64,
) -> Result<QuotaSnapshot> {
    let mut credentials = read_json(credentials_path, "Claude Code credentials")?;
    let oauth = credentials
        .get_mut("claudeAiOauth")
        .and_then(Value::as_object_mut)
        .context("Claude Code credentials are missing claudeAiOauth")?;
    let expires_at = oauth
        .get("expiresAt")
        .and_then(Value::as_i64)
        .context("Claude Code credentials are missing expiresAt")?;

    let access_token = if Utc::now().timestamp_millis() + REFRESH_THRESHOLD_MILLIS >= expires_at {
        let refresh_token = oauth
            .get("refreshToken")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .context("Claude Code credentials are missing refreshToken")?
            .to_owned();
        let refreshed = refresh_anthropic(&refresh_token, token_url, timeout_seconds)?;
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
        oauth.insert("accessToken".into(), Value::String(access.clone()));
        oauth.insert("refreshToken".into(), Value::String(new_refresh));
        oauth.insert("expiresAt".into(), Value::Number(new_expiry.into()));
        write_json_atomic(credentials_path, &credentials)?;
        access
    } else {
        oauth
            .get("accessToken")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .context("Claude Code credentials are missing accessToken")?
            .to_owned()
    };

    let response = http_agent(timeout_seconds)
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

fn refresh_anthropic(refresh_token: &str, token_url: &str, timeout_seconds: u64) -> Result<Value> {
    let response = http_agent(timeout_seconds)
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
    let result = response
        .get("result")
        .context("Codex rate-limit response omitted result")?;
    parse_codex_rate_limits(result, Utc::now())
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
    Ok(snapshot)
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn http_agent(timeout_seconds: u64) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(timeout_seconds))
        .build()
}

fn read_json(path: &Path, label: &str) -> Result<Value> {
    let bytes =
        fs::read(path).with_context(|| format!("failed to read {label} at {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {label}"))
}

fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    use std::fs::OpenOptions;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path.parent().context("credentials path has no parent")?;
    let temporary = parent.join(format!(
        ".subscription-governor-credentials-{}.tmp",
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temporary)
            .context("failed to create temporary credentials file")?;
        serde_json::to_writer_pretty(&mut file, value)
            .context("failed to serialize refreshed credentials")?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path).context("failed to install refreshed credentials")?;
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
}
