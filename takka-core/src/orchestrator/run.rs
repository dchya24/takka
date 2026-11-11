//! The agent loop: `run` (sync) and `run_stream` (async).
//!
//! Both follow the same shape:
//! 1. push the user message into memory
//! 2. enter the loop with iteration cap + cancel check
//! 3. autocompact if needed
//! 4. build the request, call the provider, accumulate response
//! 5. if there are tool calls → execute them and continue
//! 6. otherwise → finalize the assistant message and return

use std::sync::atomic::Ordering;

use crate::memory::Message;
use crate::providers::ChatRequest;
use crate::AgenticError;

use super::{Orchestrator, OrchestratorState};

impl Orchestrator {
    pub fn run(&self, input: &str) -> Result<String, AgenticError> {
        self.run_with_attachments(input, Vec::new())
    }

    /// Same as [`Self::run`] but the user message carries image (or
    /// other) attachments. The orchestrator validates capabilities up
    /// front so a vision-incompatible model fails fast with a clear
    /// error before any provider call.
    pub fn run_with_attachments(
        &self,
        input: &str,
        attachments: Vec<crate::attachments::Attachment>,
    ) -> Result<String, AgenticError> {
        self.with_session_lifecycle(|| self.run_with_attachments_inner(input, attachments))
    }

    /// Session lifecycle envelope shared by the orchestrator's own
    /// `run*` entry points and [`crate::runtime::AgentRuntime`] (P1-1):
    /// emits `SessionStarted`, drives the loop, then `SessionCompleted`
    /// or `SessionFailed` — and lands the terminal state on the P1-2
    /// machine (`Cancelled` is user-initiated, not a failure).
    pub(crate) fn with_session_lifecycle<F>(&self, run: F) -> Result<String, AgenticError>
    where
        F: FnOnce() -> Result<String, AgenticError>,
    {
        self.session_begin();
        let result = run();
        self.session_terminal(&result);
        result
    }

