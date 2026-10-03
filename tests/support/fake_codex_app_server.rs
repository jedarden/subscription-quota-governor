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
                let _ = input.read_line(&mut line);
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
        }
    }
}
