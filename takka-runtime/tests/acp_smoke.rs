//! T-07 smoke test: the `--acp` subprocess answers `initialize` and one
//! `session/new`, then exits 0 when stdin closes (the EOF teardown
//! cancels, drains, and bounded-joins every session engine before the
//! clean exit).
//!
//! The engine is pinned to a hermetic config via the TEST-ONLY
//! `TAKKA_TEST_ACP_CONFIG` env var (see `acp::serve_with_config`) — the
//! ambient `~/.config/takka/config.json` is never read and the provider
//! is pinned to the discard port, so no real provider is ever contacted.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Every poll in this test is bounded by this deadline so a regression
/// can never hang the suite.
const POLL_TIMEOUT: Duration = Duration::from_secs(10);

/// Hermetic provider config pinned to the discard port (`127.0.0.1:9`),
/// same shape as the daemon tests' fixture.
fn hermetic_config_path() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "takka-acp-smoke-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::write(
        &path,
        r#"{
  "providers": [
    {
      "name": "hermetic-test",
      "type": "openai-compatible",
      "api_base": "http://127.0.0.1:9",
      "api_key": "unused",
      "models": [
        { "model": "hermetic-model", "temperature": 0.0, "max_tokens": 64 }
      ]
    }
  ]
}"#,
    )
    .expect("write hermetic acp config");
    path
}

#[test]
fn acp_subprocess_answers_initialize_and_session_new_then_exits_zero_on_eof() {
    let config_path = hermetic_config_path();
    // The adapter is single-cwd (D11): the child inherits this test's
    // cwd, so `session/new` must carry exactly that directory.
    let cwd = std::env::current_dir().expect("process cwd");
    let mut child = Command::new(env!("CARGO_BIN_EXE_takka-runtime"))
        .arg("--acp")
        .arg("--acp-max-sessions")
        .arg("4")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("TAKKA_TEST_ACP_CONFIG", &config_path)
        .spawn()
        .expect("spawn the acp subprocess");

    let mut stdin = child.stdin.take().expect("child stdin");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1,"clientCapabilities":{{}},"clientInfo":{{"name":"smoke","version":"0.0.0"}}}}}}"#
    )
    .expect("write initialize");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}","mcpServers":[]}}}}"#,
        cwd.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
    .expect("write session/new");
    stdin.flush().expect("flush stdin");
    drop(stdin); // EOF — the teardown path under test.

    // Drain stdout on its own thread (the child writes only the two
    // responses, but never block the bounded exit poll on pipe capacity).
    let stdout = child.stdout.take().expect("child stdout");
    let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let lines_for_reader = lines.clone();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            lines_for_reader.lock().expect("lines lock").push(line);
        }
    });

    // Bounded wait: EOF teardown must join the session engine and exit 0.
    let deadline = Instant::now() + POLL_TIMEOUT;
    let status = loop {
        match child.try_wait().expect("child status") {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("acp subprocess did not exit within 10s of EOF");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    reader.join().expect("stdout reader thread");
    let _ = std::fs::remove_file(&config_path);

    assert_eq!(status.code(), Some(0), "clean EOF exit");

    let frames: Vec<serde_json::Value> = lines
        .lock()
        .expect("lines lock")
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let init = frames
        .iter()
        .find(|frame| frame["id"] == serde_json::json!(1))
        .expect("initialize must be answered");
    assert_eq!(
        init["result"]["protocolVersion"],
        serde_json::json!(1),
        "v1 negotiation over the wire: {init}"
    );
    let new_session = frames
        .iter()
        .find(|frame| frame["id"] == serde_json::json!(2))
        .expect("session/new must be answered");
    let session_id = new_session["result"]["sessionId"]
        .as_str()
        .expect("sessionId in result");
    assert!(
        session_id.starts_with("sess-"),
        "hermetic engine booted: {new_session}"
    );
    assert!(
        new_session.get("error").is_none() || new_session["error"].is_null(),
        "session/new must not fail against the hermetic config: {new_session}"
    );
}
