//! Event types for the takka runtime.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    #[serde(rename = "ready")]
    Ready { protocol: String, version: u32 },

    #[serde(rename = "init_ok")]
    InitOk { protocol: String, version: u32 },

    #[serde(rename = "thinking")]
    Thinking { content: String },

    #[serde(rename = "tool_call")]
    ToolCall {
        tool_name: String,
        arguments: serde_json::Value,
    },

    /// Emitted right before a tool actually executes (after safety passes).
    #[serde(rename = "tool_started")]
    #[serde(rename_all = "camelCase")]
    ToolStarted {
        tool_call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },

    /// Live output chunk from a streaming tool (run_command/run_script).
    #[serde(rename = "tool_delta")]
    #[serde(rename_all = "camelCase")]
    ToolDelta {
        tool_call_id: String,
        tool_name: String,
        delta: String,
    },

    #[serde(rename = "tool_output")]
    #[serde(rename_all = "camelCase")]
    ToolOutput {
        tool_name: String,
        output: serde_json::Value,
        error: Option<String>,
        tool_call_id: String,
        duration_ms: u64,
        success: bool,
        truncated: bool,
    },

    #[serde(rename = "tool_finished")]
    #[serde(rename_all = "camelCase")]
    ToolFinished {
        tool_call_id: String,
        tool_name: String,
        success: bool,
    },

    #[serde(rename = "assistant_delta")]
    AssistantDelta { content: String },

    #[serde(rename = "planning")]
    Planning { task: String },

    #[serde(rename = "warning")]
    Warning { message: String },

    #[serde(rename = "confirmation_request")]
    ConfirmationRequest {
        action: String,
        description: String,
        risk_level: String,
    },

    #[serde(rename = "question_request")]
    QuestionRequest {
        questions: Vec<crate::tools::QuestionPrompt>,
    },

    #[serde(rename = "todo_changed")]
    TodoChanged { todos: Vec<crate::tools::TodoItem> },

    #[serde(rename = "session_reset")]
    SessionReset,

    #[serde(rename = "memory_search_result")]
    MemorySearchResult {
        query: String,
        matches: Vec<MemorySearchMatch>,
    },

    /// Reply to `Request::ListTools`: the daemon's registered tool set
    /// (name + description), for `/tools` in daemon-driven frontends.
    #[serde(rename = "tool_list")]
    ToolList { tools: Vec<ToolInfo> },

    /// Reply to `Request::SkillActivate`: activation outcome (content
    /// is already injected by the daemon's orchestrator).
    #[serde(rename = "skill_activated_result")]
    SkillActivatedResult {
        skill: String,
        activated: bool,
        message: Option<String>,
        /// Full injected content (body + referenced files) for
        /// frontend preview.
        content: String,
    },

    /// Reply to `Request::Plan`: the rendered plan awaiting the user's
    /// approve/reject decision (decision flows back via
    /// `Request::ConfirmResponse`).
    #[serde(rename = "plan_approval_request")]
    PlanApprovalRequest {
        #[serde(rename = "planId")]
        plan_id: String,
        goal: String,
        steps: Vec<PlanStepInfo>,
    },

    #[serde(rename = "error")]
    Error { message: String },

    #[serde(rename = "done")]
    Done { result: String },

    #[serde(rename = "system")]
    System { message: String },

    /// Reports progress of a planner-agent plan execution.
    #[serde(rename = "plan_progress")]
    PlanProgress {
        plan_id: String,
        plan_goal: String,
        step_id: String,
        step_description: String,
        step_status: String,
        steps_total: usize,
        steps_completed: usize,
        steps_failed: usize,
        steps_pending: usize,
    },

    /// Emitted when the planner revises a plan after a step failure.
    #[serde(rename = "plan_replanned")]
    PlanReplanned {
        plan_id: String,
        plan_goal: String,
        reason: String,
        steps_carried_over: usize,
        steps_total: usize,
    },

    // ------------------------------------------------------------------
    // Session lifecycle (P0-3). UI-agnostic signals for frontends that
    // observe a run rather than drive it.
    // ------------------------------------------------------------------
    /// Emitted once at the start of every `run` / `run_stream`.
    #[serde(rename = "session_started")]
    SessionStarted,

    /// Emitted before each provider request.
    #[serde(rename = "model_request")]
    #[serde(rename_all = "camelCase")]
    ModelRequest { model: String, message_count: usize },

    /// Live text chunk from a streaming provider response.
    #[serde(rename = "model_chunk")]
    #[serde(rename_all = "camelCase")]
    ModelChunk { delta: String },

    /// Emitted when a skill becomes active for the session.
    #[serde(rename = "skill_activated")]
    #[serde(rename_all = "camelCase")]
    SkillActivated { name: String },

    /// Emitted when the planner produces a plan.
    #[serde(rename = "plan_created")]
    #[serde(rename_all = "camelCase")]
    PlanCreated { steps_total: usize },

    /// Emitted when a plan step begins executing.
    #[serde(rename = "step_started")]
    #[serde(rename_all = "camelCase")]
    StepStarted {
        index: usize,
        total: usize,
        description: String,
    },

    /// Emitted when a plan step finishes.
    #[serde(rename = "step_completed")]
    #[serde(rename_all = "camelCase")]
    StepCompleted {
        index: usize,
        total: usize,
        status: String,
    },

    /// Emitted when auto-compaction kicks in.
    #[serde(rename = "compaction_started")]
    CompactionStarted,

    /// Emitted right before the run blocks on a user confirmation.
    #[serde(rename = "waiting_for_user")]
    WaitingForUser,

    /// Emitted once when a run ends successfully. Carries the final
    /// assistant text so event-stream-only frontends (headless runtime,
    /// kanban) never need to reassemble it from `model_chunk` deltas.
    #[serde(rename = "session_completed")]
    #[serde(rename_all = "camelCase")]
    SessionCompleted { result: String },

    /// Emitted whenever the orchestrator's coarse lifecycle state
    /// changes (`idle` → `planning` → `completed`). The payload is the
    /// snake_case state name; P1-2 formalizes the machine behind it.
    #[serde(rename = "state_changed")]
    StateChanged { state: String },

    /// Emitted when a run ends in an error.
    #[serde(rename = "session_failed")]
    #[serde(rename_all = "camelCase")]
    SessionFailed { message: String },

    // ------------------------------------------------------------------
    // Steering (Fase 2b). User-initiated mid-run guidance.
    // ------------------------------------------------------------------
    /// Emitted by the engine when `Request::Steer` is accepted (immediate
    /// ack; the queued text is consumed later at an iteration boundary).
    #[serde(rename = "steer_queued")]
    SteerQueued { content: String },

    /// Emitted by the orchestrator when a queued steer is consumed at an
    /// iteration boundary and added to memory as a user message.
    #[serde(rename = "steered")]
    Steered { content: String },

    // ------------------------------------------------------------------
    // Daemon mode (Fase 3a). Adopt-on-re-init handshake.
    // ------------------------------------------------------------------
    /// Emitted by the engine right after a daemon accepted a second
    /// `Init` (adopt-on-re-init): the reconnecting frontend now drives
    /// the pre-existing session. Carries the tracked session id, if any.
    #[serde(rename = "session_adopted")]
    #[serde(rename_all = "camelCase")]
    SessionAdopted { session_id: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearchMatch {
    pub role: String,
    pub content: String,
}

/// One entry of a `ToolList` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
}

