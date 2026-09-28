//! Contract tests for the Codex app-server source (plan.md §7.2), driven
//! against the scripted fake in `tests/support/fake_codex_app_server.rs`
//! instead of a real Codex installation. Exercises the handshake,
//! interleaved notifications, timeout, child-exit, sparse-window,
//! protocol-error, supervised-session-reuse/respawn, and oversized-frame
//! cases named in that section's build requirements. The bounded-frame
//! reader's own edge cases (multi-chunk discard, resync after an oversized
//! line, EOF mid-line) are unit-tested directly in `src/source.rs`; the
//! test here proves the end-to-end wiring against a real child process.
//!
//! Every test goes through the same public entry point production code
//! uses (`subscription_governor::source::collect`), so this is a true
//! end-to-end contract test of `collect_codex`/`with_codex_session`, not a
//! unit test of an internal helper.
//!
//! Codex sessions are supervised and kept alive across `collect()` calls
//! (a process-wide cache keyed by executable path), so most tests here call
//! `shutdown_codex_sessions()` first to guarantee a fresh spawn against
//! their own script rather than reusing another test's leftover session.
//! The reuse/respawn tests deliberately skip that reset, since proving
//! session persistence across calls is their entire point.

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
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    shutdown_codex_sessions();
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

fn single_rate_limits_frame(id: i64, used_percent: f64) -> Value {
    json!({
        "id": id,
        "result": {
            "rateLimitsByLimitId": {
                "codex": {
                    "primary": {"usedPercent": used_percent, "windowDurationMins": 300, "resetsAt": 2_000_000_000_i64}
                }
            }
        }
    })
}

/// Runs `collect(&source)` `polls` times in a row under one `FAKE_CODEX_SCRIPT`
/// value, holding [`ENV_LOCK`] for the whole sequence (not just one call) so
/// no other test's `shutdown_codex_sessions()` can slip in between polls and
/// evict the session this test is trying to prove gets reused. Starts from
/// (and leaves behind) a clean session cache.
fn collect_multiple_times_holding_session(
    source: &SourceConfig,
    script_path: &std::path::Path,
    polls: usize,
) -> Vec<anyhow::Result<subscription_governor::model::QuotaSnapshot>> {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    shutdown_codex_sessions();
    std::env::set_var("FAKE_CODEX_SCRIPT", script_path);
    let results = (0..polls).map(|_| collect(source)).collect();
    std::env::remove_var("FAKE_CODEX_SCRIPT");
    shutdown_codex_sessions();
    results
}

#[test]
fn a_second_poll_reuses_the_same_session_instead_of_respawning() {
    // Exactly one initialize/initialized handshake, followed by two
    // account/rateLimits/read round trips with distinct responses and
    // continuing JSON-RPC ids (2, then 3). A client that (incorrectly)
    // spawned a fresh session for the second poll would either receive
    // round 1's data again (a fresh session replays the same script from
    // its own handshake) or fail waiting on id 3, which its own fresh
    // handshake would never generate (a new session's first request is
    // id 2) -- either way distinguishable from true reuse.
    let steps = vec![
        read_step(),
        write_step(json!({"id": 1, "result": {}})),
        read_step(),
        read_step(),
        write_step(single_rate_limits_frame(2, 10.0)),
        read_step(),
        write_step(single_rate_limits_frame(3, 90.0)),
    ];
    let (source, script) = scripted_source(&steps, 5);

    let results = collect_multiple_times_holding_session(&source, &script, 2);
    assert_eq!(results.len(), 2);
    let first = results[0].as_ref().expect("first poll should succeed");
    assert_eq!(first.windows[0].used_fraction, 0.10);

    let second = results[1].as_ref().expect(
        "second poll should reuse the live session instead of failing while waiting on a \
         request id a fresh session's own handshake would never generate",
    );
    assert_eq!(
        second.windows[0].used_fraction, 0.90,
        "the second poll must receive round 2's distinct data, proving the session (and its \
         continuing JSON-RPC id sequence) was reused rather than a fresh session replaying the \
         script from its own handshake"
    );
}

#[test]
fn a_dead_session_is_respawned_transparently_for_the_next_poll() {
    // The first session answers one round, then the app-server process
    // exits -- simulating a Codex app-server that died between polls.
    let dies_after_round_one = vec![
        read_step(),
        write_step(json!({"id": 1, "result": {}})),
        read_step(),
        read_step(),
        write_step(single_rate_limits_frame(2, 20.0)),
        json!({"action": "exit", "code": 0}),
    ];
    // The respawned session's own fresh handshake and first round.
    let respawned_session = vec![
        read_step(),
        write_step(json!({"id": 1, "result": {}})),
        read_step(),
        read_step(),
        write_step(single_rate_limits_frame(2, 55.0)),
    ];

    let source = SourceConfig::CodexAppServer {
        executable: fake_codex_executable(),
        timeout_seconds: 5,
    };
    let mut first_script = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(
        &mut first_script,
        &serde_json::to_vec(&dies_after_round_one).unwrap(),
    )
    .unwrap();
    let first_script_path = first_script.into_temp_path();

    let mut second_script = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(
        &mut second_script,
        &serde_json::to_vec(&respawned_session).unwrap(),
    )
    .unwrap();
    let second_script_path = second_script.into_temp_path();

    let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    shutdown_codex_sessions();

    std::env::set_var("FAKE_CODEX_SCRIPT", &first_script_path);
    let first = collect(&source);
    assert_eq!(
        first.expect("first poll should succeed").windows[0].used_fraction,
        0.20
    );

    // Give the app-server's `exit` step a moment to actually terminate the
    // process before the next poll probes the (now-dead) session, so the
    // test deterministically exercises the failure path instead of racing
    // the timeout window.
    std::thread::sleep(Duration::from_millis(200));

    std::env::set_var("FAKE_CODEX_SCRIPT", &second_script_path);
    let second = collect(&source);
    std::env::remove_var("FAKE_CODEX_SCRIPT");
    shutdown_codex_sessions();

    let second = second.expect(
        "a dead session must be transparently respawned on the next poll, not surfaced as a \
         permanent failure",
    );
    assert_eq!(
        second.windows[0].used_fraction, 0.55,
        "the respawned session must run its own fresh handshake against the new script, not \
         reuse stale state from the dead session"
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
