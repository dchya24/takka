mod support;

use core_agentic::events::Event;
use core_agentic::providers::{
    ChatChunk, ChatMessageResponse, ChatRequest, ChatResponse, LLMProvider, ProviderError,
    ProviderResult, StreamResult,
};
use core_agentic::runtime::engine::RuntimeEngine;
use core_agentic::runtime::protocol::{
    ConnectionCapabilities, ProtocolEvent, ProtocolRequest, Request,
};
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

/// Engine `with_session` sudah initialized: tidak ada Init, tidak ada
/// InitOk — langsung Ready → delta → Done.
#[test]
fn runtime_emits_ready_delta_and_done_without_init() {
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
    )
    .with_capabilities(ConnectionCapabilities {
        experimental_api: true,
        opt_out_notify: vec![],
    });
    let worker = std::thread::spawn(move || engine.run());

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

/// Fresh engine (belum init): request resource wajib ditolak.
#[test]
fn resource_requests_before_init_are_rejected() {
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let transport = MemoryTransport::new(request_rx, event_tx);
    let mut engine = RuntimeEngine::new(transport);
    let worker = std::thread::spawn(move || engine.run());

    request_tx
        .send(ProtocolRequest::new(
            "run-1",
            Request::Run {
                task: "hello".into(),
                attachments: vec![],
            },
        ))
        .unwrap();
    request_tx
        .send(ProtocolRequest::new("tools-1", Request::ListTools))
        .unwrap();
    drop(request_tx);
    worker.join().unwrap();

    let events: Vec<_> = event_rx.try_iter().collect();
    let errors: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Error { message } => Some(message.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        errors.len(),
        2,
        "expected exactly two not-initialized errors: {errors:?}"
    );
    assert!(
        errors.iter().all(|m| *m == "not initialized"),
        "got: {errors:?}"
    );
}

/// Init kedua kali (di sini: setelah constructor with_session yang berarti
/// sudah initialized) ditolak dengan "already initialized".
#[test]
fn second_init_is_rejected() {
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
                daemon_token: None,
            },
        ))
        .unwrap();
    drop(request_tx);
    worker.join().unwrap();

    let events: Vec<_> = event_rx.try_iter().collect();
    assert!(
        events.iter().any(
            |e| matches!(&e.event, Event::Error { message } if message == "already initialized")
        ),
        "expected already-initialized error: {events:?}"
    );
}

/// Request::Plan tanpa opt-in experimental ditolak dengan capability error.
/// (Dengan `with_session` engine sudah initialized, jadi urutan cek:
/// capability langsung dievaluasi.)
#[test]
fn plan_without_experimental_capability_is_rejected() {
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
            "plan-1",
            Request::Plan {
                goal: "fake goal".into(),
                require_approval: false,
            },
        ))
        .unwrap();
    drop(request_tx);
    worker.join().unwrap();

    let events: Vec<_> = event_rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.event, Event::Error { message }
            if message == "capability 'experimental_api' required")),
        "expected capability error: {events:?}"
    );
}

mod sink_tests {
    use core_agentic::events::Event;
    use core_agentic::runtime::engine::EventSink;
    use core_agentic::runtime::protocol::{NotificationKind, ProtocolEvent};
    use core_agentic::runtime::transport::MemoryTransport;

    fn make_sink() -> (
        std::sync::mpsc::Receiver<ProtocolEvent>,
        EventSink<MemoryTransport>,
    ) {
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        drop(request_tx); // tidak ada loop engine di sini
        let transport = std::sync::Arc::new(MemoryTransport::new(request_rx, event_tx));
        let sink = EventSink::new(transport, Default::default());
        (event_rx, sink)
    }

