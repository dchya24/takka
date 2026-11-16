use agent_client_protocol_schema::v1 as acp;
use serde_json::Value;

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
    /// Context-window size reported on `UsageUpdate` (the active model's
    /// `max_tokens`); 0 when unknown.
    context_size: u64,
}

impl TranslatorState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Translator that reports `UsageUpdate.size = context_size` (the
    /// session's active model context window).
    #[must_use]
    pub fn with_context_size(context_size: u64) -> Self {
        Self {
            context_size,
            ..Self::default()
        }
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
            // Context-window telemetry (Zed renders it as the context
            // usage indicator): `used` = the prompt size of the last
            // model call (what is in context), `size` = the active
            // model's context window.
            Event::Usage { prompt_tokens, .. } => Some(acp::SessionUpdate::UsageUpdate(
                acp::UsageUpdate::new(u64::from(*prompt_tokens), self.context_size),
            )),
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
            | Event::SessionModelChanged { .. }
            | Event::CompactionStarted
            | Event::WaitingForUser
            | Event::SessionCompleted { .. } => None,
            // Spend ceiling (`agent.max_cost_usd`): the work paid for so far
            // is still delivered, so the turn surfaces as an ordinary
            // agent message explaining why it stopped early. ACP has no
            // stop reason for a budget cut, and reporting `error` would
            // turn a policy outcome into a failed prompt.
            Event::CostLimitExceeded {
                max_cost_usd,
                spent_usd,
            } => Some(acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(text_block(format!(
                    "Spend ceiling reached (spent ${spent_usd:.4} of \
                     ${max_cost_usd:.4}); the run stopped here. Raise or remove \
                     `agent.max_cost_usd` in the takka config to continue."
                ))),
            )),
            // D12/D9: consumed by the turn loop (stop reason) and the mode
            // surface, never rendered as a stream update.
            Event::TurnLimitExceeded { .. } | Event::PermissionModeChanged { .. } => None,
        }
    }
}

/// Text content block for streamed update chunks.
pub(crate) fn text_block(text: impl Into<String>) -> acp::ContentBlock {
    acp::ContentBlock::Text(acp::TextContent::new(text.into()))
}

/// Text content attached to a tool call.
pub(crate) fn text_tool_content(text: impl Into<String>) -> acp::ToolCallContent {
    acp::ToolCallContent::Content(acp::Content::new(text_block(text)))
}

/// Streamed tool output ends up attached as its payload text.
pub(crate) fn output_display(output: &Value) -> Option<String> {
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

    // ------------------------------------------------------------------
    // T-02: translator mapping.
    // ------------------------------------------------------------------

    fn take(state: &mut TranslatorState, event: takka_core::Event) -> acp::SessionUpdate {
        state
            .translate(&event)
            .expect("event maps to an update in these scenarios")
    }

    #[test]
    fn usage_maps_to_a_usage_update_with_the_session_context_size() {
        let mut state = TranslatorState::with_context_size(198_192);
        let update = take(
            &mut state,
            takka_core::Event::Usage {
                prompt_tokens: 53_000,
                completion_tokens: 1_200,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
        );
        let acp::SessionUpdate::UsageUpdate(usage) = update else {
            panic!("usage must map to a UsageUpdate, got {update:?}");
        };
        assert_eq!(usage.used, 53_000);
        assert_eq!(usage.size, 198_192);
    }

    /// H-06: a budget stop has no ACP stop reason of its own, so it surfaces
    /// as an agent message that names the ceiling and the way out — never as
    /// an error prompt (the work paid for was delivered).
    #[test]
    fn cost_limit_exceeded_maps_to_an_explanatory_agent_message() {
        let mut state = TranslatorState::new();
        let update = take(
            &mut state,
            takka_core::Event::CostLimitExceeded {
                max_cost_usd: 0.50,
                spent_usd: 0.62,
            },
        );
        let wire = serde_json::to_value(&update).expect("update serializes");
        assert_eq!(
            wire["sessionUpdate"],
            serde_json::json!("agent_message_chunk")
        );
        let text = wire["content"]["text"].as_str().expect("text");
        assert!(text.contains("$0.62"), "{text}");
        assert!(text.contains("$0.50"), "{text}");
        assert!(text.contains("agent.max_cost_usd"), "{text}");
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
                target: None,
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
}
