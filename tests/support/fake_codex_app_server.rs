//! Scripted stdio JSON-RPC fake standing in for `codex app-server --listen
//! stdio://` (plan.md §7.2). Never built into the shipped `subgov` binary --
//! it exists only so `tests/codex_app_server_contract.rs` can drive
//! `source::collect_codex`'s real spawn/handshake/timeout logic against a
//! deterministic child process instead of a real Codex installation.
//!
//! The script is a JSON array of steps, read from the file named by the
//! `FAKE_CODEX_SCRIPT` environment variable:
//!
//! - `{"action": "read"}` -- consume the next line the client writes to our
//!   stdin (used only to sequence a step relative to what the client has
//!   sent so far; its content is never inspected).
//! - `{"action": "write", "frame": <json>}` -- write `<json>` as one line to
//!   stdout and flush.
//! - `{"action": "sleep_ms", "value": <u64>}` -- block, to simulate a slow
//!   or hung app-server.
//! - `{"action": "exit", "code": <i32>}` -- exit immediately, to simulate
//!   the app-server process dying mid-exchange.
//! - `{"action": "audit_until_eof"}` -- keep reading requests after the
//!   scripted response, so a contract test can inspect every method sent
//!   before the client closes the session.
//!
//! When `FAKE_CODEX_REQUEST_LOG` is set, every request read from stdin is
//! appended to that file as a method name. This lets contract tests enforce
//! the ADR-0001 read-only reset-credit boundary against actual JSON-RPC IO.
//!
//! Steps run in order; reaching the end of the script exits cleanly (code
//! 0). This process never validates protocol correctness on the client's
//! side -- it is a fake for the *server*, driven entirely by its script, so
//! every scenario (interleaved notifications, sparse windows, protocol
//! errors, timeouts, a mid-handshake exit) is just a different script.

use serde::Deserialize;
use serde_json::Value;
use std::io::{self, BufRead, Write};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Step {
    Read,
    Write { frame: Value },
    SleepMs { value: u64 },
    Exit { code: i32 },
    AuditUntilEof,
}

fn record_method(line: &str) {
    let method = serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|frame| frame.get("method")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "<missing method>".to_owned());
    assert_ne!(
        method, "account/rateLimitResetCredit/consume",
        "the governor must never consume a reset credit"
    );
    let Ok(path) = std::env::var("FAKE_CODEX_REQUEST_LOG") else {
        return;
    };
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("failed to open request log");
    writeln!(log, "{method}").expect("failed to record request method");
}

fn main() {
    let script_path =
        std::env::var("FAKE_CODEX_SCRIPT").expect("FAKE_CODEX_SCRIPT must name a script file");
    let script_bytes = std::fs::read(&script_path).expect("failed to read FAKE_CODEX_SCRIPT file");
    let steps: Vec<Step> =
        serde_json::from_slice(&script_bytes).expect("FAKE_CODEX_SCRIPT is not valid JSON");

    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut line = String::new();

    for step in steps {
        match step {
            Step::Read => {
                line.clear();
                // A zero-byte read means the client closed stdin; there is
                // nothing left to consume, so just move on to the next step.
                if input.read_line(&mut line).unwrap_or(0) > 0 {
                    record_method(&line);
                }
            }
            Step::Write { frame } => {
                writeln!(output, "{frame}").expect("failed to write to stdout");
                output.flush().expect("failed to flush stdout");
            }
            Step::SleepMs { value } => {
                std::thread::sleep(std::time::Duration::from_millis(value));
            }
            Step::Exit { code } => {
                std::process::exit(code);
            }
            Step::AuditUntilEof => loop {
                line.clear();
                if input.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                record_method(&line);
            },
        }
    }
}
