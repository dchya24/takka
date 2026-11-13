//! ACP (Agent Client Protocol) v1 agent adapter — Fase 4.
//!
//! Design: `docs/superpowers/specs/2026-11-10-takka-acp-agent-design.md`.
//! takka implements the **Agent** side of ACP: an ACP Client (Zed, or any other
//! ACP-speaking editor) boots `takka-runtime --acp` as a subprocess and drives
//! the connection over JSON-RPC 2.0, newline-delimited on stdin/stdout (see the
//! ACP v1 *Transports* page).
//!
//! Phase 1 (this module grows over stories T-00 → T-07):
//! - T-00: the `--acp` entry, framing loop, inbound routing, typed JSON-RPC
//!   error responses for unimplemented methods.
//! - T-01: the `initialize` responder — v1 negotiation plus the capability-set
//!   decisions D7/D9/D11/D12 of the spec.
//! - T-02: the engine-`Event` → ACP `SessionUpdate` translator — pure
//!   functions over engine events, no I/O, fully unit-tested.
//! - T-03: the session registry — `session/new` validates the single-cwd
//!   contract (D11), boots one `RuntimeEngine` per session over a
//!   `MemoryTransport` (the daemon hosting pattern, D6), advertises the
//!   takka permission modes as ACP session modes (D9); `session/prompt`
//!   maps content blocks to task + attachments and runs the turn on its
//!   own thread so the framing loop stays responsive to `session/cancel`
//!   (minimum wiring: flag + cooperative `Request::Cancel`; full
//!   `stopReason: cancelled` mapping is the T-05 lane).
//! - T-04: connection capacity — at most `AcpConfig::max_sessions` live
//!   sessions; the N+1-th `session/new` gets a typed `invalid_request`
//!   error (`session capacity reached`).
//! - T-05: mid-turn cancellation resolves the prompt with
//!   `stopReason: "cancelled"` (not an error) and the session stays
//!   usable; the permission bridge (T-06) is wired into the turn loop —
//!   a `ConfirmationRequest` becomes an outbound
//!   `session/request_permission` whose client answer (or a cancel /
//!   teardown deny) unblocks the engine via `Request::ConfirmResponse`.
//! - T-07: the EOF teardown (spec §5 step 6) — cancel every live session
//!   (flag + cooperative `Request::Cancel`), free pending permission
//!   waiters (the waiter map itself is the T-05 lane's deliverable),
//!   drain the registry, and bounded-join every engine/turn thread (the
//!   daemon `join_bounded` pattern) so the process exits 0 without
//!   stranding threads. Stragglers are detached with a warning.
//!
//! The engine is never modified: sessions each own a `RuntimeEngine` over
//! a `MemoryTransport`, exactly like the daemon host. Nothing in this
//! module writes anything to stdout except framed ACP messages — logs stay
//! on stderr.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_client_protocol_schema::{v1 as acp, ProtocolVersion};
use serde_json::Value;
use takka_core::attachments::{Attachment, AttachmentKind, AttachmentSource, ALLOWED_IMAGE_MIME};
use takka_core::runtime::engine::RuntimeEngine;
use takka_core::runtime::protocol::{
    ConnectionCapabilities, InitOverrides, ProtocolEvent, ProtocolRequest, Request,
};
use takka_core::runtime::transport::MemoryTransport;
use takka_core::PermissionMode;

use crate::acp_permission;

/// Method names we accept on the agent side of the v1 dialect
/// (mirrors `acp::AGENT_METHOD_NAMES`, kept as consts for framing tests).
const INITIALIZE: &str = "initialize";
const SESSION_NEW: &str = "session/new";
const SESSION_PROMPT: &str = "session/prompt";
const SESSION_CANCEL: &str = "session/cancel";
const SESSION_SET_MODE: &str = "session/set_mode";
const SESSION_LOAD: &str = "session/load";
/// v1 resume: the agent kept the session live (or can rebuild it from its
/// own store) and the client re-attaches without a history replay.
const SESSION_RESUME: &str = "session/resume";
const SESSION_UPDATE: &str = "session/update";
/// Outbound agent → client permission request (T-05/T-06, D8): the turn
/// thread asks, the client answers with a result frame on the same id.
const SESSION_REQUEST_PERMISSION: &str = "session/request_permission";

/// Default session capacity of one ACP connection (spec D6: bounded
/// engine-per-session; the `--acp-max-sessions` flag lands with the
/// integrator wiring `AcpConfig` in `takka-runtime/src/main.rs`).
pub const DEFAULT_MAX_SESSIONS: usize = 4;

/// Bound on the `InitOk` wait during `session/new` boot. Engine init is
/// local (config read, skill discovery, prompt assembly) — 10s is ample;
/// a hung boot must not wedge the framing loop.
const BOOT_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on waiting for the engine's `PermissionModeChanged` ack of a
/// `session/set_mode` (D9). The engine answers from its request loop, so a
/// timeout means it is wedged/dead — never a slow turn.
const MODE_SWITCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound on the EOF teardown join (T-07). Cancelling is cooperative and
/// an engine thread only exits after its active run winds down at the
/// next boundary, so the join wait must cover a realistic cancel-to-
/// terminal span; after the deadline, stragglers are left detached (the
/// daemon `join_bounded` pattern) and the process still exits.
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Configuration.
// ---------------------------------------------------------------------------

/// Adapter configuration. Production callers use [`AcpConfig::default`];
/// tests inject a hermetic config path via [`serve_over`].
pub struct AcpConfig {
    /// Maximum live sessions per connection (spec D6). The N+1-th
    /// `session/new` is rejected with `session capacity reached`.
    pub max_sessions: usize,
    /// Optional config file handed to each session engine via
    /// `InitOverrides.config_path`. `None` = engine loads the user's
    /// ambient config (production); tests pin a hermetic file here.
    pub config_path: Option<String>,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            max_sessions: DEFAULT_MAX_SESSIONS,
            config_path: None,
        }
    }
}

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
struct Writer {
    out: Mutex<std::io::BufWriter<std::io::Stdout>>,
}