    /// The run loop body. Session lifecycle wiring lives in
    /// [`Self::run_with_attachments`]; `AgentLoop::run` (P1-1) drives
    /// this directly.
    pub(crate) fn run_with_attachments_inner(
        &self,
        input: &str,
        attachments: Vec<crate::attachments::Attachment>,
    ) -> Result<String, AgenticError> {
        if !attachments.is_empty() {
            self.check_attachment_capability(&attachments)?;
        }

        self.memory
            .lock()
            .unwrap()
            .add_message(if attachments.is_empty() {
                Message::user(input)
            } else {
                Message::user_with_attachments(input, attachments)
            });

        let tool_defs = self.tools.tool_definitions();
        let mut iteration: u32 = 0;

        loop {
            iteration += 1;
            // P1-1: iteration boundary — honor pause, then cancellation.
            self.wait_if_paused();
            self.set_state(OrchestratorState::WaitingForModel);
            tracing::debug!(
                iteration,
                max = self.max_iterations,
                model = %self.model,
                "agent loop iteration start (sync)"
            );
            if iteration > self.max_iterations {
                tracing::warn!(
                    max = self.max_iterations,
                    "Agent loop exceeded max_iterations (unreachable backstop)"
                );
                return Err(AgenticError::Provider(format!(
                    "Agent loop exceeded max_iterations ({}). Aborting to prevent runaway.",
                    self.max_iterations
                )));
            }

            // The last allowed iteration is a FORCED finalization: tools
            // are stripped and a "wrap up now" nudge is injected, so the
            // model must produce a text answer. This converts the old
            // hard-abort (which threw away everything the agent found)
            // into a useful final response built from the work already
            // done.
            let finalizing = iteration == self.max_iterations;

            // Warn when approaching the limit (80% threshold): surface a
            // UI notice and inject a transient "start wrapping up" nudge
            // so the model converges naturally before forced finalization.
            let approaching = self.approaching_limit(iteration) && !finalizing;
            if approaching {
                tracing::info!(
                    iteration,
                    max = self.max_iterations,
                    "Approaching max_iterations limit"
                );
                // Emit a warning event so the UI can display it
                self.events.emit(crate::events::Event::System {
                    message: format!(
                        "⚠️ Approaching iteration limit ({}/{})",
                        iteration, self.max_iterations
                    ),
                });
            }

            if self.cancelled() {
                tracing::info!("Agent loop cancelled by user");
                return Err(AgenticError::Cancelled);
            }

            // Fase 2b: steering poll — drained once per boundary, before
            // the next provider call.
            self.drain_steering();

            self.maybe_autocompact();

            let mut messages = self.build_messages();
            if finalizing {
                messages.push(Self::finalization_message());
                self.events.emit(crate::events::Event::System {
                    message: format!(
                        "🛑 Iteration limit reached ({}) — finalizing answer",
                        self.max_iterations
                    ),
                });
            } else if approaching {
                // Transient steering nudge — not saved to memory, so it
                // only affects this one request.
                messages.push(Self::wind_down_message());
            }
            Self::log_request(iteration, &self.model, &messages);
            self.events.emit(crate::events::Event::ModelRequest {
                model: self.model.clone(),
                message_count: messages.len(),
            });
            // On finalization, omit tools entirely so the provider can't
            // return tool calls — the model is forced to answer in text.
            let mut request = ChatRequest::new(&self.model, messages);
            if !finalizing {
                request = request.with_tools(tool_defs.clone());
            }
            if let Some(ref prompt) = self.system_prompt {
                request = request.with_system_prompt(prompt.clone());
            }

            let response = self
                .provider
                .chat(request)
                .map_err(|e| AgenticError::Provider(e.to_string()))?;

            let content = response.message.content.clone().unwrap_or_default();
            Self::log_response(
                iteration,
                &self.model,
                &content,
                &response.message.tool_calls,
                response.usage.as_ref(),
                response.finish_reason.as_deref(),
            );
            if let Some(usage) = &response.usage {
                self.events.emit(crate::events::Event::Usage {
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    cache_read_input_tokens: usage.cache_read_input_tokens,
                    cache_creation_input_tokens: usage.cache_creation_input_tokens,
                });
            }

            // Forced finalization: accept whatever text the model returns
            // as the final answer and terminate. (Tools were stripped, so
            // the response carries no tool calls in practice.)
            if finalizing {
                tracing::info!(
                    iteration,
                    content_len = content.len(),
                    "Forced finalization at max_iterations"
                );
                // D12: the loop cap ended this turn - say so before the terminator.
                self.events.emit(crate::events::Event::TurnLimitExceeded {
                    kind: crate::events::TurnLimitKind::MaxTurnRequests,
                });
                self.clear_loop_detection();
                self.memory
                    .lock()
                    .unwrap()
                    .add_message(Message::assistant(&content));
                self.set_state(OrchestratorState::Completed);
                return Ok(content);
            }

            if !response.message.tool_calls.is_empty() {
                // Emit the LLM's text content as a Thought event so the user
                // can see what the model is thinking/planning before tool execution.
                if !content.is_empty() {
                    self.events.emit(crate::events::Event::Thinking {
                        content: content.clone(),
                    });
                }

                // Loop detection — record every tool call this turn as a
                // (name + arguments) signature. Only the *exact same* call
                // repeated `LOOP_DETECTION_THRESHOLD` times consecutively
                // (across turns) trips the guard; calling the same tool
                // with different arguments (e.g. loading different skills)
                // is legitimate progress and does not count.
                let loop_sigs: Vec<(&str, &str)> = response
                    .message
                    .tool_calls
                    .iter()
                    .map(|tc| (tc.function.name.as_str(), tc.function.arguments.as_str()))
                    .collect();
                if let Some((sig, run)) = self.record_tool_calls_for_loop_detection(&loop_sigs) {
                    tracing::warn!(
                        tool = %sig.tool,
                        args = %sig.args_preview,
                        run = run,
                        threshold = super::LOOP_DETECTION_THRESHOLD,
                        "Loop detected: identical tool call repeated consecutively"
                    );
                    return Err(AgenticError::Provider(format!(
                        "Loop detected: '{}' called {} times consecutively with identical \
                         arguments ({}). The model is repeating itself without making progress. \
                         Aborting to prevent an infinite loop.",
                        sig.tool, run, sig.args_preview,
                    )));
                }

                let tool_calls: Vec<(String, String, String)> = response
                    .message
                    .tool_calls
                    .iter()
                    .map(|tc| {
                        (
                            tc.id.clone(),
                            tc.function.name.clone(),
                            tc.function.arguments.clone(),
                        )
                    })
                    .collect();
                self.handle_tool_calls(&content, &tool_calls);
            } else {
                // No tool calls - model produced a final answer
                // Clear loop detection history for the next user turn
                self.clear_loop_detection();

                // D12: a provider that stopped at its output-token cap is a
                // token limit, not a natural end of turn.
                if is_token_truncated(response.finish_reason.as_deref()) {
                    self.events.emit(crate::events::Event::TurnLimitExceeded {
                        kind: crate::events::TurnLimitKind::MaxTokens,
                    });
                }

                self.memory
                    .lock()
                    .unwrap()
                    .add_message(Message::assistant(&content));

                self.set_state(OrchestratorState::Completed);
                return Ok(content);
            }
        }
    }

