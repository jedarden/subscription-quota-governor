//! Contract tests for the generic `command`/`normalized_file`/
//! `normalized_http` sources (plan.md §7.4), NOT a redo of the
//! Anthropic-specific (`tests/anthropic_usage_contract.rs`), Codex-specific
//! (`tests/codex_app_server_contract.rs`), or Z.AI-content-specific
//! (`tests/zai_collector_contract.rs`) suites -- this one exercises the
//! transport-independent behavior every generic source must share:
//! delivering a valid snapshot, rejecting malformed JSON, rejecting a
//! `validate_snapshot` violation, and rejecting an oversized payload,
//! identically regardless of which of the three transports carried it.
//!
//! [`collect_via_transport`] is the shared fixture harness: it delivers the
//! same raw byte payload over the real file, command (`/bin/cat`, never a
//! shell interpreting the payload itself), or HTTP transport and returns
//! whatever `collect()` returns, so every contract test below is written
//! once and run against all three transports via [`ALL_TRANSPORTS`] instead
//! of being triplicated per transport.

use serde_json::Value;
use std::io::{Read, Write};
use subscription_governor::config::SourceConfig;
use subscription_governor::model::QuotaSnapshot;
use subscription_governor::source::collect;

#[derive(Clone, Copy, Debug)]
enum Transport {
    File,
    Command,
    Http,
}

const ALL_TRANSPORTS: [Transport; 3] = [Transport::File, Transport::Command, Transport::Http];

/// Delivers `bytes` to `collect()` over `transport`'s real implementation
/// (a real file read, a real spawned process, or a real HTTP round trip --
/// never a mock of `collect` itself), and returns exactly what `collect()`
/// returns. `bytes` reaches every transport unmodified and unescaped: the
/// file and HTTP paths write/serve it verbatim, and the command path writes
/// it to a temp file and reads it back with `/bin/cat <path>` rather than
/// embedding it in a shell command line, so arbitrary content (oversized,
/// non-UTF8, or containing shell metacharacters) behaves identically across
/// all three without transport-specific escaping bugs skewing the results.
fn collect_via_transport(transport: Transport, bytes: &[u8]) -> anyhow::Result<QuotaSnapshot> {
    match transport {
        Transport::File => collect_via_file(bytes),
        Transport::Command => collect_via_command(bytes),
        Transport::Http => collect_via_http(bytes),
    }
}

fn collect_via_file(bytes: &[u8]) -> anyhow::Result<QuotaSnapshot> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.json");
    std::fs::write(&path, bytes).unwrap();
    collect(&SourceConfig::NormalizedFile { path })
}

#[cfg(unix)]
fn collect_via_command(bytes: &[u8]) -> anyhow::Result<QuotaSnapshot> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("payload.json");
    std::fs::write(&path, bytes).unwrap();
    // `/bin/cat` is not guaranteed to exist as a fixed path (e.g. NixOS);
    // `/bin/sh` is the one portable path this codebase's other
    // command-source tests already rely on, so resolve `cat` via its PATH
    // instead of hardcoding a coreutils location.
    let source = SourceConfig::Command {
        argv: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("cat {}", path.display()),
        ],
    };
    collect(&source)
}

fn collect_via_http(bytes: &[u8]) -> anyhow::Result<QuotaSnapshot> {
    let (url, handle) = spawn_http_server(bytes.to_vec());
    let source = SourceConfig::NormalizedHttp {
        url,
        timeout_seconds: 5,
    };
    let result = collect(&source);
    handle.join().unwrap();
    result
}

/// Serves `body` once over raw HTTP/1.1 on an ephemeral loopback port, with
/// no request-size mocking library required. A local, external-crate copy
/// of the same minimal server `src/source.rs`'s own private unit tests use
/// -- that one is not visible outside the crate, so this harness needs its
/// own.
fn spawn_http_server(body: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    (format!("http://127.0.0.1:{port}/"), handle)
}

fn valid_snapshot() -> Vec<u8> {
    br#"{"observed_at":"2026-09-28T12:00:00Z","fresh":true,"windows":[{"id":"weekly","used_fraction":0.42,"resets_at":"2026-09-30T00:00:00Z","duration_minutes":10080}]}"#.to_vec()
}

