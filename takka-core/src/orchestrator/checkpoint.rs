//! Session checkpoint wiring (P1-3).
//!
//! The orchestrator owns an optional [`SessionStore`]. When one is set:
//! - every run opens (or reuses) the session document (`session_begin`);
//! - the terminal state is persisted (`session_terminal` — success,
//!   failure, or cancellation);
//! - `Orchestrator::checkpoint()` snapshots the current state + message
//!   history, so hosts can also checkpoint at tool boundaries.
//!
//! Without a store, all of this is a no-op — the orchestrator stays
//! usable exactly as before.

use std::sync::Mutex;

use super::{Orchestrator, OrchestratorState};
use crate::events::Event;
use crate::session::{AgentSession, SessionStore};
use crate::sync;
use crate::AgenticError;

impl Orchestrator {
    // ------------------------------------------------------------------
    // Configuration
    // ------------------------------------------------------------------

    /// Attach a checkpoint store. Subsequent runs persist session
    /// documents under it.
    pub fn set_session_store(&mut self, store: SessionStore) {
        self.session_store = Some(Mutex::new(store));
    }

    /// Remove the checkpoint store (disables persistence).
    pub fn clear_session_store(&mut self) {
        self.session_store = None;
    }

    /// Drop the tracked session id — the next run opens a fresh
    /// session document (REPL turn separation). The session cost
    /// accumulator (P-07) resets with it: a new session starts at zero.
    pub fn reset_session(&mut self) {
        *sync::lock(&self.session_id) = None;
        self.reset_session_cost();
    }

    /// Id of the session being tracked, if any.
    pub fn session_id(&self) -> Option<String> {
        sync::lock(&self.session_id).clone()
    }

    /// Adopt a caller-supplied session id (ACP `session/load` / hosts with
    /// a stable session identity): the next run checkpoints under this id
    /// instead of minting a fresh uuid.
    pub fn adopt_session_id(&mut self, id: impl Into<String>) {
        *sync::lock(&self.session_id) = Some(id.into());
    }

    // ------------------------------------------------------------------
    // Lifecycle hooks (called from `run.rs`)
    // ------------------------------------------------------------------

    /// Run preamble: emit `SessionStarted` and open the checkpoint
    /// document (a new session id per run, unless one is already
    /// tracked). Without a store this is just the event emission —
    /// runs stay untracked.
    pub(crate) fn session_begin(&self) {
        self.events.emit(Event::SessionStarted);

        if self.session_store.is_none() {
            return;
        }

        // Adopt an existing id (resume) or mint a fresh one per run.
        let mut current = sync::lock(&self.session_id);
        if current.is_none() {
            *current = Some(uuid::Uuid::new_v4().to_string());
        }
        let id = current.clone().unwrap();
        drop(current);

        self.write_checkpoint("created", None);
        tracing::debug!(session = %id, "session opened");
    }

    /// Run epilogue: terminal state on the P1-2 machine, lifecycle
    /// event, and the final checkpoint.
    pub(crate) fn session_terminal(&self, result: &Result<String, AgenticError>) {
        match result {
            Ok(content) => {
                self.set_state(OrchestratorState::Completed);
                self.write_checkpoint("completed", Some(content));
                self.events.emit(Event::SessionCompleted {
                    result: content.clone(),
                });
            }
            // Cancellation is user-initiated, not a failure.
            Err(e) if !matches!(e, AgenticError::Cancelled) => {
                self.set_state(OrchestratorState::Failed);
                self.write_checkpoint("failed", None);
                self.events.emit(Event::SessionFailed {
                    message: e.to_string(),
                });
            }
            Err(_) => {
                self.set_state(OrchestratorState::Cancelled);
                self.write_checkpoint("cancelled", None);
            }
        }
    }

    // ------------------------------------------------------------------
    // Checkpointing
    // ------------------------------------------------------------------

    /// Snapshot the current lifecycle state + conversation into the
    /// session document. `extra_result` carries the final answer on
    /// success so the checkpoint matches the `SessionCompleted` event.
    ///
    /// Best-effort: checkpoint failures are logged, never surfaced —
    /// persistence must not break a live run.
    pub fn checkpoint(&self) {
        let state = self.get_state();
        self.write_checkpoint(state.as_str(), None);
    }