    #[test]
    fn sink_filters_opted_out_notifications_only() {
        let (event_rx, sink) = make_sink();
        sink.set_opt_outs(&[
            NotificationKind::Thinking,
            NotificationKind::AssistantDelta,
            NotificationKind::Warning,
            NotificationKind::TodoChanged,
        ]);

        // Diopt-out: tidak terkirim.
        sink.send(
            Some("r".into()),
            Event::Thinking {
                content: "t".into(),
            },
        );
        sink.send(
            Some("r".into()),
            Event::AssistantDelta {
                content: "d".into(),
            },
        );
        sink.send(
            Some("r".into()),
            Event::Warning {
                message: "w".into(),
            },
        );
        sink.send(Some("r".into()), Event::TodoChanged { todos: vec![] });

        // Tidak terseleksi: selalu terkirim — respons + gates.
        sink.send(
            Some("r".into()),
            Event::Done {
                result: "final".into(),
            },
        );
        sink.send(
            Some("r".into()),
            Event::Error {
                message: "boom".into(),
            },
        );
        sink.send(
            Some("r".into()),
            Event::ConfirmationRequest {
                action: "write".into(),
                description: "d".into(),
                risk_level: "medium".into(),
            },
        );
        sink.send(
            Some("r".into()),
            Event::QuestionRequest { questions: vec![] },
        );
        sink.send(
            None,
            Event::Ready {
                protocol: "agentic".into(),
                version: 1,
            },
        );

        let seen: Vec<_> = event_rx.try_iter().collect();
        assert!(seen.iter().any(|e| matches!(e.event, Event::Done { .. })));
        assert!(seen.iter().any(|e| matches!(e.event, Event::Error { .. })));
        assert!(seen
            .iter()
            .any(|e| matches!(e.event, Event::ConfirmationRequest { .. })));
        assert!(seen
            .iter()
            .any(|e| matches!(e.event, Event::QuestionRequest { .. })));
        assert!(seen.iter().any(|e| matches!(e.event, Event::Ready { .. })));
        assert!(!seen.iter().any(|e| matches!(
            e.event,
            Event::Thinking { .. }
                | Event::AssistantDelta { .. }
                | Event::Warning { .. }
                | Event::TodoChanged { .. }
        )));
        assert_eq!(
            seen.len(),
            5,
            "exactly the 5 non-opted-out events: {seen:?}"
        );
    }
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

// ── Steering (Fase 2b, Task 2): Request::Steer + engine queue ───────

/// Hard-deadline event read: test tidak pernah menggantung — setiap
/// baca dibatasi waktu, deadline lewat = panic dengan konteks.
fn recv_until(
    event_rx: &std::sync::mpsc::Receiver<ProtocolEvent>,
    deadline: std::time::Instant,
) -> ProtocolEvent {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    event_rx
        .recv_timeout(remaining)
        .expect("event stream must produce events before the deadline")
}

const STEER_TEXT: &str = "use the lake theme instead";

/// Steer saat run aktif: SteerQueued reply segera (di-tag dengan request
/// id steer), Steered di-emit dengan request id RUN (bukan id steer),
/// dan konteks provider berikutnya memuat steer sebagai user message.
#[test]
fn steer_during_run_injects_at_next_boundary() {
    let provider = std::sync::Arc::new(support::CapturingStreamingScriptedProvider::new(vec![
        // Turn 1: tool call — tool-nya sleep, jadi steer masih sempat
        // tiba sebelum boundary berikutnya.
        support::tool_call_response("t1", "grep", &serde_json::json!({"pattern": "x"})),
        // Turn 2: final.
        support::text_response("done"),
    ]));
    let registry = ToolRegistry::new();
    registry.register(Box::new(support::SlowReadTool::new(
        "grep",
        std::time::Duration::from_millis(300),
    )));
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut engine = RuntimeEngine::with_session(
        MemoryTransport::new(request_rx, event_tx),
        provider.clone(),
        registry,
    );
    let worker = std::thread::spawn(move || engine.run());

    request_tx
        .send(ProtocolRequest::new(
            "run-1",
            Request::Run {
                task: "find x".into(),
                attachments: vec![],
            },
        ))
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);

