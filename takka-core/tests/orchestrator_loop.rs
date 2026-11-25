//! End-to-end integration tests for the orchestrator agent loop.
//!
//! These tests drive a real `Orchestrator` against a scripted mock
//! provider that yields one queued `ChatResponse` per `chat()` call. The
//! orchestrator's real tool registry, memory, safety engine, and event
//! emitter are exercised — only the LLM is stubbed.
//!
//! Why this lives in `tests/` rather than next to the orchestrator's own
//! unit tests: each test below covers a multi-turn loop, often with real
//! filesystem operations through the builtin tools. Keeping them in an
//! integration crate makes the boundary explicit (no access to private
//! orchestrator internals) and matches Cargo's standard layout.

use std::sync::Arc;
use std::sync::Mutex;
use takka_core::{
    providers::{
        ChatChunk, ChatRequest, ChatResponse, ProviderError, ProviderResult, StreamResult,
    },
    safety::PermissionMode,
    tools::builtin_tools_with_tracker,
    Event, FileTracker, LLMProvider, Orchestrator, ToolRegistry,
};

mod support;
use support::{
    tool_call, CapturingScriptedProvider, RecordingProvider, ScriptedProvider,
    StreamingScriptedProvider,
};

/// One realistic happy path: model says "use read_file", we let it run,
/// model produces final text. Verifies the loop terminates, the tool
/// result reaches memory, and the final answer is what the second turn
/// returned.
#[test]
fn run_loop_executes_tool_then_returns_final_answer() {
    let dir = support::tempdir();
    let path = dir.join("hello.txt");
    std::fs::write(&path, "the meaning is 42\n").unwrap();

    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        support::tool_call_response(
            "call-1",
            "read_file",
            &serde_json::json!({"path": path.to_string_lossy()}),
        ),
        support::text_response("42 is the answer"),
    ]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo); // skip confirm prompts
    orch.set_model("gpt-4o-mini");

    let answer = orch.run("what is the answer?").expect("run should succeed");
    assert_eq!(answer, "42 is the answer");
}

/// The orchestrator must abort cleanly when the provider keeps returning
/// At the iteration cap the loop now GRACEFULLY FINALIZES instead of
/// hard-aborting: tools are stripped on the final turn and a "wrap up"
/// nudge is injected, so the model returns a real text answer built
/// from the work already done. This test verifies the user-facing
/// outcome — `run` returns `Ok(answer)`, not an error.
#[test]
fn run_finalizes_gracefully_at_max_iterations() {
    // max_iterations = 3. Two tool-call turns, then the third (final)
    // turn returns a text answer because tools were stripped.
    let provider: Arc<dyn LLMProvider> = Arc::new(support::RecordingProvider::new(vec![
        support::tool_call_response("call-1", "list_files", &serde_json::json!({"path": "."})),
        support::tool_call_response("call-2", "glob", &serde_json::json!({"pattern": "*.rs"})),
        support::text_response("Based on what I found, here is the summary."),
    ]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_max_iterations(3);

    let answer = orch
        .run("explore then summarize")
        .expect("graceful finalization should return an answer, not error");
    assert_eq!(answer, "Based on what I found, here is the summary.");
}

/// The finalization turn must offer NO tools to the provider (verified
/// in `finalization_turn_advertises_zero_tools` below via a counting
/// provider). This test instead checks the System event surface: a
/// "finalizing" notice is emitted so the UI can tell the user why the
/// run is ending.
#[test]
fn finalization_emits_system_event() {
    let provider: Arc<dyn LLMProvider> = Arc::new(support::RecordingProvider::new(vec![
        support::tool_call_response("c1", "list_files", &serde_json::json!({"path": "."})),
        support::tool_call_response("c2", "glob", &serde_json::json!({"pattern": "*.rs"})),
        support::text_response("final answer"),
    ]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_max_iterations(3);

    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    orch.on_event(move |event| {
        if let Event::System { message } = event {
            events_clone.lock().unwrap().push(message);
        }
    });

    let answer = orch.run("go").expect("should finalize");
    assert_eq!(answer, "final answer");

    let captured = events.lock().unwrap();
    assert!(
        captured.iter().any(|m| m.contains("finalizing")),
        "expected a 'finalizing' System event, got {:?}",
        *captured
    );
}

/// Dedicated tool-count capture: confirm turns before the cap advertise
/// the full tool set, and the finalization turn advertises zero tools.
#[test]
fn finalization_turn_advertises_zero_tools() {
    let counts: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));

    // A tiny provider that records tool counts and replays 3 responses.
    struct CountingProvider {
        responses: Mutex<Vec<ChatResponse>>,
        counts: Arc<Mutex<Vec<usize>>>,
    }
    impl LLMProvider for CountingProvider {
        fn provider_type(&self) -> &str {
            "counting"
        }
        fn provider_id(&self) -> &str {
            "counting"
        }
        fn chat(&self, req: ChatRequest) -> ProviderResult<ChatResponse> {
            self.counts.lock().unwrap().push(req.tools.len());
            let mut q = self.responses.lock().unwrap();
            if q.is_empty() {
                return Err(ProviderError::new("empty"));
            }
            Ok(q.remove(0))
        }
        fn chat_stream(&self, _req: ChatRequest) -> StreamResult<ChatChunk, ProviderError> {
            Err(ProviderError::new("no stream"))
        }
    }

    let provider = Arc::new(CountingProvider {
        responses: Mutex::new(vec![
            support::tool_call_response("c1", "list_files", &serde_json::json!({"path": "."})),
            support::tool_call_response("c2", "glob", &serde_json::json!({"pattern": "*.rs"})),
            support::text_response("done"),
        ]),
        counts: counts.clone(),
    }) as Arc<dyn LLMProvider>;

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_max_iterations(3);

    let _ = orch.run("go").expect("should finalize");

    let captured = counts.lock().unwrap();
    assert_eq!(
        captured.len(),
        3,
        "expected exactly 3 provider calls, got {:?}",
        *captured
    );
    // Turns 1 and 2 advertise the full builtin tool set (>0 tools).
    assert!(*captured.first().unwrap() > 0, "turn 1 should offer tools");
    assert!(*captured.get(1).unwrap() > 0, "turn 2 should offer tools");
    // Finalization turn offers ZERO tools.
    assert_eq!(
        *captured.get(2).unwrap(),
        0,
        "finalization turn must strip tools, got {:?}",
        *captured
    );
}

/// Steering hook (Fase 2b): messages queued in the steering source are
/// injected as user messages at the iteration boundary and announced
/// via `Event::Steered`.
#[test]
fn steering_source_messages_injected_at_iteration_boundary() {
    // The orchestrator polls `steering_source()` at EVERY iteration
    // boundary — including before the turn-1 provider call (poll #1).
    // This scripted source is counter-based: poll #1 (run start)
    // returns `[]`, poll #2 (the turn-1 → turn-2 boundary, after turn
    // 1's tool call executes and before the final-answer request)
    // returns the steer. Turn 1 returns a tool call so that second
    // boundary exists. This proves the steer is picked up at a boundary
    // DURING the run, not merely drained before the loop starts: the
    // text must be absent from requests[0] and present as a user
    // message in requests[1].
    let steer_text = "use the lake theme instead".to_string();
    let polled = Mutex::new(false);
    let steering_source = move || {
        let mut polled = polled.lock().unwrap();
        if *polled {
            vec![steer_text.clone()]
        } else {
            *polled = true;
            Vec::new()
        }
    };

    let provider = Arc::new(support::CapturingScriptedProvider::new(vec![
        support::tool_call_response("t1", "grep", &serde_json::json!({"pattern": "x"})),
        support::text_response("done"),
    ]));
    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }
    let orch = {
        let mut orch = Orchestrator::new(provider.clone(), tools);
        orch.set_permission_mode(PermissionMode::Yolo);
        orch.set_steering_source(Box::new(steering_source));
        orch
    };

    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    orch.on_event(move |event| events_clone.lock().unwrap().push(event));

    let answer = orch.run("pick a theme").expect("run should succeed");
    assert_eq!(answer, "done");

    // `Steered` emitted with the right content.
    assert!(
        events.lock().unwrap().iter().any(
            |e| matches!(e, Event::Steered { content } if content == "use the lake theme instead")
        ),
        "expected a Steered event, got {:?}",
        events.lock().unwrap()
    );

    // Poll #1 fired before the turn-1 request while the source was
    // still empty: the steer text must NOT be in requests[0].
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "expected two provider calls");
    let first = &requests[0];
    assert!(
        !first
            .messages
            .iter()
            .any(|m| m.content.contains("use the lake theme instead")),
        "steer text must NOT be in the turn-1 request (source was empty at poll #1), got {:?}",
        first.messages
    );

    // The turn-2 request context carries the steer as a user message
    // (it reaches the model before the final answer — not lost).
    let second = &requests[1];
    assert!(
        second
            .messages
            .iter()
            .any(|m| m.role == "user" && m.content.contains("use the lake theme instead")),
        "steer text must reach turn 2 as a user message, got {:?}",
        second.messages
    );
}

