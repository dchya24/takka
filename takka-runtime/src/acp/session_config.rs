use std::sync::atomic::Ordering;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Instant;

use agent_client_protocol_schema::v1 as acp;
use serde_json::Value;
use takka_core::runtime::protocol::{ProtocolRequest, Request};
use takka_core::PermissionMode;

use super::*;

pub(crate) fn unknown_mode_error(mode_id: &str) -> acp::Error {
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

/// takka permission mode -> the ACP mode id advertised on the wire.
pub(crate) fn permission_mode_id(mode: PermissionMode) -> &'static str {
    mode.as_str()
}

/// Config option surfaces for one session (Zed renders `configOptions` in
/// place of the legacy `modes` field, so BOTH selectors live here): the
/// `model` select groups every configured provider's models (value =
/// provider-qualified id), the `mode` select carries the permission modes.
pub(crate) fn build_config_options(
    config: &takka_core::Config,
    current_model: &str,
    current_mode: &str,
) -> Vec<acp::SessionConfigOption> {
    let mut options = Vec::new();
    let groups: Vec<acp::SessionConfigSelectGroup> = config
        .providers
        .iter()
        .filter(|provider| !provider.models.is_empty())
        .map(|provider| {
            let model_options = provider
                .models
                .iter()
                .map(|model| {
                    acp::SessionConfigSelectOption::new(
                        format!("{}/{}", provider.name, model.model),
                        model
                            .display_name
                            .clone()
                            .unwrap_or_else(|| model.model.clone()),
                    )
                })
                .collect();
            acp::SessionConfigSelectGroup::new(
                provider.name.clone(),
                provider.name.clone(),
                model_options,
            )
        })
        .collect();
    if !groups.is_empty() {
        options.push(
            acp::SessionConfigOption::select(
                "model",
                "Model",
                current_model.to_string(),
                acp::SessionConfigSelectOptions::Grouped(groups),
            )
            .category(acp::SessionConfigOptionCategory::Model),
        );
    }
    let mode_options: Vec<acp::SessionConfigSelectOption> = ["default", "plan", "yolo"]
        .iter()
        .map(|mode_id| acp::SessionConfigSelectOption::new(*mode_id, *mode_id))
        .collect();
    options.push(
        acp::SessionConfigOption::select("mode", "Mode", current_mode.to_string(), mode_options)
            .category(acp::SessionConfigOptionCategory::Mode),
    );
    options
}

/// `max_tokens` of the configured model `provider/model` (the
/// `UsageUpdate.size` for a session running it). Falls back to the
/// default memory budget when the model is not configured.
pub(crate) fn model_max_tokens(
    config: &takka_core::Config,
    provider_name: &str,
    model_id: &str,
) -> u32 {
    config
        .providers
        .iter()
        .find(|provider| provider.name.eq_ignore_ascii_case(provider_name))
        .and_then(|provider| {
            provider
                .models
                .iter()
                .find(|model| model.model.eq_ignore_ascii_case(model_id))
        })
        .map(|model| model.max_tokens)
        .unwrap_or(128_000)
}

/// D9: ACP mode id -> takka's permission mode. `None` = not advertised.
pub(crate) fn permission_mode(mode_id: &str) -> Option<PermissionMode> {
    match mode_id {
        "default" => Some(PermissionMode::Default),
        "plan" => Some(PermissionMode::Plan),
        "yolo" => Some(PermissionMode::Yolo),
        _ => None,
    }
}

/// Reply body for a successful `session/load` / `session/resume`: the
/// mode surface (permission modes) for the re-attached session.
pub(crate) fn load_modes_reply(resume: bool) -> Value {
    let modes = session_mode_state();
    if resume {
        serde_json::to_value(acp::ResumeSessionResponse::new().modes(modes))
            .expect("resume response serializes")
    } else {
        serde_json::to_value(acp::LoadSessionResponse::new().modes(modes))
            .expect("load response serializes")
    }
}