    // Tunggu sampai run terbukti in-flight: ToolStarted terlihat.
    loop {
        let event = recv_until(&event_rx, deadline);
        match event.event {
            Event::ToolStarted { .. } => break,
            Event::Done { .. } | Event::Error { .. } => {
                panic!(
                    "run ended before the steer window opened: {:?}",
                    event.event
                )
            }
            _ => {}
        }
    }

    // Steer tiba saat run aktif (tool masih dieksekusi).
    request_tx
        .send(ProtocolRequest::new(
            "steer-1",
            Request::Steer {
                content: STEER_TEXT.into(),
            },
        ))
        .unwrap();

    let mut steer_queued_ids = Vec::new();
    let mut steered = Vec::new();
    loop {
        let event = recv_until(&event_rx, deadline);
        match &event.event {
            Event::SteerQueued { content } => {
                assert_eq!(content, STEER_TEXT);
                steer_queued_ids.push(
                    event
                        .request_id
                        .clone()
                        .expect("SteerQueued must be tagged"),
                );
            }
            Event::Steered { content } => steered.push((
                event.request_id.clone().expect("Steered must be tagged"),
                content.clone(),
            )),
            Event::Done { .. } => break,
            Event::Error { message } => panic!("run error: {message}"),
            _ => {}
        }
    }
    request_tx
        .send(ProtocolRequest::new("shutdown", Request::Shutdown))
        .unwrap();
    worker.join().unwrap();

    // 1. SteerQueued di-tag dengan request id steer.
    assert_eq!(
        steer_queued_ids,
        vec!["steer-1".to_string()],
        "SteerQueued must carry the steer request id"
    );
    // 2. Steered di-tag dengan run id, BUKAN id steer.
    assert_eq!(
        steered.len(),
        1,
        "exactly one Steered expected: {steered:?}"
    );
    assert_eq!(
        steered[0].0, "run-1",
        "Steered must carry the RUN request id, not the steer id"
    );
    assert_eq!(steered[0].1, STEER_TEXT);

    // 3. Konteks model: steer tidak ada di turn 1, ada sebagai user
    //    message di turn 2 (injected at the boundary, not at run start).
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "two provider calls expected");
    assert!(
        !requests[0]
            .messages
            .iter()
            .any(|m| m.content.contains(STEER_TEXT)),
        "steer must NOT be in the turn-1 request: {:?}",
        requests[0].messages
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.role == "user" && m.content.contains(STEER_TEXT)),
        "steer must reach turn 2 as a user message: {:?}",
        requests[1].messages
    );
}

/// Fresh engine (belum init): Steer ditolak strict.
#[test]
fn steer_before_init_is_rejected() {
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut engine = RuntimeEngine::new(MemoryTransport::new(request_rx, event_tx));
    let worker = std::thread::spawn(move || engine.run());

    request_tx
        .send(ProtocolRequest::new(
            "steer-1",
            Request::Steer {
                content: "early".into(),
            },
        ))
        .unwrap();
    drop(request_tx);
    worker.join().unwrap();

    let events: Vec<_> = event_rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.event, Event::Error { message } if message == "not initialized")),
        "expected 'not initialized' error: {events:?}"
    );
}

/// Engine initialized tapi tidak ada run aktif: Steer ditolak strict.
#[test]
fn steer_when_no_run_active_is_rejected() {
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
            "steer-1",
            Request::Steer {
                content: "idle".into(),
            },
        ))
        .unwrap();
    drop(request_tx);
    worker.join().unwrap();

    let events: Vec<_> = event_rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.event, Event::Error { message } if message == "no active run")),
        "expected 'no active run' error: {events:?}"
    );
}

