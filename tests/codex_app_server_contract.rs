//! Contract tests for the Codex app-server source (plan.md §7.2), driven
//! against the scripted fake in `tests/support/fake_codex_app_server.rs`
//! instead of a real Codex installation. Exercises the handshake,
//! interleaved notifications, timeout, child-exit, sparse-window, and
//! protocol-error cases named in that section's build requirements.
//!
//! Every test goes through the same public entry point production code
//! uses (`subscription_governor::source::collect`), so this is a true
//! end-to-end contract test of `collect_codex`/`read_codex_rate_limits`,
//! not a unit test of an internal helper.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use subscription_governor::config::SourceConfig;
use subscription_governor::source::collect;

fn fake_codex_executable() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_codex_app_server"))
}

/// Writes `steps` (already-built JSON-RPC step objects) to a temp script
/// file and returns a source config wired to the fake app-server with that
/// script and `timeout_seconds`.
fn scripted_source(steps: &[Value], timeout_seconds: u64) -> (SourceConfig, tempfile::TempPath) {
    let script = serde_json::to_vec(steps).unwrap();
    let mut file = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut file, &script).unwrap();
    let path = file.into_temp_path();
    let source = SourceConfig::CodexAppServer {
        executable: fake_codex_executable(),
        timeout_seconds,
    };
    (source, path)
}

fn read_step() -> Value {
    json!({"action": "read"})
}

fn write_step(frame: Value) -> Value {
    json!({"action": "write", "frame": frame})
}

fn success_rate_limits_frame(id: i64) -> Value {
    json!({
        "id": id,
        "result": {
            "rateLimitsByLimitId": {
                "codex": {
                    "primary": {"usedPercent": 42.0, "windowDurationMins": 300, "resetsAt": 2_000_000_000_i64},
                    "secondary": {"usedPercent": 10.0, "windowDurationMins": 10080, "resetsAt": 2_000_100_000_i64}
                }
            }
        }
    })
}

/// `std::env::set_var` is process-wide, but the test harness runs `#[test]`
/// functions concurrently on separate threads within this one binary by
/// default; without serializing, two tests could interleave their
/// set/remove of `FAKE_CODEX_SCRIPT` and spawn the fake against the wrong
/// script. Every call to `collect_with_script` holds this for its whole
/// critical section.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Runs `FAKE_CODEX_SCRIPT=path collect(&source)` with the env var scoped to
/// this call only, serialized against other tests in this binary via
/// [`ENV_LOCK`].
fn collect_with_script(
    source: &SourceConfig,
    script_path: &std::path::Path,
) -> anyhow::Result<subscription_governor::model::QuotaSnapshot> {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("FAKE_CODEX_SCRIPT", script_path);
    let result = collect(source);
    std::env::remove_var("FAKE_CODEX_SCRIPT");
    result
}

#[test]
fn handshake_success_produces_a_normalized_snapshot() {
    let steps = vec![
        read_step(),                                  // initialize request
        write_step(json!({"id": 1, "result": {}})),   // initialize response
        read_step(),                                  // initialized notification
        read_step(),                                  // account/rateLimits/read request
        write_step(success_rate_limits_frame(2)),      // rateLimits response
    ];
    let (source, script) = scripted_source(&steps, 5);

    let snapshot = collect_with_script(&source, &script).expect("handshake should succeed");
    assert_eq!(snapshot.windows.len(), 2);
    assert!(snapshot
        .windows
        .iter()
        .any(|window| window.id == "codex.primary"));
    assert!(snapshot
        .windows
        .iter()
        .any(|window| window.id == "codex.secondary"));
}

#[test]
fn interleaved_notifications_between_responses_are_ignored() {
    let steps = vec![
        read_step(),
        // Unsolicited notifications with no "id" arrive before and after
        // the real response; receive_response must skip them rather than
        // mistake one for the id=1 reply.
        write_step(json!({"method": "codex/log", "params": {"line": "starting"}})),
        write_step(json!({"id": 1, "result": {}})),
        write_step(json!({"method": "codex/log", "params": {"line": "initialized"}})),
        read_step(),
        read_step(),
        write_step(json!({"method": "codex/progress", "params": {"pct": 50}})),
        write_step(success_rate_limits_frame(2)),
    ];
    let (source, script) = scripted_source(&steps, 5);

    let snapshot =
        collect_with_script(&source, &script).expect("interleaved notifications must be skipped");
    assert_eq!(snapshot.windows.len(), 2);
}

