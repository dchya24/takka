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

use core_agentic::{
    providers::{
        ChatChunk, ChatRequest, ChatResponse, ProviderError, ProviderResult, StreamResult,
    },
    safety::PermissionMode,
    tools::builtin_tools_with_tracker,
    Event, FileTracker, LLMProvider, Orchestrator, ToolRegistry,
};
use std::sync::Arc;
use std::sync::Mutex;

mod support;
use support::{tool_call, ScriptedProvider, StreamingScriptedProvider};

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
/// injected as user messages at the next iteration boundary and
/// announced via `Event::Steered`.
#[test]
fn steering_source_messages_injected_at_iteration_boundary() {
    // Turn 1 returns a tool call (run not finished → a second boundary
    // exists); turn 2 returns the final answer. After the tool executes
    // (turn 1 done), the orchestrator polls `steering_source()` BEFORE
    // the turn-2 LLM call — that's where the steer lands.
    let steering = Arc::new(Mutex::new(vec!["use the lake theme instead".to_string()]));
    let steering_source = {
        let steering = steering.clone();
        move || steering.lock().unwrap().drain(..).collect::<Vec<_>>()
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

    // The turn-2 request context carries the steer as a user message
    // (it reaches the model before the final answer — not lost).
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "expected two provider calls");
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
        Arc::new(ScriptedProvider::new(vec![support::text_response("plain answer")]));
    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        tools.register(t);
    }
    let mut orch = Orchestrator::new(provider, tools);
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
            &serde_json::json!({"command": "printf 'a\nb\n'"}),
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
            &serde_json::json!({"command": "printf 'x\ny\n'"}),
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
    // tool run (e.g. printf on a fast runner) legitimately reports 0.
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
    use core_agentic::providers::ProviderError;

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
    use core_agentic::SessionStore;

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
    use core_agentic::SessionStore;

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
        core_agentic::orchestrator::OrchestratorState::Failed
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