/// Tiga steer di-queue saat satu run aktif → dikonsumsi FIFO berurutan
/// a, b, c di boundary yang sama; tiap ack di-tag dengan id-nya.
#[test]
fn steers_consume_fifo_order() {
    let provider = std::sync::Arc::new(support::CapturingStreamingScriptedProvider::new(vec![
        support::tool_call_response("t1", "grep", &serde_json::json!({"pattern": "x"})),
        support::text_response("done"),
    ]));
    let registry = ToolRegistry::new();
    registry.register(Box::new(support::SlowReadTool::new(
        "grep",
        std::time::Duration::from_millis(300),
    )));
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut engine = RuntimeEngine::with_session(
        MemoryTransport::new(request_rx, event_tx),
        provider,
        registry,
    );
    let worker = std::thread::spawn(move || engine.run());

    request_tx
        .send(ProtocolRequest::new(
            "run-1",
            Request::Run {
                task: "find x".into(),
                attachments: vec![],
            },
        ))
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let event = recv_until(&event_rx, deadline);
        match event.event {
            Event::ToolStarted { .. } => break,
            Event::Done { .. } | Event::Error { .. } => {
                panic!(
                    "run ended before the steer window opened: {:?}",
                    event.event
                )
            }
            _ => {}
        }
    }

    for content in ["a", "b", "c"] {
        request_tx
            .send(ProtocolRequest::new(
                format!("steer-{content}"),
                Request::Steer {
                    content: content.into(),
                },
            ))
            .unwrap();
    }

    let mut queued = Vec::new();
    let mut steered_contents = Vec::new();
    loop {
        let event = recv_until(&event_rx, deadline);
        match &event.event {
            Event::SteerQueued { content } => {
                queued.push((event.request_id.clone().unwrap(), content.clone()));
            }
            Event::Steered { content } => steered_contents.push(content.clone()),
            Event::Done { .. } => break,
            Event::Error { message } => panic!("run error: {message}"),
            _ => {}
        }
    }
    request_tx
        .send(ProtocolRequest::new("shutdown", Request::Shutdown))
        .unwrap();
    worker.join().unwrap();

    assert_eq!(
        queued,
        vec![
            ("steer-a".to_string(), "a".to_string()),
            ("steer-b".to_string(), "b".to_string()),
            ("steer-c".to_string(), "c".to_string()),
        ],
        "setiap steer di-ack dengan request id-nya"
    );
    assert_eq!(
        steered_contents,
        vec!["a".to_string(), "b".to_string(), "c".to_string()],
        "drain at the boundary must be FIFO"
    );
}