impl Writer {
    fn stdout() -> Self {
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
fn write_frame_line<W: Write>(out: &mut W, frame: &Value) -> std::io::Result<()> {
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
fn framing_loop(service: &AcpService, reader: &mut dyn BufRead) -> std::io::Result<()> {
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
type FakeEngine =
    Arc<dyn Fn(mpsc::Receiver<ProtocolRequest>, mpsc::Sender<ProtocolEvent>) + Send + Sync>;

/// Test-only entry (T-05/T-07 seam): [`serve_over`] with scripted fake
/// engines and a probe that counts engine threads reaped by the EOF
/// teardown's bounded join — the observable "all engines joined" signal.
#[cfg(test)]
fn serve_over_test(
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

// ---------------------------------------------------------------------------
// T-05: the permission bridge's pending waiters — the shared state between
// the turn thread (asks + parks), the framing loop (routes the client's
// answer), and the cancel/teardown paths (deny).
// ---------------------------------------------------------------------------

/// One parked `session/request_permission` gate.
struct PermissionWaiter {
    /// Owning session — `session/cancel` denies only this session's gates.
    session_id: String,
    /// The `approved` bool back to the parked turn thread. Capacity 1:
    /// every waiter receives exactly one answer, so a send cannot block.
    answer_tx: mpsc::SyncSender<bool>,
}

/// Pending `session/request_permission` waiters keyed by the outbound
/// request id (`acp-perm-1`, `acp-perm-2`, …). Shared via `Arc` because
/// prompt-turn threads outlive the `AcpService` borrow.
#[derive(Default)]
struct PermissionWaiters {
    inner: Mutex<HashMap<String, PermissionWaiter>>,
    /// Outbound id source, unique per connection.
    counter: AtomicU64,
}

impl PermissionWaiters {
    fn new() -> Self {
        Self::default()
    }

    fn next_outbound_id(&self) -> String {
        format!(
            "acp-perm-{}",
            self.counter.fetch_add(1, Ordering::SeqCst) + 1
        )
    }

    fn insert(&self, outbound_id: String, waiter: PermissionWaiter) {
        // Called with a fresh id from `next_outbound_id` — never a collision.
        self.inner
            .lock()
            .expect("permission waiters lock")
            .insert(outbound_id, waiter);
    }

    /// Take the waiter for an answered request (framing loop).
    fn remove(&self, outbound_id: &str) -> Option<PermissionWaiter> {
        self.inner
            .lock()
            .expect("permission waiters lock")
            .remove(outbound_id)
    }

    /// Deny every waiter of one session (`session/cancel` mid-gate): the
    /// parked turn resumes with `false` and routes the cancel to its
    /// terminal instead of dead-locking on the gate.
    fn deny_session(&self, session_id: &str) {
        let denied: Vec<PermissionWaiter> = {
            let mut waiters = self.inner.lock().expect("permission waiters lock");
            let ids: Vec<String> = waiters
                .iter()
                .filter(|(_, waiter)| waiter.session_id == session_id)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| waiters.remove(&id))
                .collect()
        };
        for waiter in denied {
            let _ = waiter.answer_tx.send(false);
        }
    }

    /// Deny everything (EOF teardown, T-07).
    fn deny_all(&self) {
        let denied: Vec<PermissionWaiter> = self
            .inner
            .lock()
            .expect("permission waiters lock")
            .drain()
            .map(|(_, waiter)| waiter)
            .collect();
        for waiter in denied {
            let _ = waiter.answer_tx.send(false);
        }
    }
}

// ---------------------------------------------------------------------------
// T-03: the session service — routing + registry.
// ---------------------------------------------------------------------------

/// Result of routing one inbound frame.
#[derive(Debug)]
enum RouteOutcome {
    /// Synchronous success reply (framing loop answers requests only).
    Reply(Value),
    /// Synchronous JSON-RPC error.
    Error(acp::Error),
    /// Accepted but answered later — `session/prompt` spawns a turn thread
    /// that owns the response (the framing loop must stay free to process
    /// `session/cancel` while the turn streams).
    Deferred,
}

/// A live ACP session: one engine thread behind a `MemoryTransport`
/// (daemon pattern, D6), plus the per-session turn state.
struct Session {
    id: String,
    /// Requests into the session engine. Dropping the last clone closes
    /// the channel and the engine's run loop exits (clean teardown at EOF
    /// and on engine death).
    req_tx: mpsc::Sender<ProtocolRequest>,
    /// The engine's event stream, parked between turns. A turn takes it
    /// out (`Option::take`), drains until the terminal event, then parks
    /// it back — so the session stays reusable across prompts and the
    /// framing loop never races the turn on the same receiver.
    events: Mutex<Option<mpsc::Receiver<ProtocolEvent>>>,
    /// One prompt turn at a time per session (D6): the router flips this
    /// to `true` when accepting a prompt; the turn thread clears it at
    /// the terminal. A second prompt mid-turn gets `session busy`.
    busy: AtomicBool,
    /// Minimum `session/cancel` wiring (T-03): the flag is the T-05 lane's
    /// hook; the request below is the engine's cooperative cancel.
    cancel_requested: AtomicBool,
    /// Per-session protocol request ids (init/run/cancel) — unique within
    /// the session, derived from the session id.
    request_counter: AtomicU64,
    /// The engine thread's handle — parked for the T-07 bounded join at
    /// EOF (taken out exactly once by the teardown).
    engine: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// The in-flight prompt turn's handle (`None` while idle); joined at
    /// EOF. A finished turn leaves the handle in place — joining a
    /// finished thread returns immediately.
    turn: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Session {
    fn next_request_id(&self, kind: &str) -> String {
        format!(
            "{}-{}-{}",
            self.id,
            kind,
            self.request_counter.fetch_add(1, Ordering::SeqCst)
        )
    }
}

/// Router + session registry for one ACP connection.
struct AcpService {
    config: AcpConfig,
    sink: Arc<dyn AcpSink>,
    /// Live sessions keyed by id. Shared with prompt-turn threads (they
    /// clear the busy flag, park the receiver back, and tear dead
    /// sessions down).
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    /// Session id source: `sess-1`, `sess-2`, …
    counter: AtomicU64,
    /// T-07: set at EOF teardown under the registry lock, so a `session/new`
    /// boot racing the drain either inserts before it (and gets cancelled +
    /// joined like every other session) or is refused and its engine exits
    /// on its own when the failed session drops the senders.
    closed: AtomicBool,
    /// T-05: pending `session/request_permission` waiters — the permission
    /// bridge between the turn threads, the framing loop (client answers),
    /// and the cancel/teardown paths (deny). Shared because turn threads
    /// outlive the `AcpService` borrow.
    permission_waiters: Arc<PermissionWaiters>,
    /// Test seam (T-05/T-07): scripted fake engines replace real
    /// `RuntimeEngine` boots. `None` in production.
    #[cfg(test)]
    fake_engine: Option<FakeEngine>,
    /// Test probe (T-07): bumped after each engine-thread join in the EOF
    /// teardown. `None` in production.
    #[cfg(test)]
    joined_probe: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

impl AcpService {
    fn new(config: AcpConfig, sink: Arc<dyn AcpSink>) -> Self {
        Self {
            config,
            sink,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            counter: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            permission_waiters: Arc::new(PermissionWaiters::new()),
            #[cfg(test)]
            fake_engine: None,
            #[cfg(test)]
            joined_probe: None,
        }
    }

    /// Dispatch one inbound frame. Only requests (id present) ever receive
    /// a response; notifications and stray responses are routed but never
    /// answered (JSON-RPC forbids replying to notifications — and to
    /// result frames).
    fn handle_frame(&self, frame: &Value) {
        // T-05: a client response frame (result or error, no `method`)
        // answers an outbound agent → client request — the permission
        // bridge. It is never replied to (JSON-RPC contract), so it is
        // handled before the request router instead of falling through
        // to the notification paths.
        if frame.get("method").is_none() && frame.get("id").is_some() {
            self.route_client_response(frame);
            return;
        }
        let may_reply = frame.get("id").is_some();
        let outcome = self.route_frame(frame);
        match (&outcome, may_reply) {
            (RouteOutcome::Reply(result), true) => {
                let frame = serde_json::json!(
                    { "jsonrpc": "2.0", "id": frame.get("id"), "result": result }
                );
                self.send_raw(&frame);
            }
            (RouteOutcome::Error(error), true) => {
                let frame = serde_json::json!(
                    { "jsonrpc": "2.0", "id": frame.get("id"), "error": error }
                );
                self.send_raw(&frame);
            }
            // Deferred prompts are answered by their turn thread;
            // notifications / stray responses are never answered.
            _ => {}
        }
    }

    /// Push an already-built frame to the sink (responses; parse errors
    /// whose routing never ran; turn-thread output on the shared sink).
    fn send_raw(&self, frame: &Value) {
        self.sink.send_line(frame);
    }

    /// Classify one frame:
    ///
    /// - client request (`method` + `id`) → typed method handlers;
    /// - `session/cancel` notifications → cooperative cancel wiring;
    /// - `result`/`error` frames → no agent→client requests exist yet, ignore.
    fn route_frame(&self, frame: &Value) -> RouteOutcome {
        let method = frame.get("method").and_then(Value::as_str);
        let has_id = frame.get("id").is_some();
        match (method, has_id) {
            (Some(INITIALIZE), true) => {
                match parse_params::<acp::InitializeRequest>(frame, INITIALIZE) {
                    Ok(request) => RouteOutcome::Reply(
                        serde_json::to_value(initialize_response(&request))
                            .expect("initialize response serializes"),
                    ),
                    Err(error) => RouteOutcome::Error(error),
                }
            }
            (Some(SESSION_NEW), true) => {
                match parse_params::<acp::NewSessionRequest>(frame, SESSION_NEW) {
                    Ok(request) => match self.session_new(&request) {
                        Ok(response) => RouteOutcome::Reply(
                            serde_json::to_value(response)
                                .expect("session/new response serializes"),
                        ),
                        Err(error) => RouteOutcome::Error(error),
                    },
                    Err(error) => RouteOutcome::Error(error),
                }
            }
            (Some(SESSION_PROMPT), true) => self.session_prompt(frame),
            // D9: mode switches apply at turn boundaries (safety is
            // consulted per boundary), so a busy session is refused rather
            // than silently deferred.
            (Some(SESSION_SET_MODE), true) => self.session_set_mode(frame),
            // A stored-session lifecycle: `session/load` replays the
            // checkpoint history, `session/resume` re-attaches (replaying
            // only when the session is not live in this process).
            (Some(SESSION_LOAD), true) => self.session_load(frame, false),
            (Some(SESSION_RESUME), true) => self.session_load(frame, true),
            // session/cancel is a notification in v1; if a client carries
            // an id on it anyway, an empty result keeps the JSON-RPC
            // contract intact. Unknown/idle sessions: silence per spec.
            (Some(SESSION_CANCEL), _) => {
                self.session_cancel(frame);
                RouteOutcome::Reply(Value::Null)
            }
            // Unknown request → method not found; unknown notifications are
            // ignored rather than tearing down the connection.
            (Some(_), true) => RouteOutcome::Error(acp::Error::method_not_found()),
            _ => RouteOutcome::Reply(serde_json::json!({ "ignored": true })),
        }
    }

    /// `session/new`: validate the D11 single-cwd contract, claim a
    /// capacity slot (T-04), boot one engine (D6) and wait for `InitOk`.
    /// The response only reaches the client once the engine is usable, so
    /// a successful reply IS the implicit boot confirmation.
    fn session_new(
        &self,
        request: &acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        // D11 (option c): the adapter is single-cwd; a mismatch would
        // silently operate in the wrong directory, so it is rejected with
        // an actionable error instead.
        let process_cwd = std::env::current_dir().map_err(|error| {
            internal_error(format!("cannot read the agent working directory: {error}"))
        })?;
        if request.cwd != process_cwd {
            return Err(cwd_mismatch_error(&request.cwd, &process_cwd));
        }

        // T-04: the capacity slot is claimed at `session/new` — boot
        // failures release it again (nothing is inserted on error).
        {
            let sessions = self.sessions.lock().expect("session registry lock");
            if sessions.len() >= self.config.max_sessions {
                return Err(capacity_error(self.config.max_sessions));
            }
        }

        // D6: one engine per session, hosted exactly like the daemon.
        let session_id = format!("sess-{}", self.counter.fetch_add(1, Ordering::SeqCst) + 1);
        let session = self.boot_engine(&session_id)?;
        {
            // T-07: the insert must not sneak past the EOF drain — the
            // closed flag is checked under the same lock the insert runs
            // under (see `AcpService::closed`).
            let mut registry = self.sessions.lock().expect("session registry lock");
            if self.closed.load(Ordering::SeqCst) {
                return Err(internal_error("connection is shutting down"));
            }
            registry.insert(session_id.clone(), session);
        }

        Ok(
            acp::NewSessionResponse::new(acp::SessionId::new(session_id))
                .modes(session_mode_state()),
        )
    }

    /// Boot the session engine thread, send `Init` with the adapter's
    /// overrides, and wait (bounded) for `InitOk`. A failure — engine
    /// died, engine error, boot timeout — is propagated as a JSON-RPC
    /// error; nothing is registered, so the capacity slot is free again.
    fn boot_engine(&self, session_id: &str) -> Result<Arc<Session>, acp::Error> {
        let (req_tx, req_rx) = mpsc::channel::<ProtocolRequest>();
        let (evt_tx, evt_rx) = mpsc::channel::<ProtocolEvent>();
        let engine_name = format!("acp-engine-{session_id}");

        // T-07: the handle is parked on the session for the bounded EOF
        // join. The `#[cfg(test)]` seam may substitute a scripted fake
        // engine for the real one (never compiled into production).
        let engine_handle = self.spawn_engine(engine_name, req_rx, evt_tx)?;

        let session = Arc::new(Session {
            id: session_id.to_owned(),
            req_tx,
            events: Mutex::new(Some(evt_rx)),
            busy: AtomicBool::new(false),
            cancel_requested: AtomicBool::new(false),
            request_counter: AtomicU64::new(0),
            engine: Mutex::new(Some(engine_handle)),
            turn: Mutex::new(None),
        });

        let init_id = session.next_request_id("init");
        let init = ProtocolRequest::new(
            init_id,
            Request::Init {
                overrides: InitOverrides {
                    config_path: self.config.config_path.clone(),
                    permission_mode: None,
                    model: None,
                    system_prompt: None,
                    // The ACP session id IS the checkpoint id: a later
                    // `session/load` finds the persisted document under it.
                    session_id: Some(session_id.to_owned()),
                },
                capabilities: ConnectionCapabilities::default(),
                daemon_token: None,
            },
        );
        if session.req_tx.send(init).is_err() {
            return Err(internal_error("session engine died before init"));
        }

        // Skip the engine's `Ready` banner; `InitOk` is the first real
        // word. An `Error` event (bad config, no provider, …) is the boot
        // failure and is surfaced verbatim.
        let evt_rx_guard = session.events.lock().expect("session events lock");
        let deadline = Instant::now() + BOOT_TIMEOUT;
        loop {
            let Some(receiver) = evt_rx_guard.as_ref() else {
                return Err(internal_error("session engine stream is not available"));
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            match receiver.recv_timeout(remaining) {
                Ok(event) => match &event.event {
                    takka_core::Event::Ready { .. } => continue,
                    takka_core::Event::InitOk { .. } => break,
                    takka_core::Event::Error { message } => {
                        return Err(internal_error(format!(
                            "session engine failed to initialize: {message}"
                        )));
                    }
                    _ => continue,
                },
                Err(RecvTimeoutError::Timeout) => {
                    return Err(internal_error(
                        "session engine did not initialize within the boot deadline",
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(internal_error("session engine died during init"));
                }
            }
        }
        drop(evt_rx_guard);
        Ok(session)
    }

    /// Spawn the session's engine thread. Production boots the real
    /// [`RuntimeEngine`] over a `MemoryTransport` (the daemon hosting
    /// pattern); the `#[cfg(test)]` seam substitutes a scripted fake
    /// engine when one is installed for this connection.
    fn spawn_engine(
        &self,
        name: String,
        req_rx: mpsc::Receiver<ProtocolRequest>,
        evt_tx: mpsc::Sender<ProtocolEvent>,
    ) -> Result<std::thread::JoinHandle<()>, acp::Error> {
        #[cfg(test)]
        if let Some(fake) = self.fake_engine.clone() {
            return std::thread::Builder::new()
                .name(name)
                .spawn(move || fake(req_rx, evt_tx))
                .map_err(|error| {
                    internal_error(format!("failed to spawn the session engine: {error}"))
                });
        }
        spawn_engine_thread(name, req_rx, evt_tx)
            .map_err(|error| internal_error(format!("failed to spawn the session engine: {error}")))
    }

    /// `session/set_mode` (D9): switch the session's permission mode.
    ///
    /// The session's one turn slot must be free — mode laws are honored at
    /// turn boundaries, so switching mid-turn is refused with `session busy`
    /// instead of being silently deferred to an unknown later moment. The
    /// engine acks with `Event::PermissionModeChanged`, which is awaited
    /// (bounded) before replying so the client never sees a mode it did not
    /// actually get.
    fn session_set_mode(&self, frame: &Value) -> RouteOutcome {
        let request = match parse_params::<acp::SetSessionModeRequest>(frame, SESSION_SET_MODE) {
            Ok(request) => request,
            Err(error) => return RouteOutcome::Error(error),
        };
        let session_id = request.session_id.to_string();
        let session = {
            let sessions = self.sessions.lock().expect("session registry lock");
            sessions.get(&session_id).cloned()
        };
        let Some(session) = session else {
            return RouteOutcome::Error(acp::Error::session_not_found(request.session_id));
        };
        if session.busy.load(Ordering::SeqCst) {
            return RouteOutcome::Error(busy_error(&session.id));
        }
        let Some(mode) = permission_mode(request.mode_id.0.as_ref()) else {
            return RouteOutcome::Error(unknown_mode_error(request.mode_id.0.as_ref()));
        };

        // Idle session: the event stream is parked and nobody else can take
        // it (the busy gate above proves no turn is in flight, and a new
        // prompt cannot be dispatched while this handler runs on the framing
        // loop).
        let receiver = match session.events.lock().expect("session events lock").take() {
            Some(receiver) => receiver,
            None => {
                return RouteOutcome::Error(internal_error(
                    "session engine stream is not available",
                ))
            }
        };
        let request_id = session.next_request_id("mode");
        let sent = session.req_tx.send(ProtocolRequest::new(
            request_id,
            Request::SetPermissionMode { mode },
        ));
        if sent.is_err() {
            *session.events.lock().expect("session events lock") = Some(receiver);
            return RouteOutcome::Error(internal_error(
                "runtime engine died before the mode switch",
            ));
        }

        let deadline = Instant::now() + MODE_SWITCH_TIMEOUT;
        let mut ack: Result<(), acp::Error> = Err(internal_error(
            "session engine did not acknowledge the mode switch",
        ));
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match receiver.recv_timeout(remaining) {
                Ok(event) => match &event.event {
                    takka_core::Event::PermissionModeChanged { mode: applied } => {
                        debug_assert_eq!(*applied, mode);
                        ack = Ok(());
                        break;
                    }
                    takka_core::Event::Error { message } => {
                        ack = Err(internal_error(message.clone()));
                        break;
                    }
                    // Stray events (a finished turn's tail, notices) are not
                    // mode answers: keep waiting for the ack.
                    _ => continue,
                },
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    ack = Err(internal_error("runtime engine died during the mode switch"));
                    break;
                }
            }
        }
        // Park the stream back so the next prompt can run.
        *session.events.lock().expect("session events lock") = Some(receiver);
        match ack {
            Ok(()) => RouteOutcome::Reply(
                serde_json::to_value(acp::SetSessionModeResponse::default())
                    .expect("set mode response serializes"),
            ),
            Err(error) => RouteOutcome::Error(error),
        }
    }

    /// `session/load` / `session/resume`: attach a stored checkpoint to a
    /// fresh engine booted under the client-supplied session id (the id
    /// doubles as the checkpoint filename, so the client's saved id keeps
    /// working across agent restarts).
    ///
    /// `session/load` streams the stored history to the client as
    /// `session/update` notifications BEFORE the response (the replay
    /// contract); `session/resume` on a session still live in this process
    /// re-attaches without any engine traffic. A failed load tears the
    /// freshly booted session down again and frees its capacity slot.
    fn session_load(&self, frame: &Value, resume: bool) -> RouteOutcome {
        let (session_id, cwd) = if resume {
            match parse_params::<acp::ResumeSessionRequest>(frame, SESSION_RESUME) {
                Ok(request) => (request.session_id.to_string(), request.cwd),
                Err(error) => return RouteOutcome::Error(error),
            }
        } else {
            match parse_params::<acp::LoadSessionRequest>(frame, SESSION_LOAD) {
                Ok(request) => (request.session_id.to_string(), request.cwd),
                Err(error) => return RouteOutcome::Error(error),
            }
        };

        // D11: the same single-cwd contract as `session/new`.
        let process_cwd = match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(error) => {
                return RouteOutcome::Error(internal_error(format!(
                    "cannot read the agent working directory: {error}"
                )))
            }
        };
        if cwd != process_cwd {
            return RouteOutcome::Error(cwd_mismatch_error(&cwd, &process_cwd));
        }
        if let Err(message) = validate_session_id(&session_id) {
            return RouteOutcome::Error(invalid_params_error(message));
        }

        {
            let sessions = self.sessions.lock().expect("session registry lock");
            if sessions.contains_key(&session_id) {
                if resume {
                    // The agent kept the session live: just re-attach.
                    return RouteOutcome::Reply(load_modes_reply(true));
                }
                return RouteOutcome::Error(internal_error(format!(
                    "session {session_id} is already live"
                )));
            }
            if self.closed.load(Ordering::SeqCst) {
                return RouteOutcome::Error(internal_error("connection is shutting down"));
            }
            if sessions.len() >= self.config.max_sessions {
                return RouteOutcome::Error(capacity_error(self.config.max_sessions));
            }
        }
        // Adopted ids never collide with later minted `sess-N` ids.
        if let Some(number) = session_id
            .strip_prefix("sess-")
            .and_then(|number| number.parse::<u64>().ok())
        {
            self.counter.fetch_max(number + 1, Ordering::SeqCst);
        }

        let session = match self.boot_engine(&session_id) {
            Ok(session) => session,
            Err(error) => return RouteOutcome::Error(error),
        };

        let request_id = session.next_request_id("load");
        let sent = session.req_tx.send(ProtocolRequest::new(
            request_id,
            Request::LoadSession {
                session_id: session_id.clone(),
            },
        ));
        if sent.is_err() {
            return RouteOutcome::Error(internal_error("session engine died before the load"));
        }

        // Take the parked stream; the replayed history flows out as session
        // updates while the `SessionLoaded` ack is awaited.
        let receiver = match session.events.lock().expect("session events lock").take() {
            Some(receiver) => receiver,
            None => {
                return RouteOutcome::Error(internal_error(
                    "session engine stream is not available",
                ))
            }
        };
        let mut translator = TranslatorState::new();
        let mut ack: Result<(), acp::Error> = Err(internal_error(
            "session engine did not acknowledge the load",
        ));
        loop {
            match receiver.recv_timeout(BOOT_TIMEOUT) {
                Ok(event) => match &event.event {
                    takka_core::Event::SessionLoaded { .. } => {
                        ack = Ok(());
                        break;
                    }
                    takka_core::Event::Error { message } => {
                        ack = Err(invalid_params_error(message.clone()));
                        break;
                    }
                    // Replay chunk: surface it, keep waiting for the ack.
                    event => {
                        if let Some(update) = translator.translate(event) {
                            let notification = acp::SessionNotification::new(
                                acp::SessionId::new(session.id.as_str()),
                                update,
                            );
                            self.send_raw(&serde_json::json!({
                                "jsonrpc": "2.0", "method": SESSION_UPDATE,
                                "params": serde_json::to_value(notification)
                                    .expect("session notification serializes")
                            }));
                        }
                    }
                },
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    ack = Err(internal_error("session engine died during the load"));
                    break;
                }
            }
        }
        // Park the stream back so the next prompt can run.
        *session.events.lock().expect("session events lock") = Some(receiver);
        match ack {
            Ok(()) => RouteOutcome::Reply(load_modes_reply(resume)),
            Err(error) => {
                // A failed load leaves a useless session: drop the registry
                // entry so the engine exits with its dropped senders and
                // the capacity slot is free again.
                self.sessions
                    .lock()
                    .expect("session registry lock")
                    .remove(&session_id);
                RouteOutcome::Error(error)
            }
        }
    }

    /// `session/prompt`: map content blocks, gate on busy, then hand the
    /// turn to its own thread (`Deferred`) so the framing loop keeps
    /// processing `session/cancel` while the turn streams.
    fn session_prompt(&self, frame: &Value) -> RouteOutcome {
        let request = match parse_params::<acp::PromptRequest>(frame, SESSION_PROMPT) {
            Ok(request) => request,
            Err(error) => return RouteOutcome::Error(error),
        };
        let session_id = request.session_id.to_string();
        let session = {
            let sessions = self.sessions.lock().expect("session registry lock");
            sessions.get(&session_id).cloned()
        };
        let Some(session) = session else {
            return RouteOutcome::Error(acp::Error::session_not_found(request.session_id));
        };

        let (task, attachments) = match prompt_content(&request.prompt) {
            Ok(parts) => parts,
            Err(error) => return RouteOutcome::Error(error),
        };

        // Busy gate: one turn at a time per session (D6). Cleared by the
        // turn thread before its response reaches the wire, so a client
        // that saw the response can prompt again immediately.
        if session.busy.swap(true, Ordering::SeqCst) {
            return RouteOutcome::Error(busy_error(&session.id));
        }

        let spawn = std::thread::Builder::new()
            .name(format!("acp-turn-{}", session.id))
            .spawn({
                let sessions = self.sessions.clone();
                let session = session.clone();
                let sink = self.sink.clone();
                let permissions = self.permission_waiters.clone();
                let reply_id = frame.get("id").cloned().unwrap_or(Value::Null);
                move || {
                    run_prompt_turn(
                        sessions,
                        session,
                        sink,
                        permissions,
                        reply_id,
                        task,
                        attachments,
                    )
                }
            });
        match spawn {
            Ok(handle) => {
                // T-07: park the handle so the EOF teardown can join the
                // turn bounded.
                *session.turn.lock().expect("session turn lock") = Some(handle);
                RouteOutcome::Deferred
            }
            Err(error) => {
                session.busy.store(false, Ordering::SeqCst);
                RouteOutcome::Error(internal_error(format!(
                    "failed to spawn the prompt turn: {error}"
                )))
            }
        }
    }

    /// `session/cancel` (T-03 minimum wiring): set the cancel flag (the
    /// T-05 lane turns it into a `cancelled` stop reason) and forward the
    /// cooperative `Request::Cancel` to the engine. Cancelling never
    /// destroys the session; unknown sessions are silently ignored.
    fn session_cancel(&self, frame: &Value) {
        let Ok(notification) = parse_params::<acp::CancelNotification>(frame, SESSION_CANCEL)
        else {
            return; // malformed notification: silence (it cannot be answered)
        };
        let session_id = notification.session_id.to_string();
        let session = {
            let sessions = self.sessions.lock().expect("session registry lock");
            sessions.get(&session_id).cloned()
        };
        let Some(session) = session else {
            return;
        };
        session.cancel_requested.store(true, Ordering::SeqCst);
        // T-05: free this session's permission gates BEFORE the engine
        // sees the cancel — a turn parked on `session/request_permission`
        // cannot observe the cooperative cancel until its gate unblocks.
        // The deny lets the turn resume (engine gets `ConfirmResponse`
        // first), after which the cancel runs to its `cancelled` terminal.
        self.permission_waiters.deny_session(&session_id);
        let cancel = ProtocolRequest::new(session.next_request_id("cancel"), Request::Cancel);
        let _ = session.req_tx.send(cancel);
    }

    /// EOF teardown (T-07, spec §5 step 6): settle the connection so the
    /// process can exit 0 without leaving session threads behind. Order
    /// matters:
    ///
    /// 1. cooperative cancel for every live session — an engine thread
    ///    only exits after its active run winds down at the next
    ///    boundary, and the cancel flag + `Request::Cancel` make that
    ///    quick;
    /// 2. deny every pending permission waiter so a turn parked on the
    ///    gate cannot dead-lock the joins below;
    /// 3. close the registry (no late boots sneak past the drain) and
    ///    drain it;
    /// 4. join the TURN threads first — each holds a `req_tx` clone, and
    ///    the engine only observes EOF on its request channel once every
    ///    sender is gone;
    /// 5. drop the sessions (last senders die → idle engines exit on
    ///    `read_request → Ok(None)`) and bounded-join the engine threads.
    ///
    /// Stragglers are detached with a warning; the process still exits.
    fn shutdown(&self) {
        let deadline = Instant::now() + SHUTDOWN_JOIN_TIMEOUT;
        let sessions: Vec<Arc<Session>> = {
            let mut registry = self.sessions.lock().expect("session registry lock");
            self.closed.store(true, Ordering::SeqCst);
            for session in registry.values() {
                session.cancel_requested.store(true, Ordering::SeqCst);
                let cancel =
                    ProtocolRequest::new(session.next_request_id("cancel"), Request::Cancel);
                // A dead engine has nothing to cancel — ignore the send.
                let _ = session.req_tx.send(cancel);
            }
            self.deny_pending_permissions();
            registry.drain().map(|(_, session)| session).collect()
        };
        let turn_handles: Vec<std::thread::JoinHandle<()>> = sessions
            .iter()
            .filter_map(|session| session.turn.lock().expect("session turn lock").take())
            .collect();
        let engine_handles: Vec<std::thread::JoinHandle<()>> = sessions
            .iter()
            .filter_map(|session| session.engine.lock().expect("session engine lock").take())
            .collect();
        // Dropping the sessions drops the last `req_tx` clones: engines
        // that are still winding down see the closed channel and exit.
        drop(sessions);
        join_bounded(turn_handles, deadline, None);
        #[cfg(test)]
        let probe = self.joined_probe.as_deref();
        #[cfg(not(test))]
        let probe = None;
        join_bounded(engine_handles, deadline, probe);
    }

    /// Deny every pending `session/request_permission` waiter so a turn
    /// parked on the permission gate cannot dead-lock the bounded join
    /// (T-05: the waiter registry now exists; T-06's mapping supplies the
    /// wire shapes).
    fn deny_pending_permissions(&self) {
        self.permission_waiters.deny_all();
    }

    /// Route a client response frame (result or error, no method) to the
    /// pending permission waiter it answers (T-05). The client's outcome
    /// maps through `acp_permission::outcome_approves` — allow variants
    /// approve, everything else (reject, cancelled, `non_exhaustive`
    /// newcomers) denies. An error-shaped answer denies (fail-closed).
    /// A frame answering no known waiter is dropped silently — per
    /// JSON-RPC a stray response is never an error and never a reply.
    fn route_client_response(&self, frame: &Value) {
        let answered: acp::Response<acp::RequestPermissionResponse> =
            match serde_json::from_value(frame.clone()) {
                Ok(answered) => answered,
                // Neither a well-formed result nor error frame: not a
                // shape the bridge asked for — ignore.
                Err(_) => return,
            };
        let (request_id, approved) = match answered {
            acp::Response::Result { id, result } => {
                (id, acp_permission::outcome_approves(&result.outcome))
            }
            acp::Response::Error { id, .. } => (id, false),
        };
        // Outbound ids are strings (`acp-perm-N`); other id shapes can
        // never match one and fall through to the same silent drop.
        if let Some(waiter) = self
            .permission_waiters
            .remove(request_id.to_string().as_str())
        {
            let _ = waiter.answer_tx.send(approved);
        }
    }
}

/// Spawn the real session engine thread (production path, also used by
/// seam-less test boots): one [`RuntimeEngine`] over a `MemoryTransport`,
/// exactly like the daemon host.
fn spawn_engine_thread(
    name: String,
    req_rx: mpsc::Receiver<ProtocolRequest>,
    evt_tx: mpsc::Sender<ProtocolEvent>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name(name).spawn(move || {
        let mut engine = RuntimeEngine::new(MemoryTransport::new(req_rx, evt_tx));
        engine.run();
    })
}

/// Join thread handles with a deadline (the daemon `join_bounded`
/// pattern): everything that finishes in time is reaped; a straggler is
/// detached with a warning and the remaining handles are left too (the
/// deadline has passed). `probe` — a test hook — is bumped after each
/// completed join.
fn join_bounded(
    handles: Vec<std::thread::JoinHandle<()>>,
    deadline: Instant,
    probe: Option<&std::sync::atomic::AtomicUsize>,
) {
    for handle in handles {
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                tracing::warn!(
                    "acp: session thread did not finish before the shutdown deadline; left detached"
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = handle.join();
        if let Some(probe) = probe {
            probe.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Body of one prompt turn (owns the response for `reply_id`).
///
/// Takes the parked event receiver, runs `Request::Run`, translates every
/// engine event into a `session/update` notification, and resolves the
/// response at the terminal: `Done` → `stopReason: end_turn`; `Error` →
/// JSON-RPC error with the engine's message; engine death → error + session
/// teardown. The receiver is parked back before the response is written so
/// the session survives failed turns (no poisoned sessions).
fn run_prompt_turn(
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    session: Arc<Session>,
    sink: Arc<dyn AcpSink>,
    permissions: Arc<PermissionWaiters>,
    reply_id: Value,
    task: String,
    attachments: Vec<Attachment>,
) {
    let respond = |payload: &Result<acp::PromptResponse, acp::Error>| {
        let frame = match payload {
            Ok(response) => serde_json::json!({
                "jsonrpc": "2.0", "id": reply_id,
                "result": serde_json::to_value(response).expect("prompt response serializes")
            }),
            Err(error) => serde_json::json!({
                "jsonrpc": "2.0", "id": reply_id, "error": error
            }),
        };
        sink.send_line(&frame);
    };

    // Take the parked stream — the busy flag guarantees we are the only
    // taker. `None` means the stream is already gone (engine died): the
    // session is beyond repair and is torn down.
    let receiver = match session.events.lock().expect("session events lock").take() {
        Some(receiver) => receiver,
        None => {
            sessions
                .lock()
                .expect("session registry lock")
                .remove(&session.id);
            session.busy.store(false, Ordering::SeqCst);
            respond(&Err(internal_error(
                "session engine stream is not available",
            )));
            return;
        }
    };

    let run_id = session.next_request_id("run");
    let run = ProtocolRequest::new(run_id, Request::Run { task, attachments });
    if session.req_tx.send(run).is_err() {
        // Engine thread is gone — no session without an engine.
        sessions
            .lock()
            .expect("session registry lock")
            .remove(&session.id);
        session.busy.store(false, Ordering::SeqCst);
        respond(&Err(internal_error(
            "runtime engine died before the turn started",
        )));
        return;
    }

    let mut translator = TranslatorState::new();
    let mut engine_died = false;
    // D12: a turn that ends because a limit was hit must report the real
    // reason instead of degrading to `end_turn`. The engine emits the limit
    // event right before its terminator, so remember it here.
    let mut turn_limit: Option<acp::StopReason> = None;
    let outcome: Result<acp::PromptResponse, acp::Error> = loop {
        // No overall deadline here (a legitimate coding turn can run for
        // minutes; the daemon drain has the same shape). A dead engine is
        // detected immediately via the disconnected channel.
        match receiver.recv() {
            Ok(event) => match &event.event {
                takka_core::Event::TurnLimitExceeded { kind } => {
                    turn_limit = Some(match kind {
                        takka_core::events::TurnLimitKind::MaxTokens => acp::StopReason::MaxTokens,
                        takka_core::events::TurnLimitKind::MaxTurnRequests => {
                            acp::StopReason::MaxTurnRequests
                        }
                    });
                }
                takka_core::Event::Done { .. } => {
                    break Ok(acp::PromptResponse::new(
                        turn_limit.unwrap_or(acp::StopReason::EndTurn),
                    ));
                }
                takka_core::Event::Error { message } => {
                    if session.cancel_requested.load(Ordering::SeqCst) {
                        // T-05: the engine surfaced the cooperative cancel
                        // as its terminal (`AgenticError::Cancelled` maps
                        // to `Event::Error`) — the client gets the ACP
                        // `cancelled` stop reason, never an error, and
                        // the session stays reusable for the next prompt.
                        break Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
                    }
                    break Err(internal_error(message.clone()));
                }
                takka_core::Event::ConfirmationRequest {
                    action,
                    description,
                    ..
                } => {
                    // T-05: the permission bridge (D8). Ask the client,
                    // park until its answer frame (routed by the framing
                    // loop) or a cancel/teardown deny frees the waiter,
                    // then unblock the engine with `ConfirmResponse` so
                    // the turn continues either way.
                    let approved =
                        request_permission(&session, &permissions, &sink, action, description);
                    let confirm_id = session.next_request_id("confirm");
                    let confirm = ProtocolRequest::new(
                        confirm_id,
                        acp_permission::confirm_response(None, approved),
                    );
                    if session.req_tx.send(confirm).is_err() {
                        engine_died = true;
                        break Err(internal_error("runtime engine died mid-turn"));
                    }
                }
                event => {
                    if let Some(update) = translator.translate(event) {
                        let notification = acp::SessionNotification::new(
                            acp::SessionId::new(session.id.as_str()),
                            update,
                        );
                        sink.send_line(&serde_json::json!({
                            "jsonrpc": "2.0", "method": SESSION_UPDATE,
                            "params": serde_json::to_value(notification)
                                .expect("session notification serializes")
                        }));
                    }
                }
            },
            // The engine's event senders live inside the engine thread
            // (its `MemoryTransport`); an error here means the engine is
            // gone for good.
            Err(_) => {
                engine_died = true;
                break Err(internal_error("runtime engine died mid-turn"));
            }
        }
    };

    // Reopen the event channel and release the turn slot BEFORE the
    // response hits the wire — a client that saw the response must be
    // able to prompt again right away (T-03: no poisoned sessions).
    if engine_died {
        sessions
            .lock()
            .expect("session registry lock")
            .remove(&session.id);
    } else {
        *session.events.lock().expect("session events lock") = Some(receiver);
    }
    session.busy.store(false, Ordering::SeqCst);

    match outcome {
        Ok(response) => respond(&Ok(response)),
        Err(error) => respond(&Err(error)),
    }
}

/// One permission gate (T-05): send `session/request_permission` (D8) and
/// block the turn until the client's answer frame arrives (routed by the
/// framing loop through [`AcpService::route_client_response`]) or a
/// deny frees the waiter (`session/cancel` for this session, or the EOF
/// teardown). A deny and a dropped waiter both count as rejection —
/// fail-closed, and the engine is always unblocked so the turn can run
/// to its terminal.
fn request_permission(
    session: &Session,
    permissions: &PermissionWaiters,
    sink: &Arc<dyn AcpSink>,
    action: &str,
    description: &str,
) -> bool {
    let outbound_id = permissions.next_outbound_id();
    let (answer_tx, answer_rx) = mpsc::sync_channel::<bool>(1);
    permissions.insert(
        outbound_id.clone(),
        PermissionWaiter {
            session_id: session.id.clone(),
            answer_tx,
        },
    );
    let request =
        acp_permission::permission_request(&session.id, &outbound_id, action, description);
    sink.send_line(&serde_json::json!({
        "jsonrpc": "2.0", "id": outbound_id,
        "method": SESSION_REQUEST_PERMISSION,
        "params": serde_json::to_value(request).expect("permission request serializes")
    }));
    // No deadline here (a user may take minutes to decide) — cancel and
    // teardown are the events that free this park early. A dropped
    // sender (waiter removed without an answer) is also a denial.
    answer_rx.recv().unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Prompt content mapping (spec §4, client → agent).
// ---------------------------------------------------------------------------

/// Map ACP content blocks to one task string + engine attachments.
///
/// `Text` blocks join with newlines into the task; `Image` blocks become
/// engine `Attachment`s (data-URL source, whitelisted vision MIME types
/// only); everything else (`resource_link`, `resource`, `audio`, future
/// variants) is rejected — the adapter advertises `image: true` and nothing
/// else (D7). An empty prompt is a parameter error.
fn prompt_content(prompt: &[acp::ContentBlock]) -> Result<(String, Vec<Attachment>), acp::Error> {
    let mut task = String::new();
    let mut attachments = Vec::new();
    for block in prompt {
        match block {
            acp::ContentBlock::Text(text) => {
                if !task.is_empty() {
                    task.push('\n');
                }
                task.push_str(&text.text);
            }
            acp::ContentBlock::Image(image) => {
                let mime_type = image.mime_type.to_ascii_lowercase();
                if !ALLOWED_IMAGE_MIME.contains(&mime_type.as_str()) {
                    return Err(unsupported_content_type(
                        &format!("image ({})", image.mime_type),
                        "supported image MIME types: image/png, image/jpeg, image/gif, image/webp",
                    ));
                }
                attachments.push(Attachment {
                    kind: AttachmentKind::Image,
                    mime_type,
                    data_base64: image.data.clone(),
                    source: AttachmentSource::DataUrl,
                    size_bytes: base64_decoded_len(&image.data),
                });
            }
            other => {
                return Err(unsupported_content_type(
                    content_block_kind(other),
                    "inline the text, or attach the file as an image (PNG/JPEG/GIF/WebP) — \
                     the agent advertises text + image prompts only",
                ));
            }
        }
    }
    if task.trim().is_empty() {
        return Err(acp::Error::invalid_params().data(serde_json::json!(
            { "method": SESSION_PROMPT, "reason": "prompt must contain at least one non-empty text block" }
        )));
    }
    Ok((task, attachments))
}

/// Stable wire-ish tag for a (non-exhaustively matched) content block.
fn content_block_kind(block: &acp::ContentBlock) -> &'static str {
    match block {
        acp::ContentBlock::Text(_) => "text",
        acp::ContentBlock::Image(_) => "image",
        acp::ContentBlock::Audio(_) => "audio",
        acp::ContentBlock::ResourceLink(_) => "resource_link",
        acp::ContentBlock::Resource(_) => "resource",
        _ => "unknown content block",
    }
}

/// Decoded byte-length estimate for a standard-base64 payload — feeds the
/// diagnostic `size_bytes` on an `Attachment` without a decode pass.
fn base64_decoded_len(encoded: &str) -> usize {
    encoded.trim_end_matches('=').len() * 3 / 4
}

// ---------------------------------------------------------------------------
// Typed wire errors (the messages clients act on).
// ---------------------------------------------------------------------------

fn internal_error(message: impl Into<String>) -> acp::Error {
    acp::Error::new(i32::from(acp::ErrorCode::InternalError), message)
}

/// D11: single-cwd adapter — the mismatch error carries the actionable
/// restart sentence verbatim (clients show it to the user).
fn cwd_mismatch_error(requested: &Path, actual: &Path) -> acp::Error {
    let reason = format!(
        "restart the agent from project directory {} (session cwd {} does not match)",
        actual.display(),
        requested.display()
    );
    acp::Error::new(i32::from(acp::ErrorCode::InvalidParams), reason.clone()).data(
        serde_json::json!({
            "method": SESSION_NEW,
            "reason": reason,
            "expected": actual.to_string_lossy(),
            "received": requested.to_string_lossy(),
        }),
    )
}

/// T-04: the N+1-th session gets a typed capacity rejection.
fn capacity_error(capacity: usize) -> acp::Error {
    acp::Error::new(
        i32::from(acp::ErrorCode::InvalidRequest),
        "session capacity reached",
    )
    .data(serde_json::json!({
        "method": SESSION_NEW,
        "reason": "session capacity reached",
        "capacity": capacity,
    }))
}

/// D6: one turn at a time per session.
fn busy_error(session_id: &str) -> acp::Error {
    acp::Error::new(i32::from(acp::ErrorCode::InvalidRequest), "session busy").data(
        serde_json::json!({
            "method": SESSION_PROMPT,
            "reason": "session busy",
            "sessionId": session_id,
        }),
    )
}

fn unsupported_content_type(kind: &str, hint: &str) -> acp::Error {
    let message = format!("unsupported content type {kind}: {hint}");
    acp::Error::new(i32::from(acp::ErrorCode::InvalidParams), message).data(serde_json::json!(
        { "method": SESSION_PROMPT, "reason": "unsupported content type", "contentType": kind }
    ))
}

fn unknown_mode_error(mode_id: &str) -> acp::Error {
    acp::Error::new(
        i32::from(acp::ErrorCode::InvalidParams),
        format!("unknown session mode: {mode_id}"),
    )
    .data(serde_json::json!({
        "method": SESSION_SET_MODE,
        "reason": "unknown session mode",
        "requested": mode_id,
        "available": ["default", "plan", "yolo"],
    }))
}

/// D9: ACP mode id -> takka's permission mode. `None` = not advertised.
fn permission_mode(mode_id: &str) -> Option<PermissionMode> {
    match mode_id {
        "default" => Some(PermissionMode::Default),
        "plan" => Some(PermissionMode::Plan),
        "yolo" => Some(PermissionMode::Yolo),
        _ => None,
    }
}
fn invalid_params_error(message: impl Into<String>) -> acp::Error {
    acp::Error::new(i32::from(acp::ErrorCode::InvalidParams), message.into())
}

/// Same id law as the checkpoint store: plain identifiers only, never path
/// separators — the id becomes a filename on disk.
fn validate_session_id(session_id: &str) -> Result<(), String> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(format!("invalid session id: {session_id:?}"));
    }
    Ok(())
}

/// Reply body for a successful `session/load` / `session/resume`: the
/// mode surface (permission modes) for the re-attached session.
fn load_modes_reply(resume: bool) -> Value {
    let modes = session_mode_state();
    if resume {
        serde_json::to_value(acp::ResumeSessionResponse::new().modes(modes))
            .expect("resume response serializes")
    } else {
        serde_json::to_value(acp::LoadSessionResponse::new().modes(modes))
            .expect("load response serializes")
    }
}

/// Unwraps `params` and deserializes into a typed request body.
fn parse_params<T: serde::de::DeserializeOwned>(
    frame: &Value,
    method: &str,
) -> Result<T, acp::Error> {
    let params = frame.get("params").cloned().unwrap_or(Value::Null);
    serde_json::from_value(params)
        .map_err(|error| acp::Error::invalid_params().data(params_error_data(method, &error)))
}

fn params_error_data(method: &str, error: &serde_json::Error) -> Value {
    serde_json::json!({ "method": method, "reason": error.to_string() })
}

// ---------------------------------------------------------------------------
// D9: takka permission modes as ACP session modes. The current mode is
// always `default` at session creation; `session/set_mode` wiring (into
// `InitOverrides.permission_mode`) lands with a later lane.
// ---------------------------------------------------------------------------

fn session_mode_state() -> acp::SessionModeState {
    acp::SessionModeState::new(
        acp::SessionModeId::new("default"),
        vec![
            mode(
                "default",
                "Default",
                "Ask before writes and command execution",
            ),
            mode(
                "plan",
                "Plan",
                "Read-only planning; writes and commands are denied",
            ),
            mode("yolo", "Yolo", "Allow everything (trusted automation only)"),
        ],
    )
}

fn mode(id: &str, name: &str, description: &str) -> acp::SessionMode {
    acp::SessionMode::new(acp::SessionModeId::new(id), name).description(description)
}

// ---------------------------------------------------------------------------
// T-01: initialize responder — v1 negotiation + capability set (D7/D9/D11).
// ---------------------------------------------------------------------------

/// Pick the per-connection protocol version and answer with capabilities.
///
/// Decision D3: takka speaks ACP v1. A Client asking for v2 (still draft)
/// receives a v1 answer — per the negotiation contract the Client then decides
/// whether it can fall back. Version 0 is the pre-release, unnamed version and
/// also yields v1.
pub fn initialize_response(_request: &acp::InitializeRequest) -> acp::InitializeResponse {
    acp::InitializeResponse::new(ProtocolVersion::V1)
        .agent_capabilities(agent_capabilities())
        .agent_info(agent_implementation())
}

/// The capability set advertised to every connected Client (spec D7).
///
/// takka owns the filesystem and command execution itself (its own safety
/// layer runs the tools), so nothing branches on Client-side `fs`/`terminal`
/// capabilities — the agent simply never calls those methods, whatever the
/// Client advertises. `mcpCapabilities` stays false: takka's MCP servers come
/// from its own config today (client-provided servers are phase 2).
fn agent_capabilities() -> acp::AgentCapabilities {
    acp::AgentCapabilities::new()
        .prompt_capabilities(acp::PromptCapabilities::new().image(true))
        .mcp_capabilities(acp::McpCapabilities::new())
        .load_session(true)
        .session_capabilities(
            acp::SessionCapabilities::new().resume(acp::SessionResumeCapabilities::new()),
        )
}

fn agent_implementation() -> acp::Implementation {
    acp::Implementation::new("takka", env!("CARGO_PKG_VERSION")).title("takka")
}

// ---------------------------------------------------------------------------
// T-02: engine `Event` → ACP `SessionUpdate` translator.
// Pure: engine events in, session updates out. One `TranslatorState` per
// session turn, owned by the prompt-turn loop.
// ---------------------------------------------------------------------------

/// Carry-through state while one prompt turn streams.
#[derive(Default)]
pub struct TranslatorState {
    /// Per-tool accumulated streamed output (`tool_delta` chunks are appended
    /// here and each update carries the full accumulated text, because ACP
    /// `ToolCallUpdateFields.content` is overwritten, not extended).
    stream_buffer: std::collections::HashMap<String, String>,
}

impl TranslatorState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Map an engine event to the ACP session update it should surface.
    ///
    /// Events with no v1 counterpart (local UI concepts, replies the prompt
    /// loop consumes itself) return `None`, matching the phase-1 mapping
    /// table in the spec.
    #[must_use]
    pub fn translate(&mut self, event: &takka_core::Event) -> Option<acp::SessionUpdate> {
        use takka_core::Event;
        match event {
            Event::Thinking { content } => Some(acp::SessionUpdate::AgentThoughtChunk(
                acp::ContentChunk::new(text_block(content)),
            )),
            // Stored-history replay (`Request::LoadSession`): user turns
            // surface as user chunks, assistant turns as agent chunks; tool
            // turns carry no standalone text and are skipped.
            Event::HistoryReplay { role, content } => match role.as_str() {
                "user" => Some(acp::SessionUpdate::UserMessageChunk(
                    acp::ContentChunk::new(text_block(content)),
                )),
                "assistant" => Some(acp::SessionUpdate::AgentMessageChunk(
                    acp::ContentChunk::new(text_block(content)),
                )),
                _ => None,
            },
            Event::AssistantDelta { content } => Some(acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(text_block(content)),
            )),
            Event::ToolStarted {
                tool_call_id,
                tool_name,
                arguments,
            } => Some(acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(
                    acp::ToolCallId::new(tool_call_id.to_string()),
                    tool_name.to_string(),
                )
                .name(tool_name.to_string())
                .kind(tool_kind(tool_name))
                .status(acp::ToolCallStatus::InProgress)
                .raw_input(arguments.clone()),
            )),
            Event::ToolDelta {
                tool_call_id,
                delta,
                ..
            } => {
                let buffer = self.stream_buffer.entry(tool_call_id.clone()).or_default();
                buffer.push_str(delta);
                Some(acp::SessionUpdate::ToolCallUpdate(
                    acp::ToolCallUpdate::new(
                        acp::ToolCallId::new(tool_call_id.to_string()),
                        acp::ToolCallUpdateFields::new()
                            .content(vec![text_tool_content(buffer.as_str())]),
                    ),
                ))
            }
            Event::ToolOutput {
                tool_call_id,
                output,
                error,
                success,
                truncated,
                ..
            } => {
                let stream = self.stream_buffer.remove(tool_call_id);
                let mut fields = acp::ToolCallUpdateFields::new().status(if *success {
                    acp::ToolCallStatus::Completed
                } else {
                    acp::ToolCallStatus::Failed
                });
                if let Some(buffer) = stream {
                    fields = fields.content(vec![text_tool_content(&buffer)]);
                } else if let Some(displayed) = output_display(output) {
                    fields = fields.content(vec![text_tool_content(&displayed)]);
                }
                let failure = error.as_deref();
                if let Some(text) = failure {
                    fields = fields.content(vec![text_tool_content(text)]);
                }
                if *truncated {
                    fields =
                        fields.title("tool output truncated per the tool-result budget".to_owned());
                }
                Some(acp::SessionUpdate::ToolCallUpdate(
                    acp::ToolCallUpdate::new(
                        acp::ToolCallId::new(tool_call_id.to_string()),
                        fields,
                    ),
                ))
            }
            // Not an update for the stream: replies & bookkeeping the prompt
            // loop consumes, permission bridge (T-05), and logging.
            Event::Ready { .. }
            | Event::InitOk { .. }
            | Event::ToolCall { .. }
            | Event::ToolFinished { .. }
            | Event::Planning { .. }
            | Event::ConfirmationRequest { .. }
            | Event::QuestionRequest { .. }
            | Event::TodoChanged { .. }
            | Event::SessionReset
            | Event::MemorySearchResult { .. }
            | Event::Usage { .. }
            | Event::ToolList { .. }
            | Event::SkillActivatedResult { .. }
            | Event::PlanApprovalRequest { .. }
            | Event::PlanProgress { .. }
            | Event::PlanReplanned { .. }
            | Event::Error { .. }
            | Event::Done { .. }
            | Event::System { .. }
            | Event::SessionStarted
            | Event::ModelRequest { .. }
            | Event::ModelChunk { .. }
            | Event::SkillActivated { .. }
            | Event::PlanCreated { .. }
            | Event::Warning { .. }
            | Event::StepStarted { .. }
            | Event::StepCompleted { .. }
            | Event::StateChanged { .. }
            | Event::SessionFailed { .. }
            | Event::SteerQueued { .. }
            | Event::Steered { .. }
            | Event::SessionAdopted { .. }
            | Event::SessionLoaded { .. }
            | Event::CompactionStarted
            | Event::WaitingForUser
            | Event::SessionCompleted { .. } => None,
            // D12/D9: consumed by the turn loop (stop reason) and the mode
            // surface, never rendered as a stream update.
            Event::TurnLimitExceeded { .. } | Event::PermissionModeChanged { .. } => None,
        }
    }
}

/// Text content block for streamed update chunks.
fn text_block(text: impl Into<String>) -> acp::ContentBlock {
    acp::ContentBlock::Text(acp::TextContent::new(text.into()))
}

/// Text content attached to a tool call.
fn text_tool_content(text: impl Into<String>) -> acp::ToolCallContent {
    acp::ToolCallContent::Content(acp::Content::new(text_block(text)))
}

/// Streamed tool output ends up attached as its payload text.
fn output_display(output: &Value) -> Option<String> {
    match output {
        Value::String(text) => Some(text.clone()),
        Value::Null => None,
        _ => serde_json::to_string_pretty(output).ok(),
    }
}

/// takka tool name → ACP `ToolKind` (spec §4 mapping table).
#[must_use]
pub fn tool_kind(tool_name: &str) -> acp::ToolKind {
    match tool_name {
        "read_file" | "read_memory" => acp::ToolKind::Read,
        "glob" | "grep" | "search" => acp::ToolKind::Search,
        "write_file" | "edit_file" | "apply_patch" => acp::ToolKind::Edit,
        "run_command" | "run_script" => acp::ToolKind::Execute,
        "fetch" | "web_search" => acp::ToolKind::Fetch,
        _ => acp::ToolKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::Sender;

    // ------------------------------------------------------------------
    // T-01: initialize negotiation + capability set.
    // ------------------------------------------------------------------

    fn init_request(version: u16) -> acp::InitializeRequest {
        serde_json::from_value(serde_json::json!({
            "protocolVersion": version,
            "clientCapabilities": { "fs": { "readTextFile": true } },
            "clientInfo": { "name": "test-client", "version": "0.0.0" }
        }))
        .expect("initialize request json shape")
    }

    fn init_response(version: u16) -> acp::InitializeResponse {
        initialize_response(&init_request(version))
    }

    #[test]
    fn initialize_answers_v1_regardless_of_requested_version() {
        for requested in [0u16, 1, 2] {
            let response = init_response(requested);
            assert_eq!(response.protocol_version, ProtocolVersion::V1);
        }
    }

    /// Decision D7: image-capable prompts only; no audio/embedded-context,
    /// no MCP transports, no loadSession, no auth methods, takka agentInfo.
    #[test]
    fn initialize_advertises_the_d7_capability_set() {
        let response = init_response(1);
        let caps = &response.agent_capabilities;
        // Stored-session lifecycle is supported: `session/load` replays the
        // checkpoint history, `session/resume` re-attaches live sessions.
        assert!(caps.load_session);
        assert!(caps.session_capabilities.resume.is_some());
        assert!(caps.prompt_capabilities.image);
        assert!(!caps.prompt_capabilities.audio);
        assert!(!caps.prompt_capabilities.embedded_context);
        assert!(!caps.mcp_capabilities.http);
        assert!(!caps.mcp_capabilities.sse);
        // auth: ACP AgentAuthCapabilities defaults to no logout flow; takka
        // reads its own config so no ACP auth methods exist.
        assert!(caps.auth.logout.is_none());
        let agent_info = response.agent_info.expect("agentInfo must be present");
        assert_eq!(agent_info.name, "takka");
        assert_eq!(agent_info.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn initialize_response_serializes_to_the_v1_wire_shape() {
        let value = serde_json::to_value(init_response(1)).expect("serializes");
        assert_eq!(value["protocolVersion"], serde_json::json!(1));
        assert_eq!(
            value["agentCapabilities"]["promptCapabilities"]["image"],
            serde_json::json!(true)
        );
    }

    // ------------------------------------------------------------------
    // T-00 routing (unit level — no I/O): initialize happy path and the
    // honest rejections for methods this phase does not implement.
    // ------------------------------------------------------------------

    fn route(payload: serde_json::Value) -> RouteOutcome {
        let sink: Arc<dyn AcpSink> = Arc::new(VecSink::default());
        AcpService::new(AcpConfig::default(), sink).route_frame(&payload)
    }

    #[test]
    fn initialize_frame_routes_to_a_v1_payload() {
        let frame = serde_json::json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": { "protocolVersion": 1 }
        });
        let RouteOutcome::Reply(response) = route(frame) else {
            panic!("initialize always routes to a reply");
        };
        assert_eq!(response["protocolVersion"], serde_json::json!(1));
    }

    /// `session/load` replays the stored history as `session/update`
    /// notifications (user chunks then assistant chunks) BEFORE the
    /// response, and the response carries the D9 mode surface.
    #[test]
    fn session_load_replays_history_before_the_response() {
        let (acp, _probe) = TestAcp::spawn_with_fake(
            DEFAULT_MAX_SESSIONS,
            Arc::new(|req_rx, evt_tx| {
                let emit = |event: takka_core::Event| {
                    let _ = evt_tx.send(ProtocolEvent::new(None, event));
                };
                while let Ok(request) = req_rx.recv() {
                    match request.request {
                        Request::Init { .. } => {
                            emit(takka_core::Event::InitOk {
                                protocol: "takka".to_owned(),
                                version: 2,
                            });
                        }
                        Request::LoadSession { .. } => {
                            emit(takka_core::Event::HistoryReplay {
                                role: "user".to_owned(),
                                content: "hi".to_owned(),
                            });
                            emit(takka_core::Event::HistoryReplay {
                                role: "assistant".to_owned(),
                                content: "hello".to_owned(),
                            });
                            emit(takka_core::Event::SessionLoaded {
                                session_id: "loaded-1".to_owned(),
                            });
                        }
                        _ => {}
                    }
                }
            }),
        );
        let cwd = std::env::current_dir()
            .expect("process cwd")
            .to_string_lossy()
            .into_owned();
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": SESSION_LOAD,
            "params": { "sessionId": "loaded-1", "cwd": cwd, "mcpServers": [] }
        }));
        let response = acp.response(1);
        assert!(
            response["error"].is_null(),
            "load failed: {}",
            response["error"]
        );
        let modes = &response["result"]["modes"];
        assert_eq!(
            modes["currentModeId"],
            serde_json::json!("default"),
            "{response}"
        );

        // Both replayed turns reached the client as session updates.
        let updates = acp.sink.updates();
        assert_eq!(updates.len(), 2, "updates: {updates:?}");
        assert_eq!(
            updates[0]["params"]["update"]["sessionUpdate"],
            serde_json::json!("user_message_chunk")
        );
        assert_eq!(
            updates[0]["params"]["update"]["content"]["text"],
            serde_json::json!("hi")
        );
        assert_eq!(
            updates[1]["params"]["update"]["sessionUpdate"],
            serde_json::json!("agent_message_chunk")
        );
        assert_eq!(
            updates[1]["params"]["update"]["content"]["text"],
            serde_json::json!("hello")
        );
        acp.close_and_join();
    }

    /// `session/load` of a session the engine cannot find answers
    /// invalid-params with the engine's reason, and the capacity slot is
    /// freed again (the next `session/new` still succeeds).
    #[test]
    fn session_load_of_an_unknown_session_frees_its_slot() {
        let (acp, _probe) = TestAcp::spawn_with_fake(
            DEFAULT_MAX_SESSIONS,
            Arc::new(|req_rx, evt_tx| {
                let emit = |event: takka_core::Event| {
                    let _ = evt_tx.send(ProtocolEvent::new(None, event));
                };
                while let Ok(request) = req_rx.recv() {
                    match request.request {
                        Request::Init { .. } => {
                            emit(takka_core::Event::InitOk {
                                protocol: "takka".to_owned(),
                                version: 2,
                            });
                        }
                        Request::LoadSession { .. } => {
                            emit(takka_core::Event::Error {
                                message: "session not found: nope-1".to_owned(),
                            });
                        }
                        _ => {}
                    }
                }
            }),
        );
        let cwd = std::env::current_dir()
            .expect("process cwd")
            .to_string_lossy()
            .into_owned();
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": SESSION_LOAD,
            "params": { "sessionId": "nope-1", "cwd": cwd, "mcpServers": [] }
        }));
        let response = acp.response(1);
        assert_eq!(
            response["error"]["code"],
            serde_json::json!(-32602),
            "invalid params: {response}"
        );
        assert!(
            format!("{}", response["error"]["message"]).contains("session not found"),
            "{response}"
        );