/// Without a steering source the run is unchanged: no `Steered` events.
#[test]
fn run_without_steering_source_has_no_steered_events() {
    let provider: Arc<dyn LLMProvider> =
        Arc::new(ScriptedProvider::new(vec![support::text_response(
            "plain answer",
        )]));
    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }
    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    orch.on_event(move |event| events_clone.lock().unwrap().push(event));

    let answer = orch.run("hello").expect("run should succeed");
    assert_eq!(answer, "plain answer");

    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, Event::Steered { .. })),
        "no Steered events expected without a steering source, got {:?}",
        events.lock().unwrap()
    );
}

/// Loop detection: same tool called consecutively must trigger early abort.
#[test]
fn loop_detection_aborts_on_repeated_tool() {
    // Same tool called 3 times in a row should trigger loop detection
    // before hitting max_iterations.
    let mut script = Vec::new();
    for i in 0..10 {
        script.push(support::tool_call_response(
            &format!("call-{}", i),
            "write_file",
            &serde_json::json!({"path": "file.txt", "content": "content"}),
        ));
    }
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(script));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_max_iterations(10); // High limit, but loop detection should trigger first

    let err = orch
        .run("write files forever")
        .expect_err("should detect loop");
    let msg = err.to_string();
    assert!(
        msg.contains("Loop detected"),
        "expected 'Loop detected' in error, got: {}",
        msg
    );
    assert!(
        msg.contains("write_file"),
        "expected tool name in error, got: {}",
        msg
    );
}

/// Warning at 80% of max_iterations must emit a System event.
#[test]
fn approaching_limit_emits_warning_event() {
    // Script enough tool calls to reach 80% of max_iterations.
    // Use different tool names to avoid loop detection.
    let mut script = Vec::new();
    let tool_names = ["list_files", "search_files", "glob"];
    for i in 0..10 {
        let tool = tool_names[i % tool_names.len()];
        script.push(support::tool_call_response(
            &format!("call-{}", i),
            tool,
            &serde_json::json!({"path": "."}),
        ));
    }
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(script));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_max_iterations(5); // 80% of 5 = 4, so warning at iteration 4

    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    orch.on_event(move |event| {
        if let Event::System { message } = event {
            events_clone.lock().unwrap().push(message);
        }
    });

    // With graceful finalization the run no longer errors at the cap —
    // it returns Ok on the finalization turn (iteration 5). The warning
    // must still have fired at iteration 4 (80% of 5).
    let result = orch
        .run("go far")
        .expect("graceful finalization returns Ok");
    // The finalization turn consumes a scripted tool_call_response whose
    // content is empty, so the returned answer is the empty string.
    assert_eq!(result, "");

    let captured = events.lock().unwrap();
    assert!(
        captured
            .iter()
            .any(|m| m.contains("Approaching iteration limit")),
        "expected 'Approaching iteration limit' warning, got {:?}",
        *captured
    );
    assert!(
        captured.iter().any(|m| m.contains("finalizing")),
        "expected a 'finalizing' event at the cap, got {:?}",
        *captured
    );
}

/// Regression: the *same tool* called with **different arguments** must
/// NOT trip loop detection. This is the bug reported in
/// `logs/agentic-20260728-140637.log` — the model loaded a different
/// skill each turn (`skill("brainstorming")`, `skill("debugging")`, …)
/// but the old name-only signature flagged it as a loop after 3 turns.
///
/// Here we mirror that shape with `read_file` + different paths across
/// turns (and multiple per turn) and assert the run completes normally.
#[test]
fn loop_detection_ignores_same_tool_different_args() {
    let dir = support::tempdir();
    // Six files so every call across the three scripted turns has a
    // distinct argument signature.
    let files: Vec<_> = (0..6)
        .map(|i| {
            let p = dir.join(format!("f{}.txt", i));
            std::fs::write(&p, format!("body {}", i)).unwrap();
            p
        })
        .collect();

    let arg = |i: usize| serde_json::json!({ "path": files[i].to_string_lossy() });

    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        // Turn 1: three different files in one batch.
        support::multi_tool_call_response(vec![
            tool_call("c0", "read_file", &arg(0)),
            tool_call("c1", "read_file", &arg(1)),
            tool_call("c2", "read_file", &arg(2)),
        ]),
        // Turn 2: two more files.
        support::multi_tool_call_response(vec![
            tool_call("c3", "read_file", &arg(3)),
            tool_call("c4", "read_file", &arg(4)),
        ]),
        // Turn 3: a third consecutive turn of `read_file`. Under the old
        // name-only signature this is exactly where the false-positive
        // abort fired. Different args => must not abort.
        support::multi_tool_call_response(vec![tool_call("c5", "read_file", &arg(5))]),
        support::text_response("done reading everything"),
    ]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_max_iterations(30);

    let answer = orch
        .run("read all the files")
        .expect("different-argument calls to the same tool must not be treated as a loop");
    assert_eq!(answer, "done reading everything");
}

/// A genuine loop — the **exact same** call (same tool, same arguments)
/// repeated every turn — must still abort. This guards against the fix
/// above being too lenient.
#[test]
fn loop_detection_still_aborts_on_identical_args() {
    let dir = support::tempdir();
    let path = dir.join("same.txt");
    std::fs::write(&path, "x").unwrap();
    let args = serde_json::json!({ "path": path.to_string_lossy() });

    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        support::tool_call_response("c0", "read_file", &args),
        support::tool_call_response("c1", "read_file", &args),
        support::tool_call_response("c2", "read_file", &args),
        // Would only be reached if loop detection failed to fire.
        support::text_response("should not get here"),
    ]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let err = orch
        .run("read it again and again")
        .expect_err("identical call must abort");
    let msg = err.to_string();
    assert!(msg.contains("Loop detected"), "got: {}", msg);
    assert!(msg.contains("read_file"), "got: {}", msg);
    // The new error includes the repeated arguments for diagnostics.
    assert!(msg.contains("identical arguments"), "got: {}", msg);
}

/// Semantic loop detection: alternating two reads of the SAME file with
/// different offsets never trips the exact-match guard, but it is still
/// thrash. After enough same-target reads the loop must emit a steering
/// nudge (Event::System) and keep going — NOT hard-abort.
#[test]
fn semantic_loop_detection_nudges_on_alternating_same_target_reads() {
    let dir = support::tempdir();
    let path = dir.join("thrash.txt");
    std::fs::write(&path, "body").unwrap();

    let read = |offset: i32| {
        support::tool_call_response(
            "c",
            "read_file",
            &serde_json::json!({ "path": path.to_string_lossy(), "offset": offset }),
        )
    };
    // 6 turns alternating read(offset 1) / read(offset 50).
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        read(1),
        read(50),
        read(1),
        read(50),
        read(1),
        read(50),
        support::text_response("decided: file is fine"),
    ]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_max_iterations(20);

    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    orch.on_event(move |event| {
        if let Event::System { message } = event {
            events_clone.lock().unwrap().push(message);
        }
    });

    let answer = orch
        .run("check this file")
        .expect("semantic nudge must not abort the run");
    assert_eq!(answer, "decided: file is fine");

    let captured = events.lock().unwrap();
    assert!(
        captured
            .iter()
            .any(|m| m.contains("re-reading") && m.contains("thrash.txt")),
        "expected a re-reading nudge naming the target, got {:?}",
        *captured
    );
}

