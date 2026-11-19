use std::io::{BufRead, Read};
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use takka_core::runtime::protocol::{ProtocolEvent, ProtocolRequest, Request};

use super::*;

// ------------------------------------------------------------------
// T-03/T-04: session lifecycle + capacity — hermetic integration over
// `serve_over` (injected reader + in-memory sink; no stdio, no ambient
// config, no real provider). The pinned provider `http://127.0.0.1:9`
// refuses connections instantly, so turns fail fast.
// ------------------------------------------------------------------

/// Every poll in these tests is bounded by this deadline so a
/// regression can never hang the suite.
pub(crate) const POLL_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn deadline() -> Instant {
    Instant::now() + POLL_TIMEOUT
}

/// In-memory sink: records every frame the service writes.
#[derive(Default)]
pub(crate) struct VecSink {
    pub(crate) lines: Mutex<Vec<Value>>,
}

impl AcpSink for VecSink {
    fn send_line(&self, frame: &Value) {
        lock(&self.lines).push(frame.clone());
    }
}

impl VecSink {
    /// Newest-first lookup of a JSON-RPC response by request id.
    pub(crate) fn response_for(&self, id: i64) -> Option<Value> {
        let lines = lock(&self.lines);
        lines
            .iter()
            .rev()
            .find(|frame| frame.get("id") == Some(&serde_json::json!(id)))
            .cloned()
    }

    /// All `session/update` notifications seen so far.
    pub(crate) fn updates(&self) -> Vec<Value> {
        let lines = lock(&self.lines);
        lines
            .iter()
            .filter(|frame| frame.get("method").and_then(Value::as_str) == Some(SESSION_UPDATE))
            .cloned()
            .collect()
    }

