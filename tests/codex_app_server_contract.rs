//! Contract tests for the Codex app-server source (plan.md §7.2), driven
//! against the scripted fake in `tests/support/fake_codex_app_server.rs`
//! instead of a real Codex installation. Exercises the handshake,
//! interleaved notifications, timeout, child-exit, sparse-window,
//! protocol-error, push-notification caching, reconciliation-masked
//! session death, and oversized-frame cases named in that section's build
//! requirements. The bounded-frame reader's own edge cases (multi-chunk
//! discard, resync after an oversized line, EOF mid-line) and the cache's
//! own expiry decision (`codex_cache_is_usable`) are unit-tested directly
//! in `src/source.rs`; the tests here prove the end-to-end wiring against a
//! real child process.
//!
//! Every test goes through the same public entry point production code
//! uses (`subscription_governor::source::collect`), so this is a true
//! end-to-end contract test of `collect_codex`/`with_codex_session`, not a
//! unit test of an internal helper.
//!
//! Codex sessions are supervised and kept alive across `collect()` calls
//! (a process-wide cache keyed by executable path), so every test here calls
//! `shutdown_codex_sessions()` first to guarantee a fresh spawn against its
//! own script rather than reusing another test's leftover session. The two
//! multi-poll tests (notification caching, reconciliation-masked death)
//! deliberately hold the session across both `collect()` calls without
//! resetting in between, since proving session persistence is their point.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use subscription_governor::config::SourceConfig;
use subscription_governor::source::{collect, shutdown_codex_sessions};

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
/// [`ENV_LOCK`]. Resets the supervised Codex session cache first, so this
/// call always spawns a fresh app-server against its own script rather than
/// reusing a session another test (or an earlier call in this same test)
/// left alive.
fn collect_with_script(
    source: &SourceConfig,
    script_path: &std::path::Path,
) -> anyhow::Result<subscription_governor::model::QuotaSnapshot> {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    shutdown_codex_sessions();
    std::env::set_var("FAKE_CODEX_SCRIPT", script_path);
    let result = collect(source);
    std::env::remove_var("FAKE_CODEX_SCRIPT");
    result
}

