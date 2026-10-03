//! Contract tests for a slow, hanging, or unreachable SSH-style remote
//! target (plan.md §22.8/§22.10), driven entirely through the public API
//! (`subscription_governor::source::collect_resource`, the §22.5
//! resource-source collector a `hosts.<name>.resource_source` config uses)
//! against local `/bin/sh` stand-ins -- never a real network connection or
//! `ssh` binary, so these tests stay deterministic and network-free.
//!
//! `docs/notes/ssh-remote-command-policy.md` documents that `ssh` is "just
//! argv[0]": there is no SSH-specific code path anywhere in `subgov`, so a
//! local stand-in that reproduces the same failure *shapes* a real SSH
//! invocation produces -- an immediate failure (connection refused, or a
//! bounded `ConnectTimeout` expiring; `ssh`'s own conventional exit code for
//! either is 255), an indefinite hang (a stuck interactive prompt or a
//! black-holed network path), or ordinary latency that still succeeds --
//! exercises the identical code path a real `argv: [ssh, host, ...]` config
//! would run through (`src/source.rs`'s `read_generic_command_bytes`,
//! already unit-tested directly for its timeout/process-group-kill
//! mechanics in `src/source.rs` itself, built in subgov-911d2c11). What
//! these tests add on top of that mechanism-level coverage is the
//! *contract*: through the public API, with no special-casing anywhere, a
//! remote target's failure degrades exactly like a stale local source --
//! bounded, isolated to that host only, and never mistaken for confirmed
//! (but empty) data.

use std::time::{Duration, Instant};
use subscription_governor::config::SourceConfig;
use subscription_governor::source::collect_resource;

fn command(script: &str) -> SourceConfig {
    SourceConfig::Command {
        argv: vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
    }
}

/// A resource snapshot literal a well-behaved remote `resource-probe`
/// script would print (plan.md §22.4/§22.5).
const VALID_RESOURCE_JSON: &str = r#"{
    "observed_at": "2026-09-28T12:00:00Z",
    "fresh": true,
    "host_id": "lab",
    "cpu_available_fraction": 0.42,
    "mem_available_mb": 12288,
    "mem_total_mb": 65536
}"#;

#[cfg(unix)]
#[test]
fn an_unreachable_target_fails_fast_and_is_isolated_from_a_healthy_host() {
    // `ssh`'s own conventional exit code for a connection it could not
    // establish at all (refused, unresolvable, or a bounded ConnectTimeout
    // expiring) is 255 -- fast, not a hang.
    let unreachable = command("exit 255");
    let start = Instant::now();
    let result = collect_resource(&unreachable);
    assert!(
        result.is_err(),
        "an unreachable target must fail, not succeed"
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "a fast connection failure must not wait anywhere near the command timeout"
    );

    // A second, entirely independent host's collection must be completely
    // unaffected by the first one's failure -- the SSH policy note: "a
    // failure on this account/host is still isolated from every other
    // account and host" (plan.md §7.4).
    let healthy = command(&format!("echo '{VALID_RESOURCE_JSON}'"));
    let snapshot =
        collect_resource(&healthy).expect("an independent healthy host must still succeed");
    assert_eq!(snapshot.host_id, "lab");
    assert!(snapshot.fresh);
}

#[cfg(unix)]
#[test]
fn an_unreachable_target_never_returns_partial_or_degraded_data() {
    // plan.md §22.8: a stale/failed host is *frozen*, never treated as
    // confirmed-empty data that could be mistaken for real headroom (or its
    // absence). The only way to guarantee that structurally is that failure
    // is always `Err`, never an `Ok` snapshot standing in for "unknown."
    let unreachable = command("exit 255");
    assert!(collect_resource(&unreachable).is_err());
}

#[cfg(unix)]
#[test]
fn a_slow_but_reachable_target_still_succeeds_within_the_command_timeout() {
    // Ordinary latency (a slow SSH connection setup, a remote script that
    // takes a moment) must not be punished -- only a genuine hang is
    // bounded. 2s is comfortably inside the real, wired-up 15s command
    // timeout (plan.md §22.10: "size timeout_seconds generously enough to
    // cover a slow SSH connection attempt on top of the remote script's own
    // runtime").
    let slow = command(&format!("sleep 2; echo '{VALID_RESOURCE_JSON}'"));
    let start = Instant::now();
    let snapshot = collect_resource(&slow)
        .expect("a slow but eventually-responding target must still succeed");
    assert!(start.elapsed() >= Duration::from_secs(2));
    assert_eq!(snapshot.mem_total_mb, 65536);
}

#[cfg(unix)]
#[test]
fn a_hanging_target_is_bounded_by_the_real_command_timeout_not_left_to_hang_forever() {
    // Unlike the tests above (and src/source.rs's own unit tests, which
    // inject a short timeout to keep mechanism-level coverage fast), this
    // drives the actual public `collect_resource` entry point with no
    // timeout override available -- proving the real, wired-up 15s
    // production `COMMAND_TIMEOUT` (not just the parameterized helper it
    // wraps) genuinely bounds a hang reached through this exact call, the
    // one a configured `hosts.<name>.resource_source` uses. Deliberately
    // the one slow test in this suite: `sleep 30` would hang for 30s if the
    // timeout wiring ever regressed, so the upper bound below is a real
    // regression check, not a formality.
    let hanging = command("sleep 30");
    let start = Instant::now();
    let result = collect_resource(&hanging);
    let elapsed = start.elapsed();
    assert!(
        result.is_err(),
        "a hung remote target must never be reported as a successful snapshot"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "expected the ~15s command timeout to bound the hang, not the full 30s sleep; took {elapsed:?}"
    );
}