/// One step of a plan awaiting approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStepInfo {
    pub id: String,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    Thinking,
    Ready,
    InitOk,
    ToolCall,
    ToolStarted,
    ToolDelta,
    ToolOutput,
    ToolFinished,
    AssistantDelta,
    Planning,
    Warning,
    ConfirmationRequest,
    QuestionRequest,
    TodoChanged,
    SessionReset,
    MemorySearchResult,
    ToolList,
    SkillActivatedResult,
    PlanApprovalRequest,
    Error,
    Done,
    System,
    PlanProgress,
    PlanReplanned,
    SessionStarted,
    ModelRequest,
    ModelChunk,
    SkillActivated,
    PlanCreated,
    StepStarted,
    StepCompleted,
    CompactionStarted,
    WaitingForUser,
    SessionCompleted,
    StateChanged,
    SessionFailed,
    SteerQueued,
    Steered,
    SessionAdopted,
}

impl Event {
    pub fn event_type(&self) -> EventType {
        match self {
            Event::Thinking { .. } => EventType::Thinking,
            Event::Ready { .. } => EventType::Ready,
            Event::InitOk { .. } => EventType::InitOk,
            Event::ToolCall { .. } => EventType::ToolCall,
            Event::ToolStarted { .. } => EventType::ToolStarted,
            Event::ToolDelta { .. } => EventType::ToolDelta,
            Event::ToolOutput { .. } => EventType::ToolOutput,
            Event::ToolFinished { .. } => EventType::ToolFinished,
            Event::AssistantDelta { .. } => EventType::AssistantDelta,
            Event::Planning { .. } => EventType::Planning,
            Event::Warning { .. } => EventType::Warning,
            Event::ConfirmationRequest { .. } => EventType::ConfirmationRequest,
            Event::QuestionRequest { .. } => EventType::QuestionRequest,
            Event::TodoChanged { .. } => EventType::TodoChanged,
            Event::SessionReset => EventType::SessionReset,
            Event::MemorySearchResult { .. } => EventType::MemorySearchResult,
            Event::ToolList { .. } => EventType::ToolList,
            Event::SkillActivatedResult { .. } => EventType::SkillActivatedResult,
            Event::PlanApprovalRequest { .. } => EventType::PlanApprovalRequest,
            Event::Error { .. } => EventType::Error,
            Event::Done { .. } => EventType::Done,
            Event::System { .. } => EventType::System,
            Event::PlanProgress { .. } => EventType::PlanProgress,
            Event::PlanReplanned { .. } => EventType::PlanReplanned,
            Event::SessionStarted => EventType::SessionStarted,
            Event::ModelRequest { .. } => EventType::ModelRequest,
            Event::ModelChunk { .. } => EventType::ModelChunk,
            Event::SkillActivated { .. } => EventType::SkillActivated,
            Event::PlanCreated { .. } => EventType::PlanCreated,
            Event::StepStarted { .. } => EventType::StepStarted,
            Event::StepCompleted { .. } => EventType::StepCompleted,
            Event::CompactionStarted => EventType::CompactionStarted,
            Event::WaitingForUser => EventType::WaitingForUser,
            Event::SessionCompleted { .. } => EventType::SessionCompleted,
            Event::StateChanged { .. } => EventType::StateChanged,
            Event::SessionFailed { .. } => EventType::SessionFailed,
            Event::SteerQueued { .. } => EventType::SteerQueued,
            Event::Steered { .. } => EventType::Steered,
            Event::SessionAdopted { .. } => EventType::SessionAdopted,
        }
    }
}