    fn write_checkpoint(&self, state: &str, extra_result: Option<&str>) {
        let store_guard = match &self.session_store {
            Some(s) => sync::lock(s),
            None => return,
        };

        let session_id = match sync::lock(&self.session_id).clone() {
            Some(id) => id,
            None => return,
        };

        // Load-or-create: keeps created_at/created ordering stable
        // across checkpoints of the same session.
        let mut session = match store_guard.load(&session_id) {
            Ok(existing) => existing,
            Err(_) => AgentSession::new(&session_id, self.model.clone()),
        };

        let mut messages: Vec<crate::memory::Message> =
            sync::lock(&self.memory).get_messages().to_vec();
        if let Some(final_text) = extra_result {
            if let Some(last) = messages.last_mut() {
                if last.role.as_str() == "assistant" && last.content.is_empty() {
                    last.content = final_text.to_string();
                }
            }
        }
        session.checkpoint(state, messages);
        // P-07: persist the session cost accumulator with every checkpoint.
        session.session_cost_usd = self.session_cost_usd();
        // O-05: persist the plan-then-execute step list in the `plan` slot.
        let plan_steps = sync::lock(&self.plan_steps).clone();
        if !plan_steps.is_empty() {
            session.plan = Some(serde_json::json!(plan_steps));
        }
        if let Err(e) = store_guard.save(&session) {
            tracing::warn!(error = %e, "session checkpoint failed");
        }
    }

    // ------------------------------------------------------------------
    // Resume
    // ------------------------------------------------------------------

    /// Resume a stored session onto this orchestrator: attaches the
    /// store, replaces the conversation history with the checkpoint's,
    /// adopts its session id, and sets the model. The lifecycle state
    /// machine restarts from `Created` on the next `run` — the
    /// checkpoint's recorded state is informational (what the previous
    /// run died doing). (Distinct from [`Orchestrator::resume`], the
    /// pause-release.)
    ///
    /// Runs of tool turns are compressed into one short `HistoryReplay`
    /// summary each (P-05) so a tool-heavy resume shows what was already
    /// explored. The summaries are replay events only — memory receives
    /// the exact stored messages, unchanged and in order.
    pub fn resume_session(
        &mut self,
        store: SessionStore,
        session_id: &str,
    ) -> Result<(), AgenticError> {
        let session = store.load(session_id)?;
        let message_count = session.messages.len();
        {
            let mut mem = sync::lock(&self.memory);
            mem.clear();
            let mut iter = session.messages.into_iter().peekable();
            // call-id -> target, from each assistant's tool_calls.
            let mut targets: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            while let Some(message) = iter.next() {
                match message.role.clone() {
                    crate::memory::MessageRole::User | crate::memory::MessageRole::Assistant => {
                        for call in &message.metadata.tool_calls {
                            if let Some(target) = target_from_call_args(&call.function.arguments) {
                                targets.insert(call.id.clone(), target);
                            }
                        }
                        self.events.emit(Event::HistoryReplay {
                            role: message.role.as_str().to_string(),
                            content: message.content.clone(),
                        });
                        mem.add_message(message);
                    }
                    crate::memory::MessageRole::Tool { .. } => {
                        // Consecutive tool turns collapse into one
                        // summary replay event (UI context only).
                        let mut run = vec![message];
                        while matches!(
                            iter.peek().map(|m| m.role.clone()),
                            Some(crate::memory::MessageRole::Tool { .. })
                        ) {
                            run.push(iter.next().unwrap());
                        }
                        self.events.emit(Event::HistoryReplay {
                            role: "assistant".to_string(),
                            content: summarize_tool_run(&run, &targets),
                        });
                        for message in run {
                            mem.add_message(message);
                        }
                    }
                    crate::memory::MessageRole::System => {
                        mem.add_message(message);
                    }
                }
            }
        }
        // P-07: restore the session cost accumulator so the session
        // ceiling keeps counting across resumes instead of resetting.
        self.set_session_cost_usd(session.session_cost_usd);
        // O-05: restore the plan from the checkpoint slot and announce it
        // so the next request prepends the plan state again.
        if let Some(plan) = session.plan.as_ref().and_then(|v| v.as_array()) {
            let steps: Vec<String> = plan
                .iter()
                .filter_map(|step| step.as_str().map(str::to_string))
                .collect();
            if !steps.is_empty() {
                self.events.emit(Event::PlanCreated {
                    steps_total: steps.len(),
                });
                self.set_plan_steps(steps);
            }
        }
        self.model = session.model;
        self.session_store = Some(Mutex::new(store));
        *sync::lock(&self.session_id) = Some(session.session_id.clone());
        tracing::info!(
            session = %session.session_id,
            messages = message_count,
            died_in = %session.state,
            "session resumed"
        );
        Ok(())
    }
}