    /// Bounded wait for the first frame matching `predicate`.
    pub(crate) fn wait_until(&self, predicate: impl Fn(&Value) -> bool, what: &str) -> Value {
        let deadline = deadline();
        loop {
            let hit = lock(&self.lines)
                .iter()
                .find(|frame| predicate(frame))
                .cloned();
            if let Some(frame) = hit {
                return frame;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Test reader fed line-by-line over a channel: `recv()` blocks (a
/// live pipe), dropping the sender closes it (EOF). Portable, std-only.
pub(crate) struct ChanReader {
    pub(crate) pending: Vec<u8>,
    pub(crate) pos: usize,
    pub(crate) rx: std::sync::mpsc::Receiver<String>,
    pub(crate) eof: bool,
}

impl ChanReader {
    pub(crate) fn new() -> (Sender<String>, Self) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            tx,
            Self {
                pending: Vec::new(),
                pos: 0,
                rx,
                eof: false,
            },
        )
    }
}

impl Read for ChanReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let filled = self.fill_buf()?;
        let n = filled.len().min(buf.len());
        buf[..n].copy_from_slice(&filled[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for ChanReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.pos < self.pending.len() {
            return Ok(&self.pending[self.pos..]);
        }
        if self.eof {
            return Ok(&[]);
        }
        match self.rx.recv() {
            Ok(line) => {
                self.pending = line.into_bytes();
                self.pos = 0;
                Ok(&self.pending[..])
            }
            Err(_) => {
                self.eof = true;
                Ok(&[])
            }
        }
    }

    fn consume(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.pending.len());
    }
}

/// One hermetic ACP connection under test: channel-fed reader, memory
/// sink, and the `serve_over` loop on its own thread.
pub(crate) struct TestAcp {
    pub(crate) tx: Option<Sender<String>>,
    pub(crate) sink: Arc<VecSink>,
    pub(crate) serve: std::thread::JoinHandle<std::io::Result<()>>,
}

impl TestAcp {
    pub(crate) fn spawn(max_sessions: usize) -> Self {
        let (tx, mut reader) = ChanReader::new();
        let sink = Arc::new(VecSink::default());
        let config = AcpConfig {
            max_sessions,
            config_path: Some(hermetic_config_path()),
        };
        let sink_for_serve = sink.clone();
        let serve = std::thread::Builder::new()
            .name("acp-serve-test".into())
            .spawn(move || serve_over(&mut reader, sink_for_serve, config))
            .expect("spawn the serve thread");
        Self {
            tx: Some(tx),
            sink,
            serve,
        }
    }

    /// T-07 variant: scripted fake engines + a probe counting the
    /// engine threads the EOF teardown reaped. Goes through the same
    /// public framing path via the `serve_over_test` seam.
    pub(crate) fn spawn_probed(max_sessions: usize) -> (Self, Arc<AtomicUsize>) {
        Self::spawn_with_fake(max_sessions, Arc::new(fake_engine_script))
    }

    /// T-05 generalization: the same probed seam with any scripted
    /// fake engine, so a test can stage its own turn behavior
    /// (cancel terminals, permission gates, recorded requests).
    pub(crate) fn spawn_with_fake(
        max_sessions: usize,
        fake_engine: FakeEngine,
    ) -> (Self, Arc<AtomicUsize>) {
        let (tx, mut reader) = ChanReader::new();
        let sink = Arc::new(VecSink::default());
        let config = AcpConfig {
            max_sessions,
            config_path: Some(hermetic_config_path()),
        };
        let probe = Arc::new(AtomicUsize::new(0));
        let sink_for_serve = sink.clone();
        let probe_for_serve = probe.clone();
        let serve = std::thread::Builder::new()
            .name("acp-serve-test".into())
            .spawn(move || {
                serve_over_test(
                    &mut reader,
                    sink_for_serve,
                    config,
                    fake_engine,
                    probe_for_serve,
                )
            })
            .expect("spawn the serve thread");
        (
            Self {
                tx: Some(tx),
                sink,
                serve,
            },
            probe,
        )
    }

    pub(crate) fn send(&self, frame: Value) {
        self.tx
            .as_ref()
            .expect("connection open")
            .send(format!("{frame}\n"))
            .expect("serve loop alive");
    }

    /// Feed one raw line — valid JSON or not (the parse-error helper).
    pub(crate) fn send_line(&self, line: &str) {
        self.tx
            .as_ref()
            .expect("connection open")
            .send(format!("{line}\n"))
            .expect("serve loop alive");
    }

    /// Wait (bounded) for the JSON-RPC response with this request id.
    pub(crate) fn response(&self, id: i64) -> Value {
        let deadline = deadline();
        loop {
            if let Some(frame) = self.sink.response_for(id) {
                return frame;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the response to request id {id}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Close stdin (EOF) and verify `serve_over` returns clean `Ok(())`
    /// within the poll bound — the T-03 EOF contract.
    pub(crate) fn close_and_join(mut self) {
        self.tx = None; // drop the sender → reader EOF
        let deadline = deadline();
        while !self.serve.is_finished() {
            assert!(
                Instant::now() < deadline,
                "serve_over did not return on EOF within 10s"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        self.serve
            .join()
            .expect("serve thread panicked")
            .expect("serve_over returned Ok on EOF");
    }
}

/// Hermetic provider config pinned to the discard port
/// (`127.0.0.1:9`): `Init` succeeds, a `Run` fails fast with a
/// connection error and never leaves the machine — no ambient
/// `~/.config/takka/config.json` is ever read (same shape as the
/// daemon tests' fixture).
pub(crate) fn hermetic_config_path() -> String {
    let path = std::env::temp_dir().join(format!(
        "takka-acp-test-{}-{}.json",
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
    path.to_string_lossy().into_owned()
}

/// Hermetic config with `agent.default_permission_mode` set (P-04):
/// same discard-port provider, sessions boot in `mode`.
pub(crate) fn hermetic_config_with_default_mode_path(mode: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "takka-acp-test-mode-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    let json = format!(
        r#"{{
  "providers": [
    {{
      "name": "hermetic-test",
      "type": "openai-compatible",
      "api_base": "http://127.0.0.1:9",
      "api_key": "unused",
      "models": [
        {{ "model": "hermetic-model", "temperature": 0.0, "max_tokens": 64 }}
      ]
    }}
  ],
  "agent": {{ "default_permission_mode": "{mode}" }}
}}"#
    );
    std::fs::write(&path, json).expect("write hermetic acp config with mode");
    path.to_string_lossy().into_owned()
}

impl TestAcp {
    /// Spawn with an explicit config path (hermetic variants for tests
    /// that need a non-default `agent` block).
    pub(crate) fn spawn_with_config(max_sessions: usize, config_path: String) -> Self {
        let (tx, mut reader) = ChanReader::new();
        let sink = Arc::new(VecSink::default());
        let config = AcpConfig {
            max_sessions,
            config_path: Some(config_path),
        };
        let sink_for_serve = sink.clone();
        let serve = std::thread::Builder::new()
            .name("acp-serve-test".into())
            .spawn(move || serve_over(&mut reader, sink_for_serve, config))
            .expect("spawn the serve thread");
        Self {
            tx: Some(tx),
            sink,
            serve,
        }
    }
}

pub(crate) fn new_session(acp: &TestAcp, id: i64) -> String {
    let cwd = std::env::current_dir()
        .expect("process cwd")
        .to_string_lossy()
        .into_owned();
    acp.send(serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": SESSION_NEW,
        "params": { "cwd": cwd, "mcpServers": [] }
    }));
    let response = acp.response(id);
    assert!(
        response["error"].is_null(),
        "session/new failed: {}",
        response["error"]
    );
    response["result"]["sessionId"]
        .as_str()
        .expect("sessionId in result")
        .to_owned()
}

pub(crate) fn send_prompt(acp: &TestAcp, id: i64, session: &str, prompt: Value) {
    acp.send(serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": SESSION_PROMPT,
        "params": { "sessionId": session, "prompt": prompt }
    }));
}

pub(crate) fn text_prompt(text: &str) -> Value {
    serde_json::json!([{ "type": "text", "text": text }])
}

// ------------------------------------------------------------------
// T-07: EOF teardown — cancel, drain, bounded join, clean exit.
// ------------------------------------------------------------------

/// Scripted fake engine (the T-05-brief seam, deterministic):
/// `Init` → `InitOk`; `Run` → one assistant delta, then park until
/// `Cancel` (→ terminal `Error{"cancelled"}` — the real engine's
/// cancel terminal) or channel close (→ exit — the real engine's EOF
/// exit path).
pub(crate) fn fake_engine_script(
    req_rx: std::sync::mpsc::Receiver<ProtocolRequest>,
    evt_tx: std::sync::mpsc::Sender<ProtocolEvent>,
) {
    fake_engine_with_gate(req_rx, evt_tx, false);
}

/// Same fake, but the run also emits a `ConfirmationRequest` (the
/// engine-side shape of a permission gate) before parking.
pub(crate) fn fake_confirmation_engine_script(
    req_rx: std::sync::mpsc::Receiver<ProtocolRequest>,
    evt_tx: std::sync::mpsc::Sender<ProtocolEvent>,
) {
    fake_engine_with_gate(req_rx, evt_tx, true);
}

/// Shared fake-engine body. `gate` stages a `ConfirmationRequest`
/// mid-run so the teardown can be observed from behind an engine
/// parked awaiting client word.
pub(crate) fn fake_engine_with_gate(
    req_rx: std::sync::mpsc::Receiver<ProtocolRequest>,
    evt_tx: std::sync::mpsc::Sender<ProtocolEvent>,
    gate: bool,
) {
    let emit = |event: takka_core::Event| {
        let _ = evt_tx.send(ProtocolEvent::new(None, event));
    };
    while let Ok(request) = req_rx.recv() {
        match request.request {
            Request::Init { .. } => emit(takka_core::Event::InitOk {
                protocol: "takka".to_owned(),
                version: 2,
            }),
            Request::Run { .. } => {
                emit(takka_core::Event::AssistantDelta {
                    content: "fake turn running".to_owned(),
                });
                if gate {
                    emit(takka_core::Event::ConfirmationRequest {
                        target: None,
                        action: "run".to_owned(),
                        description: "delete the build dir".to_owned(),
                        risk_level: "MEDIUM".to_owned(),
                    });
                }
                // Park until the teardown's cancel or the channel close.
                while let Ok(request) = req_rx.recv() {
                    if matches!(request.request, Request::Cancel) {
                        emit(takka_core::Event::Error {
                            message: "cancelled".to_owned(),
                        });
                        break;
                    }
                }
                // The outer recv() returns Err once the teardown drops
                // the senders — the thread exits like a real engine.
            }
            _ => {}
        }
    }
}

// ------------------------------------------------------------------
// D12: turn-limit stop reasons.
// ------------------------------------------------------------------

/// Fake engine whose turns end at a limit: it emits
/// `Event::TurnLimitExceeded` right before the terminator, exactly like
/// the real loop does after `forced finalization` / a truncated answer.
pub(crate) fn d12_fake_engine(kind: Option<takka_core::events::TurnLimitKind>) -> FakeEngine {
    Arc::new(
        move |req_rx: std::sync::mpsc::Receiver<ProtocolRequest>,
              evt_tx: std::sync::mpsc::Sender<ProtocolEvent>| {
            let emit = |event: takka_core::Event| {
                let _ = evt_tx.send(ProtocolEvent::new(None, event));
            };
            while let Ok(request) = req_rx.recv() {
                match request.request {
                    Request::Init { .. } => emit(takka_core::Event::InitOk {
                        protocol: "takka".to_owned(),
                        version: 2,
                    }),
                    Request::Run { .. } => {
                        emit(takka_core::Event::AssistantDelta {
                            content: "partial answer".to_owned(),
                        });
                        if let Some(kind) = kind {
                            emit(takka_core::Event::TurnLimitExceeded { kind });
                        }
                        emit(takka_core::Event::Done {
                            result: "partial answer".to_owned(),
                        });
                    }
                    _ => {}
                }
            }
        },
    )
}

// ------------------------------------------------------------------
// T-05: cancel → `cancelled` stop reason + the permission bridge.
// ------------------------------------------------------------------

/// The parameterized T-05 fake engine. The first `Run` emits an
/// assistant delta and — when `gate` — a `ConfirmationRequest`, then
/// parks inside the run until something frees it:
///
/// - `ConfirmResponse` is recorded into `seen`; with
///   `finish_on_confirm` the run then completes with `Done` (the
///   allow/reject flow tests); without it the engine keeps parking
///   (so the test can cancel next and observe the deny order);
/// - `Cancel` ends the run with the engine's cancel terminal
///   `Error{"cancelled"}` (mirroring `AgenticError::Cancelled`).
///
/// Every later `Run` completes immediately with `Done` — the
/// second prompt of the cancel-recovery test. Channel close exits
/// the engine like a real one (the teardown path).
pub(crate) fn t05_fake_engine(
    gate: bool,
    finish_on_confirm: bool,
    seen: std::sync::mpsc::Sender<Request>,
) -> FakeEngine {
    Arc::new(
        move |req_rx: std::sync::mpsc::Receiver<ProtocolRequest>,
              evt_tx: std::sync::mpsc::Sender<ProtocolEvent>| {
            let emit = |event: takka_core::Event| {
                let _ = evt_tx.send(ProtocolEvent::new(None, event));
            };
            let mut runs = 0usize;
            while let Ok(request) = req_rx.recv() {
                match request.request {
                    Request::Init { .. } => emit(takka_core::Event::InitOk {
                        protocol: "takka".to_owned(),
                        version: 2,
                    }),
                    Request::Run { task, attachments } => {
                        let _ = seen.send(Request::Run { task, attachments });
                        runs += 1;
                        emit(takka_core::Event::AssistantDelta {
                            content: "fake turn running".to_owned(),
                        });
                        if runs == 1 {
                            let mut confirm_pending = false;
                            if gate {
                                emit(takka_core::Event::ConfirmationRequest {
                                    target: None,
                                    action: "run".to_owned(),
                                    description: "delete the build dir".to_owned(),
                                    risk_level: "MEDIUM".to_owned(),
                                });
                                confirm_pending = true;
                            }
                            // Park inside the run until freed. Like the
                            // real engine: the cancel is a flag, and a
                            // pending confirmation still consumes its
                            // answer — whichever of the deny-unblocked
                            // confirm and the cancel request lands first,
                            // the answer is recorded and the run ends
                            // with the cancel terminal (or `Done` when
                            // `finish_on_confirm`).
                            let mut cancelled = false;
                            loop {
                                match req_rx.recv() {
                                    Ok(next) => match next.request {
                                        Request::ConfirmResponse {
                                            approved,
                                            request_id,
                                        } => {
                                            let _ = seen.send(Request::ConfirmResponse {
                                                request_id,
                                                approved,
                                            });
                                            confirm_pending = false;
                                            if finish_on_confirm {
                                                emit(takka_core::Event::Done {
                                                    result: "confirmed".to_owned(),
                                                });
                                                break;
                                            }
                                            if cancelled {
                                                emit(takka_core::Event::Error {
                                                    message: "cancelled".to_owned(),
                                                });
                                                break;
                                            }
                                        }
                                        Request::Cancel => {
                                            cancelled = true;
                                            if !confirm_pending {
                                                emit(takka_core::Event::Error {
                                                    message: "cancelled".to_owned(),
                                                });
                                                break;
                                            }
                                            // A confirmation is pending —
                                            // keep waiting for its answer.
                                        }
                                        _ => {}
                                    },
                                    Err(_) => return, // senders gone: teardown exit
                                }
                            }
                        } else {
                            emit(takka_core::Event::Done {
                                result: "later turn done".to_owned(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        },
    )
}

/// Bounded wait for the outbound `session/request_permission` frame
/// of the (single) permission gate under test.
pub(crate) fn wait_for_permission_request(acp: &TestAcp) -> Value {
    acp.sink.wait_until(
        |frame| frame.get("method").and_then(Value::as_str) == Some(SESSION_REQUEST_PERMISSION),
        "the session/request_permission request",
    )
}

/// Every non-`Init` request the fake engine received (Run,
/// ConfirmResponse, …), drained without blocking — the response the
/// test waits for is only written after the engine recorded the
/// request, so the record is complete when this runs.
pub(crate) fn seen_requests(receiver: &std::sync::mpsc::Receiver<Request>) -> Vec<Request> {
    receiver.try_iter().collect()
}

// ------------------------------------------------------------------
// H-02: poisoned-lock recovery. A panicking holder poisons a bookkeeping
// mutex; every later reader/writer must recover instead of cascading the
// panic into unrelated sessions.
// ------------------------------------------------------------------

/// Take `m`'s guard and panic while holding it, leaving `m` poisoned (the
/// poisoner's panic is caught so the test process stays clean). The guard
/// is dropped by the unwinding panic — that is what marks the mutex
/// poisoned.
pub(crate) fn poison<T>(m: &Mutex<T>) {
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = m.lock().expect("guard to poison");
        panic!("poison the mutex on purpose");
    }));
    assert!(
        poisoned.is_err(),
        "the poisoner must panic while holding the lock"
    );
    assert!(m.lock().is_err(), "the mutex should be poisoned");
}