#[cfg(unix)]
#[test]
fn a_valid_snapshot_collects_successfully_via_every_transport() {
    let bytes = valid_snapshot();
    for transport in ALL_TRANSPORTS {
        let snapshot = collect_via_transport(transport, &bytes)
            .unwrap_or_else(|error| panic!("{transport:?} should collect a valid snapshot: {error:#}"));
        assert_eq!(snapshot.windows.len(), 1, "{transport:?}");
        assert_eq!(snapshot.windows[0].id, "weekly", "{transport:?}");
        assert_eq!(snapshot.windows[0].used_fraction, 0.42, "{transport:?}");
        assert!(snapshot.fresh, "{transport:?}");
    }
}

#[cfg(unix)]
#[test]
fn syntactically_invalid_json_is_rejected_by_every_transport() {
    let bytes = b"this is not json at all {{{".to_vec();
    for transport in ALL_TRANSPORTS {
        assert!(
            collect_via_transport(transport, &bytes).is_err(),
            "{transport:?} should reject syntactically invalid JSON"
        );
    }
}

#[cfg(unix)]
#[test]
fn an_out_of_range_used_fraction_is_rejected_by_every_transport() {
    let bytes = br#"{"observed_at":"2026-09-28T12:00:00Z","windows":[{"id":"weekly","used_fraction":1.5,"resets_at":"2026-09-30T00:00:00Z"}]}"#.to_vec();
    for transport in ALL_TRANSPORTS {
        assert!(
            collect_via_transport(transport, &bytes).is_err(),
            "{transport:?} should apply the same used_fraction validation as any other source"
        );
    }
}

#[cfg(unix)]
#[test]
fn an_empty_windows_array_is_rejected_by_every_transport() {
    let bytes = br#"{"observed_at":"2026-09-28T12:00:00Z","windows":[]}"#.to_vec();
    for transport in ALL_TRANSPORTS {
        assert!(
            collect_via_transport(transport, &bytes).is_err(),
            "{transport:?} should reject an empty windows array (not unlimited capacity, \
             plan.md §6.1)"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_missing_required_field_is_rejected_by_every_transport() {
    let bytes = br#"{"observed_at":"2026-09-28T12:00:00Z","windows":[{"id":"weekly","resets_at":"2026-09-30T00:00:00Z"}]}"#.to_vec();
    for transport in ALL_TRANSPORTS {
        assert!(
            collect_via_transport(transport, &bytes).is_err(),
            "{transport:?} should reject a window missing used_fraction"
        );
    }
}

#[cfg(unix)]
#[test]
fn an_oversized_payload_is_rejected_by_every_transport() {
    // Comfortably larger than any reasonable shared size bound
    // (source.rs's own MAX_GENERIC_SOURCE_BYTES is 1 MiB at the time of
    // writing); this only needs to exceed it, not pin the exact constant.
    let bytes = vec![b'0'; 2_000_000];
    for transport in ALL_TRANSPORTS {
        assert!(
            collect_via_transport(transport, &bytes).is_err(),
            "{transport:?} should reject a payload over the shared size bound"
        );
    }
}

#[cfg(unix)]
#[test]
fn windows_survive_every_transport_byte_for_byte() {
    // A stronger parity check than the individual contract tests above:
    // the exact same input, collected through all three transports, must
    // produce field-for-field identical snapshots (aside from `observed_at`,
    // which each transport's own collector call independently timestamps --
    // this fixture pins it explicitly so it too must match).
    let bytes = valid_snapshot();
    let results: Vec<QuotaSnapshot> = ALL_TRANSPORTS
        .iter()
        .map(|&transport| collect_via_transport(transport, &bytes).unwrap())
        .collect();
    assert_eq!(results[0], results[1], "File vs Command parity");
    assert_eq!(results[1], results[2], "Command vs Http parity");
}

#[test]
fn every_transport_helper_produces_a_value_serde_json_can_round_trip() {
    // Sanity check on the harness itself: the raw fixture bytes this suite
    // hands to every transport are valid QuotaSnapshot JSON, not an
    // accidentally-malformed literal that would make every "valid" test
    // above pass for the wrong reason.
    let value: Value = serde_json::from_slice(&valid_snapshot()).expect("fixture must be valid JSON");
    let _: QuotaSnapshot =
        serde_json::from_value(value).expect("fixture must deserialize as QuotaSnapshot");
}