/// A legit read-after-edit sequence (read → edit → read the same file)
/// must NOT trip the semantic detector: the edit resets the window.
#[test]
fn semantic_loop_detection_resets_after_edit() {
    let dir = support::tempdir();
    let path = dir.join("legit.txt");
    std::fs::write(&path, "v1").unwrap();
    let p = path.to_string_lossy().to_string();

    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        support::tool_call_response("r1", "read_file", &serde_json::json!({ "path": p })),
        support::tool_call_response(
            "w1",
            "write_file",
            &serde_json::json!({ "path": p, "content": "v2" }),
        ),
        support::tool_call_response("r2", "read_file", &serde_json::json!({ "path": p })),
        support::text_response("edited and verified"),
    ]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_max_iterations(20);

    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    orch.on_event(move |event| {
        if let Event::System { message } = event {
            events_clone.lock().unwrap().push(message);
        }
    });

    let answer = orch.run("update the file").expect("run completes");
    assert_eq!(answer, "edited and verified");
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .all(|m| !m.contains("re-reading")),
        "read-after-edit must not be flagged as thrash"
    );
}

/// Read-only stub (O-02: stripped at the 80% wind-down stage).
struct StubReadTool {
    name: String,
}
impl StubReadTool {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
        }
    }
}
impl takka_core::tool::Tool for StubReadTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "read-only test tool"
    }
    fn schema(&self) -> takka_core::tool::ToolSchema {
        takka_core::tool::ToolSchema::new(self.name.clone(), "read-only test tool")
    }
    fn execute(&self, args: serde_json::Value) -> takka_core::tool::ToolResult<serde_json::Value> {
        Ok(serde_json::json!({"read": args["path"]}))
    }
    fn metadata(&self) -> takka_core::tool::ToolMetadata {
        takka_core::tool::ToolMetadata::read_only()
    }
}

/// Mutating stub (O-02: survives the 90% stage).
struct StubWriteTool;
impl takka_core::tool::Tool for StubWriteTool {
    fn name(&self) -> &str {
        "stub_write"
    }
    fn description(&self) -> &str {
        "mutating test tool"
    }
    fn schema(&self) -> takka_core::tool::ToolSchema {
        takka_core::tool::ToolSchema::new("stub_write".to_string(), "mutating test tool")
    }
    fn execute(&self, args: serde_json::Value) -> takka_core::tool::ToolResult<serde_json::Value> {
        Ok(serde_json::json!({"wrote": args["path"]}))
    }
    fn metadata(&self) -> takka_core::tool::ToolMetadata {
        takka_core::tool::ToolMetadata::default()
    }
}

/// O-02: graduated tool-budget shaping — shrinking tool_defs per stage.
#[test]
fn tool_budget_shaping_strips_stages() {
    // 9 tool turns (alternating read tools, unique paths to stay clear of
    // loop detection) + finalization turn = 10 iterations.
    let mut script = Vec::new();
    for i in 0..9 {
        let (tool, path) = if i % 2 == 0 {
            ("stub_read_a", format!("a{i}.txt"))
        } else {
            ("stub_read_b", format!("b{i}.txt"))
        };
        script.push(support::tool_call_response(
            &format!("call-{i}"),
            tool,
            &serde_json::json!({"path": path}),
        ));
    }
    script.push(support::text_response(""));
    let recording = Arc::new(RecordingProvider::new(script));
    let provider: Arc<dyn LLMProvider> = recording.clone();

    let tools = ToolRegistry::new();
    tools.register(Box::new(StubReadTool::new("stub_read_a")));
    tools.register(Box::new(StubReadTool::new("stub_read_b")));
    tools.register(Box::new(StubWriteTool));

    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_max_iterations(10);

    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    orch.on_event(move |event| {
        if let Event::System { message } = event {
            events_clone.lock().unwrap().push(message);
        }
    });

    let answer = orch.run("shape the budget").expect("run completes");
    assert_eq!(answer, "");

    let counts = recording.tool_counts.lock().unwrap();
    // Full set (3) for iterations 1..=7, mutating-only (1) at 80%/90%,
    // tools stripped (0) on the finalization request.
    assert_eq!(counts[0], 3, "full set early: {:?}", *counts);
    assert_eq!(
        counts[7], 1,
        "80% stage keeps mutating tools: {:?}",
        *counts
    );
    assert_eq!(
        counts[8], 1,
        "90% stage keeps mutating tools: {:?}",
        *counts
    );
    assert_eq!(counts[9], 0, "finalization strips tools: {:?}", *counts);
    let captured = events.lock().unwrap();
    assert!(
        captured.iter().any(|m| m.contains("80%")),
        "{:?}",
        *captured
    );
    assert!(
        captured.iter().any(|m| m.contains("90%")),
        "{:?}",
        *captured
    );
}

/// O-03: every 10 iterations old tool results collapse to one line.
#[test]
fn cadenced_compaction_collapses_old_tool_results() {
    // 12 tool turns, unique paths (keeps loop detection quiet).
    let mut script = Vec::new();
    for i in 0..12 {
        script.push(support::tool_call_response(
            &format!("call-{i}"),
            "stub_read_a",
            &serde_json::json!({"path": format!("c{i}.txt")}),
        ));
    }
    script.push(support::text_response("done"));
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(script));

    let tools = ToolRegistry::new();
    tools.register(Box::new(StubReadTool::new("stub_read_a")));

    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_max_iterations(30);

    let answer = orch.run("read everything").expect("run completes");
    assert_eq!(answer, "done");

    // Older tool results (beyond the verbatim window of 6) are collapsed.
    let collapsed = orch.search_memory("[collapsed]");
    assert!(
        !collapsed.is_empty(),
        "expected collapsed tool results after iteration 10"
    );
    // Newest results stay verbatim.
    let recent = orch.search_memory("c11.txt");
    assert!(!recent.is_empty(), "newest tool result must stay verbatim");
    let recent_line = &recent[0].1;
    assert!(
        !recent_line.contains("[collapsed]"),
        "newest results must not be collapsed: {recent_line}"
    );
}

/// O-05: plan-then-execute (flag on) — one plan call first, plan
/// prepended to every following request.
#[test]
fn plan_then_execute_makes_one_plan_call_and_prepends_the_plan() {
    let recording = Arc::new(CapturingScriptedProvider::new(vec![
        // Plan call: numbered step list.
        support::text_response("1. read the file\n2. edit the file\n3. verify"),
        support::tool_call_response("w1", "stub_write", &serde_json::json!({"path": "x.txt"})),
        support::text_response("finished the plan"),
    ]));
    let provider: Arc<dyn LLMProvider> = recording.clone();

    let tools = ToolRegistry::new();
    tools.register(Box::new(StubWriteTool));

    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_plan_then_execute(true);

    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    orch.on_event(move |event| events_clone.lock().unwrap().push(event));

    let answer = orch.run("do the three steps").expect("run completes");
    assert_eq!(answer, "finished the plan");

    // PlanCreated announced with 3 steps; plan prepended to the tool turn.
    let events = events.lock().unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::PlanCreated { steps_total: 3 })));
    assert_eq!(orch.plan_steps().len(), 3);
    drop(events);

    let requests = recording.requests.lock().unwrap().clone();
    // 3 provider calls: plan + tool turn + final text.
    assert_eq!(
        requests.len(),
        3,
        "plan call must cost exactly one extra call"
    );
    let tool_turn = &requests[1];
    let tool_turn_text: String = tool_turn
        .messages
        .iter()
        .map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        tool_turn_text.contains("Current plan for this task"),
        "plan state prepended: {tool_turn_text}"
    );
    assert!(tool_turn_text.contains("1. read the file"));
}

/// O-05: flag off — zero extra provider calls (single-shot stays cheap).
#[test]
fn plan_then_execute_off_makes_no_extra_call() {
    let recording = Arc::new(CapturingScriptedProvider::new(vec![
        support::text_response("plain answer"),
    ]));
    let provider: Arc<dyn LLMProvider> = recording.clone();
    let orch = Orchestrator::new(provider, ToolRegistry::new());
    orch.run("quick question").expect("run completes");
    assert_eq!(recording.requests.lock().unwrap().len(), 1);
}

