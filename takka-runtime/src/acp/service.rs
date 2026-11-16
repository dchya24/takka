use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_client_protocol_schema::v1 as acp;
use serde_json::Value;
use takka_core::runtime::engine::RuntimeEngine;
use takka_core::runtime::protocol::{ProtocolEvent, ProtocolRequest, Request};
use takka_core::runtime::transport::MemoryTransport;

use crate::acp_permission::{self as acp_permission, PermissionAnswer};

use super::*;

// ---------------------------------------------------------------------------
// T-03: the session service — routing + registry.
// ---------------------------------------------------------------------------

/// Result of routing one inbound frame.
#[derive(Debug)]
pub(crate) enum RouteOutcome {
    /// Synchronous success reply (framing loop answers requests only).
    Reply(Value),
    /// Synchronous JSON-RPC error.
    Error(acp::Error),
    /// Accepted but answered later — `session/prompt` spawns a turn thread
    /// that owns the response (the framing loop must stay free to process
    /// `session/cancel` while the turn streams).
    Deferred,
}

/// Router + session registry for one ACP connection.
pub(crate) struct AcpService {
    pub(crate) config: AcpConfig,
    pub(crate) sink: Arc<dyn AcpSink>,
    /// Live sessions keyed by id. Shared with prompt-turn threads (they
    /// clear the busy flag, park the receiver back, and tear dead
    /// sessions down).
    pub(crate) sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    /// Session id source: `sess-1`, `sess-2`, …
    pub(crate) counter: AtomicU64,
    /// T-07: set at EOF teardown under the registry lock, so a `session/new`
    /// boot racing the drain either inserts before it (and gets cancelled +
    /// joined like every other session) or is refused and its engine exits
    /// on its own when the failed session drops the senders.
    pub(crate) closed: AtomicBool,
    /// T-05: pending `session/request_permission` waiters — the permission
    /// bridge between the turn threads, the framing loop (client answers),
    /// and the cancel/teardown paths (deny). Shared because turn threads
    /// outlive the `AcpService` borrow.
    pub(crate) permission_waiters: Arc<PermissionWaiters>,
    /// Test seam (T-05/T-07): scripted fake engines replace real
    /// `RuntimeEngine` boots. `None` in production.
    #[cfg(test)]
    pub(crate) fake_engine: Option<FakeEngine>,
    /// Test probe (T-07): bumped after each engine-thread join in the EOF
    /// teardown. `None` in production.
    #[cfg(test)]
    pub(crate) joined_probe: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

/// Spawn the real session engine thread (production path, also used by
/// seam-less test boots): one [`RuntimeEngine`] over a `MemoryTransport`,
/// exactly like the daemon host.
pub(crate) fn spawn_engine_thread(
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
pub(crate) fn join_bounded(
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

impl AcpService {
    pub(crate) fn new(config: AcpConfig, sink: Arc<dyn AcpSink>) -> Self {
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
    pub(crate) fn handle_frame(&self, frame: &Value) {
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
    pub(crate) fn send_raw(&self, frame: &Value) {
        self.sink.send_line(frame);
    }

    /// Classify one frame:
    ///
    /// - client request (`method` + `id`) → typed method handlers;
    /// - `session/cancel` notifications → cooperative cancel wiring;
    /// - `result`/`error` frames → no agent→client requests exist yet, ignore.
    pub(crate) fn route_frame(&self, frame: &Value) -> RouteOutcome {
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
            // Config-option switches (model / mode selects).
            (Some(SESSION_SET_CONFIG_OPTION), true) => self.session_set_config_option(frame),
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

    /// Config for the session surfaces (model list, context size). Same
    /// precedence as the engine's init: the pinned config path when set,
    /// else the user's ambient config; `None` when nothing loads.
    pub(crate) fn adapter_config(&self) -> Option<takka_core::Config> {
        match self.config.config_path.as_deref() {
            Some(path) => takka_core::Config::load_from_path(path),
            None => takka_core::Config::load(),
        }
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
    pub(crate) fn shutdown(&self) {
        let deadline = Instant::now() + SHUTDOWN_JOIN_TIMEOUT;
        let sessions: Vec<Arc<Session>> = {
            let mut registry = lock(&self.sessions);
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
            .filter_map(|session| lock(&session.turn).take())
            .collect();
        let engine_handles: Vec<std::thread::JoinHandle<()>> = sessions
            .iter()
            .filter_map(|session| lock(&session.engine).take())
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
    pub(crate) fn deny_pending_permissions(&self) {
        self.permission_waiters.deny_all();
    }

    /// Route a client response frame (result or error, no method) to the
    /// pending permission waiter it answers (T-05). The client's outcome
    /// maps through `acp_permission::outcome_approves` — allow variants
    /// approve, everything else (reject, cancelled, `non_exhaustive`
    /// newcomers) denies. An error-shaped answer denies (fail-closed).
    /// A frame answering no known waiter is dropped silently — per
    /// JSON-RPC a stray response is never an error and never a reply.
    pub(crate) fn route_client_response(&self, frame: &Value) {
        let answered: acp::Response<acp::RequestPermissionResponse> =
            match serde_json::from_value(frame.clone()) {
                Ok(answered) => answered,
                // Neither a well-formed result nor error frame: not a
                // shape the bridge asked for — ignore.
                Err(_) => return,
            };
        let (request_id, answer) = match answered {
            acp::Response::Result { id, result } => {
                (id, acp_permission::outcome_answer(&result.outcome))
            }
            acp::Response::Error { id, .. } => (id, PermissionAnswer::Deny),
        };
        // Outbound ids are strings (`acp-perm-N`); other id shapes can
        // never match one and fall through to the same silent drop.
        if let Some(waiter) = self
            .permission_waiters
            .remove(request_id.to_string().as_str())
        {
            let _ = waiter.answer_tx.send(answer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::test_support::*;

    use std::sync::atomic::AtomicUsize;

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

    /// H-02: the session registry is bookkeeping shared by the framing
    /// loop and every turn thread — one panicking request must not turn
    /// every later lookup into a `PoisonError` panic that kills unrelated
    /// sessions. The registry is poisoned in place, then the real routing
    /// paths (`session/new`, `session/prompt`, the EOF teardown) must keep
    /// working.
    #[test]
    fn a_poisoned_session_registry_stays_usable() {
        let sink: Arc<dyn AcpSink> = Arc::new(VecSink::default());
        let service = AcpService::new(
            AcpConfig {
                max_sessions: DEFAULT_MAX_SESSIONS,
                config_path: Some(hermetic_config_path()),
            },
            sink.clone(),
        );

        // Register a live session, then poison the registry behind it.
        let cwd = std::env::current_dir().expect("process cwd");
        let request = acp::NewSessionRequest::new(cwd);
        let first = service.session_new(&request).expect("session/new boots");
        assert_eq!(first.session_id.to_string(), "sess-1");
        poison(&service.sessions);
        assert!(service.sessions.lock().is_err(), "registry is poisoned");

        // Routing lookups recover: a prompt for the live session reaches
        // its engine (the turn thread answers, not a poisoned panic), and
        // an unknown session still gets its typed error.
        let prompt = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": SESSION_PROMPT,
            "params": { "sessionId": "sess-1", "prompt": [{ "type": "text", "text": "hi" }] }
        });
        assert!(matches!(
            service.route_frame(&prompt),
            RouteOutcome::Deferred
        ));
        let unknown = serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": SESSION_PROMPT,
            "params": { "sessionId": "sess-404", "prompt": [{ "type": "text", "text": "hi" }] }
        });
        let RouteOutcome::Error(error) = service.route_frame(&unknown) else {
            panic!("an unknown session must be a typed error");
        };
        assert_eq!(i32::from(error.code), -32002);

        // A capacity check on the recovered registry still holds.
        let second = service.session_new(&request).expect("second session boots");
        assert_eq!(second.session_id.to_string(), "sess-2");

        // The EOF teardown drains the recovered registry and joins.
        service.shutdown();
        assert!(lock(&service.sessions).is_empty());
        assert!(service.closed.load(Ordering::SeqCst));
    }
}