/// Max summary lines per tool run; the rest collapse into a "+M more" tail.
const TOOL_RUN_SUMMARY_LINES: usize = 20;

/// Target string of one tool call from its JSON arguments
/// (`command` > `path` > `file_path` — the safety engine's convention).
fn target_from_call_args(arguments: &str) -> Option<String> {
    let args: serde_json::Value = serde_json::from_str(arguments).ok()?;
    ["command", "path", "file_path"]
        .iter()
        .find_map(|key| args.get(key).and_then(|v| v.as_str()))
        .map(str::to_string)
}

/// One compact summary of a consecutive run of tool messages (P-05):
/// `tool_name(target) → outcome` lines, capped at
/// [`TOOL_RUN_SUMMARY_LINES`]. A result starting with `Tool error:` or
/// `Blocked:` shows its first line as the outcome; everything else
/// reads `ok`.
fn summarize_tool_run(
    run: &[crate::memory::Message],
    targets: &std::collections::HashMap<String, String>,
) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(run.len());
    for message in run {
        let name = match &message.role {
            crate::memory::MessageRole::Tool { tool_name, .. } => tool_name.clone(),
            _ => continue,
        };
        let call_id = message.metadata.tool_call_id.as_deref().unwrap_or_default();
        let target = targets.get(call_id).map(String::as_str).unwrap_or("-");
        let outcome = if let Some(err) = message
            .content
            .strip_prefix("Tool error: ")
            .or_else(|| message.content.strip_prefix("Blocked: "))
        {
            let first_line = err.lines().next().unwrap_or("");
            let mut line: String = first_line.chars().take(80).collect();
            if line.chars().count() < first_line.chars().count() {
                line.push('\u{2026}');
            }
            format!("error: {line}")
        } else {
            "ok".to_string()
        };
        lines.push(format!("{name}({target}) → {outcome}"));
    }

    if lines.len() > TOOL_RUN_SUMMARY_LINES {
        let overflow = lines.len() - TOOL_RUN_SUMMARY_LINES;
        lines.truncate(TOOL_RUN_SUMMARY_LINES);
        lines.push(format!("\u{2026} +{overflow} more"));
    }
    format!(
        "[replayed tool work: {} calls]\n{}",
        run.len(),
        lines.join("\n")
    )
}

#[cfg(test)]
mod resume_summary_tests {
    use super::*;
    use crate::providers::{
        ChatResponse, LLMProvider, ProviderError, ProviderResult, StreamResult,
    };
    use crate::tool_registry::ToolRegistry;
    use std::sync::Arc;

    struct UnusedProvider;
    impl LLMProvider for UnusedProvider {
        fn provider_type(&self) -> &str {
            "unused"
        }
        fn provider_id(&self) -> &str {
            "unused"
        }
        fn chat(&self, _r: crate::providers::ChatRequest) -> ProviderResult<ChatResponse> {
            Err(ProviderError::new("unused"))
        }
        fn chat_stream(
            &self,
            _r: crate::providers::ChatRequest,
        ) -> StreamResult<crate::providers::ChatChunk, ProviderError> {
            Err(ProviderError::new("unused"))
        }
    }

    fn tool_message(name: &str, call_id: &str, result: &str) -> crate::memory::Message {
        crate::memory::Message::tool(name, call_id, result)
    }

    fn assistant_with_calls(
        calls: Vec<crate::providers::ToolCallResponse>,
    ) -> crate::memory::Message {
        crate::memory::Message::assistant_with_tool_calls("", calls)
    }

    fn call(id: &str, name: &str, args: &serde_json::Value) -> crate::providers::ToolCallResponse {
        crate::providers::ToolCallResponse {
            id: id.to_string(),
            call_type: "function".into(),
            function: crate::providers::ToolCallFunction {
                name: name.to_string(),
                arguments: args.to_string(),
            },
        }
    }

    fn result_for(call_id: &str, ok: bool) -> String {
        if ok {
            format!("{{\"ok\": true, \"call\": \"{call_id}\"}}")
        } else {
            "Tool error: operation timed out".to_string()
        }
    }