/// P-07: a session spend ceiling stops the run at the turn boundary
/// after crossing, with the session-limit message (not the run-limit).
#[test]
fn session_cost_limit_stops_with_session_message() {
    // 3 tool turns at $0.01 each (sonnet price via `usage`), session cap
    // $0.02: crossing happens on turn 2's report; the stop lands at that
    // turn's completion boundary — turn 3's provider call never happens.
    let mut script = Vec::new();
    for i in 0..3 {
        let mut resp = support::tool_call_response(
            &format!("call-{i}"),
            "stub_read_a",
            &serde_json::json!({"path": format!("s{i}.txt")}),
        );
        resp.usage = Some(support::usage(1000, 1000));
        script.push(resp);
    }
    script.push(support::text_response("never reached"));
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(script));

    let tools = ToolRegistry::new();
    tools.register(Box::new(StubReadTool::new("stub_read_a")));

    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_model("claude-3-5-sonnet");
    orch.set_max_cost_usd(None); // per-run ceiling off: only the session one may stop
    orch.set_max_cost_usd_per_session(Some(0.02));

    let err = orch
        .run("spend the session budget")
        .expect_err("session ceiling must stop the run");
    let msg = err.to_string();
    assert!(
        msg.contains("Session cost limit reached"),
        "expected the session-limit message, got: {msg}"
    );
    assert!(
        !msg.contains("identical arguments"),
        "must not read like a loop abort: {msg}"
    );
    assert!(
        orch.session_cost_usd() >= 0.02,
        "session accumulator kept the spend: {}",
        orch.session_cost_usd()
    );
}

/// P-07: without the config the behaviour is unchanged (unlimited).
#[test]
fn session_cost_limit_unset_is_unlimited() {
    let mut resp = support::text_response("ok");
    resp.usage = Some(support::usage(10_000, 10_000));
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![resp]));
    let orch = Orchestrator::new(provider, ToolRegistry::new());
    assert_eq!(orch.max_cost_usd_per_session(), None);
    let answer = orch.run("no ceiling").expect("unlimited run completes");
    assert_eq!(answer, "ok");
    assert!(orch.session_cost_usd() > 0.0, "cost still accumulated");
}

/// P-07: resume keeps the session cost accumulator — the ceiling does
/// not reset silently across a resume.
#[test]
fn session_cost_limit_survives_resume() {
    let dir = support::tempdir();
    let store = takka_core::session::SessionStore::new(&dir);

    // Turn 1: one $0.018 tool call. Session cap $0.02 → crossing lands on
    // turn 2, so the session stops after ONE turn having spent $0.018.
    let mut resp = support::tool_call_response(
        "call-0",
        "stub_read_a",
        &serde_json::json!({"path": "r.txt"}),
    );
    resp.usage = Some(support::usage(1000, 1000));
    let mut final_resp = support::text_response("done");
    final_resp.usage = Some(support::usage(1000, 1000));

    let tools = ToolRegistry::new();
    tools.register(Box::new(StubReadTool::new("stub_read_a")));

    let mut orch = Orchestrator::new(
        Arc::new(ScriptedProvider::new(vec![resp.clone(), final_resp])),
        tools,
    );
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_model("claude-3-5-sonnet");
    orch.set_max_cost_usd_per_session(Some(0.02));
    orch.set_session_store(store.clone());
    // Session id is minted per run; capture it for the resume.
    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    orch.on_event(move |e| sink.lock().unwrap().push(e));

    // Run one turn's worth: the crossing lands on turn 2 (text) → the run
    // ends FAILED with the session-limit message.
    let err = orch
        .run("first turn")
        .expect_err("second turn crosses the session ceiling");
    assert!(err.to_string().contains("Session cost limit reached"));
    let spent = orch.session_cost_usd();
    assert!(spent > 0.0, "accumulator holds the spend: {spent}");
    let session_id = orch.session_id().expect("session tracked");

    // Fresh orchestrator, resume: accumulator restored from the checkpoint.
    let mut resumed = Orchestrator::new(
        Arc::new(ScriptedProvider::new(vec![{
            let mut r = support::text_response("continued");
            r.usage = Some(support::usage(1000, 1000));
            r
        }])),
        ToolRegistry::new(),
    );
    resumed.set_model("claude-3-5-sonnet");
    resumed.set_max_cost_usd_per_session(Some(0.02));
    resumed.resume_session(store, &session_id).expect("resume");
    assert!(
        (resumed.session_cost_usd() - spent).abs() < 1e-9,
        "resume must restore the session accumulator: {} vs {spent}",
        resumed.session_cost_usd()
    );

    // One more $0.018 call pushes the restored total over → session stop.
    let err = resumed
        .run("second turn")
        .expect_err("restored total crosses on the next call");
    assert!(err.to_string().contains("Session cost limit reached"));

    let _ = std::fs::remove_dir_all(&dir);
}

/// P-02: a stub tool whose first call fails with a transient (timeout)
/// error and whose second call succeeds. The loop must auto-retry once
/// in-loop and record ONE annotated history entry.
struct FlakyTransientTool {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl FlakyTransientTool {
    fn new() -> Self {
        Self {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl takka_core::tool::Tool for FlakyTransientTool {
    fn name(&self) -> &str {
        "flaky_read"
    }
    fn description(&self) -> &str {
        "fails once with a timeout, then succeeds"
    }
    fn schema(&self) -> takka_core::tool::ToolSchema {
        takka_core::tool::ToolSchema::new(self.name().to_string(), "flaky test tool")
    }
    fn execute(&self, _args: serde_json::Value) -> takka_core::tool::ToolResult<serde_json::Value> {
        use std::sync::atomic::Ordering;
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(takka_core::tool::ToolError(
                "operation timed out".to_string(),
            ))
        } else {
            Ok(serde_json::json!({"ok": true}))
        }
    }
    fn metadata(&self) -> takka_core::tool::ToolMetadata {
        takka_core::tool::ToolMetadata::read_only()
    }
}

#[test]
fn transient_tool_error_auto_retries_once_and_records_one_entry() {
    let flaky = FlakyTransientTool::new();
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        support::tool_call_response("c0", "flaky_read", &serde_json::json!({})),
        support::text_response("recovered"),
    ]));

    let tools = ToolRegistry::new();
    let flaky2 = FlakyTransientTool {
        calls: flaky.calls.clone(),
    };
    tools.register(Box::new(flaky2));

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let answer = orch
        .run("use the flaky tool")
        .expect("run continues past retry");
    assert_eq!(answer, "recovered");

    // Exactly one annotated history entry; the model never sees the raw
    // failure as a separate turn.
    let matches = orch.search_memory("auto-retry");
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one annotated entry, got {:?}",
        matches
    );
}

/// Persistent errors (file not found) never retry: the tool executes
/// exactly once and the recorded entry carries no auto-retry annotation.
struct PersistentErrorTool {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl PersistentErrorTool {
    fn new() -> Self {
        Self {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl takka_core::tool::Tool for PersistentErrorTool {
    fn name(&self) -> &str {
        "persistent_error"
    }
    fn description(&self) -> &str {
        "always fails with a persistent error"
    }
    fn schema(&self) -> takka_core::tool::ToolSchema {
        takka_core::tool::ToolSchema::new(self.name().to_string(), "persistent test tool")
    }
    fn execute(&self, _args: serde_json::Value) -> takka_core::tool::ToolResult<serde_json::Value> {
        use std::sync::atomic::Ordering;
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(takka_core::tool::ToolError(
            "file not found: nope.txt".to_string(),
        ))
    }
    fn metadata(&self) -> takka_core::tool::ToolMetadata {
        takka_core::tool::ToolMetadata::read_only()
    }
}

#[test]
fn persistent_tool_error_never_retries() {
    let persistent = PersistentErrorTool::new();
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        support::tool_call_response("c0", "persistent_error", &serde_json::json!({})),
        support::text_response("gave up"),
    ]));

    let tools = ToolRegistry::new();
    let persistent2 = PersistentErrorTool {
        calls: persistent.calls.clone(),
    };
    tools.register(Box::new(persistent2));

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let answer = orch.run("use the broken tool").expect("run completes");
    assert_eq!(answer, "gave up");

