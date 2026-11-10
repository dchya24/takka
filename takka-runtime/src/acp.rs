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
//!
//! The engine is never modified: sessions (T-03+) each own a `RuntimeEngine`
//! over a `MemoryTransport`, exactly like the daemon host. Nothing in this
//! module writes anything to stdout except framed ACP messages — logs stay on
//! stderr.

use std::io::{BufRead, Write};

use agent_client_protocol_schema::{v1 as acp, ProtocolVersion};
use serde_json::Value;

/// Method names we accept on the agent side of the v1 dialect
/// (mirrors `acp::AGENT_METHOD_NAMES`, kept as consts for framing tests).
const INITIALIZE: &str = "initialize";
const SESSION_NEW: &str = "session/new";
const SESSION_PROMPT: &str = "session/prompt";
const SESSION_CANCEL: &str = "session/cancel";
const SESSION_SET_MODE: &str = "session/set_mode";
const SESSION_LOAD: &str = "session/load";

// ---------------------------------------------------------------------------
// T-00: framing loop + routing.
// ---------------------------------------------------------------------------

/// Serve the ACP connection on stdin/stdout until EOF.
///
/// T-00 scope: one framing loop, one routed responder (`initialize`), typed
/// JSON-RPC errors everywhere else; EOF exits cleanly. Later stories (T-03+)
/// replace the stubs with the session plumbing.
pub fn serve() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();
    let writer = Writer::stdout();

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            // EOF: the Client is gone — settle the process with the same clean
            // exit as the takka-protocol stdio path.
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        let frame: Value = match serde_json::from_str(line.trim()) {
            Ok(frame) => frame,
            Err(_) => {
                // Malformed JSON: a parse error with a null id per JSON-RPC.
                writer.send_response(None, &Err(&acp::Error::parse_error()))?;
                continue;
            }
        };
        writer.handle_frame(&frame);
    }
}

/// Result of routing one inbound frame: an optional response payload.
type FrameResult = Result<Value, acp::Error>;

/// Everything that goes to stdout — responses now; session updates and
/// agent→client requests (`session/request_permission`) arrive with later
/// stories and share this writer.
struct Writer {
    out: std::sync::Mutex<std::io::BufWriter<std::io::Stdout>>,
}

impl Writer {
    fn stdout() -> Self {
        Self {
            out: std::sync::Mutex::new(std::io::BufWriter::new(std::io::stdout())),
        }
    }

    /// Dispatch one inbound frame. Only requests (id present) ever receive
    /// a response; notifications and stray responses are routed but never
    /// answered (JSON-RPC forbids replying to notifications).
    fn handle_frame(&self, frame: &Value) {
        let may_reply = frame.get("id").is_some();
        let outcome = self.route_frame(frame);
        let frame_out = match (&outcome, may_reply) {
            (Ok(result), true) => {
                serde_json::json!({ "jsonrpc": "2.0", "id": frame.get("id"), "result": result })
            }
            (Err(error), true) => {
                serde_json::json!({ "jsonrpc": "2.0", "id": frame.get("id"), "error": error })
            }
            _ => return, // notification / stray response: never answered
        };
        self.send_line(&frame_out)
            .expect("stdout stays writable while a client lives");
    }

    /// Classify one frame:
    ///
    /// - client request (`method` + `id`) → typed method handlers;
    /// - notifications (`method`, no id) → unobserved in this phase
    ///   (`session/cancel` handling lands with the session plumbing);
    /// - `result`/`error` frames → no agent→client requests exist yet, ignore.
    fn route_frame(&self, frame: &Value) -> FrameResult {
        let method = frame.get("method").and_then(Value::as_str);
        let has_id = frame.get("id").is_some();
        match (method, has_id) {
            (Some(INITIALIZE), true) => {
                let request: acp::InitializeRequest = parse_params(frame, INITIALIZE)?;
                Ok(serde_json::to_value(initialize_response(&request))
                    .expect("initialize response serializes"))
            }
            // Session plumbing lands with T-03+; fail loudly so a half-built
            // client sees the honest state instead of waiting on silence.
            (Some(method), true)
                if matches!(
                    method,
                    SESSION_NEW | SESSION_PROMPT | SESSION_SET_MODE | SESSION_LOAD
                ) =>
            {
                Err(before_sessions(method))
            }
            // Mid-turn cancelling; once the turn lives it becomes T-04 work.
            // session/cancel is a notification in v1; if a client carries an
            // id on it anyway, an empty result keeps the JSON-RPC contract
            // intact (real cancel routing becomes T-04 work).
            (Some(SESSION_CANCEL), _) => Ok(Value::Null),
            // Unknown request → method not found; unknown notifications are
            // ignored rather than tearing down the connection.
            (Some(_), true) => Err(acp::Error::method_not_found()),
            _ => Ok(serde_json::json!({ "ignored": true })),
        }
    }

    /// Write a JSON-RPC response (one JSON object per line).
    fn send_response(
        &self,
        id: Option<Value>,
        payload: &Result<&Value, &acp::Error>,
    ) -> std::io::Result<()> {
        let frame = match payload {
            Ok(result) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": error }),
        };
        self.send_line(&frame)
    }

