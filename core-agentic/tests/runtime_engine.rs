mod support;

use core_agentic::events::Event;
use core_agentic::providers::{
    ChatChunk, ChatMessageResponse, ChatRequest, ChatResponse, LLMProvider, ProviderError,
    ProviderResult, StreamResult,
};
use core_agentic::runtime::engine::RuntimeEngine;
use core_agentic::runtime::protocol::{ProtocolRequest, Request};
use core_agentic::runtime::transport::MemoryTransport;
use core_agentic::{QuestionAnswer, ToolRegistry};

struct TextProvider;

impl LLMProvider for TextProvider {
    fn provider_type(&self) -> &str {
        "test"
    }
    fn provider_id(&self) -> &str {
        "test"
    }
    fn chat(&self, _request: ChatRequest) -> ProviderResult<ChatResponse> {
        Ok(ChatResponse {
            id: "response".into(),
            model: "test".into(),
            message: ChatMessageResponse {
                role: "assistant".into(),
                content: Some("unused".into()),
                tool_calls: vec![],
            },
            finish_reason: Some("stop".into()),
            usage: None,
        })
    }
    fn chat_stream(&self, _request: ChatRequest) -> StreamResult<ChatChunk, ProviderError> {
        Ok(Box::pin(futures::stream::iter(vec![Ok(ChatChunk {
            id: "chunk".into(),
            delta: "hello".into(),
            finish_reason: Some("stop".into()),
            tool_calls: vec![],
            usage: None,
        })])))
    }
}