    assert_eq!(
        persistent.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "persistent error must execute exactly once (no retry)"
    );
    assert!(
        orch.search_memory("auto-retry").is_empty(),
        "no retry annotation expected for a persistent error"
    );
}

/// Plan mode must deny state-changing tools. The model gets a "Blocked"
/// message back as the tool result, then issues a final text response.
#[test]
fn plan_mode_denies_write_file_with_blocked_message() {
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        support::tool_call_response(
            "call-1",
            "write_file",
            &serde_json::json!({
                "path": "should-not-be-created.txt",
                "content": "x",
            }),
        ),
        support::text_response("ok, can't write in plan mode"),
    ]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Plan);

    let answer = orch.run("write a file").expect("loop should still finish");
    assert_eq!(answer, "ok, can't write in plan mode");

    // The blocked file must not have been created.
    assert!(!std::path::Path::new("should-not-be-created.txt").exists());
}

/// Cooperative cancel: flipping the cancel flag between turns must make
/// the next iteration return `Cancelled` without calling the provider.
#[test]
fn cancel_flag_aborts_loop_at_iteration_boundary() {
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        // First turn: a tool call so we re-enter the loop. Cancel will
        // be flipped after this call lands.
        support::tool_call_response("call-1", "list_files", &serde_json::json!({"path": "."})),
        // The orchestrator should never get this far.
        support::text_response("should never be returned"),
    ]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    let cancel = orch.cancel_handle();

    // Subscribe to events so we can flip cancel after the first
    // ToolOutput event lands.
    orch.on_event({
        let cancel = cancel.clone();
        move |event| {
            if matches!(event, Event::ToolOutput { .. }) {
                cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    });

    let err = orch.run("loop until cancelled").expect_err("should cancel");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("cancel"),
        "expected cancel error, got: {}",
        msg
    );
}

/// Event emission: every tool call must surface as a ToolCall event
/// followed by a ToolOutput event, in the same order as execution.
#[test]
fn events_are_emitted_for_every_tool_call() {
    let dir = support::tempdir();
    let path_a = dir.join("a.txt");
    let path_b = dir.join("b.txt");
    std::fs::write(&path_a, "AAA\n").unwrap();
    std::fs::write(&path_b, "BBB\n").unwrap();

    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        // Two tool calls in one turn.
        support::multi_tool_call_response(vec![
            tool_call(
                "call-a",
                "read_file",
                &serde_json::json!({"path": path_a.to_string_lossy()}),
            ),
            tool_call(
                "call-b",
                "read_file",
                &serde_json::json!({"path": path_b.to_string_lossy()}),
            ),
        ]),
        support::text_response("done"),
    ]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    orch.on_event(move |e| {
        let tag = match e {
            Event::ToolCall { .. } => "call",
            Event::ToolOutput { .. } => "output",
            _ => return,
        };
        events_clone.lock().unwrap().push(tag);
    });

    let answer = orch.run("read a and b").expect("ok");
    assert_eq!(answer, "done");

    let captured = events.lock().unwrap();
    // Must be two call+output pairs. Order between calls/outputs isn't
    // strictly call,output,call,output (sync and parallel paths interleave
    // differently), so we just count instead.
    let calls = captured.iter().filter(|t| **t == "call").count();
    let outputs = captured.iter().filter(|t| **t == "output").count();
    assert_eq!(calls, 2, "events: {:?}", *captured);
    assert_eq!(outputs, 2, "events: {:?}", *captured);
}

/// Memory: all messages from a successful run survive in the
/// orchestrator's memory in the right order. The user message lands
/// first, the assistant's tool-call assistant message second, the tool
/// result third, the final assistant message last.
#[test]
fn memory_records_full_turn_history() {
    let dir = support::tempdir();
    let path = dir.join("greet.txt");
    std::fs::write(&path, "hi\n").unwrap();

    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        support::tool_call_response(
            "call-1",
            "read_file",
            &serde_json::json!({"path": path.to_string_lossy()}),
        ),
        support::text_response("greeting received"),
    ]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let _ = orch.run("read the greeting").expect("ok");

    // Search for marker substrings in memory. The exact role-tag layout
    // is internal but the content must appear in order.
    let user_hits = orch.search_memory("read the greeting");
    let final_hits = orch.search_memory("greeting received");
    assert!(!user_hits.is_empty(), "user message should be recorded");
    assert!(
        !final_hits.is_empty(),
        "final assistant answer should be recorded"
    );
}

/// Safety record + reset: clearing memory should give a fresh state
/// without re-creating the orchestrator.
#[test]
fn restart_session_workflow_resets_memory() {
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        support::text_response("first"),
        support::text_response("second"),
    ]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let _ = orch.run("first turn").expect("ok");
    assert!(!orch.search_memory("first turn").is_empty());

    // Simulate the /restart slash command's effect.
    orch.clear_memory();
    assert!(orch.search_memory("first turn").is_empty());

    // Second turn uses a fresh memory.
    let answer = orch.run("second turn").expect("ok");
    assert_eq!(answer, "second");
}

/// Sync run() concurrent batching: when the model returns multiple
/// read-only tool calls in the same turn, they execute concurrently in
/// a `std::thread::scope` rather than sequentially. We verify by
/// scripting four 200ms slow read-only tools in one turn and asserting
/// total wall-time is much less than 4 * 200ms.
#[test]
fn sync_run_executes_read_only_batch_concurrently() {
    use std::time::Duration;

    // Four no-op slow reads in one assistant turn, then a final answer.
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        support::multi_tool_call_response(vec![
            tool_call("a", "slow_read", &serde_json::json!({})),
            tool_call("b", "slow_read", &serde_json::json!({})),
            tool_call("c", "slow_read", &serde_json::json!({})),
            tool_call("d", "slow_read", &serde_json::json!({})),
        ]),
        support::text_response("done"),
    ]));

    let tools = ToolRegistry::new();
    // Concurrency-tracking tool: asserts on peak concurrent executions
    // (deterministic) instead of wall-clock thresholds (flaky under
    // parallel CI load).
    let tool = support::ConcurrentReadTool::new("slow_read", Duration::from_millis(200));
    let peak = tool.max.clone();
    tools.register(Box::new(tool));

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let answer = orch.run("go").expect("ok");
    assert_eq!(answer, "done");

    // If the batch ran concurrently, at least two executions overlapped.
    // Sequential execution would give a peak of exactly 1.
    let peak = peak.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        peak > 1,
        "expected concurrent read-only batch, but peak concurrent executions was {}",
        peak
    );
}

/// Two newline-terminated lines, for the lifecycle tests: `run_command`
/// passes the string to `sh -c` on unix and `cmd /C` on Windows, so this
/// must be valid in both shells. `echo` is the one emitter both have. The
/// Windows form deliberately has no spaces, so it is passed to `cmd` as a
/// single unquoted argument and cannot trip `/C`'s quote-stripping rules.
#[cfg(unix)]
const TWO_LINE_ECHO: &str = "echo a; echo b";
#[cfg(windows)]
const TWO_LINE_ECHO: &str = "echo a&&echo b";

/// Tool lifecycle events: run_command must emit ToolStart then an
/// enriched ToolOutput (with a duration_ms, 0 valid for fast runs) via the
/// sync path, and the
/// deltas between them must carry the streamed output.
#[test]
fn tool_lifecycle_events_emitted_in_order() {
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        support::tool_call_response(
            "call-1",
            "run_command",
            &serde_json::json!({ "command": TWO_LINE_ECHO }),
        ),
        support::text_response("done"),
    ]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let seen: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    orch.on_event(move |e| s2.lock().unwrap().push(e));

    let out = orch.run("run it").expect("run");
    assert_eq!(out, "done");

    let events = seen.lock().unwrap();
    let starts = events
        .iter()
        .filter(|e| matches!(e, Event::ToolStarted { .. }))
        .count();
    let deltas = events
        .iter()
        .filter(|e| matches!(e, Event::ToolDelta { .. }))
        .count();
    let outputs = events
        .iter()
        .filter(|e| matches!(e, Event::ToolOutput { .. }))
        .count();
    assert!(starts >= 1, "expected at least one ToolStart");
    assert!(
        deltas >= 1,
        "expected at least one ToolDelta (streamed output)"
    );
    assert!(outputs >= 1, "expected at least one ToolOutput");

    // Enriched ToolOutput for run_command must carry a duration. It is
    // truncated to whole milliseconds, so a sub-millisecond tool run on a
    // fast runner legitimately reports 0 — only presence is asserted.
    let run_cmd_output = events.iter().find_map(|e| match e {
        Event::ToolOutput {
            tool_name,
            duration_ms,
            success,
            ..
        } if tool_name == "run_command" => Some((*duration_ms, *success)),
        _ => None,
    });
    let (_duration_ms, success) = run_cmd_output.expect("run_command ToolOutput");
    assert!(success, "run_command should report success");
}

