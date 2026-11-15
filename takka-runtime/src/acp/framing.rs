use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use agent_client_protocol_schema::v1 as acp;
use serde_json::Value;

use super::*;

#[cfg(test)]
use std::sync::mpsc;
#[cfg(test)]
use takka_core::runtime::protocol::{ProtocolEvent, ProtocolRequest};

/// Method names we accept on the agent side of the v1 dialect
/// (mirrors `acp::AGENT_METHOD_NAMES`, kept as consts for framing tests).
pub(crate) const INITIALIZE: &str = "initialize";
pub(crate) const SESSION_NEW: &str = "session/new";
pub(crate) const SESSION_PROMPT: &str = "session/prompt";
pub(crate) const SESSION_CANCEL: &str = "session/cancel";
pub(crate) const SESSION_SET_MODE: &str = "session/set_mode";
pub(crate) const SESSION_LOAD: &str = "session/load";
/// v1 resume: the agent kept the session live (or can rebuild it from its
/// own store) and the client re-attaches without a history replay.
pub(crate) const SESSION_RESUME: &str = "session/resume";
/// v1 config-option switch (the `model` / `mode` selects advertised on
/// `session/new.configOptions`).
pub(crate) const SESSION_SET_CONFIG_OPTION: &str = "session/set_config_option";
pub(crate) const SESSION_UPDATE: &str = "session/update";
/// Outbound agent → client permission request (T-05/T-06, D8): the turn
/// thread asks, the client answers with a result frame on the same id.
pub(crate) const SESSION_REQUEST_PERMISSION: &str = "session/request_permission";

// ---------------------------------------------------------------------------
// Output sink: where framed JSON goes (stdout in production, memory in
// tests). The framing loop, the session router, and the prompt-turn
// threads all share one sink.
// ---------------------------------------------------------------------------

/// One framed JSON-RPC message destination. Implementations must be safe
/// to call from any thread (responses are written by the framing loop and
/// concurrently by prompt-turn threads).
pub trait AcpSink: Send + Sync {
    /// One JSON frame per line — never embedded newlines.
    fn send_line(&self, frame: &Value);
}

/// Everything that goes to stdout. Send failures mean the client is gone;
/// they are logged and swallowed — the framing loop notices the EOF on its
/// next read and exits cleanly.
pub(crate) struct Writer {
    pub(crate) out: Mutex<std::io::BufWriter<std::io::Stdout>>,
}

impl Writer {
    pub(crate) fn stdout() -> Self {
        Self {
            out: Mutex::new(std::io::BufWriter::new(std::io::stdout())),
        }
    }
}

impl AcpSink for Writer {
    fn send_line(&self, frame: &Value) {
        let mut out = self.out.lock().expect("writer lock");
        if let Err(error) = write_frame_line(&mut *out, frame) {
            tracing::warn!(%error, "acp: failed to write a frame to stdout");
        }
    }
}

/// One JSON object + newline, flushed (same framing as the takka
/// protocol stdio transport).
pub(crate) fn write_frame_line<W: Write>(out: &mut W, frame: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *out, frame)?;
    out.write_all(b"\n")?;
    out.flush()
}

// ---------------------------------------------------------------------------
// T-03: entry points — stdio in production, injected I/O in tests.
// ---------------------------------------------------------------------------

/// Serve the ACP connection on stdin/stdout until EOF.
pub fn serve() -> std::io::Result<()> {
    serve_with_config(AcpConfig::default())
}

/// Serve the ACP connection on stdin/stdout with explicit configuration.
/// `takka-runtime --acp [--acp-max-sessions N]` wires its parsed flag(s)
/// through this entry.
///
/// The `TAKKA_TEST_ACP_CONFIG` environment variable is a TEST-ONLY pin
/// (never set in production): when present it replaces the config path
/// handed to every session engine, so subprocess tests can point the
/// adapter at a hermetic config file instead of the user's ambient
/// `~/.config/takka/config.json`. Setting it outside tests would
/// silently override the user's real config.
pub fn serve_with_config(mut config: AcpConfig) -> std::io::Result<()> {
    if let Ok(path) = std::env::var("TAKKA_TEST_ACP_CONFIG") {
        config.config_path = Some(path);
    }
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let sink: Arc<dyn AcpSink> = Arc::new(Writer::stdout());
    serve_over(&mut reader, sink, config)
}