#[test]
fn runtime_emits_ready_init_ok_delta_and_done() {
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let transport = MemoryTransport::new(request_rx, event_tx);
    let mut engine = RuntimeEngine::with_session(
        transport,
        std::sync::Arc::new(TextProvider),
        ToolRegistry::new(),
    );
    let worker = std::thread::spawn(move || engine.run());

    request_tx
        .send(ProtocolRequest::new(
            "init-1",
            Request::Init {
                overrides: Default::default(),
                capabilities: Default::default(),
            },
        ))
        .unwrap();
    request_tx
        .send(ProtocolRequest::new(
            "run-1",
            Request::Run {
                task: "hello".into(),
                attachments: vec![],
            },
        ))
        .unwrap();
    drop(request_tx);
    worker.join().unwrap();

    let events: Vec<_> = event_rx.try_iter().collect();
    assert!(events
        .iter()
        .any(|event| matches!(event.event, Event::Ready { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event.event, Event::InitOk { .. })));
    assert!(events.iter().any(
        |event| matches!(&event.event, Event::AssistantDelta { content } if content == "hello")
    ));
    assert!(events
        .iter()
        .any(|event| matches!(&event.event, Event::Done { result } if result == "hello")));
}

#[test]
fn question_request_waits_for_client_response() {
    let provider = support::StreamingScriptedProvider::new(vec![
        support::tool_call_response(
            "q1",
            "question",
            &serde_json::json!({"questions":[{"question":"Pick?","options":["A","B"]}]}),
        ),
        support::text_response("thanks"),
    ]);
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut engine = RuntimeEngine::with_provider(
        MemoryTransport::new(request_rx, event_tx),
        std::sync::Arc::new(provider),
    );
    let worker = std::thread::spawn(move || engine.run());

    request_tx
        .send(ProtocolRequest::new(
            "run-q",
            Request::Run {
                task: "ask".into(),
                attachments: vec![],
            },
        ))
        .unwrap();
    let mut saw_question = false;
    loop {
        let event = event_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        if matches!(event.event, Event::QuestionRequest { .. }) {
            saw_question = true;
            request_tx
                .send(ProtocolRequest::new(
                    "answer-q",
                    Request::QuestionResponse {
                        request_id: Some("run-q".into()),
                        answers: vec![QuestionAnswer {
                            question: "Pick?".into(),
                            answer: vec!["A".into()],
                            skipped: false,
                        }],
                    },
                ))
                .unwrap();
        }
        if matches!(event.event, Event::Done { .. }) {
            break;
        }
    }
    request_tx
        .send(ProtocolRequest::new("shutdown", Request::Shutdown))
        .unwrap();
    worker.join().unwrap();
    assert!(saw_question);
}

#[test]
fn todo_changes_are_forwarded_as_events() {
    let provider = support::StreamingScriptedProvider::new(vec![
        support::tool_call_response(
            "todo1",
            "todowrite",
            &serde_json::json!({"todos":[{"content":"ship runtime"}]}),
        ),
        support::text_response("tracked"),
    ]);
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut engine = RuntimeEngine::with_provider(
        MemoryTransport::new(request_rx, event_tx),
        std::sync::Arc::new(provider),
    );
    let worker = std::thread::spawn(move || engine.run());
    request_tx
        .send(ProtocolRequest::new(
            "run-t",
            Request::Run {
                task: "track".into(),
                attachments: vec![],
            },
        ))
        .unwrap();

    let mut saw_todo = false;
    let mut saw_tool_finished = false;
    loop {
        let event = event_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        if matches!(event.event, Event::TodoChanged { .. }) {
            saw_todo = true;
        }
        if matches!(event.event, Event::ToolFinished { .. }) {
            saw_tool_finished = true;
        }
        if matches!(event.event, Event::Done { .. }) {
            break;
        }
    }
    request_tx
        .send(ProtocolRequest::new("shutdown", Request::Shutdown))
        .unwrap();
    worker.join().unwrap();
    assert!(saw_todo);
    assert!(saw_tool_finished);
}

#[test]
fn confirmation_request_waits_for_client_approval() {
    let provider = support::StreamingScriptedProvider::new(vec![
        support::tool_call_response(
            "cmd1",
            "run_command",
            &serde_json::json!({"command":"curl --version"}),
        ),
        support::text_response("executed"),
    ]);
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut engine = RuntimeEngine::with_provider(
        MemoryTransport::new(request_rx, event_tx),
        std::sync::Arc::new(provider),
    );
    let worker = std::thread::spawn(move || engine.run());
    request_tx
        .send(ProtocolRequest::new(
            "run-c",
            Request::Run {
                task: "execute".into(),
                attachments: vec![],
            },
        ))
        .unwrap();

    let mut saw_confirmation = false;
    loop {
        let event = event_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        if matches!(event.event, Event::ConfirmationRequest { .. }) {
            saw_confirmation = true;
            request_tx
                .send(ProtocolRequest::new(
                    "approve-c",
                    Request::ConfirmResponse {
                        request_id: Some("run-c".into()),
                        approved: true,
                    },
                ))
                .unwrap();
        }
        if matches!(event.event, Event::Done { .. }) {
            break;
        }
    }
    request_tx
        .send(ProtocolRequest::new("shutdown", Request::Shutdown))
        .unwrap();
    worker.join().unwrap();
    assert!(saw_confirmation);
}

/// `Request::ListTools` replies with the daemon's registered tools.
#[test]
fn list_tools_replies_with_tool_list() {
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let registry = ToolRegistry::new();
    registry.register(Box::new(support::SlowReadTool::new(
        "probe_tool",
        std::time::Duration::from_millis(1),
    )));
    let mut engine = RuntimeEngine::with_session(
        MemoryTransport::new(request_rx, event_tx),
        std::sync::Arc::new(TextProvider),
        registry,
    );
    let worker = std::thread::spawn(move || engine.run());

    request_tx
        .send(ProtocolRequest::new(
            "init-1",
            Request::Init {
                overrides: Default::default(),
                capabilities: Default::default(),
            },
        ))
        .unwrap();
    request_tx
        .send(ProtocolRequest::new("tools-1", Request::ListTools))
        .unwrap();
    drop(request_tx);
    worker.join().unwrap();

    let events: Vec<_> = event_rx.try_iter().collect();
    let listing = events.iter().find_map(|e| match &e.event {
        Event::ToolList { tools } => Some(tools.clone()),
        _ => None,
    });
    let tools = listing.expect("expected ToolList reply");
    assert!(tools.iter().any(|t| t.name == "probe_tool"));
    assert!(tools.iter().all(|t| !t.description.is_empty()));
}

/// `Request::SkillActivate` without a discovered index answers a clean
/// negative (no panic, activated=false).
#[test]
fn skill_activate_without_index_answers_negative() {
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut engine = RuntimeEngine::with_session(
        MemoryTransport::new(request_rx, event_tx),
        std::sync::Arc::new(TextProvider),
        ToolRegistry::new(),
    );
    let worker = std::thread::spawn(move || engine.run());

    request_tx
        .send(ProtocolRequest::new(
            "init-1",
            Request::Init {
                overrides: Default::default(),
                capabilities: Default::default(),
            },
        ))
        .unwrap();
    request_tx
        .send(ProtocolRequest::new(
            "skill-1",
            Request::SkillActivate {
                name: "nope".into(),
            },
        ))
        .unwrap();
    drop(request_tx);
    worker.join().unwrap();

    let events: Vec<_> = event_rx.try_iter().collect();
    let reply = events.iter().find_map(|e| match &e.event {
        Event::SkillActivatedResult {
            skill,
            activated,
            message,
            ..
        } => Some((skill.clone(), *activated, message.clone())),
        _ => None,
    });
    let (skill, activated, message) = reply.expect("expected SkillActivatedResult");
    assert_eq!(skill, "nope");
    assert!(!activated);
    assert!(message.unwrap().contains("no skills discovered"));
}

/// `Request::Plan` full cycle: LLM plan creation → approval gate
/// (PlanApprovalRequest answered via ConfirmResponse) → execution →
/// Done, with planner lifecycle events streaming in between.
#[test]
fn plan_request_runs_creation_approval_and_execution() {
    use core_agentic::events::PlanStepInfo;

    // The LLM "plans" by answering create_plan with a JSON step array,
    // then (after approval) the execution phase runs tool-less steps.
    let provider = support::ScriptedProvider::new(vec![
        support::text_response(r#"[{"description": "Step A"}, {"description": "Step B"}]"#),
        support::text_response("executed"),
    ]);
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut engine = RuntimeEngine::with_provider(
        MemoryTransport::new(request_rx, event_tx),
        std::sync::Arc::new(provider),
    );
    let worker = std::thread::spawn(move || engine.run());

    request_tx
        .send(ProtocolRequest::new(
            "init-1",
            Request::Init {
                overrides: Default::default(),
                capabilities: Default::default(),
            },
        ))
        .unwrap();
    request_tx
        .send(ProtocolRequest::new(
            "plan-1",
            Request::Plan {
                goal: "do the thing".into(),
                require_approval: true,
            },
        ))
        .unwrap();

    // Read events until the approval gate opens, then approve.
    let approval: Option<(String, Vec<PlanStepInfo>)> = loop {
        let event = event_rx.recv().unwrap();
        if event.request_id.as_deref() == Some("plan-1") {
            match &event.event {
                Event::PlanApprovalRequest { plan_id, steps, .. } => {
                    break Some((plan_id.clone(), steps.clone()));
                }
                Event::Error { message } => panic!("plan error: {message}"),
                _ => {}
            }
        }
    };
    assert!(approval.is_some(), "approval gate must open");
    request_tx
        .send(ProtocolRequest::new(
            "confirm-1",
            Request::ConfirmResponse {
                request_id: Some("plan-1".into()),
                approved: true,
            },
        ))
        .unwrap();

    // Drain until Done; planner lifecycle events must have streamed.
    let mut done = false;
    let mut saw_progress = false;
    while !done {
        let event = event_rx.recv().unwrap();
        if event.request_id.as_deref() != Some("plan-1") {
            continue;
        }
        match event.event {
            Event::Done { result } => {
                assert!(result.contains("completed"), "summary: {result}");
                done = true;
            }
            Event::Error { message } => panic!("plan error: {message}"),
            Event::PlanProgress { .. } | Event::StepStarted { .. } => saw_progress = true,
            _ => {}
        }
    }
    assert!(saw_progress, "planner lifecycle events must stream");
    drop(request_tx);
    worker.join().unwrap();
}

/// Golden event-stream test: verify the daemon produces the expected
/// event sequence for a simple text-only run (no tools).
#[test]
fn golden_event_stream_text_only_run() {
    let provider = support::StreamingScriptedProvider::new(vec![support::text_response(
        "Hello from the agent!",
    )]);
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut engine = RuntimeEngine::with_provider(
        MemoryTransport::new(request_rx, event_tx),
        std::sync::Arc::new(provider),
    );
    let worker = std::thread::spawn(move || engine.run());

    request_tx
        .send(ProtocolRequest::new(
            "init-1",
            Request::Init {
                overrides: Default::default(),
                capabilities: Default::default(),
            },
        ))
        .unwrap();
    request_tx
        .send(ProtocolRequest::new(
            "run-1",
            Request::Run {
                task: "say hello".into(),
                attachments: vec![],
            },
        ))
        .unwrap();
    drop(request_tx);
    worker.join().unwrap();

    let events: Vec<_> = event_rx.try_iter().collect();

    // Filter to only our request's events
    let relevant: Vec<&str> = events
        .iter()
        .filter(|e| e.request_id.as_deref() == Some("run-1"))
        .filter_map(|e| match &e.event {
            Event::AssistantDelta { .. } => Some("delta"),
            Event::Done { .. } => Some("done"),
            Event::Error { .. } => Some("error"),
            Event::SessionStarted => Some("session_started"),
            Event::SessionCompleted { .. } => Some("session_completed"),
            _ => None,
        })
        .collect();

    // Golden sequence: SessionStarted → StateChanged → delta(s) → StateChanged →
    // SessionCompleted → Done. SessionStarted and SessionCompleted bracket
    // the run; assistant delta carries the text; Done signals completion.
    assert!(
        relevant.contains(&"session_started"),
        "expected SessionStarted, got: {:?}",
        relevant
    );
    assert!(
        relevant.contains(&"done"),
        "expected Done, got: {:?}",
        relevant
    );
    assert!(
        relevant.contains(&"delta"),
        "expected AssistantDelta, got: {:?}",
        relevant
    );
    assert!(
        relevant.contains(&"session_completed"),
        "expected SessionCompleted, got: {:?}",
        relevant
    );
    // SessionStarted must come first; Done must come last
    assert_eq!(relevant.first(), Some(&"session_started"));
    assert_eq!(relevant.last(), Some(&"done"));
}