    pub async fn run_stream<F>(&self, input: &str, on_chunk: F) -> Result<String, AgenticError>
    where
        F: FnMut(String),
    {
        self.run_stream_with_attachments(input, Vec::new(), on_chunk)
            .await
    }

    /// Streaming variant of [`Self::run_with_attachments`]. The same
    /// capability check runs up front; on failure the loop returns
    /// `AgenticError::Provider` before opening a stream.
    pub async fn run_stream_with_attachments<F>(
        &self,
        input: &str,
        attachments: Vec<crate::attachments::Attachment>,
        on_chunk: F,
    ) -> Result<String, AgenticError>
    where
        F: FnMut(String),
    {
        self.session_begin();
        let result = self
            .run_stream_with_attachments_inner(input, attachments, on_chunk)
            .await;
        self.session_terminal(&result);
        result
    }

    /// The stream loop body. Session lifecycle wiring lives in
    /// [`Self::run_stream_with_attachments`]; `AgentLoop::run_stream`
    /// (P1-1) drives this directly.
    pub(crate) async fn run_stream_with_attachments_inner<F>(
        &self,
        input: &str,
        attachments: Vec<crate::attachments::Attachment>,
        mut on_chunk: F,
    ) -> Result<String, AgenticError>
    where
        F: FnMut(String),
    {
        use std::collections::HashMap;

        use futures::stream::StreamExt;

        if !attachments.is_empty() {
            self.check_attachment_capability(&attachments)?;
        }

        self.memory
            .lock()
            .unwrap()
            .add_message(if attachments.is_empty() {
                Message::user(input)
            } else {
                Message::user_with_attachments(input, attachments)
            });

        let tool_defs = self.tools.tool_definitions();
        let mut iteration: u32 = 0;

        loop {
            iteration += 1;
            // P1-1: iteration boundary — honor pause, then cancellation.
            self.wait_if_paused();
            self.set_state(OrchestratorState::WaitingForModel);
            tracing::debug!(
                iteration,
                max = self.max_iterations,
                model = %self.model,
                "agent loop iteration start (stream)"
            );
            if iteration > self.max_iterations {
                tracing::warn!(
                    max = self.max_iterations,
                    "Agent stream loop exceeded max_iterations (unreachable backstop)"
                );
                return Err(AgenticError::Provider(format!(
                    "Agent loop exceeded max_iterations ({}). Aborting to prevent runaway.",
                    self.max_iterations
                )));
            }

            // The last allowed iteration is a FORCED finalization: tools
            // are stripped and a "wrap up now" nudge is injected, so the
            // model must produce a text answer. See the sync path for
            // the full rationale.
            let finalizing = iteration == self.max_iterations;

            // Warn when approaching the limit (80% threshold): UI notice
            // + transient "start wrapping up" nudge so the model can
            // converge naturally before forced finalization.
            let approaching = self.approaching_limit(iteration) && !finalizing;
            if approaching {
                tracing::info!(
                    iteration,
                    max = self.max_iterations,
                    "Approaching max_iterations limit"
                );
                self.events.emit(crate::events::Event::System {
                    message: format!(
                        "⚠️ Approaching iteration limit ({}/{})",
                        iteration, self.max_iterations
                    ),
                });
            }

            if self.cancelled() {
                tracing::info!("Agent stream loop cancelled by user");
                return Err(AgenticError::Cancelled);
            }

            // Fase 2b: steering poll — drained once per boundary, before
            // the next provider call.
            self.drain_steering();

            self.maybe_autocompact();

            let mut messages = self.build_messages();
            if finalizing {
                messages.push(Self::finalization_message());
                self.events.emit(crate::events::Event::System {
                    message: format!(
                        "🛑 Iteration limit reached ({}) — finalizing answer",
                        self.max_iterations
                    ),
                });
            } else if approaching {
                messages.push(Self::wind_down_message());
            }
            Self::log_request(iteration, &self.model, &messages);
            self.events.emit(crate::events::Event::ModelRequest {
                model: self.model.clone(),
                message_count: messages.len(),
            });
            // On finalization, omit tools entirely so the provider can't
            // return tool calls — the model is forced to answer in text.
            let mut request = ChatRequest::new(&self.model, messages);
            if !finalizing {
                request = request.with_tools(tool_defs.clone());
            }
            request = request.stream();
            if let Some(ref prompt) = self.system_prompt {
                request = request.with_system_prompt(prompt.clone());
            }

            let mut content_buf = String::new();
            let mut tool_calls_map: HashMap<u32, (String, String, String)> = HashMap::new();
            // Providers report usage on the final chunk only (and not all of
            // them do) — remember the last non-empty report of this call.
            let mut stream_usage: Option<crate::providers::ChatUsage> = None;
            // D12: the final chunk of a stream carries the provider's stop
            // reason; remember it so a token-cap stop can be reported.
            let mut stream_finish_reason: Option<String> = None;

            match self.provider.chat_stream(request) {
                Ok(mut stream) => {
                    while let Some(chunk_result) = stream.next().await {
                        match chunk_result {
                            Ok(chunk) => {
                                if !chunk.delta.is_empty() {
                                    on_chunk(chunk.delta.clone());
                                    self.events.emit(crate::events::Event::ModelChunk {
                                        delta: chunk.delta.clone(),
                                    });
                                    content_buf.push_str(&chunk.delta);
                                }
                                if chunk.usage.is_some() {
                                    stream_usage = chunk.usage.clone();
                                }
                                if chunk.finish_reason.is_some() {
                                    stream_finish_reason = chunk.finish_reason.clone();
                                }
                                for tc in chunk.tool_calls {
                                    let entry =
                                        tool_calls_map.entry(tc.index).or_insert_with(|| {
                                            (String::new(), String::new(), String::new())
                                        });
                                    if let Some(id) = tc.id {
                                        entry.0 = id;
                                    }
                                    if let Some(name) = tc.function_name {
                                        entry.1 = name;
                                    }
                                    if let Some(args) = tc.function_arguments {
                                        entry.2.push_str(&args);
                                    }
                                }
                            }
                            Err(e) => {
                                return Err(AgenticError::Provider(e.to_string()));
                            }
                        }
                    }
                }
                Err(e) => {
                    return Err(AgenticError::Provider(e.to_string()));
                }
            }

            let accumulated_tool_calls: Vec<(String, String, String)> = {
                let mut indices: Vec<u32> = tool_calls_map.keys().copied().collect();
                indices.sort();
                indices
                    .into_iter()
                    .map(|i| {
                        let (id, name, args) = tool_calls_map.remove(&i).unwrap();
                        (id, name, args)
                    })
                    .collect()
            };

            Self::log_stream_response(
                iteration,
                &self.model,
                &content_buf,
                &accumulated_tool_calls,
            );
            if let Some(usage) = stream_usage {
                self.events.emit(crate::events::Event::Usage {
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    cache_read_input_tokens: usage.cache_read_input_tokens,
                    cache_creation_input_tokens: usage.cache_creation_input_tokens,
                });
            }

            // Forced finalization: accept whatever text the model returns
            // as the final answer and terminate. (Tools were stripped, so
            // the response carries no tool calls in practice.)
            if finalizing {
                tracing::info!(
                    iteration,
                    content_len = content_buf.len(),
                    "Forced finalization at max_iterations (stream)"
                );
                // D12: the loop cap ended this turn - say so before the terminator.
                self.events.emit(crate::events::Event::TurnLimitExceeded {
                    kind: crate::events::TurnLimitKind::MaxTurnRequests,
                });
                self.clear_loop_detection();
                self.memory
                    .lock()
                    .unwrap()
                    .add_message(Message::assistant(&content_buf));
                self.set_state(OrchestratorState::Completed);
                return Ok(content_buf);
            }

            if !accumulated_tool_calls.is_empty() {
                // Emit the LLM's text content as a Thought event so the user
                // can see what the model is thinking/planning before tool execution.
                if !content_buf.is_empty() {
                    self.events.emit(crate::events::Event::Thinking {
                        content: content_buf.clone(),
                    });
                }

                // Loop detection — see the sync path for the rationale.
                // Only identical (name + args) calls repeated back-to-back
                // across turns trip the guard; different-args calls and
                // same-turn parallel batches are treated as progress.
                let loop_sigs: Vec<(&str, &str)> = accumulated_tool_calls
                    .iter()
                    .map(|(_, name, args)| (name.as_str(), args.as_str()))
                    .collect();
                if let Some((sig, run)) = self.record_tool_calls_for_loop_detection(&loop_sigs) {
                    tracing::warn!(
                        tool = %sig.tool,
                        args = %sig.args_preview,
                        run = run,
                        threshold = super::LOOP_DETECTION_THRESHOLD,
                        "Loop detected: identical tool call repeated consecutively"
                    );
                    return Err(AgenticError::Provider(format!(
                        "Loop detected: '{}' called {} times consecutively with identical \
                         arguments ({}). The model is repeating itself without making progress. \
                         Aborting to prevent an infinite loop.",
                        sig.tool, run, sig.args_preview,
                    )));
                }

                self.handle_tool_calls_parallel(&content_buf, &accumulated_tool_calls)
                    .await;
                continue;
            }

            // No tool calls - model produced a final answer
            // Clear loop detection history for the next user turn
            self.clear_loop_detection();

            // D12: a provider that stopped at its output-token cap is a
            // token limit, not a natural end of turn.
            if is_token_truncated(stream_finish_reason.as_deref()) {
                self.events.emit(crate::events::Event::TurnLimitExceeded {
                    kind: crate::events::TurnLimitKind::MaxTokens,
                });
            }

            self.memory
                .lock()
                .unwrap()
                .add_message(Message::assistant(&content_buf));

            self.set_state(OrchestratorState::Completed);

            return Ok(content_buf);
        }
    }