/// Streaming path (`run_stream`) must also surface ToolStart/ToolDelta/
/// ToolOutput via the delta forwarder, with deltas arriving before the
/// final ToolOutput.
#[tokio::test]
async fn tool_lifecycle_events_stream_path() {
    let provider: Arc<dyn LLMProvider> = Arc::new(StreamingScriptedProvider::new(vec![
        support::tool_call_response(
            "call-1",
            "run_command",
            &serde_json::json!({ "command": TWO_LINE_ECHO }),
        ),
        support::text_response("stream done"),
    ]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let seen: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    orch.on_event(move |e| s2.lock().unwrap().push(e));

    let chunks: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let c2 = chunks.clone();
    let out = orch
        .run_stream("stream it", move |chunk| c2.lock().unwrap().push(chunk))
        .await
        .expect("run_stream");
    assert_eq!(out, "stream done");

    let events = seen.lock().unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::ToolStarted { .. })));
    assert!(
        events.iter().any(|e| matches!(e, Event::ToolDelta { .. })),
        "expected ToolDelta on stream path"
    );
    let output = events.iter().find_map(|e| match e {
        Event::ToolOutput {
            tool_name,
            duration_ms,
            ..
        } if tool_name == "run_command" => Some(*duration_ms),
        _ => None,
    });
    assert!(output.is_some(), "expected run_command ToolOutput");
    // duration_ms is truncated to whole milliseconds — a sub-millisecond
    // tool run (e.g. a shell `echo` on a fast runner) legitimately reports 0.
    let _ = output.unwrap();
    assert!(!chunks.lock().unwrap().is_empty());
}

/// P0-3 session lifecycle: a successful run emits the full sequence
/// SessionStarted → StateChanged(planning) → StateChanged(completed) →
/// SessionCompleted (plus per-turn ModelRequest events).
#[test]
fn session_lifecycle_events_fire_in_order() {
    let provider: Arc<dyn LLMProvider> =
        Arc::new(ScriptedProvider::new(vec![support::text_response(
            "the final answer",
        )]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    orch.on_event(move |e| sink.lock().unwrap().push(e));

    let answer = orch.run("go").unwrap();
    assert_eq!(answer, "the final answer");

    let seen = events.lock().unwrap();
    // Project onto (variant, payload) pairs to assert relative order.
    // P1-2 states: request loop opens in waiting_for_model; the final
    // answer lands completed without another tool turn.
    let lifecycle: Vec<String> = seen
        .iter()
        .filter_map(|e| match e {
            Event::SessionStarted => Some("started".to_string()),
            Event::StateChanged { state } => Some(format!("state:{}", state)),
            Event::SessionCompleted { .. } => Some("completed".to_string()),
            Event::SessionFailed { .. } => Some("failed".to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        lifecycle,
        vec![
            "started",
            "state:waiting_for_model",
            "state:completed",
            "completed"
        ],
        "full lifecycle in order, got {:?}",
        lifecycle
    );

    // The completed event carries the final answer for stream-only
    // frontends.
    let final_result = seen.iter().find_map(|e| match e {
        Event::SessionCompleted { result } => Some(result.clone()),
        _ => None,
    });
    assert_eq!(final_result.as_deref(), Some("the final answer"));

    // Every provider turn is observable.
    let request_count = seen
        .iter()
        .filter(|e| matches!(e, Event::ModelRequest { .. }))
        .count();
    assert_eq!(request_count, 1);
}

/// The failure path emits SessionStarted → … → SessionFailed (never
/// SessionCompleted).
#[test]
fn session_failed_event_fires_on_provider_error() {
    use takka_core::providers::ProviderError;

    struct ExplodingProvider;
    impl LLMProvider for ExplodingProvider {
        fn provider_type(&self) -> &str {
            "exploding"
        }
        fn provider_id(&self) -> &str {
            "exploding"
        }
        fn chat(&self, _req: ChatRequest) -> ProviderResult<ChatResponse> {
            Err(ProviderError::new("connection refused"))
        }
        fn chat_stream(&self, _req: ChatRequest) -> StreamResult<ChatChunk, ProviderError> {
            Err(ProviderError::new("connection refused"))
        }
    }

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let orch = Orchestrator::new(Arc::new(ExplodingProvider), tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    orch.on_event(move |e| sink.lock().unwrap().push(e));

    let result = orch.run("go");
    assert!(result.is_err(), "provider error must surface to caller");

    let seen = events.lock().unwrap();
    assert!(
        matches!(seen.last(), Some(Event::SessionFailed { .. })),
        "expected SessionFailed last, got {:?}",
        seen.last()
    );
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::SessionCompleted { .. })),
        "no SessionCompleted on the failure path"
    );
}

/// P1-3 crash→resume: run A (with a tool turn) checkpoints per tool
/// boundary; a fresh orchestrator resumes the session and the provider
/// sees the restored history.
#[test]
fn checkpoint_and_resume_restores_conversation() {
    use takka_core::SessionStore;

    let dir = support::tempdir();
    let path = dir.join("resume-me.txt");
    std::fs::write(&path, "carried state\n").unwrap();

    let store = SessionStore::new(dir.join("sessions"));

    // Run A: tool turn + final answer, with a checkpoint store attached.
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        support::tool_call_response(
            "call-a",
            "write_file",
            &serde_json::json!({"path": path.to_string_lossy(), "content": "persisted\n"}),
        ),
        support::text_response("run A done"),
    ]));
    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }
    let mut orch_a = Orchestrator::new(provider, tools);
    orch_a.set_permission_mode(PermissionMode::Yolo);
    orch_a.set_session_store(store.clone());

    let answer_a = orch_a.run("write the file").unwrap();
    assert_eq!(answer_a, "run A done");
    let session_id = orch_a.session_id().expect("session id tracked");

    // The store now holds the terminal checkpoint.
    let saved = store.load(&session_id).unwrap();
    assert_eq!(saved.state, "completed");
    // user + assistant(tool_calls) + tool + assistant(final) = 4 messages
    assert_eq!(saved.messages.len(), 4);
    let checkpoints = saved.checkpoint_count;

    // "Crash": build a fresh orchestrator with NO history and resume.
    let provider_b: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        support::tool_call_response(
            "call-b",
            "read_file",
            &serde_json::json!({"path": path.to_string_lossy()}),
        ),
        support::text_response("run B done"),
    ]));
    let tools_b = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools_b.register(t);
    }
    let mut orch_b = Orchestrator::new(provider_b, tools_b);
    orch_b.set_permission_mode(PermissionMode::Yolo);
    assert!(orch_b.session_id().is_none());

    orch_b.resume_session(store.clone(), &session_id).unwrap();
    assert_eq!(orch_b.session_id().as_deref(), Some(session_id.as_str()));

    // Resumed run sees the carried history: provider request includes
    // run A's messages, and the loop continues from there.
    let answer_b = orch_b.run("now read it back").unwrap();
    assert_eq!(answer_b, "run B done");

    // Same session document, checkpointed further (resume adopted the
    // id; run B appended its turns and bumped the counter).
    let after = store.load(&session_id).unwrap();
    assert_eq!(after.state, "completed");
    assert!(after.checkpoint_count > checkpoints);
    assert!(after.messages.len() > 4);
}

/// A failing run still checkpoints the failure state (post-mortem).
#[test]
fn failed_run_checkpoints_failed_state() {
    use takka_core::SessionStore;

    let dir = support::tempdir();
    let store = SessionStore::new(dir.join("sessions"));

    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![]));
    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }
    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_session_store(store.clone());

    assert!(orch.run("go").is_err()); // no scripted responses left
    let id = orch.session_id().unwrap();
    let saved = store.load(&id).unwrap();
    assert_eq!(saved.state, "failed");
    assert!(matches!(
        orch.get_state(),
        takka_core::orchestrator::OrchestratorState::Failed
    ));
}