type EventHandler = Box<dyn Fn(Event) + Send + Sync>;

pub struct EventEmitter {
    handlers: Mutex<Vec<EventHandler>>,
}

impl EventEmitter {
    pub fn new() -> Self {
        Self {
            handlers: Mutex::new(Vec::new()),
        }
    }

    pub fn on<F>(&self, handler: F)
    where
        F: Fn(Event) + Send + Sync + 'static,
    {
        self.handlers.lock().unwrap().push(Box::new(handler));
    }

    pub fn emit(&self, event: Event) {
        let handlers = self.handlers.lock().unwrap();
        for handler in handlers.iter() {
            handler(event.clone());
        }
    }

    #[allow(dead_code)]
    pub fn clear(&self) {
        self.handlers.lock().unwrap().clear();
    }
}

impl Default for EventEmitter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_lifecycle_variants_map_to_type() {
        let start = Event::ToolStarted {
            tool_call_id: "c1".into(),
            tool_name: "run_command".into(),
            arguments: serde_json::json!({ "command": "echo hi" }),
        };
        assert_eq!(start.event_type(), EventType::ToolStarted);

        let delta = Event::ToolDelta {
            tool_call_id: "c1".into(),
            tool_name: "run_command".into(),
            delta: "hi\n".to_string(),
        };
        assert_eq!(delta.event_type(), EventType::ToolDelta);