/// Gate-safety: steer saat confirmation gate pending TETAP di-queue
/// (run masih aktif meski diblokir menunggu approval), lalu dikonsumsi
/// di boundary setelah gate disetujui — steer tidak mengganggu gate.
#[test]
fn steer_during_pending_confirmation_gate_is_queued_and_consumed_after_gate() {
    let provider = std::sync::Arc::new(support::CapturingStreamingScriptedProvider::new(vec![
        // Turn 1: run_command → butuh approval di Default mode.
        support::tool_call_response(
            "c1",
            "run_command",
            &serde_json::json!({"command": "curl --version"}),
        ),
        // Turn 2: final.
        support::text_response("executed"),
    ]));
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut engine =
        RuntimeEngine::with_provider(MemoryTransport::new(request_rx, event_tx), provider.clone());
    let worker = std::thread::spawn(move || engine.run());

    request_tx
        .send(ProtocolRequest::new(
            "run-g",
            Request::Run {
                task: "execute".into(),
                attachments: vec![],
            },
        ))
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    // Tunggu gate terbuka — run thread memblokir menunggu approval.
    loop {
        let event = recv_until(&event_rx, deadline);
        match event.event {
            Event::ConfirmationRequest { .. } => break,
            Event::Done { .. } | Event::Error { .. } => panic!(
                "run ended before the confirmation gate opened: {:?}",
                event.event
            ),
            _ => {}
        }
    }

    // Steer saat gate pending: run aktif → harus di-queue, bukan error.
    request_tx
        .send(ProtocolRequest::new(
            "steer-g",
            Request::Steer {
                content: "steer at the gate".into(),
            },
        ))
        .unwrap();
    // Lalu jawab gate (disposisi FIFO: Steer diproses lebih dulu).
    request_tx
        .send(ProtocolRequest::new(
            "approve-g",
            Request::ConfirmResponse {
                request_id: Some("run-g".into()),
                approved: true,
            },
        ))
        .unwrap();

    let mut saw_queued = false;
    let mut steered = Vec::new();
    loop {
        let event = recv_until(&event_rx, deadline);
        match &event.event {
            Event::SteerQueued { content } => {
                assert_eq!(content, "steer at the gate");
                assert_eq!(event.request_id.as_deref(), Some("steer-g"));
                saw_queued = true;
            }
            Event::Steered { content } => steered.push((
                event.request_id.clone().expect("Steered must be tagged"),
                content.clone(),
            )),
            Event::Done { .. } => break,
            Event::Error { message } => panic!("run error: {message}"),
            _ => {}
        }
    }
    request_tx
        .send(ProtocolRequest::new("shutdown", Request::Shutdown))
        .unwrap();
    worker.join().unwrap();

    assert!(saw_queued, "steer during a pending gate must be queued");
    assert_eq!(
        steered,
        vec![("run-g".to_string(), "steer at the gate".to_string())],
        "steer consumed after the gate, tagged with the run id"
    );
    let requests = provider.requests.lock().unwrap();
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.role == "user" && m.content.contains("steer at the gate")),
        "steer must reach the model after the gate: {:?}",
        requests[1].messages
    );
}

// ── Daemon mode (Fase 3a, Task 1): daemon_token + adopt-on-re-init ──

/// (non-daemon engine) token di-ignore; (daemon engine) token salah →
/// "unauthorized" SEBELUM adopt logic — init tidak pernah berulang.
#[test]
fn daemon_wrong_token_is_unauthorized_before_init() {
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut engine = RuntimeEngine::with_session(
        MemoryTransport::new(request_rx, event_tx),
        std::sync::Arc::new(TextProvider),
        ToolRegistry::new(),
    );
    engine.set_daemon_token(Some("secret".into()));
    let worker = std::thread::spawn(move || engine.run());

    request_tx
        .send(ProtocolRequest::new(
            "init-1",
            Request::Init {
                overrides: Default::default(),
                capabilities: Default::default(),
                daemon_token: Some("wrong".into()),
            },
        ))
        .unwrap();
    drop(request_tx);
    worker.join().unwrap();

    let events: Vec<_> = event_rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.event, Event::Error { message } if message == "unauthorized")),
        "expected unauthorized error: {events:?}"
    );
    // Token salah tidak pernah sampai ke jalur init/adopt: tidak ada
    // InitOk dan tidak ada SessionAdopted.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.event, Event::InitOk { .. })),
        "wrong token must not produce InitOk: {events:?}"
    );
    assert!(!events
        .iter()
        .any(|e| matches!(e.event, Event::SessionAdopted { .. })));
}

/// Adopt path bahagia: engine daemon (with_session + adopt flag + token)
/// menerima Init kedua yang kompatibel → InitOk lalu SessionAdopted,
/// keduanya di-tag dengan request id adopt.
#[test]
fn adopt_on_reinit_accepts_compatible_init_and_emits_session_adopted() {
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut engine = RuntimeEngine::with_session(
        MemoryTransport::new(request_rx, event_tx),
        std::sync::Arc::new(TextProvider),
        ToolRegistry::new(),
    );
    engine.set_adopt_on_reinit(true);
    engine.set_daemon_token(Some("secret".into()));
    let worker = std::thread::spawn(move || engine.run());

    request_tx
        .send(ProtocolRequest::new(
            "adopt-1",
            Request::Init {
                overrides: Default::default(),
                capabilities: Default::default(),
                daemon_token: Some("secret".into()),
            },
        ))
        .unwrap();
    drop(request_tx);
    worker.join().unwrap();

    let events: Vec<_> = event_rx.try_iter().collect();
    let init_ok = events
        .iter()
        .find(|e| matches!(e.event, Event::InitOk { .. }))
        .expect("adopt must reply InitOk: {events:?}");
    assert_eq!(init_ok.request_id.as_deref(), Some("adopt-1"));
    let adopted = events
        .iter()
        .find(|e| matches!(e.event, Event::SessionAdopted { .. }))
        .expect("adopt must emit SessionAdopted: {events:?}");
    assert_eq!(adopted.request_id.as_deref(), Some("adopt-1"));
    // Tidak ada error di jalur bahagia.
    assert!(!events
        .iter()
        .any(|e| matches!(e.event, Event::Error { .. })));
}