#[test]
fn handshake_success_produces_a_normalized_snapshot() {
    let steps = vec![
        read_step(),                                // initialize request
        write_step(json!({"id": 1, "result": {}})), // initialize response
        read_step(),                                // initialized notification
        read_step(),                                // account/rateLimits/read request
        write_step(success_rate_limits_frame(2)),   // rateLimits response
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
    assert!(
        result.is_err(),
        "no usable windows must be an error, not an empty snapshot"
    );
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
    assert!(
        rendered.contains("-32001"),
        "the safe error code should survive: {rendered}"
    );
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
    assert!(
        rendered.contains("-32002"),
        "the safe error code should survive: {rendered}"
    );
    assert!(
        !rendered.contains("hunter2-super-secret"),
        "the provider-controlled error message must never leak into our error text: {rendered}"
    );
}

/// A `rateLimitsByLimitId`-shaped payload usable both as a full read's
/// `result` and (per plan.md §7.2, which documents `account/rateLimits/read`
/// and `account/rateLimits/updated` on the same protocol) as a push
/// notification's `params`.
fn rate_limits_payload(used_percent: f64) -> Value {
    json!({
        "rateLimitsByLimitId": {
            "codex": {
                "primary": {"usedPercent": used_percent, "windowDurationMins": 300, "resetsAt": 2_000_000_000_i64}
            }
        }
    })
}

#[test]
fn a_push_notification_updates_the_cache_and_the_next_poll_avoids_a_full_read() {
    // One handshake and one account/rateLimits/read round trip, followed by
    // an unprompted account/rateLimits/updated notification -- with no
    // further read/write steps scripted at all. The notification's value
    // (77%) is deliberately distinct from round 1's (42%): a correct
    // implementation answers the second poll from the notification (77%)
    // without any further round trip, while a wrong implementation that
    // still issues a second full read on this same session would find no
    // scripted response and fail, and one that (incorrectly) respawned a
    // fresh session instead would get that fresh session's own honest first
    // read -- 42% again, replayed from the top of this same script -- never
    // 77%. Either wrong behavior is distinguishable from the correct one.
    let steps = vec![
        read_step(),
        write_step(json!({"id": 1, "result": {}})),
        read_step(),
        read_step(),
        write_step(json!({"id": 2, "result": rate_limits_payload(42.0)})),
        write_step(json!({
            "method": "account/rateLimits/updated",
            "params": rate_limits_payload(77.0)
        })),
    ];
    let (source, script) = scripted_source(&steps, 5);

    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    shutdown_codex_sessions();
    std::env::set_var("FAKE_CODEX_SCRIPT", &script);

    let first = collect(&source).expect("first poll should succeed");
    assert_eq!(first.windows[0].used_fraction, 0.42);

    // Give the session's background reader thread a moment to receive and
    // cache the notification (written by the fake server immediately after
    // its round-1 response, with no client read to synchronize on) before
    // the next poll checks the cache.
    std::thread::sleep(Duration::from_millis(200));

    let second = collect(&source);
    std::env::remove_var("FAKE_CODEX_SCRIPT");
    shutdown_codex_sessions();

    let second = second.expect("second poll should succeed entirely from the cache");
    assert_eq!(
        second.windows[0].used_fraction, 0.77,
        "the second poll must reflect the pushed notification instead of issuing a fresh full \
         read -- on this session (no more scripted steps to answer one) or a wrongly-respawned \
         one (whose own honest first read would replay 42%, not this value)"
    );
}

#[test]
fn a_session_death_after_a_successful_poll_is_masked_by_the_cache_until_reconciliation() {
    // The session answers one round, then the app-server process exits --
    // simulating a Codex app-server that died between polls. Per plan.md
    // §7.2's periodic-reconciliation design, a session that already has a
    // cached result does not attempt any network round trip (and so cannot
    // notice the death) until CODEX_RECONCILE_INTERVAL elapses; the second
    // poll must therefore still succeed, serving the now-stale cached
    // result, rather than erroring or hanging.
    let dies_after_round_one = vec![
        read_step(),
        write_step(json!({"id": 1, "result": {}})),
        read_step(),
        read_step(),
        write_step(json!({"id": 2, "result": rate_limits_payload(20.0)})),
        json!({"action": "exit", "code": 0}),
    ];
    let (source, script) = scripted_source(&dies_after_round_one, 5);

    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    shutdown_codex_sessions();
    std::env::set_var("FAKE_CODEX_SCRIPT", &script);

    let first = collect(&source);
    assert_eq!(
        first.expect("first poll should succeed").windows[0].used_fraction,
        0.20
    );

    // Give the app-server's `exit` step a moment to actually terminate the
    // process, so this deterministically exercises "session already dead"
    // rather than racing process teardown.
    std::thread::sleep(Duration::from_millis(200));

    let second = collect(&source);
    std::env::remove_var("FAKE_CODEX_SCRIPT");
    shutdown_codex_sessions();

    let second = second.expect(
        "a poll within the reconcile window must be answered from the cache even if the \
         underlying session has since died, rather than erroring",
    );
    assert_eq!(
        second.windows[0].used_fraction, 0.20,
        "with no notification and no elapsed reconcile interval, the second poll must return \
         the same cached value as the first, not attempt (and fail) a fresh read against the \
         dead process"
    );
}

#[test]
fn an_oversized_stdout_frame_is_discarded_without_breaking_the_real_response() {
    // Comfortably larger than any reasonable per-frame bound (source.rs's
    // own MAX_CODEX_FRAME_BYTES is 1 MiB at the time of writing) -- this
    // test only needs "large enough that an unbounded reader would notice,
    // and a bounded one must discard", not to pin the exact constant.
    let huge_garbage_line = Value::String("g".repeat(2_000_000));
    let steps = vec![
        read_step(),
        write_step(json!({"id": 1, "result": {}})),
        read_step(),
        read_step(),
        write_step(huge_garbage_line),
        write_step(success_rate_limits_frame(2)),
    ];
    let (source, script) = scripted_source(&steps, 5);

    let snapshot = collect_with_script(&source, &script).expect(
        "an oversized frame ahead of the real response must be discarded, not hang, crash the \
         reader thread, or otherwise prevent the real response from being delivered",
    );
    assert_eq!(snapshot.windows.len(), 2);
}
