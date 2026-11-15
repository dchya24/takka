use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use agent_client_protocol_schema::v1 as acp;
use serde_json::Value;
use takka_core::runtime::protocol::{
    ConnectionCapabilities, InitOverrides, ProtocolEvent, ProtocolRequest, Request,
};

use super::*;

/// A live ACP session: one engine thread behind a `MemoryTransport`
/// (daemon pattern, D6), plus the per-session turn state.
pub(crate) struct Session {
    pub(crate) id: String,
    /// Requests into the session engine. Dropping the last clone closes
    /// the channel and the engine's run loop exits (clean teardown at EOF
    /// and on engine death).
    pub(crate) req_tx: mpsc::Sender<ProtocolRequest>,
    /// The engine's event stream, parked between turns. A turn takes it
    /// out (`Option::take`), drains until the terminal event, then parks
    /// it back — so the session stays reusable across prompts and the
    /// framing loop never races the turn on the same receiver.
    pub(crate) events: Mutex<Option<mpsc::Receiver<ProtocolEvent>>>,
    /// One prompt turn at a time per session (D6): the router flips this
    /// to `true` when accepting a prompt; the turn thread clears it at
    /// the terminal. A second prompt mid-turn gets `session busy`.
    pub(crate) busy: AtomicBool,
    /// Minimum `session/cancel` wiring (T-03): the flag is the T-05 lane's
    /// hook; the request below is the engine's cooperative cancel.
    pub(crate) cancel_requested: AtomicBool,
    /// Per-session protocol request ids (init/run/cancel) — unique within
    /// the session, derived from the session id.
    pub(crate) request_counter: AtomicU64,
    /// The engine thread's handle — parked for the T-07 bounded join at
    /// EOF (taken out exactly once by the teardown).
    pub(crate) engine: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// The in-flight prompt turn's handle (`None` while idle); joined at
    /// EOF. A finished turn leaves the handle in place — joining a
    /// finished thread returns immediately.
    pub(crate) turn: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Current permission mode id (the `mode` config option's value).
    pub(crate) current_mode: Mutex<String>,
    /// Current model as the provider-qualified config id
    /// (`"<provider>/<model>"` — the `model` config option's value).
    pub(crate) current_model: Mutex<String>,
    /// Context-window size reported on `UsageUpdate` (the active model's
    /// `max_tokens`); updated when the model switches.
    pub(crate) context_size: std::sync::atomic::AtomicU64,
}

impl Session {
    pub(crate) fn next_request_id(&self, kind: &str) -> String {
        format!(
            "{}-{}-{}",
            self.id,
            kind,
            self.request_counter.fetch_add(1, Ordering::SeqCst)
        )
    }
}

/// D11: single-cwd adapter — the mismatch error carries the actionable
/// restart sentence verbatim (clients show it to the user).
pub(crate) fn cwd_mismatch_error(requested: &Path, actual: &Path) -> acp::Error {
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
pub(crate) fn capacity_error(capacity: usize) -> acp::Error {
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
pub(crate) fn busy_error(session_id: &str) -> acp::Error {
    acp::Error::new(i32::from(acp::ErrorCode::InvalidRequest), "session busy").data(
        serde_json::json!({
            "method": SESSION_PROMPT,
            "reason": "session busy",
            "sessionId": session_id,
        }),
    )
}

/// Same id law as the checkpoint store: plain identifiers only, never path
/// separators — the id becomes a filename on disk.
pub(crate) fn validate_session_id(session_id: &str) -> Result<(), String> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(format!("invalid session id: {session_id:?}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// D9: takka permission modes as ACP session modes. The current mode is
// always `default` at session creation; `session/set_mode` wiring (into
// `InitOverrides.permission_mode`) lands with a later lane.
// ---------------------------------------------------------------------------

pub(crate) fn session_mode_state() -> acp::SessionModeState {
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

pub(crate) fn mode(id: &str, name: &str, description: &str) -> acp::SessionMode {
    acp::SessionMode::new(acp::SessionModeId::new(id), name).description(description)
}

impl AcpService {
    /// `session/new`: validate the D11 single-cwd contract, claim a
    /// capacity slot (T-04), boot one engine (D6) and wait for `InitOk`.
    /// The response only reaches the client once the engine is usable, so
    /// a successful reply IS the implicit boot confirmation.
    pub(crate) fn session_new(
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
            registry.insert(session_id.clone(), session.clone());
        }

        let mut response = acp::NewSessionResponse::new(acp::SessionId::new(session_id))
            .modes(session_mode_state());
        if let Some(config) = self.adapter_config() {
            let current_model = session
                .current_model
                .lock()
                .expect("current model lock")
                .clone();
            let current_mode = session
                .current_mode
                .lock()
                .expect("current mode lock")
                .clone();
            response = response.config_options(build_config_options(
                &config,
                &current_model,
                &current_mode,
            ));
        }
        Ok(response)
    }

    /// Boot the session engine thread, send `Init` with the adapter's
    /// overrides, and wait (bounded) for `InitOk`. A failure — engine
    /// died, engine error, boot timeout — is propagated as a JSON-RPC
    /// error; nothing is registered, so the capacity slot is free again.
    pub(crate) fn boot_engine(&self, session_id: &str) -> Result<Arc<Session>, acp::Error> {
        let (req_tx, req_rx) = mpsc::channel::<ProtocolRequest>();
        let (evt_tx, evt_rx) = mpsc::channel::<ProtocolEvent>();
        let engine_name = format!("acp-engine-{session_id}");

        // T-07: the handle is parked on the session for the bounded EOF
        // join. The `#[cfg(test)]` seam may substitute a scripted fake
        // engine for the real one (never compiled into production).
        let engine_handle = self.spawn_engine(engine_name, req_rx, evt_tx)?;

        // Config-derived session state: the active model (qualified id)
        // and its context window — the `model` config option's value and
        // the `UsageUpdate.size`. Default mode is `default` (D9).
        let (current_model, context_size) = self
            .adapter_config()
            .and_then(|config| {
                let model = config.active_model()?;
                let provider = config.active_provider()?;
                Some((
                    format!("{}/{}", provider.name, model.model),
                    u64::from(model.max_tokens),
                ))
            })
            .unwrap_or_else(|| ("openai/gpt-4o".to_string(), 128_000));
        let session = Arc::new(Session {
            id: session_id.to_owned(),
            req_tx,
            events: Mutex::new(Some(evt_rx)),
            busy: AtomicBool::new(false),
            cancel_requested: AtomicBool::new(false),
            request_counter: AtomicU64::new(0),
            engine: Mutex::new(Some(engine_handle)),
            turn: Mutex::new(None),
            current_mode: Mutex::new("default".to_string()),
            current_model: Mutex::new(current_model),
            context_size: std::sync::atomic::AtomicU64::new(context_size),
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
    pub(crate) fn spawn_engine(
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

    /// `session/prompt`: map content blocks, gate on busy, then hand the
    /// turn to its own thread (`Deferred`) so the framing loop keeps
    /// processing `session/cancel` while the turn streams.
    pub(crate) fn session_prompt(&self, frame: &Value) -> RouteOutcome {
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
    pub(crate) fn session_cancel(&self, frame: &Value) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::test_support::*;

    use takka_core::runtime::protocol::Request;
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
}