        // The failed boot did not leak a capacity slot.
        let session = new_session(&acp, 2);
        assert_eq!(session, "sess-1", "fresh mint after the failed load");
        acp.close_and_join();
    }

    /// `session/resume` of a session still live in this process re-attaches
    /// without any engine traffic (the fake engine would ignore a
    /// `LoadSession` request forever — a successful reply proves it was
    /// never needed).
    #[test]
    fn session_resume_of_a_live_session_reattaches_without_engine_traffic() {
        let (acp, _probe) =
            TestAcp::spawn_with_fake(DEFAULT_MAX_SESSIONS, Arc::new(fake_engine_script));
        let session = new_session(&acp, 1);
        let cwd = std::env::current_dir()
            .expect("process cwd")
            .to_string_lossy()
            .into_owned();
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": SESSION_RESUME,
            "params": { "sessionId": session, "cwd": cwd, "mcpServers": [] }
        }));
        let response = acp.response(2);
        assert!(
            response["error"].is_null(),
            "resume failed: {}",
            response["error"]
        );
        assert_eq!(
            response["result"]["modes"]["currentModeId"],
            serde_json::json!("default")
        );
        assert!(
            acp.sink.updates().is_empty(),
            "resume must not replay history"
        );
        acp.close_and_join();
    }

    /// D11: `session/load` upholds the same single-cwd contract as
    /// `session/new` — a mismatched cwd is refused with the actionable
    /// restart error before any engine boots.
    #[test]
    fn session_load_refuses_a_mismatched_cwd() {
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": SESSION_LOAD,
            "params": { "sessionId": "loaded-1", "cwd": "/definitely/not/the/process/cwd", "mcpServers": [] }
        }));
        let response = acp.response(1);
        assert_eq!(
            response["error"]["code"],
            serde_json::json!(-32602),
            "{response}"
        );
        assert!(
            format!("{}", response["error"]["message"]).contains("restart the agent"),
            "{response}"
        );
        acp.close_and_join();
    }

    /// An adopted `sess-N` id reserves its number: later minted ids skip
    /// past it instead of colliding with the loaded session.
    #[test]
    fn session_load_adopts_the_client_id_without_future_collision() {
        let (acp, _probe) = TestAcp::spawn_with_fake(
            DEFAULT_MAX_SESSIONS,
            Arc::new(|req_rx, evt_tx| {
                let emit = |event: takka_core::Event| {
                    let _ = evt_tx.send(ProtocolEvent::new(None, event));
                };
                while let Ok(request) = req_rx.recv() {
                    match request.request {
                        Request::Init { .. } => {
                            emit(takka_core::Event::InitOk {
                                protocol: "takka".to_owned(),
                                version: 2,
                            });
                        }
                        Request::LoadSession { session_id } => {
                            emit(takka_core::Event::SessionLoaded { session_id });
                        }
                        _ => {}
                    }
                }
            }),
        );
        let cwd = std::env::current_dir()
            .expect("process cwd")
            .to_string_lossy()
            .into_owned();
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": SESSION_LOAD,
            "params": { "sessionId": "sess-3", "cwd": cwd, "mcpServers": [] }
        }));
        let response = acp.response(1);
        assert!(
            response["error"].is_null(),
            "load failed: {}",
            response["error"]
        );

        let session = new_session(&acp, 2);
        assert!(
            session != "sess-3",
            "minted id must not collide with the adopted one: {session}"
        );
        acp.close_and_join();
    }

    /// D9: an unadvertised mode id is rejected as invalid params and names
    /// the modes the client may actually use.
    #[test]
    fn session_set_mode_rejects_unknown_modes() {
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        let session = new_session(&acp, 1);
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": SESSION_SET_MODE,
            "params": { "sessionId": session, "modeId": "turbo" }
        }));
        let response = acp.response(2);
        let error = &response["error"];
        assert_eq!(error["code"], serde_json::json!(-32602), "{response}");
        let data = &error["data"];
        assert_eq!(
            data["available"],
            serde_json::json!(["default", "plan", "yolo"])
        );
        assert_eq!(data["requested"], serde_json::json!("turbo"));
        acp.close_and_join();
    }

    /// D9: switching an idle session's mode reaches the engine and answers
    /// with an empty result (the ACP `SetSessionModeResponse` shape).
    #[test]
    fn session_set_mode_switches_an_idle_session() {
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        let session = new_session(&acp, 1);
        for (id, mode) in [(2, "plan"), (3, "yolo"), (4, "default")] {
            acp.send(serde_json::json!({
                "jsonrpc": "2.0", "id": id, "method": SESSION_SET_MODE,
                "params": { "sessionId": session, "modeId": mode }
            }));
            let response = acp.response(id);
            assert!(
                response["error"].is_null(),
                "set_mode({mode}) must succeed, got {}",
                response["error"]
            );
            assert!(response["result"].is_object(), "{response}");
        }
        // The session survives the switches: a prompt still works.
        send_prompt(&acp, 5, &session, text_prompt("still alive"));
        let response = acp.response(5);
        assert!(
            response["result"].is_object() || response["error"].is_object(),
            "the session must still answer after mode switches, got {response}"
        );
        acp.close_and_join();
    }

    /// D9: set_mode on a session that does not exist is not-found, never a
    /// silent success (the client would believe the mode changed).
    #[test]
    fn session_set_mode_on_unknown_session_is_not_found() {
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": SESSION_SET_MODE,
            "params": { "sessionId": "sess-nope", "modeId": "plan" }
        }));
        let response = acp.response(2);
        assert_eq!(
            response["error"]["code"],
            serde_json::json!(-32002),
            "resource not found: {response}"
        );
        acp.close_and_join();
    }

    #[test]
    fn unknown_request_is_a_method_not_found() {
        let frame = serde_json::json!({
            "jsonrpc": "2.0", "id": 9, "method": "_takka/test", "params": {}
        });
        let RouteOutcome::Error(error) = route(frame) else {
            panic!("not an ACP method");
        };
        assert_eq!(i32::from(error.code), -32601);
    }

    /// `session/cancel` for an unknown session is silent per spec — routed
    /// (empty reply if a client insists on an id), never an error.
    #[test]
    fn cancel_for_an_unknown_session_is_silent() {
        let frame = serde_json::json!({
            "jsonrpc": "2.0", "id": 11, "method": SESSION_CANCEL,
            "params": { "sessionId": "sess-404" }
        });
        let RouteOutcome::Reply(reply) = route(frame) else {
            panic!("cancel routes to an empty reply");
        };
        assert_eq!(reply, Value::Null);
    }

    // ------------------------------------------------------------------
    // T-02: translator mapping.
    // ------------------------------------------------------------------

    fn take(state: &mut TranslatorState, event: takka_core::Event) -> acp::SessionUpdate {
        state
            .translate(&event)
            .expect("event maps to an update in these scenarios")
    }

    #[test]
    fn thought_chunks_and_answer_chunks_map_to_the_right_tags() {
        for (event, expected_tag, expected_text) in [
            (
                takka_core::Event::Thinking {
                    content: "hmm".to_owned(),
                },
                "agent_thought_chunk",
                "hmm",
            ),
            (
                takka_core::Event::AssistantDelta {
                    content: "answer".to_owned(),
                },
                "agent_message_chunk",
                "answer",
            ),
        ] {
            let mut state = TranslatorState::new();
            let update = state.translate(&event).expect("maps");
            let wire = serde_json::to_value(&update).expect("update serializes");
            assert_eq!(wire["sessionUpdate"], serde_json::json!(expected_tag));
            assert_eq!(wire["content"]["text"], serde_json::json!(expected_text));
        }
    }

    #[test]
    fn tool_started_is_an_in_progress_tool_call() {
        let mut state = TranslatorState::new();
        let update = take(
            &mut state,
            takka_core::Event::ToolStarted {
                tool_call_id: "tclid-1".to_owned(),
                tool_name: "read_file".to_owned(),
                arguments: serde_json::json!({ "path": "x.rs" }),
            },
        );
        let acp::SessionUpdate::ToolCall(tool_call) = update else {
            panic!("expected a tool_call update");
        };
        assert_eq!(tool_call.tool_call_id.to_string(), "tclid-1");
        assert_eq!(tool_call.name.as_deref(), Some("read_file"));
        assert_eq!(tool_call.title, "read_file");
        assert_eq!(tool_call.kind, acp::ToolKind::Read);
        assert_eq!(tool_call.status, acp::ToolCallStatus::InProgress);
        assert_eq!(
            tool_call.raw_input,
            Some(serde_json::json!({ "path": "x.rs" }))
        );
    }

    #[test]
    fn tool_kind_mapping_covers_the_spec_table() {
        let cases = [
            ("read_file", acp::ToolKind::Read),
            ("glob", acp::ToolKind::Search),
            ("grep", acp::ToolKind::Search),
            ("write_file", acp::ToolKind::Edit),
            ("apply_patch", acp::ToolKind::Edit),
            ("run_command", acp::ToolKind::Execute),
            ("run_script", acp::ToolKind::Execute),
            ("fetch", acp::ToolKind::Fetch),
            ("web_search", acp::ToolKind::Fetch),
            ("question", acp::ToolKind::Other),
        ];
        for (tool_name, expected) in cases {
            assert_eq!(tool_kind(tool_name), expected, "{tool_name}");
        }
    }

    #[test]
    fn tool_delta_chunks_accumulate_into_the_stream_buffer() {
        let mut state = TranslatorState::new();
        state
            .translate(&takka_core::Event::ToolStarted {
                tool_call_id: "t1".to_owned(),
                tool_name: "run_command".to_owned(),
                arguments: serde_json::json!({}),
            })
            .expect("start maps");
        for (delta, expected_so_far) in [("ls ", "ls "), ("-la", "ls -la")] {
            let update = take(
                &mut state,
                takka_core::Event::ToolDelta {
                    tool_call_id: "t1".to_owned(),
                    tool_name: "run_command".to_owned(),
                    delta: delta.to_owned(),
                },
            );
            let acp::SessionUpdate::ToolCallUpdate(update) = update else {
                panic!("expected a tool_call_update");
            };
            let content = update.fields.content.expect("content attached");
            assert_eq!(content.len(), 1, "one content block per update");
            let acp::ToolCallContent::Content(block) = &content[0] else {
                panic!("expected embedded text content");
            };
            let acp::ContentBlock::Text(text) = &block.content else {
                panic!("expected a text block");
            };
            assert_eq!(text.text, expected_so_far);
        }
    }

    #[test]
    fn tool_output_success_completes_and_failure_fails_with_error_text() {
        let mut state = TranslatorState::new();
        state
            .translate(&takka_core::Event::ToolStarted {
                tool_call_id: "t2".to_owned(),
                tool_name: "run_command".to_owned(),
                arguments: serde_json::json!({}),
            })
            .expect("start maps");
        let update = take(
            &mut state,
            takka_core::Event::ToolOutput {
                tool_name: "run_command".to_owned(),
                output: serde_json::json!(Value::Null),
                error: None,
                tool_call_id: "t2".to_owned(),
                duration_ms: 12,
                success: true,
                truncated: false,
            },
        );
        let acp::SessionUpdate::ToolCallUpdate(update) = update else {
            panic!("expected a tool_call_update");
        };
        assert_eq!(update.fields.status, Some(acp::ToolCallStatus::Completed));

        let update = state
            .translate(&takka_core::Event::ToolOutput {
                tool_name: "run_command".to_owned(),
                output: serde_json::json!(Value::Null),
                error: Some("no such file".to_owned()),
                tool_call_id: "t3".to_owned(),
                duration_ms: 3,
                success: false,
                truncated: false,
            })
            .expect("failure maps");
        let acp::SessionUpdate::ToolCallUpdate(update) = update else {
            panic!("expected a tool_call_update");
        };
        assert_eq!(update.fields.status, Some(acp::ToolCallStatus::Failed));
        let content = update.fields.content.expect("error text attached");
        let acp::ToolCallContent::Content(block) = &content[0] else {
            panic!("expected embedded text content");
        };
        let acp::ContentBlock::Text(text) = &block.content else {
            panic!("expected a text block");
        };
        assert_eq!(text.text, "no such file");
    }

    #[test]
    fn truncated_output_surfaces_in_the_title() {
        let mut state = TranslatorState::new();
        let update = state
            .translate(&takka_core::Event::ToolOutput {
                tool_name: "read_file".to_owned(),
                output: serde_json::json!("tail"),
                error: None,
                tool_call_id: "t4".to_owned(),
                duration_ms: 1,
                success: true,
                truncated: true,
            })
            .expect("maps");
        let acp::SessionUpdate::ToolCallUpdate(update) = update else {
            panic!("expected a tool_call_update");
        };
        let title = update.fields.title.expect("truncation note attached");
        assert!(title.contains("truncated"));
    }

    #[test]
    fn non_stream_events_map_to_nothing() {
        let mut state = TranslatorState::new();
        let events = [
            takka_core::Event::Done {
                result: "ok".to_owned(),
            },
            takka_core::Event::Warning {
                message: "w".to_owned(),
            },
            takka_core::Event::ConfirmationRequest {
                action: "run".to_owned(),
                description: "x".to_owned(),
                risk_level: "MEDIUM".to_owned(),
            },
            takka_core::Event::SessionStarted,
        ];
        for event in events {
            assert!(
                state.translate(&event).is_none(),
                "{event:?} must not leak as a session update"
            );
        }
    }

    /// The prompt-turn loop wraps updates in `session/update` notifications;
    /// pin the envelope shape so the wiring builds on a stable wire form.
    #[test]
    fn session_update_notification_wire_shape_is_stable() {
        let mut state = TranslatorState::new();
        let update = state
            .translate(&takka_core::Event::AssistantDelta {
                content: "z".to_owned(),
            })
            .expect("maps");
        let notification = acp::SessionNotification::new(acp::SessionId::new("sess-1"), update);
        let wire = serde_json::to_value(&notification).expect("serializes");
        assert_eq!(wire["sessionId"], serde_json::json!("sess-1"));
        assert_eq!(
            wire["update"]["sessionUpdate"],
            serde_json::json!("agent_message_chunk")
        );
        assert_eq!(wire["update"]["content"]["text"], serde_json::json!("z"));
    }

    // ------------------------------------------------------------------
    // T-03/T-04: session lifecycle + capacity — hermetic integration over
    // `serve_over` (injected reader + in-memory sink; no stdio, no ambient
    // config, no real provider). The pinned provider `http://127.0.0.1:9`
    // refuses connections instantly, so turns fail fast.
    // ------------------------------------------------------------------

    /// Every poll in these tests is bounded by this deadline so a
    /// regression can never hang the suite.
    const POLL_TIMEOUT: Duration = Duration::from_secs(10);

    fn deadline() -> Instant {
        Instant::now() + POLL_TIMEOUT
    }

    /// In-memory sink: records every frame the service writes.
    #[derive(Default)]
    struct VecSink {
        lines: Mutex<Vec<Value>>,
    }

    impl AcpSink for VecSink {
        fn send_line(&self, frame: &Value) {
            self.lines.lock().expect("sink lock").push(frame.clone());
        }
    }

    impl VecSink {
        /// Newest-first lookup of a JSON-RPC response by request id.
        fn response_for(&self, id: i64) -> Option<Value> {
            let lines = self.lines.lock().expect("sink lock");
            lines
                .iter()
                .rev()
                .find(|frame| frame.get("id") == Some(&serde_json::json!(id)))
                .cloned()
        }

        /// All `session/update` notifications seen so far.
        fn updates(&self) -> Vec<Value> {
            let lines = self.lines.lock().expect("sink lock");
            lines
                .iter()
                .filter(|frame| frame.get("method").and_then(Value::as_str) == Some(SESSION_UPDATE))
                .cloned()
                .collect()
        }

        /// Bounded wait for the first frame matching `predicate`.
        fn wait_until(&self, predicate: impl Fn(&Value) -> bool, what: &str) -> Value {
            let deadline = deadline();
            loop {
                let hit = self
                    .lines
                    .lock()
                    .expect("sink lock")
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
    struct ChanReader {
        pending: Vec<u8>,
        pos: usize,
        rx: std::sync::mpsc::Receiver<String>,
        eof: bool,
    }

    impl ChanReader {
        fn new() -> (Sender<String>, Self) {
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
    struct TestAcp {
        tx: Option<Sender<String>>,
        sink: Arc<VecSink>,
        serve: std::thread::JoinHandle<std::io::Result<()>>,
    }

    impl TestAcp {
        fn spawn(max_sessions: usize) -> Self {
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
        fn spawn_probed(max_sessions: usize) -> (Self, Arc<AtomicUsize>) {
            Self::spawn_with_fake(max_sessions, Arc::new(fake_engine_script))
        }

        /// T-05 generalization: the same probed seam with any scripted
        /// fake engine, so a test can stage its own turn behavior
        /// (cancel terminals, permission gates, recorded requests).
        fn spawn_with_fake(
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

        fn send(&self, frame: Value) {
            self.tx
                .as_ref()
                .expect("connection open")
                .send(format!("{frame}\n"))
                .expect("serve loop alive");
        }

        /// Feed one raw line — valid JSON or not (the parse-error helper).
        fn send_line(&self, line: &str) {
            self.tx
                .as_ref()
                .expect("connection open")
                .send(format!("{line}\n"))
                .expect("serve loop alive");
        }

        /// Wait (bounded) for the JSON-RPC response with this request id.
        fn response(&self, id: i64) -> Value {
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
        fn close_and_join(mut self) {
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
    fn hermetic_config_path() -> String {
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

    fn new_session(acp: &TestAcp, id: i64) -> String {
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

    fn send_prompt(acp: &TestAcp, id: i64, session: &str, prompt: Value) {
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": SESSION_PROMPT,
            "params": { "sessionId": session, "prompt": prompt }
        }));
    }

    fn text_prompt(text: &str) -> Value {
        serde_json::json!([{ "type": "text", "text": text }])
    }

    /// T-03: `session/new` with the process cwd boots the session engine
    /// (a successful reply implies the internal `InitOk` wait) and
    /// advertises the D9 mode set with `default` current.
    #[test]
    fn session_new_boots_an_engine_and_advertises_modes() {
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        let session = new_session(&acp, 1);
        assert!(session.starts_with("sess-"), "session id: {session}");

        // Re-send the exact same wire frame and re-read nothing: the
        // response for id 1 was already produced, meaning boot finished.
        // Assert the D9 mode state on the recorded reply instead.
        let response = acp.response(1);
        let modes = &response["result"]["modes"];
        assert_eq!(modes["currentModeId"], serde_json::json!("default"));
        let ids: Vec<&str> = modes["availableModes"]
            .as_array()
            .expect("availableModes array")
            .iter()
            .map(|m| m["id"].as_str().expect("mode id"))
            .collect();
        assert_eq!(ids, vec!["default", "plan", "yolo"]);
        acp.close_and_join();
    }

    /// T-03 core scenario: with the provider pinned to `127.0.0.1:9`, a
    /// prompt turn fails cleanly (JSON-RPC error, fast), and the SECOND
    /// prompt still works — no poisoned session.
    #[test]
    fn prompt_turn_fails_cleanly_and_the_session_survives_for_a_second_prompt() {
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        let session = new_session(&acp, 1);

        send_prompt(&acp, 2, &session, text_prompt("say hi"));
        let first = acp.response(2);
        assert!(
            !first["error"].is_null(),
            "unreachable provider must surface as a JSON-RPC error: {first}"
        );
        assert!(
            !first["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "the error carries the engine's message"
        );
        // A cleanly failed turn streams no phantom answer chunks.
        let phantom_chunks = acp.sink.updates().iter().any(|frame| {
            frame["params"]["update"]["sessionUpdate"] == serde_json::json!("agent_message_chunk")
        });
        assert!(
            !phantom_chunks,
            "a failed turn must not leak message chunks"
        );

        send_prompt(&acp, 3, &session, text_prompt("and again"));
        let second = acp.response(3);
        assert!(
            !second["error"].is_null(),
            "second prompt must also complete (failing again, not hanging): {second}"
        );
        assert_ne!(
            second["error"]["message"],
            serde_json::json!("session busy"),
            "the second prompt must not be swallowed by the busy gate"
        );
        acp.close_and_join();
    }

    /// T-03 (D11): a `session/new` whose cwd differs from the process cwd
    /// is rejected with invalid_params and the actionable restart sentence.
    #[test]
    fn session_new_with_a_foreign_cwd_is_rejected_with_an_actionable_error() {
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        let foreign = std::env::temp_dir().join(format!(
            "takka-acp-not-the-cwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": SESSION_NEW,
            "params": { "cwd": foreign.to_string_lossy(), "mcpServers": [] }
        }));
        let response = acp.response(1);
        assert_eq!(response["error"]["code"], serde_json::json!(-32602));
        let text = serde_json::to_string(&response["error"]).expect("error serializes");
        assert!(
            text.contains("restart the agent"),
            "actionable message: {text}"
        );
        acp.close_and_join();
    }

    /// T-03: `resource_link` content is refused — only text + image are
    /// advertised (D7).
    #[test]
    fn prompt_with_a_resource_link_is_rejected_as_unsupported() {
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        let session = new_session(&acp, 1);
        send_prompt(
            &acp,
            2,
            &session,
            serde_json::json!([
                { "type": "resource_link", "name": "notes", "uri": "file:///tmp/notes.md" }
            ]),
        );
        let response = acp.response(2);
        assert_eq!(response["error"]["code"], serde_json::json!(-32602));
        let text = serde_json::to_string(&response["error"]).expect("error serializes");
        assert!(text.contains("unsupported content type"), "{text}");
        acp.close_and_join();
    }

    /// T-03: prompting an unknown session is `session not found`.
    #[test]
    fn prompt_for_an_unknown_session_is_session_not_found() {
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        send_prompt(&acp, 1, "sess-404", text_prompt("hi"));
        let response = acp.response(1);
        // acp::Error::session_not_found → JSON-RPC ResourceNotFound (-32002).
        assert_eq!(response["error"]["code"], serde_json::json!(-32002));
        assert_eq!(
            response["error"]["data"]["sessionId"],
            serde_json::json!("sess-404")
        );
        acp.close_and_join();
    }

    /// T-04: capacity = 1 — the second `session/new` gets the typed
    /// `session capacity reached` rejection with the capacity in `data`.
    #[test]
    fn session_new_over_capacity_is_rejected() {
        let acp = TestAcp::spawn(1);
        let _first = new_session(&acp, 1);

        let cwd = std::env::current_dir()
            .expect("process cwd")
            .to_string_lossy()
            .into_owned();
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": SESSION_NEW,
            "params": { "cwd": cwd, "mcpServers": [] }
        }));
        let response = acp.response(2);
        assert_eq!(response["error"]["code"], serde_json::json!(-32600));
        assert_eq!(
            response["error"]["data"]["reason"],
            serde_json::json!("session capacity reached")
        );
        assert_eq!(response["error"]["data"]["capacity"], serde_json::json!(1));

        // The refused session consumed no slot of its own — the live one
        // still prompts (failing against the pinned provider, as always).
        let first = new_session(&acp, 1);
        send_prompt(&acp, 3, &first, text_prompt("still alive"));
        let response = acp.response(3);
        assert!(!response["error"].is_null(), "turn completes: {response}");
        assert_ne!(
            response["error"]["message"],
            serde_json::json!("session busy"),
            "the refused session/new must not have disturbed the live one"
        );
        acp.close_and_join();
    }

    // ------------------------------------------------------------------
    // T-07: EOF teardown — cancel, drain, bounded join, clean exit.
    // ------------------------------------------------------------------

    /// Scripted fake engine (the T-05-brief seam, deterministic):
    /// `Init` → `InitOk`; `Run` → one assistant delta, then park until
    /// `Cancel` (→ terminal `Error{"cancelled"}` — the real engine's
    /// cancel terminal) or channel close (→ exit — the real engine's EOF
    /// exit path).
    fn fake_engine_script(
        req_rx: std::sync::mpsc::Receiver<ProtocolRequest>,
        evt_tx: std::sync::mpsc::Sender<ProtocolEvent>,
    ) {
        fake_engine_with_gate(req_rx, evt_tx, false);
    }

    /// Same fake, but the run also emits a `ConfirmationRequest` (the
    /// engine-side shape of a permission gate) before parking.
    fn fake_confirmation_engine_script(
        req_rx: std::sync::mpsc::Receiver<ProtocolRequest>,
        evt_tx: std::sync::mpsc::Sender<ProtocolEvent>,
    ) {
        fake_engine_with_gate(req_rx, evt_tx, true);
    }

    /// Shared fake-engine body. `gate` stages a `ConfirmationRequest`
    /// mid-run so the teardown can be observed from behind an engine
    /// parked awaiting client word.
    fn fake_engine_with_gate(
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
    fn d12_fake_engine(kind: Option<takka_core::events::TurnLimitKind>) -> FakeEngine {
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

    /// D12: the engine's limit kinds map onto the ACP stop reasons instead
    /// of degrading to `end_turn` (the whole point of the additive event).
    #[test]
    fn turn_limit_kinds_map_to_acp_stop_reasons() {
        for (kind, expected) in [
            (takka_core::events::TurnLimitKind::MaxTokens, "max_tokens"),
            (
                takka_core::events::TurnLimitKind::MaxTurnRequests,
                "max_turn_requests",
            ),
        ] {
            let (acp, _probe) =
                TestAcp::spawn_with_fake(DEFAULT_MAX_SESSIONS, d12_fake_engine(Some(kind)));
            let session = new_session(&acp, 1);
            send_prompt(&acp, 2, &session, text_prompt("go"));
            let response = acp.response(2);
            assert_eq!(
                response["result"]["stopReason"],
                serde_json::json!(expected),
                "limit {kind:?} must surface as stopReason {expected}, got {response}"
            );
            acp.close_and_join();
        }
    }

    /// D12 negative: a turn with no limit event still ends `end_turn` — the
    /// mapping must not invent a limit where none was reported.
    #[test]
    fn turn_without_a_limit_event_is_end_turn() {
        let (acp, _probe) = TestAcp::spawn_with_fake(DEFAULT_MAX_SESSIONS, d12_fake_engine(None));
        let session = new_session(&acp, 1);
        send_prompt(&acp, 2, &session, text_prompt("go"));
        let response = acp.response(2);
        assert_eq!(
            response["result"]["stopReason"],
            serde_json::json!("end_turn"),
            "{response}"
        );
        acp.close_and_join();
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
    fn t05_fake_engine(
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
    fn wait_for_permission_request(acp: &TestAcp) -> Value {
        acp.sink.wait_until(
            |frame| frame.get("method").and_then(Value::as_str) == Some(SESSION_REQUEST_PERMISSION),
            "the session/request_permission request",
        )
    }

    /// Every non-`Init` request the fake engine received (Run,
    /// ConfirmResponse, …), drained without blocking — the response the
    /// test waits for is only written after the engine recorded the
    /// request, so the record is complete when this runs.
    fn seen_requests(receiver: &std::sync::mpsc::Receiver<Request>) -> Vec<Request> {
        receiver.try_iter().collect()
    }

    /// T-05 core scenario: `session/cancel` mid-turn resolves the prompt
    /// with `stopReason: "cancelled"` (never an error) and the session
    /// recovers — the second prompt is answered with `end_turn`.
    #[test]
    fn cancel_mid_turn_resolves_cancelled() {
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<Request>();
        let fake = t05_fake_engine(false, false, seen_tx);
        let (acp, _probe) = TestAcp::spawn_with_fake(DEFAULT_MAX_SESSIONS, fake);
        let session = new_session(&acp, 1);

        send_prompt(&acp, 2, &session, text_prompt("long turn"));
        acp.sink.wait_until(
            |frame| {
                frame.get("method").and_then(Value::as_str) == Some(SESSION_UPDATE)
                    && frame["params"]["update"]["sessionUpdate"]
                        == serde_json::json!("agent_message_chunk")
            },
            "the in-flight turn's assistant delta",
        );

        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "method": SESSION_CANCEL,
            "params": { "sessionId": session }
        }));

        let response = acp.response(2);
        assert!(
            response["error"].is_null(),
            "a cancelled turn is not an error: {response}"
        );
        assert_eq!(
            response["result"]["stopReason"],
            serde_json::json!("cancelled")
        );

        // Session recovery: the second prompt is answered normally.
        send_prompt(&acp, 3, &session, text_prompt("second"));
        let second = acp.response(3);
        assert!(
            second["error"].is_null(),
            "second prompt must run: {second}"
        );
        assert_eq!(
            second["result"]["stopReason"],
            serde_json::json!("end_turn")
        );
        // The engine saw exactly the two runs (no phantom re-runs).
        let runs = seen_requests(&seen_rx)
            .iter()
            .filter(|request| matches!(request, Request::Run { .. }))
            .count();
        assert_eq!(runs, 2, "both prompts reached the engine");
        acp.close_and_join();
    }

    /// T-05 + T-07: `session/cancel` while the turn is parked on a
    /// pending `session/request_permission` — the waiter is denied (the
    /// engine gets `ConfirmResponse(false)`), then the cancel runs to
    /// its `cancelled` terminal. Everything unwinds within the bound —
    /// no dead-lock on the gate.
    #[test]
    fn cancel_during_pending_permission_does_not_deadlock() {
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<Request>();
        let fake = t05_fake_engine(true, false, seen_tx);
        let (acp, _probe) = TestAcp::spawn_with_fake(DEFAULT_MAX_SESSIONS, fake);
        let session = new_session(&acp, 1);

        send_prompt(&acp, 2, &session, text_prompt("gated turn"));
        let permission = wait_for_permission_request(&acp);
        let outbound_id = permission["id"].clone();

        // Cancel instead of answering the permission.
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "method": SESSION_CANCEL,
            "params": { "sessionId": session }
        }));

        let response = acp.response(2);
        assert_eq!(
            response["result"]["stopReason"],
            serde_json::json!("cancelled")
        );
        // The gate was freed with a deny: the engine saw the false
        // confirmation before the cancel terminal.
        assert!(
            seen_requests(&seen_rx).iter().any(|request| matches!(
                request,
                Request::ConfirmResponse {
                    approved: false,
                    ..
                }
            )),
            "a cancelled permission gate must deny (approved=false)"
        );
        // The abandoned outbound request never got a JSON-RPC reply —
        // the deny is internal, not a response frame to the client.
        // (The request frame itself carries the id; a reply would be a
        // frame with that id and no method.)
        let replied = acp
            .sink
            .lines
            .lock()
            .expect("sink lock")
            .iter()
            .any(|frame| frame.get("id") == Some(&outbound_id) && frame.get("method").is_none());
        assert!(!replied, "no reply for the abandoned permission frame");

        acp.close_and_join();
    }

    /// T-05/T-06 integration, allow path: the gate's wire shape (D8) and
    /// the full round trip — client answers `allow_once` with the echoed
    /// id → engine receives `ConfirmResponse { approved: true }` → run
    /// completes with `stopReason: end_turn`.
    #[test]
    fn permission_allow_and_confirm_response_flow() {
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<Request>();
        let fake = t05_fake_engine(true, true, seen_tx);
        let (acp, _probe) = TestAcp::spawn_with_fake(DEFAULT_MAX_SESSIONS, fake);
        let session = new_session(&acp, 1);

        send_prompt(&acp, 2, &session, text_prompt("gated turn"));
        let permission = wait_for_permission_request(&acp);

        // Wire shape: envelope + sessionId + toolCall identity/kind/title
        // + the default allow_once/reject_once option pair.
        assert_eq!(permission["jsonrpc"], serde_json::json!("2.0"));
        assert_eq!(
            permission["params"]["sessionId"],
            serde_json::json!(session)
        );
        assert_eq!(
            permission["params"]["toolCall"]["toolCallId"],
            serde_json::json!("acp-perm-1")
        );
        assert_eq!(
            permission["params"]["toolCall"]["kind"],
            serde_json::json!("execute")
        );
        assert_eq!(
            permission["params"]["toolCall"]["title"],
            serde_json::json!("delete the build dir")
        );
        let options = permission["params"]["options"]
            .as_array()
            .expect("options array");
        assert_eq!(options.len(), 2);
        assert_eq!(options[0]["optionId"], serde_json::json!("allow_once"));
        assert_eq!(options[1]["optionId"], serde_json::json!("reject_once"));

        // The client answers allow_once, echoing the request id.
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": permission["id"],
            "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } }
        }));

        let response = acp.response(2);
        assert!(response["error"].is_null(), "allow flow: {response}");
        assert_eq!(
            response["result"]["stopReason"],
            serde_json::json!("end_turn")
        );
        assert!(
            seen_requests(&seen_rx)
                .iter()
                .any(|request| matches!(request, Request::ConfirmResponse { approved: true, .. })),
            "the engine must receive ConfirmResponse(approved=true)"
        );
        acp.close_and_join();
    }

    /// T-05/T-06 integration, reject path: `reject_once` routes
    /// `approved: false` to the engine (fail-closed mapping).
    #[test]
    fn permission_reject_routes_false() {
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<Request>();
        let fake = t05_fake_engine(true, true, seen_tx);
        let (acp, _probe) = TestAcp::spawn_with_fake(DEFAULT_MAX_SESSIONS, fake);
        let session = new_session(&acp, 1);

        send_prompt(&acp, 2, &session, text_prompt("gated turn"));
        let permission = wait_for_permission_request(&acp);
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": permission["id"],
            "result": { "outcome": { "outcome": "selected", "optionId": "reject_once" } }
        }));

        let response = acp.response(2);
        assert_eq!(
            response["result"]["stopReason"],
            serde_json::json!("end_turn")
        );
        assert!(
            seen_requests(&seen_rx).iter().any(|request| matches!(
                request,
                Request::ConfirmResponse {
                    approved: false,
                    ..
                }
            )),
            "the engine must receive ConfirmResponse(approved=false)"
        );
        acp.close_and_join();
    }

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

    /// T-07 core scenario: EOF with two live sessions — one mid-turn
    /// (fake engine parked inside `Run`), one idle — cancels the turn to
    /// its terminal, lets both engines exit on channel close, reaps both
    /// threads via the bounded join (probe == engine count), and returns
    /// `Ok(())` within the test bound.
    #[test]
    fn eof_joins_engines_and_returns_clean() {
        let (acp, probe) = TestAcp::spawn_probed(DEFAULT_MAX_SESSIONS);
        let first = new_session(&acp, 1);
        let _second = new_session(&acp, 2);

        // A turn in flight on the first session (the fake engine parked
        // mid-run once its delta hit the sink).
        send_prompt(&acp, 10, &first, text_prompt("long turn"));
        acp.sink.wait_until(
            |frame| {
                frame.get("method").and_then(Value::as_str) == Some(SESSION_UPDATE)
                    && frame["params"]["update"]["sessionUpdate"]
                        == serde_json::json!("agent_message_chunk")
            },
            "the fake turn's assistant delta",
        );

        // Client gone: cancel → drain → bounded join → clean return.
        acp.close_and_join();
        assert_eq!(
            probe.load(Ordering::SeqCst),
            2,
            "both session engines must be joined at EOF"
        );
    }

    /// T-07, T-05-pending variant: a turn parked behind a confirmation
    /// gate (`ConfirmationRequest` emitted, engine awaiting the client's
    /// word) must not wedge the EOF teardown — the cancel reaches it, the
    /// turn ends, both engines are joined, `serve_over` returns clean.
    /// (The full permission-gate variant — a denied outbound
    /// `session/request_permission` waiter — needs the T-05 permission
    /// bridge; this pins the same teardown behavior at the confirmation
    /// boundary, through the real adapter paths.)
    #[test]
    fn eof_during_pending_confirmation_unblocks_the_turn() {
        let (tx, mut reader) = ChanReader::new();
        let sink = Arc::new(VecSink::default());
        let probe = Arc::new(AtomicUsize::new(0));
        let sink_for_serve = sink.clone();
        let probe_for_serve = probe.clone();
        let serve = std::thread::Builder::new()
            .name("acp-serve-test".into())
            .spawn(move || {
                serve_over_test(
                    &mut reader,
                    sink_for_serve,
                    AcpConfig {
                        max_sessions: DEFAULT_MAX_SESSIONS,
                        config_path: Some(hermetic_config_path()),
                    },
                    Arc::new(fake_confirmation_engine_script),
                    probe_for_serve,
                )
            })
            .expect("spawn the serve thread");
        let acp = TestAcp {
            tx: Some(tx),
            sink,
            serve,
        };

        let session = new_session(&acp, 1);
        send_prompt(&acp, 2, &session, text_prompt("gated turn"));
        acp.sink.wait_until(
            |frame| {
                frame.get("method").and_then(Value::as_str) == Some(SESSION_UPDATE)
                    && frame["params"]["update"]["sessionUpdate"]
                        == serde_json::json!("agent_message_chunk")
            },
            "the gated turn's assistant delta",
        );

        // EOF while the turn is parked on the gate: teardown must unwind
        // everything and still come back clean within the bound.
        acp.close_and_join();
        assert_eq!(
            probe.load(Ordering::SeqCst),
            1,
            "the gated session's engine must be joined at EOF"
        );
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