    /// Internal helper: returns true when cancel was requested.
    pub(super) fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Steering poll (Fase 2b): called once per iteration boundary —
    /// after the pause/cancel guards, before `maybe_autocompact` and the
    /// next provider call. The source is drained exactly once (it
    /// returns the whole ready queue, FIFO); each message is added to
    /// memory as a user message and announced via `Event::Steered`.
    fn drain_steering(&self) {
        if let Some(source) = &self.steering_source {
            for message in source() {
                tracing::debug!(
                    len = message.len(),
                    "steering message injected at iteration boundary"
                );
                self.memory
                    .lock()
                    .unwrap()
                    .add_message(crate::memory::Message::user(message.clone()));
                self.events
                    .emit(crate::events::Event::Steered { content: message });
            }
        }
    }

    /// Pre-flight check: every attachment must be supported by the
    /// active model. Today only image attachments are checked, against
    /// `ModelCapabilities::vision`. Lookup falls back to the conservative
    /// default (`vision: false`) for unknown models, so the safe
    /// behavior is to reject rather than silently send.
    pub(super) fn check_attachment_capability(
        &self,
        attachments: &[crate::attachments::Attachment],
    ) -> Result<(), AgenticError> {
        let needs_vision = attachments
            .iter()
            .any(|a| matches!(a.kind, crate::attachments::AttachmentKind::Image));
        if !needs_vision {
            return Ok(());
        }
        let caps = crate::capabilities::resolve(&self.model);
        if caps.vision {
            return Ok(());
        }
        Err(AgenticError::Provider(format!(
            "Model '{}' does not support image input. Switch to a vision-capable model \
             (e.g. gpt-4o, gpt-4o-mini, claude-3-5-sonnet, claude-3-5-haiku) via `/models`.",
            self.model
        )))
    }
}