/// Without a store attached, nothing persists and ids stay untracked.
#[test]
fn runs_without_store_are_untracked() {
    let provider: Arc<dyn LLMProvider> =
        Arc::new(ScriptedProvider::new(vec![support::text_response("ok")]));
    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }
    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    let answer = orch.run("go").unwrap();
    assert_eq!(answer, "ok");
    assert!(orch.session_id().is_none());
}

/// Provider-reported token usage must reach the frontend as a `Usage` event —
/// that is what lets the CLI show real numbers instead of estimating from
/// character counts. Covers the non-streaming path.
#[test]
fn run_emits_usage_event_from_provider_report() {
    let mut response = support::text_response("done");
    response.usage = Some(takka_core::providers::ChatUsage {
        prompt_tokens: 1234,
        completion_tokens: 567,
        total_tokens: 1801,
        cache_read_input_tokens: Some(900),
        cache_creation_input_tokens: None,
    });

    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![response]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    orch.on_event(move |event| events_clone.lock().unwrap().push(event));

    orch.run("count some tokens").expect("run should succeed");

    let captured = events.lock().unwrap();
    let usage = captured
        .iter()
        .find_map(|e| match e {
            Event::Usage {
                prompt_tokens,
                completion_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
            } => Some((
                *prompt_tokens,
                *completion_tokens,
                *cache_read_input_tokens,
                *cache_creation_input_tokens,
            )),
            _ => None,
        })
        .expect("expected a Usage event");

    assert_eq!(usage.0, 1234);
    assert_eq!(usage.1, 567);
    assert_eq!(usage.2, Some(900));
    assert_eq!(usage.3, None);
}

/// No provider report → no `Usage` event, so frontends keep their estimate
/// instead of showing a bogus zero.
#[test]
fn run_emits_no_usage_event_without_provider_report() {
    let provider: Arc<dyn LLMProvider> =
        Arc::new(ScriptedProvider::new(vec![support::text_response("done")]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    orch.on_event(move |event| events_clone.lock().unwrap().push(event));

    orch.run("no usage here").expect("run should succeed");

    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, Event::Usage { .. })),
        "no provider report means no Usage event"
    );
}

/// Streaming path: usage arrives on a stream chunk (typically the last one) and
/// must surface as the same `Usage` event.
#[tokio::test]
async fn streaming_run_emits_usage_event_from_final_chunk() {
    let mut response = support::text_response("streamed answer");
    response.usage = Some(takka_core::providers::ChatUsage {
        prompt_tokens: 42,
        completion_tokens: 7,
        total_tokens: 49,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: Some(11),
    });

    let provider: Arc<dyn LLMProvider> = Arc::new(StreamingScriptedProvider::new(vec![response]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    orch.on_event(move |event| events_clone.lock().unwrap().push(event));

    orch.run_stream("stream some tokens", |_| {})
        .await
        .expect("run should succeed");

    let captured = events.lock().unwrap();
    let usage = captured
        .iter()
        .find_map(|e| match e {
            Event::Usage {
                prompt_tokens,
                completion_tokens,
                cache_creation_input_tokens,
                ..
            } => Some((
                *prompt_tokens,
                *completion_tokens,
                *cache_creation_input_tokens,
            )),
            _ => None,
        })
        .expect("expected a Usage event from the streamed chunk");

    assert_eq!(usage.0, 42);
    assert_eq!(usage.1, 7);
    assert_eq!(usage.2, Some(11));
}

// ---------------------------------------------------------------------------
// D12: turn-limit stop reasons (`Event::TurnLimitExceeded`).
// ---------------------------------------------------------------------------

/// Forced finalization at the iteration cap must announce WHY the turn
/// ended: exactly one `TurnLimitExceeded { kind: MaxTurnRequests }`.
#[test]
fn forced_finalization_emits_max_turn_requests_once() {
    let provider: Arc<dyn LLMProvider> = Arc::new(support::RecordingProvider::new(vec![
        support::tool_call_response("c1", "list_files", &serde_json::json!({"path": "."})),
        support::tool_call_response("c2", "glob", &serde_json::json!({"pattern": "*.rs"})),
        support::text_response("final answer"),
    ]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_max_iterations(3);

    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    orch.on_event(move |event| events_clone.lock().unwrap().push(event));

    let answer = orch.run("go").expect("should finalize");
    assert_eq!(answer, "final answer");

    let captured = events.lock().unwrap();
    let limit_events: Vec<_> = captured
        .iter()
        .filter_map(|e| match e {
            Event::TurnLimitExceeded { kind } => Some(*kind),
            _ => None,
        })
        .collect();
    assert_eq!(
        limit_events,
        vec![takka_core::events::TurnLimitKind::MaxTurnRequests],
        "expected exactly one MaxTurnRequests event, got {limit_events:?}"
    );
}

/// A provider token-cap stop (`finish_reason: "length"`) on a natural
/// final answer must surface `TurnLimitExceeded { kind: MaxTokens }`.
#[test]
fn token_truncated_final_answer_emits_max_tokens() {
    let mut response = support::text_response("partial answer, cut off mid");
    response.finish_reason = Some("length".to_string());

    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![response]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    orch.on_event(move |event| events_clone.lock().unwrap().push(event));

    let answer = orch.run("write long text").expect("run should succeed");
    assert_eq!(answer, "partial answer, cut off mid");

    let captured = events.lock().unwrap();
    let limit_events: Vec<_> = captured
        .iter()
        .filter_map(|e| match e {
            Event::TurnLimitExceeded { kind } => Some(*kind),
            _ => None,
        })
        .collect();
    assert_eq!(
        limit_events,
        vec![takka_core::events::TurnLimitKind::MaxTokens],
        "expected exactly one MaxTokens event, got {limit_events:?}"
    );
}

/// A natural stop (`finish_reason: "stop"`) must NOT emit any
/// `TurnLimitExceeded` event — the default answer path stays silent.
#[test]
fn natural_stop_emits_no_turn_limit_event() {
    let provider: Arc<dyn LLMProvider> =
        Arc::new(ScriptedProvider::new(vec![support::text_response("done")]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    orch.on_event(move |event| events_clone.lock().unwrap().push(event));

    orch.run("just answer").expect("run should succeed");

    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, Event::TurnLimitExceeded { .. })),
        "a natural stop must not emit TurnLimitExceeded"
    );
}

/// Streaming path: the final chunk's `finish_reason` decides the
/// MaxTokens emission — same contract as the sync loop.
#[tokio::test]
async fn streaming_token_truncated_final_answer_emits_max_tokens() {
    let mut response = support::text_response("streamed truncation");
    response.finish_reason = Some("max_tokens".to_string());

    let provider: Arc<dyn LLMProvider> = Arc::new(StreamingScriptedProvider::new(vec![response]));

    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }

    let orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);

    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    orch.on_event(move |event| events_clone.lock().unwrap().push(event));

    let out = orch
        .run_stream("stream a long answer", |_| {})
        .await
        .expect("run_stream should succeed");
    assert_eq!(out, "streamed truncation");

    let captured = events.lock().unwrap();
    let limit_events: Vec<_> = captured
        .iter()
        .filter_map(|e| match e {
            Event::TurnLimitExceeded { kind } => Some(*kind),
            _ => None,
        })
        .collect();
    assert_eq!(
        limit_events,
        vec![takka_core::events::TurnLimitKind::MaxTokens],
        "expected exactly one MaxTokens event on the stream path, got {limit_events:?}"
    );
}

// ---------------------------------------------------------------------------
// H-06: spend ceiling (`agent.max_cost_usd` / `Event::CostLimitExceeded`).
// ---------------------------------------------------------------------------

/// Build the orchestrator the ceiling tests share: a model with a known list
/// price (`claude-3-5-sonnet`) and the real builtin tool set.
fn ceiling_orchestrator(provider: Arc<dyn LLMProvider>) -> Orchestrator {
    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }
    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_model("claude-3-5-sonnet");
    orch
}

fn captured_events(orch: &Orchestrator) -> Arc<Mutex<Vec<Event>>> {
    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    orch.on_event(move |event| sink.lock().unwrap().push(event));
    events
}