#[test]
fn a_hung_app_server_times_out_rather_than_blocking_forever() {
    let steps = vec![
        read_step(),
        // Sleep well past timeout_seconds without ever responding.
        json!({"action": "sleep_ms", "value": 3_000}),
    ];
    let (source, script) = scripted_source(&steps, 1);

    let started = Instant::now();
    let result = collect_with_script(&source, &script);
    let elapsed = started.elapsed();

    assert!(result.is_err(), "a hung app-server must surface as Err");
    assert!(
        elapsed < Duration::from_secs(3),
        "must time out near timeout_seconds (1s), not wait for the full 3s sleep: {elapsed:?}"
    );
}

#[test]
fn the_app_server_exiting_mid_handshake_surfaces_as_an_error() {
    let steps = vec![
        read_step(),
        // Die instead of responding to initialize.
        json!({"action": "exit", "code": 1}),
    ];
    let (source, script) = scripted_source(&steps, 5);

    let started = Instant::now();
    let result = collect_with_script(&source, &script);
    let elapsed = started.elapsed();

    assert!(result.is_err(), "a dead app-server must surface as Err");
    assert!(
        elapsed < Duration::from_secs(4),
        "a closed stdout should fail fast via channel disconnection, not wait out the \
         full 5s timeout: {elapsed:?}"
    );
}

#[test]
fn a_sparse_rate_limit_bucket_yields_only_its_present_windows() {
    // Only `secondary` is present, and it also omits usedPercent -- both
    // should be skipped without panicking, leaving one usable window.
    let steps = vec![
        read_step(),
        write_step(json!({"id": 1, "result": {}})),
        read_step(),
        read_step(),
        write_step(json!({
            "id": 2,
            "result": {
                "rateLimitsByLimitId": {
                    "codex": {
                        "primary": {"usedPercent": 15.0, "windowDurationMins": 300, "resetsAt": 2_000_000_000_i64},
                        "secondary": {"windowDurationMins": 10080}
                    }
                }
            }
        })),
    ];
    let (source, script) = scripted_source(&steps, 5);

    let snapshot =
        collect_with_script(&source, &script).expect("a sparse bucket should still parse");
    assert_eq!(snapshot.windows.len(), 1);
    assert_eq!(snapshot.windows[0].id, "codex.primary");
}

#[test]
fn a_fully_empty_rate_limit_response_is_a_clean_error() {
    let steps = vec![
        read_step(),
        write_step(json!({"id": 1, "result": {}})),
        read_step(),
        read_step(),
        write_step(json!({"id": 2, "result": {"rateLimitsByLimitId": {}}})),
    ];
    let (source, script) = scripted_source(&steps, 5);

    let result = collect_with_script(&source, &script);
    assert!(result.is_err(), "no usable windows must be an error, not an empty snapshot");
}

#[test]
fn a_protocol_error_on_initialize_is_reported_without_leaking_its_message() {
    let steps = vec![
        read_step(),
        write_step(json!({
            "id": 1,
            "error": {"code": -32001, "message": "unauthorized: token sk-leaked-secret-abc"}
        })),
    ];
    let (source, script) = scripted_source(&steps, 5);

    let error = collect_with_script(&source, &script).unwrap_err();
    let rendered = format!("{error:#}");
    assert!(rendered.contains("-32001"), "the safe error code should survive: {rendered}");
    assert!(
        !rendered.contains("sk-leaked-secret-abc"),
        "the provider-controlled error message must never leak into our error text: {rendered}"
    );
}

#[test]
fn a_protocol_error_on_rate_limits_read_is_reported_without_leaking_its_message() {
    let steps = vec![
        read_step(),
        write_step(json!({"id": 1, "result": {}})),
        read_step(),
        read_step(),
        write_step(json!({
            "id": 2,
            "error": {"code": -32002, "message": "internal: db password hunter2-super-secret"}
        })),
    ];
    let (source, script) = scripted_source(&steps, 5);

    let error = collect_with_script(&source, &script).unwrap_err();
    let rendered = format!("{error:#}");
    assert!(rendered.contains("-32002"), "the safe error code should survive: {rendered}");
    assert!(
        !rendered.contains("hunter2-super-secret"),
        "the provider-controlled error message must never leak into our error text: {rendered}"
    );
}