// ---------------------------------------------------------------------------
// Loop instrumentation
// ---------------------------------------------------------------------------
// These associated fns emit structured tracing events at each agent-loop
// boundary so a debug log file captures the full request/response trace:
// what messages were sent, what the model replied, and exactly which tool
// calls (with args) it requested. This is the primary diagnostic surface
// for diagnosing things like the `grep`-called-3x loop-detection abort.
//
// All events use module-path targets (e.g. `takka_core::orchestrator::run`)
// so the file layer's `takka_core=trace` filter picks them up.

impl Orchestrator {
    /// Summarize the request we are about to send to the provider.
    /// Logs each message's role + content length so cleared/truncated
    /// tool results (Layer 2 compression) are visible at a glance.
    fn log_request(iteration: u32, model: &str, messages: &[crate::providers::ChatMessageRequest]) {
        let summary: Vec<String> = messages
            .iter()
            .map(|m| format!("{}:{}", m.role, m.content.len()))
            .collect();
        tracing::debug!(
            iteration,
            model = %model,
            messages = messages.len(),
            roles = %summary.join(" | "),
            "request \u{2192} provider"
        );
    }

    /// Summarize a non-streaming provider response: token usage,
    /// finish reason, content length, and each requested tool call.
    fn log_response(
        iteration: u32,
        model: &str,
        content: &str,
        tool_calls: &[crate::providers::ToolCallResponse],
        usage: Option<&crate::providers::ChatUsage>,
        finish_reason: Option<&str>,
    ) {
        tracing::info!(
            iteration,
            model = %model,
            finish_reason,
            prompt_tokens = usage.map(|u| u.prompt_tokens).unwrap_or(0),
            completion_tokens = usage.map(|u| u.completion_tokens).unwrap_or(0),
            content_len = content.len(),
            tool_calls = tool_calls.len(),
            "response \u{2190} provider"
        );
        for tc in tool_calls {
            tracing::debug!(
                iteration,
                tool = %tc.function.name,
                id = %tc.id,
                args = %Self::truncate_preview(&tc.function.arguments, 300),
                "tool call requested"
            );
        }
    }