/// Framing loop over an injected reader + sink — the testable seam
/// (no stdio). Reads newline-delimited JSON-RPC frames until EOF; EOF
/// runs the T-07 teardown (cancel, deny pending gates, drain, bounded
/// join) before the clean `Ok(())`.
pub fn serve_over(
    reader: &mut dyn BufRead,
    sink: Arc<dyn AcpSink>,
    config: AcpConfig,
) -> std::io::Result<()> {
    let service = AcpService::new(config, sink);
    framing_loop(&service, reader)
}

/// The frame loop shared by the production and `#[cfg(test)]` entries:
/// read → route → answer, until EOF settles the service.
pub(crate) fn framing_loop(service: &AcpService, reader: &mut dyn BufRead) -> std::io::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            // EOF: the Client is gone — cancel everything, drain the
            // registry, and bounded-join the session threads before the
            // same clean exit as the takka-protocol stdio path (T-07).
            service.shutdown();
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        let frame: Value = match serde_json::from_str(line.trim()) {
            Ok(frame) => frame,
            Err(_) => {
                // Malformed JSON: a parse error with a null id per JSON-RPC.
                service.send_raw(&serde_json::json!(
                    { "jsonrpc": "2.0", "id": Value::Null, "error": acp::Error::parse_error() }
                ));
                continue;
            }
        };
        service.handle_frame(&frame);
    }
}

/// Scripted fake engine body — the T-05/T-07 test seam. It consumes the
/// session's request channel and produces its event channel exactly like
/// the real engine thread would, but with test-controlled behavior, so
/// in-flight turns (and later: permission gates) can be staged
/// deterministically through the public framing path. Never compiled
/// into the production binary.
#[cfg(test)]
pub(crate) type FakeEngine =
    Arc<dyn Fn(mpsc::Receiver<ProtocolRequest>, mpsc::Sender<ProtocolEvent>) + Send + Sync>;

/// Test-only entry (T-05/T-07 seam): [`serve_over`] with scripted fake
/// engines and a probe that counts engine threads reaped by the EOF
/// teardown's bounded join — the observable "all engines joined" signal.
#[cfg(test)]
pub(crate) fn serve_over_test(
    reader: &mut dyn BufRead,
    sink: Arc<dyn AcpSink>,
    config: AcpConfig,
    fake_engine: FakeEngine,
    joined_probe: Arc<std::sync::atomic::AtomicUsize>,
) -> std::io::Result<()> {
    let mut service = AcpService::new(config, sink);
    service.fake_engine = Some(fake_engine);
    service.joined_probe = Some(joined_probe);
    framing_loop(&service, reader)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::test_support::*;

    /// JSON-RPC contract: client response frames (result/error, no
    /// method) are never answered — whether they answer a known waiter
    /// (handled silently) or match nothing at all. The connection stays
    /// alive and serving afterwards.
    #[test]
    fn client_result_frames_never_get_replies() {
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": "acp-perm-999",
            "result": { "outcome": { "outcome": "cancelled" } }
        }));
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 77,
            "error": { "code": -32603, "message": "boom" }
        }));

        // Still serving: a normal request round-trips (frames are
        // processed in order, so both strays were routed by now).
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 42, "method": INITIALIZE,
            "params": { "protocolVersion": 1 }
        }));
        let response = acp.response(42);
        assert_eq!(response["result"]["protocolVersion"], serde_json::json!(1));

        let lines = acp.sink.lines.lock().expect("sink lock");
        assert!(
            !lines
                .iter()
                .any(|frame| frame.get("id") == Some(&serde_json::json!("acp-perm-999"))),
            "a stray result frame must never be answered"
        );
        assert!(
            !lines
                .iter()
                .any(|frame| frame.get("id") == Some(&serde_json::json!(77))),
            "a stray error frame must never be answered"
        );
        drop(lines);
        acp.close_and_join();
    }

    /// T-00 contract kept in the current shape: a non-JSON line yields
    /// exactly one parse-error frame (null id, -32700) and the connection
    /// stays alive — the next valid request is answered normally.
    #[test]
    fn serve_over_malformed_line_stays_alive() {
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        acp.send_line("this is not json");
        let parse_error = acp.sink.wait_until(
            |frame| {
                frame.get("id") == Some(&Value::Null)
                    && frame["error"]["code"] == serde_json::json!(-32700)
            },
            "the parse error frame",
        );
        assert_eq!(parse_error["jsonrpc"], serde_json::json!("2.0"));

        // Still serving: initialize round-trips after the garbage line.
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 42, "method": INITIALIZE,
            "params": { "protocolVersion": 1 }
        }));
        let response = acp.response(42);
        assert_eq!(response["result"]["protocolVersion"], serde_json::json!(1));
        acp.close_and_join();
    }
}