/// Adopt path menolak Init yang TIDAK kompatibel: capability baru yang
/// lebih besar → capability error; overrides berbeda → config error.
#[test]
fn adopt_rejects_new_capability_and_overrides_mismatch() {
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut engine = RuntimeEngine::with_session(
        MemoryTransport::new(request_rx, event_tx),
        std::sync::Arc::new(TextProvider),
        ToolRegistry::new(),
    );
    engine.set_adopt_on_reinit(true);
    engine.set_daemon_token(Some("secret".into()));
    let worker = std::thread::spawn(move || engine.run());

    // Init kedua: minta capability experimental yang belum dipegang engine
    // → ditolak (compatible_caps = implisit: engine lama tetap menang).
    request_tx
        .send(ProtocolRequest::new(
            "adopt-caps",
            Request::Init {
                overrides: Default::default(),
                capabilities: ConnectionCapabilities {
                    experimental_api: true,
                    opt_out_notify: vec![],
                },
                daemon_token: Some("secret".into()),
            },
        ))
        .unwrap();
    // Init ketiga: overrides berbeda dari yang berjalan → ditolak.
    request_tx
        .send(ProtocolRequest::new(
            "adopt-overrides",
            Request::Init {
                overrides: core_agentic::runtime::protocol::InitOverrides {
                    model: Some("x".into()),
                    ..Default::default()
                },
                capabilities: Default::default(),
                daemon_token: Some("secret".into()),
            },
        ))
        .unwrap();
    drop(request_tx);
    worker.join().unwrap();

    let events: Vec<_> = event_rx.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.event, Event::Error { message }
        if message == "capability 'experimental_api' required")),
        "expected capability error on adopt: {events:?}"
    );
    assert!(events.iter().any(|e| matches!(&e.event, Event::Error { message }
    if message == "daemon already running with different config; restart daemon or match its config")),
        "expected overrides-mismatch error on adopt: {events:?}");
    // Kedua penolakan tidak meng-adopt: tidak ada InitOk/SessionAdopted.
    assert!(!events
        .iter()
        .any(|e| matches!(e.event, Event::InitOk { .. })));
    assert!(!events
        .iter()
        .any(|e| matches!(e.event, Event::SessionAdopted { .. })));
}

/// Regresi eksplisit Fase 2a: engine TANPA adopt flag tetap strict —
/// Init kedua = "already initialized" (byte-for-byte), bukan adopt.
#[test]
fn strict_already_initialized_still_default_without_adopt_flag() {
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
            "init-2",
            Request::Init {
                overrides: Default::default(),
                capabilities: Default::default(),
                daemon_token: None,
            },
        ))
        .unwrap();
    drop(request_tx);
    worker.join().unwrap();

    let events: Vec<_> = event_rx.try_iter().collect();
    assert!(
        events.iter().any(
            |e| matches!(&e.event, Event::Error { message } if message == "already initialized")
        ),
        "expected strict already-initialized error: {events:?}"
    );
    assert!(!events
        .iter()
        .any(|e| matches!(e.event, Event::SessionAdopted { .. })));
}