    /// Streaming variant: the stream path accumulates tool calls as
    /// `(id, name, args)` triples and doesn't track token usage on the
    /// orchestrator side (the final chunk may carry it, but we don't
    /// surface it here).
    fn log_stream_response(
        iteration: u32,
        model: &str,
        content: &str,
        tool_calls: &[(String, String, String)],
    ) {
        tracing::info!(
            iteration,
            model = %model,
            content_len = content.len(),
            tool_calls = tool_calls.len(),
            "response \u{2190} provider (stream)"
        );
        for (_id, name, args) in tool_calls {
            tracing::debug!(
                iteration,
                tool = %name,
                args = %Self::truncate_preview(args, 300),
                "tool call requested"
            );
        }
    }

    /// Truncate a string to `max` bytes on a UTF-8 char boundary,
    /// appending a marker with how many bytes were elided. Used so a
    /// giant tool-call arguments blob doesn't drown the log line.
    fn truncate_preview(s: &str, max: usize) -> String {
        if s.len() <= max {
            return s.to_string();
        }
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\u{2026} (+{} chars)", &s[..end], s.len() - end)
    }

    // ── Iteration-limit steering nudges ────────────────────────
    //
    // Both are injected as transient trailing `user` messages (NOT saved
    // to memory) so they steer only the request they're appended to.
    // `user` role is used because every provider accepts a trailing user
    // message and the slice is guaranteed well-formed by
    // `crate::context::builder::sanitize_for_provider`.