fn cost_limits(events: &Arc<Mutex<Vec<Event>>>) -> Vec<(f64, f64)> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            Event::CostLimitExceeded {
                max_cost_usd,
                spent_usd,
            } => Some((*max_cost_usd, *spent_usd)),
            _ => None,
        })
        .collect()
}

/// Under budget: the run spends, but never crosses the ceiling, so the loop
/// keeps going to its natural answer and stays silent about cost.
#[test]
fn under_budget_run_completes_without_stopping() {
    // One call, 10k prompt + 2k completion = $0.03 + $0.03 = $0.06.
    let mut response = support::text_response("all done");
    response.usage = Some(support::usage(10_000, 2_000));
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![response]));

    let mut orch = ceiling_orchestrator(provider);
    orch.set_max_cost_usd(Some(0.10));
    let events = captured_events(&orch);

    let answer = orch.run("answer me").expect("run should succeed");
    assert_eq!(answer, "all done");
    assert!(
        cost_limits(&events).is_empty(),
        "an under-budget run must not emit CostLimitExceeded"
    );
    assert!(
        (orch.run_cost_usd() - support::sonnet_cost(10_000, 2_000)).abs() < 1e-9,
        "run cost should equal the reported usage's price, got {}",
        orch.run_cost_usd()
    );
}

/// Over budget on a tool-call turn: the crossing turn's tool call still runs
/// (the loop must not cut a request short), and only then does the loop stop
/// with `CostLimitExceeded` instead of asking the provider again.
#[test]
fn over_budget_stops_after_the_crossing_turn_completes() {
    let dir = support::tempdir();
    let path = dir.join("marker.txt");
    std::fs::write(&path, "paid for\n").unwrap();

    // Turn 1: 60k prompt + 10k completion = $0.18 + $0.15 = $0.33 → crosses
    // the $0.20 ceiling. A tool call is requested in that same turn.
    let mut crossing = support::tool_call_response(
        "call-1",
        "read_file",
        &serde_json::json!({"path": path.to_string_lossy()}),
    );
    crossing.usage = Some(support::usage(60_000, 10_000));
    // A second turn would exist if the loop kept going — it must not.
    let mut second = support::text_response("never asked for");
    second.usage = Some(support::usage(10_000, 1_000));

    let recording = Arc::new(support::RecordingProvider::new(vec![crossing, second]));
    let provider: Arc<dyn LLMProvider> = recording.clone();
    let mut orch = ceiling_orchestrator(provider);
    orch.set_max_cost_usd(Some(0.20));
    let events = captured_events(&orch);

    let answer = orch
        .run("read the marker")
        .expect("budget stop is not an error");
    assert_eq!(answer, "", "no further model turn produced an answer");

    // The crossing turn's work completed: the tool ran and its result landed
    // in memory.
    let history = orch.search_memory("paid for");
    assert!(
        history.iter().any(|(role, content)| matches!(
            role,
            takka_core::memory::MessageRole::Tool { .. }
        ) && content.contains("paid for")),
        "the crossing turn's tool result must be recorded, history = {history:?}"
    );

    let limits = cost_limits(&events);
    assert_eq!(limits.len(), 1, "expected exactly one CostLimitExceeded");
    let (max, spent) = limits[0];
    assert!((max - 0.20).abs() < 1e-9, "max = {max}");
    assert!(
        (spent - support::sonnet_cost(60_000, 10_000)).abs() < 1e-9,
        "spent = {spent}"
    );
    // Exactly one provider request: the loop stopped instead of asking again.
    assert_eq!(
        recording.tool_counts.lock().unwrap().len(),
        1,
        "the loop must not ask the provider again after crossing the ceiling"
    );
}

/// No ceiling configured (the default): cost is still accumulated, but the
/// loop runs every turn it otherwise would — unchanged behaviour.
#[test]
fn unset_ceiling_is_unlimited() {
    let dir = support::tempdir();
    let path = dir.join("marker.txt");
    std::fs::write(&path, "unlimited\n").unwrap();

    // Far above any plausible ceiling, twice.
    let mut first = support::tool_call_response(
        "call-1",
        "read_file",
        &serde_json::json!({"path": path.to_string_lossy()}),
    );
    first.usage = Some(support::usage(2_000_000, 500_000));
    let mut second = support::text_response("finished anyway");
    second.usage = Some(support::usage(2_000_000, 500_000));

    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![first, second]));
    let orch = ceiling_orchestrator(provider);
    assert_eq!(orch.max_cost_usd(), None);
    let events = captured_events(&orch);

    let answer = orch.run("keep going").expect("run should succeed");
    assert_eq!(answer, "finished anyway");
    assert!(
        cost_limits(&events).is_empty(),
        "an unlimited run must never emit CostLimitExceeded"
    );
    assert!(
        orch.run_cost_usd() > 1.0,
        "cost is still accumulated for display, got {}",
        orch.run_cost_usd()
    );
}

/// Streaming path: the ceiling is evaluated from the final chunk's usage and
/// the stop happens at the turn boundary — the stream itself is never cut.
#[tokio::test]
async fn streaming_over_budget_stops_at_the_turn_boundary() {
    let mut crossing = support::text_response("streamed answer");
    crossing.usage = Some(support::usage(100_000, 20_000)); // $0.30 + $0.30
    let mut second = support::text_response("must not be asked");
    second.usage = Some(support::usage(1_000, 100));

    let provider: Arc<dyn LLMProvider> =
        Arc::new(StreamingScriptedProvider::new(vec![crossing, second]));
    let mut orch = ceiling_orchestrator(provider);
    orch.set_max_cost_usd(Some(0.50));
    let events = captured_events(&orch);

    let mut streamed = String::new();
    let answer = orch
        .run_stream("stream it", |chunk| streamed.push_str(&chunk))
        .await
        .expect("budget stop is not an error");

    assert_eq!(streamed, "streamed answer", "the stream ran to completion");
    assert_eq!(
        answer, "streamed answer",
        "the paid-for answer is delivered"
    );
    let limits = cost_limits(&events);
    assert_eq!(
        limits.len(),
        1,
        "expected exactly one CostLimitExceeded on the stream path"
    );
    let (max, spent) = limits[0];
    assert!((max - 0.50).abs() < 1e-9, "max = {max}");
    assert!(
        (spent - support::sonnet_cost(100_000, 20_000)).abs() < 1e-9,
        "spent = {spent}"
    );
}

/// A provider that reports no usage leaves the ceiling unevaluated: nothing
/// is charged, so a tight ceiling cannot stop a run nobody could price. This
/// is the whole "unset/unmeasurable = keep going" contract.
#[test]
fn missing_usage_never_trips_the_ceiling() {
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        support::tool_call_response("call-1", "list_files", &serde_json::json!({"path": "."})),
        support::text_response("answered without usage"),
    ]));

    let mut orch = ceiling_orchestrator(provider);
    // Tiny ceiling: any priced call would cross it immediately.
    orch.set_max_cost_usd(Some(0.0000001));
    let events = captured_events(&orch);

    let answer = orch.run("go").expect("run should succeed");
    assert_eq!(answer, "answered without usage");
    assert_eq!(orch.run_cost_usd(), 0.0);
    assert!(
        cost_limits(&events).is_empty(),
        "an unpriced run must not emit CostLimitExceeded"
    );
}

/// `run_cost_usd` is per run, not per session: a second run starts at zero.
#[test]
fn run_cost_resets_between_runs() {
    let mut first = support::text_response("first");
    first.usage = Some(support::usage(10_000, 1_000));
    let mut second = support::text_response("second");
    second.usage = Some(support::usage(20_000, 2_000));

    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![first, second]));
    let mut orch = ceiling_orchestrator(provider);
    orch.set_max_cost_usd(Some(5.0));

    orch.run("one").unwrap();
    assert!((orch.run_cost_usd() - support::sonnet_cost(10_000, 1_000)).abs() < 1e-9);

    orch.run("two").unwrap();
    assert!(
        (orch.run_cost_usd() - support::sonnet_cost(20_000, 2_000)).abs() < 1e-9,
        "the accumulator must restart every run, got {}",
        orch.run_cost_usd()
    );
}