        let out = Event::ToolOutput {
            tool_name: "run_command".into(),
            output: serde_json::json!({ "stdout": "hi" }),
            error: None,
            tool_call_id: "c1".into(),
            duration_ms: 42,
            success: true,
            truncated: false,
        };
        assert_eq!(out.event_type(), EventType::ToolOutput);
    }

    #[test]
    fn tool_delta_serializes_with_type_tag() {
        let delta = Event::ToolDelta {
            tool_call_id: "c1".into(),
            tool_name: "run_command".into(),
            delta: "hi\n".to_string(),
        };
        let json = serde_json::to_value(&delta).unwrap();
        assert_eq!(json["type"], "tool_delta");
        assert_eq!(json["toolName"], "run_command");
    }

    #[test]
    fn session_lifecycle_variants_serialize() {
        // Every P0-3 variant must round-trip with its wire tag so
        // non-Rust frontends (TUI/kanban) can consume the stream.
        let cases: Vec<(Event, &str)> = vec![
            (Event::SessionStarted, "session_started"),
            (
                Event::ModelRequest {
                    model: "gpt-4o".into(),
                    message_count: 3,
                },
                "model_request",
            ),
            (Event::ModelChunk { delta: "hi".into() }, "model_chunk"),
            (
                Event::SkillActivated {
                    name: "postgres".into(),
                },
                "skill_activated",
            ),
            (Event::PlanCreated { steps_total: 4 }, "plan_created"),
            (
                Event::StepStarted {
                    index: 1,
                    total: 4,
                    description: "write tests".into(),
                },
                "step_started",
            ),
            (
                Event::StepCompleted {
                    index: 1,
                    total: 4,
                    status: "completed".into(),
                },
                "step_completed",
            ),
            (Event::CompactionStarted, "compaction_started"),
            (Event::WaitingForUser, "waiting_for_user"),
            (
                Event::SessionCompleted {
                    result: "done".into(),
                },
                "session_completed",
            ),
            (
                Event::StateChanged {
                    state: "planning".into(),
                },
                "state_changed",
            ),
            (
                Event::SessionFailed {
                    message: "provider down".into(),
                },
                "session_failed",
            ),
        ];
        for (event, tag) in cases {
            let json = serde_json::to_value(&event).unwrap();
            assert_eq!(json["type"], tag, "wrong tag for {event:?}");
            let parsed: Event = serde_json::from_value(json).unwrap();
            assert_eq!(parsed.event_type(), event.event_type());
        }
    }

    #[test]
    fn model_request_uses_camel_case_fields() {
        let event = Event::ModelRequest {
            model: "claude-3".into(),
            message_count: 7,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["model"], "claude-3");
        assert_eq!(json["messageCount"], 7);
    }

    #[test]
    fn steer_events_serialize_with_wire_tags() {
        // Fase 2b: engine ack (steer_queued) and orchestrator boundary
        // consumption (steered) must round-trip with stable wire tags so
        // non-Rust frontends can consume the stream.
        let queued = Event::SteerQueued {
            content: "focus on tests".into(),
        };
        let json = serde_json::to_value(&queued).unwrap();
        assert_eq!(json["type"], "steer_queued");
        assert_eq!(json["content"], "focus on tests");
        let parsed: Event = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.event_type(), EventType::SteerQueued);

        let steered = Event::Steered {
            content: "focus on tests".into(),
        };
        let json = serde_json::to_value(&steered).unwrap();
        assert_eq!(json["type"], "steered");
        assert_eq!(json["content"], "focus on tests");
        let parsed: Event = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.event_type(), EventType::Steered);
    }

    #[test]
    fn session_adopted_serializes_with_wire_tag() {
        // Fase 3a: adopt-on-re-init announcement must round-trip with
        // the stable snake_case tag `session_adopted` and carry the
        // adopted session id (absent when untracked).
        let adopted = Event::SessionAdopted {
            session_id: Some("sess-1".into()),
        };
        let json = serde_json::to_value(&adopted).unwrap();
        assert_eq!(json["type"], "session_adopted");
        assert_eq!(json["sessionId"], "sess-1");
        let parsed: Event = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.event_type(), EventType::SessionAdopted);

        let untracked = Event::SessionAdopted { session_id: None };
        let json = serde_json::to_value(&untracked).unwrap();
        assert_eq!(json["type"], "session_adopted");
        let parsed: Event = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.event_type(), EventType::SessionAdopted);
    }
}