    /// Soft nudge injected once the run crosses ~80% of the iteration
    /// budget. Asks the model to start converging so it ideally
    /// finishes on its own before the forced finalization turn.
    fn wind_down_message() -> crate::providers::ChatMessageRequest {
        crate::providers::ChatMessageRequest::user(
            "[system] You are approaching the tool-call iteration limit. \
             Start wrapping up: finish only the essential remaining steps, \
             then give your final answer to the user.",
        )
    }

    /// Hard nudge injected on the final allowed iteration. Paired with
    /// stripping tools from the request, this forces a text answer so
    /// the user gets a result instead of a hard abort.
    fn finalization_message() -> crate::providers::ChatMessageRequest {
        crate::providers::ChatMessageRequest::user(
            "[system] You have reached the tool-call iteration limit and can \
             no longer call tools. Using only what you have already learned, \
             provide your best final answer to the user now.",
        )
    }
}

/// Does the provider's stop reason mean "cut off at the output-token cap"?
///
/// D12 (ACP Fase 4): such a stop is a turn limit, not a natural end of turn,
/// so the loop announces it via `Event::TurnLimitExceeded`. Providers spell
/// it differently - OpenAI `length`, Anthropic `max_tokens`, Google
/// `MAX_TOKENS` / `max_output_tokens` - hence the case-insensitive compare.
fn is_token_truncated(finish_reason: Option<&str>) -> bool {
    match finish_reason.map(str::to_ascii_lowercase) {
        Some(reason) => matches!(
            reason.as_str(),
            "length" | "max_tokens" | "max_output_tokens"
        ),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::is_token_truncated;

    #[test]
    fn token_truncation_matches_all_provider_spellings() {
        // OpenAI uses `length`, Anthropic `max_tokens`, Google
        // `MAX_TOKENS` - all three must be recognized as a token-cap stop
        // (case-insensitive).
        assert!(is_token_truncated(Some("length")));
        assert!(is_token_truncated(Some("max_tokens")));
        assert!(is_token_truncated(Some("MAX_TOKENS")));
        assert!(is_token_truncated(Some("max_output_tokens")));
    }

    #[test]
    fn natural_stops_are_not_token_truncation() {
        assert!(!is_token_truncated(Some("stop")));
        assert!(!is_token_truncated(Some("end_turn")));
        assert!(!is_token_truncated(Some("tool_calls")));
        assert!(!is_token_truncated(None));
    }
}