impl AcpService {
    /// `session/set_mode` (D9): switch the session's permission mode.
    ///
    /// The session's one turn slot must be free — mode laws are honored at
    /// turn boundaries, so switching mid-turn is refused with `session busy`
    /// instead of being silently deferred to an unknown later moment. The
    /// engine acks with `Event::PermissionModeChanged`, which is awaited
    /// (bounded) before replying so the client never sees a mode it did not
    /// actually get.
    pub(crate) fn session_set_mode(&self, frame: &Value) -> RouteOutcome {
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
        let Some(mode) = permission_mode(request.mode_id.0.as_ref()) else {
            return RouteOutcome::Error(unknown_mode_error(request.mode_id.0.as_ref()));
        };
        match self.switch_mode_on_engine(&session, mode) {
            Ok(()) => RouteOutcome::Reply(
                serde_json::to_value(acp::SetSessionModeResponse::default())
                    .expect("set mode response serializes"),
            ),
            Err(error) => RouteOutcome::Error(error),
        }
    }

    /// Shared engine dance of both mode switch paths (`session/set_mode`
    /// and the `mode` config option): the session's one turn slot must be
    /// free (mode laws are honored at turn boundaries, so switching
    /// mid-turn is refused instead of deferred); the engine acks with
    /// `Event::PermissionModeChanged`, awaited (bounded) so the client
    /// never sees a mode it did not actually get.
    pub(crate) fn switch_mode_on_engine(
        &self,
        session: &Arc<Session>,
        mode: PermissionMode,
    ) -> Result<(), acp::Error> {
        if session.busy.load(Ordering::SeqCst) {
            return Err(busy_error(&session.id));
        }
        // Idle session: the event stream is parked and nobody else can take
        // it (the busy gate above proves no turn is in flight, and a new
        // prompt cannot be dispatched while this handler runs on the framing
        // loop).
        let receiver = match session.events.lock().expect("session events lock").take() {
            Some(receiver) => receiver,
            None => {
                return Err(internal_error("session engine stream is not available"));
            }
        };
        let request_id = session.next_request_id("mode");
        let sent = session.req_tx.send(ProtocolRequest::new(
            request_id,
            Request::SetPermissionMode { mode },
        ));
        if sent.is_err() {
            *session.events.lock().expect("session events lock") = Some(receiver);
            return Err(internal_error("runtime engine died before the mode switch"));
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
        if ack.is_ok() {
            *session.current_mode.lock().expect("current mode lock") =
                permission_mode_id(mode).to_string();
        }
        ack
    }

    /// `session/set_config_option`: switch a config option the session
    /// advertised — the `mode` select maps to a permission-mode switch,
    /// the `model` select to a provider + model switch. The response
    /// carries the full, refreshed option list (the client's source of
    /// truth for the selectors).
    pub(crate) fn session_set_config_option(&self, frame: &Value) -> RouteOutcome {
        let request = match parse_params::<acp::SetSessionConfigOptionRequest>(
            frame,
            SESSION_SET_CONFIG_OPTION,
        ) {
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
        let config_id = request.config_id.0.to_string();
        let Some(value) = request.value.as_value_id() else {
            return RouteOutcome::Error(invalid_params_error(format!(
                "config option {config_id:?} takes a value id (select), got a non-select value"
            )));
        };
        match config_id.as_str() {
            "mode" => {
                let Some(mode) = permission_mode(value.0.as_ref()) else {
                    return RouteOutcome::Error(unknown_mode_error(value.0.as_ref()));
                };
                match self.switch_mode_on_engine(&session, mode) {
                    Ok(()) => {}
                    Err(error) => return RouteOutcome::Error(error),
                }
            }
            "model" => {
                let Some(config) = self.adapter_config() else {
                    return RouteOutcome::Error(internal_error("no provider configuration"));
                };
                let Some((provider_name, model_name)) = value.0.split_once('/') else {
                    return RouteOutcome::Error(invalid_params_error(format!(
                        "invalid model id: {value:?} (expected \"<provider>/<model>\")"
                    )));
                };
                let known = config
                    .providers
                    .iter()
                    .find(|provider| provider.name.eq_ignore_ascii_case(provider_name))
                    .and_then(|provider| {
                        provider
                            .models
                            .iter()
                            .find(|model| model.model.eq_ignore_ascii_case(model_name))
                    });
                if known.is_none() {
                    return RouteOutcome::Error(invalid_params_error(format!(
                        "model '{value}' is not configured"
                    )));
                }
                if session.busy.load(Ordering::SeqCst) {
                    return RouteOutcome::Error(busy_error(&session.id));
                }
                let receiver = match session.events.lock().expect("session events lock").take() {
                    Some(receiver) => receiver,
                    None => {
                        return RouteOutcome::Error(internal_error(
                            "session engine stream is not available",
                        ))
                    }
                };
                let request_id = session.next_request_id("model");
                let sent = session.req_tx.send(ProtocolRequest::new(
                    request_id,
                    Request::SetSessionModel {
                        model: value.0.to_string(),
                    },
                ));
                if sent.is_err() {
                    *session.events.lock().expect("session events lock") = Some(receiver);
                    return RouteOutcome::Error(internal_error(
                        "runtime engine died before the model switch",
                    ));
                }
                let deadline = Instant::now() + MODE_SWITCH_TIMEOUT;
                let mut ack: Result<(), acp::Error> = Err(internal_error(
                    "session engine did not acknowledge the model switch",
                ));
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    match receiver.recv_timeout(remaining) {
                        Ok(event) => match &event.event {
                            takka_core::Event::SessionModelChanged { provider, model } => {
                                *session.current_model.lock().expect("current model lock") =
                                    format!("{provider}/{model}");
                                session.context_size.store(
                                    u64::from(model_max_tokens(&config, provider, model)),
                                    Ordering::SeqCst,
                                );
                                ack = Ok(());
                                break;
                            }
                            takka_core::Event::Error { message } => {
                                ack = Err(invalid_params_error(message.clone()));
                                break;
                            }
                            _ => continue,
                        },
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            ack = Err(internal_error(
                                "runtime engine died during the model switch",
                            ));
                            break;
                        }
                    }
                }
                *session.events.lock().expect("session events lock") = Some(receiver);
                if let Err(error) = ack {
                    return RouteOutcome::Error(error);
                }
            }
            other => {
                return RouteOutcome::Error(invalid_params_error(format!(
                    "unknown config option: {other:?} (available: \"model\", \"mode\")"
                )));
            }
        }
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
        let config = self.adapter_config();
        let options = match &config {
            Some(config) => build_config_options(config, &current_model, &current_mode),
            None => Vec::new(),
        };
        RouteOutcome::Reply(
            serde_json::to_value(acp::SetSessionConfigOptionResponse::new(options))
                .expect("set config option response serializes"),
        )
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
    pub(crate) fn session_load(&self, frame: &Value, resume: bool) -> RouteOutcome {
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
        {
            // Register BEFORE the load: without this the session Arc (and
            // its engine) died when this handler returned, and the first
            // prompt after a successful load answered `session not found`.
            // Same EOF-drain guard as `session/new`.
            let mut registry = self.sessions.lock().expect("session registry lock");
            if self.closed.load(Ordering::SeqCst) {
                return RouteOutcome::Error(internal_error("connection is shutting down"));
            }
            registry.insert(session_id.clone(), session.clone());
        }

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::test_support::*;

    use takka_core::runtime::protocol::{ProtocolEvent, Request};
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

    /// Regression: a successfully loaded session must be usable — the
    /// first prompt after `session/load` reaches the engine and completes
    /// (previously the session died with the handler and the prompt
    /// answered `session not found`).
    #[test]
    fn session_load_is_promptable_afterwards() {
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
                            emit(takka_core::Event::SessionLoaded {
                                session_id: "loaded-1".to_owned(),
                            });
                        }
                        Request::Run { .. } => {
                            emit(takka_core::Event::AssistantDelta {
                                content: "after load".to_owned(),
                            });
                            emit(takka_core::Event::Done {
                                result: "after load".to_owned(),
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
        send_prompt(&acp, 2, "loaded-1", text_prompt("are you alive"));
        let response = acp.response(2);
        assert!(
            response["error"].is_null(),
            "prompt after load must work, got {}",
            response["error"]
        );
        acp.close_and_join();
    }

    /// `session/new` advertises the config-option surface: a `model`
    /// select grouped by provider (current = the active qualified model)
    /// and a `mode` select carrying the permission modes (current =
    /// `default`). Zed renders `configOptions` instead of the legacy
    /// `modes` field, so both selectors must be here.
    #[test]
    fn session_new_advertises_model_and_mode_config_options() {
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        let _session = new_session(&acp, 1);
        let response = acp.response(1);
        let options = &response["result"]["configOptions"];
        assert!(options.is_array(), "configOptions: {response}");
        let model = options
            .as_array()
            .expect("array")
            .iter()
            .find(|option| option["id"] == serde_json::json!("model"))
            .expect("model option")
            .clone();
        assert_eq!(model["name"], serde_json::json!("Model"));
        assert_eq!(model["category"], serde_json::json!("model"));
        assert_eq!(
            model["currentValue"],
            serde_json::json!("hermetic-test/hermetic-model")
        );
        let groups = model["options"].as_array().expect("grouped options");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["group"], serde_json::json!("hermetic-test"));
        assert_eq!(
            groups[0]["options"][0]["value"],
            serde_json::json!("hermetic-test/hermetic-model")
        );
        assert_eq!(
            groups[0]["options"][0]["name"],
            serde_json::json!("hermetic-model")
        );

        let mode = options
            .as_array()
            .expect("array")
            .iter()
            .find(|option| option["id"] == serde_json::json!("mode"))
            .expect("mode option")
            .clone();
        assert_eq!(mode["category"], serde_json::json!("mode"));
        assert_eq!(mode["currentValue"], serde_json::json!("default"));
        let mode_values: Vec<&str> = mode["options"]
            .as_array()
            .expect("flat mode options")
            .iter()
            .map(|value| value["value"].as_str().expect("value id"))
            .collect();
        assert_eq!(mode_values, vec!["default", "plan", "yolo"]);
        acp.close_and_join();
    }

    /// `session/set_config_option` on the `mode` select switches the
    /// permission mode and returns the refreshed option list with the new
    /// current value.
    #[test]
    fn session_set_config_option_switches_the_mode() {
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
                        Request::SetPermissionMode { mode } => {
                            emit(takka_core::Event::PermissionModeChanged { mode });
                        }
                        _ => {}
                    }
                }
            }),
        );
        let session = new_session(&acp, 1);
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": SESSION_SET_CONFIG_OPTION,
            "params": { "sessionId": session, "configId": "mode", "value": "plan" }
        }));
        let response = acp.response(2);
        assert!(
            response["error"].is_null(),
            "switch failed: {}",
            response["error"]
        );
        let mode = response["result"]["configOptions"]
            .as_array()
            .expect("options array")
            .iter()
            .find(|option| option["id"] == serde_json::json!("mode"))
            .expect("mode option")
            .clone();
        assert_eq!(mode["currentValue"], serde_json::json!("plan"));
        acp.close_and_join();
    }

    /// `session/set_config_option` on an unconfigured model is invalid
    /// params naming the miss.
    #[test]
    fn session_set_config_option_rejects_unknown_models() {
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        let session = new_session(&acp, 1);
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": SESSION_SET_CONFIG_OPTION,
            "params": { "sessionId": session, "configId": "model", "value": "no-provider/no-model" }
        }));
        let response = acp.response(2);
        assert_eq!(
            response["error"]["code"],
            serde_json::json!(-32602),
            "{response}"
        );
        assert!(
            format!("{}", response["error"]["message"]).contains("not configured"),
            "{response}"
        );
        acp.close_and_join();
    }

    /// An unknown config id is invalid params naming the available ids.
    #[test]
    fn session_set_config_option_rejects_unknown_ids() {
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        let session = new_session(&acp, 1);
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": SESSION_SET_CONFIG_OPTION,
            "params": { "sessionId": session, "configId": "temperature", "value": "0.7" }
        }));
        let response = acp.response(2);
        assert_eq!(
            response["error"]["code"],
            serde_json::json!(-32602),
            "{response}"
        );
        assert!(
            format!("{}", response["error"]["message"]).contains("temperature"),
            "{response}"
        );
        acp.close_and_join();
    }

    /// `session/set_config_option` on the `model` select reaches the
    /// engine (`Request::SetSessionModel`), and the response reports the
    /// new current value.
    #[test]
    fn session_set_config_option_switches_the_model() {
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
                        Request::SetSessionModel { model } => {
                            let (provider, model) = model.split_once('/').expect("qualified");
                            emit(takka_core::Event::SessionModelChanged {
                                provider: provider.to_owned(),
                                model: model.to_owned(),
                            });
                        }
                        _ => {}
                    }
                }
            }),
        );
        let session = new_session(&acp, 1);
        acp.send(serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": SESSION_SET_CONFIG_OPTION,
                "params": { "sessionId": session, "configId": "model", "value": "hermetic-test/hermetic-model" }
            }));
        let response = acp.response(2);
        assert!(
            response["error"].is_null(),
            "switch failed: {}",
            response["error"]
        );
        let model = response["result"]["configOptions"]
            .as_array()
            .expect("options array")
            .iter()
            .find(|option| option["id"] == serde_json::json!("model"))
            .expect("model option")
            .clone();
        assert_eq!(
            model["currentValue"],
            serde_json::json!("hermetic-test/hermetic-model")
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
}