    /// One JSON frame per line — never embedded newlines.
    fn send_line(&self, frame: &Value) -> std::io::Result<()> {
        let mut out = self.out.lock().expect("writer lock");
        serde_json::to_writer(&mut *out, frame)?;
        out.write_all(b"\n")?;
        out.flush()
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

/// Placeholder until T-03 installs the real session handlers.
fn before_sessions(method: &str) -> acp::Error {
    acp::Error::method_not_found().data(
        serde_json::json!({ "method": method, "state": "pre-T-03 (sessions not yet hosted)" }),
    )
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
}

fn agent_implementation() -> acp::Implementation {
    acp::Implementation::new("takka", env!("CARGO_PKG_VERSION")).title("takka")
}

// ---------------------------------------------------------------------------
// T-02: engine `Event` → ACP `SessionUpdate` translator.
// Pure: engine events in, session updates out. One `TranslatorState` per
// session, owned by the prompt-turn loop (T-04).
// ---------------------------------------------------------------------------

/// Carry-through state while one prompt turn streams.
/// (Dead-code provisionally: T-03/T-04 wire the translator into the prompt
/// loop; it is exercised by the unit tests from story T-02 today.)
#[derive(Default)]
#[allow(dead_code)]
pub struct TranslatorState {
    /// Per-tool accumulated streamed output (`tool_delta` chunks are appended
    /// here and each update carries the full accumulated text, because ACP
    /// `ToolCallUpdateFields.content` is overwritten, not extended).
    stream_buffer: std::collections::HashMap<String, String>,
}

impl TranslatorState {
    #[must_use]
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Map an engine event to the ACP session update it should surface.
    ///
    /// Events with no v1 counterpart (local UI concepts, replies the prompt
    /// loop consumes itself) return `None`, matching the phase-1 mapping
    /// table in the spec.
    #[must_use]
    #[allow(dead_code)]
    pub fn translate(&mut self, event: &takka_core::Event) -> Option<acp::SessionUpdate> {
        use takka_core::Event;
        match event {
            Event::Thinking { content } => Some(acp::SessionUpdate::AgentThoughtChunk(
                acp::ContentChunk::new(text_block(content)),
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
            // loop (T-04), permission bridge (T-05), and logging consume.
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
            | Event::CompactionStarted
            | Event::WaitingForUser
            | Event::SessionCompleted { .. } => None,
        }
    }
}

/// Text content block for streamed update chunks.
#[allow(dead_code)]
fn text_block(text: impl Into<String>) -> acp::ContentBlock {
    acp::ContentBlock::Text(acp::TextContent::new(text.into()))
}

/// Text content attached to a tool call.
#[allow(dead_code)]
fn text_tool_content(text: impl Into<String>) -> acp::ToolCallContent {
    acp::ToolCallContent::Content(acp::Content::new(text_block(text)))
}

/// Streamed tool output ends up attached as its payload text.
#[allow(dead_code)]
fn output_display(output: &Value) -> Option<String> {
    match output {
        Value::String(text) => Some(text.clone()),
        Value::Null => None,
        _ => serde_json::to_string_pretty(output).ok(),
    }
}

/// takka tool name → ACP `ToolKind` (spec §4 mapping table).
#[must_use]
#[allow(dead_code)]
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
        assert!(!caps.load_session);
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
    // T-00: routing (initialize happy path + pre-T-03 gaps).
    // route_frame performs no I/O, so calling it via Writer::stdout() here
    // never touches test output.
    // ------------------------------------------------------------------

    fn route(payload: serde_json::Value) -> FrameResult {
        Writer::stdout().route_frame(&payload)
    }

    #[test]
    fn initialize_frame_routes_to_a_v1_payload() {
        let frame = serde_json::json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": { "protocolVersion": 1 }
        });
        let response = route(frame).expect("initialize always routes");
        assert_eq!(response["protocolVersion"], serde_json::json!(1));
    }

    #[test]
    fn session_methods_fail_loudly_before_t03() {
        for method in [SESSION_NEW, SESSION_PROMPT, SESSION_SET_MODE, SESSION_LOAD] {
            let frame = serde_json::json!({
                "jsonrpc": "2.0", "id": 7, "method": method, "params": {}
            });
            let error = route(frame).expect_err("pre-T-03 routes fail loudly");
            let data = serde_json::to_value(&error.data).expect("error data attached");
            assert_eq!(data["method"], serde_json::json!(method));
            assert_eq!(
                data["state"],
                serde_json::json!("pre-T-03 (sessions not yet hosted)")
            );
        }
    }

    #[test]
    fn unknown_request_is_a_method_not_found() {
        let frame = serde_json::json!({
            "jsonrpc": "2.0", "id": 9, "method": "_takka/test", "params": {}
        });
        let _ = route(frame).expect_err("not an ACP method");
    }

    // notifications (session/cancel without id) and stray responses are
    // answered inside handle_frame, which writes to stdout — the e2e smoke
    // story covers those; routing them here would pollute test stdout.

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

    /// T-04 will wrap updates in `session/update` notifications; pin the
    /// envelope shape now so the later wiring builds on a stable wire form.
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