    #[test]
    fn resume_replays_a_summary_per_tool_run_and_keeps_memory_identical() {
        let dir = std::env::temp_dir().join(format!("resume_sum_{}", uuid::Uuid::new_v4()));
        let store = SessionStore::new(&dir);

        // Simulate a run's messages: user → assistant(tool_calls) → 3 tools → assistant.
        let user = crate::memory::Message::user("explore");
        let assistant = assistant_with_calls(vec![
            call(
                "c1",
                "stub_read",
                &serde_json::json!({"path": "src/main.rs"}),
            ),
            call(
                "c2",
                "stub_read",
                &serde_json::json!({"path": "src/lib.rs"}),
            ),
            call(
                "c3",
                "stub_read",
                &serde_json::json!({"path": "src/err.rs"}),
            ),
        ]);
        let t1 = tool_message("stub_read", "c1", &result_for("c1", true));
        let t2 = tool_message("stub_read", "c2", &result_for("c2", true));
        let t3 = tool_message("stub_read", "c3", &result_for("c3", false));
        let final_assistant = crate::memory::Message::assistant("done");

        let stored = vec![
            user.clone(),
            assistant.clone(),
            t1.clone(),
            t2.clone(),
            t3.clone(),
            final_assistant.clone(),
        ];
        let mut session = crate::session::AgentSession::new("resume-sum-1", "m");
        session.checkpoint("completed", stored.clone());
        store.save(&session).unwrap();

        let mut orch = Orchestrator::new(Arc::new(UnusedProvider), ToolRegistry::new());
        let events: Arc<std::sync::Mutex<Vec<Event>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = events.clone();
        orch.on_event(move |e| sink.lock().unwrap().push(e));
        orch.resume_session(store, "resume-sum-1").expect("resume");

        // Memory is byte-identical to the stored messages.
        let loaded: Vec<crate::memory::Message> = sync::lock(&orch.memory).get_messages().to_vec();
        assert_eq!(loaded.len(), stored.len(), "message count preserved");
        for (a, b) in loaded.iter().zip(stored.iter()) {
            assert_eq!(
                serde_json::to_string(a).unwrap(),
                serde_json::to_string(b).unwrap(),
                "memory message must be byte-identical"
            );
        }

        // One summary replay per tool run, with target + outcome lines.
        let replays: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Event::HistoryReplay { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        let summary = replays
            .iter()
            .find(|c| c.contains("replayed tool work"))
            .expect("tool-run summary replayed");
        assert!(summary.contains("stub_read(src/main.rs) → ok"), "{summary}");
        assert!(summary.contains("stub_read(src/lib.rs) → ok"), "{summary}");
        assert!(
            summary.contains("stub_read(src/err.rs) → error: operation timed out"),
            "{summary}"
        );
        // User + assistant text replays still happen.
        assert!(replays.iter().any(|c| c == "explore"));
        assert!(replays.iter().any(|c| c == "done"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_run_summary_caps_at_20_lines_with_overflow_tail() {
        let mut messages = vec![assistant_with_calls(Vec::new())];
        let mut calls = Vec::new();
        for i in 0..44 {
            let id = format!("c{i}");
            calls.push(call(
                &id,
                "stub_read",
                &serde_json::json!({"path": format!("f{i}.rs")}),
            ));
            messages.push(tool_message("stub_read", &id, &result_for(&id, true)));
        }
        messages[0] = assistant_with_calls(calls);

        let summary = summarize_tool_run(&messages[1..], &std::collections::HashMap::new());
        let body_lines: Vec<&str> = summary.lines().skip(1).collect();
        assert_eq!(body_lines.len(), 21, "20 lines + overflow tail: {summary}");
        assert!(body_lines[20].contains("+24 more"), "{summary}");
        assert!(summary.contains("44 calls"));
    }

    #[test]
    fn target_from_call_args_prefers_command_then_path() {
        assert_eq!(
            target_from_call_args(r#"{"command":"ls -la","path":"x"}"#).as_deref(),
            Some("ls -la")
        );
        assert_eq!(
            target_from_call_args(r#"{"file_path":"a.rs"}"#).as_deref(),
            Some("a.rs")
        );
        assert_eq!(target_from_call_args("not json"), None);
    }
}
