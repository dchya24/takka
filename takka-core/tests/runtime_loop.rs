//! Integration tests for the P1-1 `AgentRuntime` / `AgentLoop` split.
//!
//! Covers: standard loop end-to-end (sync + streaming), custom loop
//! dispatch (no conditionals in the runtime), pause/resume at
//! iteration boundaries, cancel plumbing, status observation, and the
//! P1-2 `on_state_change` handler.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use takka_core::{
    AgentLoop, AgentRuntime, Event, FileTracker, LLMProvider, Orchestrator, OrchestratorState,
    ToolRegistry,
};

mod support;
use support::{text_response, tool_call_response, ScriptedProvider, StreamingScriptedProvider};
use takka_core::providers::ChatResponse;

fn tools() -> ToolRegistry {
    let reg = ToolRegistry::new();
    for t in takka_core::tools::builtin_tools_with_tracker(Arc::new(FileTracker::new())) {
        reg.register(t);
    }
    reg
}

fn scripted_orchestrator(responses: Vec<ChatResponse>) -> Orchestrator {
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(responses));
    let orch = Orchestrator::new(provider, tools());
    orch.set_permission_mode(takka_core::safety::PermissionMode::Yolo);
    orch
}

/// The standard loop drives a real tool turn through the runtime.
#[test]
fn runtime_standard_loop_runs_tool_turn_end_to_end() {
    let dir = support::tempdir();
    let path = dir.join("note.txt");
    std::fs::write(&path, "payload\n").unwrap();

    let orch = scripted_orchestrator(vec![
        tool_call_response(
            "c1",
            "read_file",
            &serde_json::json!({"path": path.to_string_lossy()}),
        ),
        text_response("all done"),
    ]);

    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    orch.on_event(move |e| sink.lock().unwrap().push(e));

    let runtime = AgentRuntime::new(Arc::new(orch));

    let answer = runtime.run("read the file").unwrap();
    assert_eq!(answer, "all done");

    // Lifecycle envelope emitted exactly once, terminal state landed.
    let seen = events.lock().unwrap();
    assert!(matches!(seen.first(), Some(Event::SessionStarted)));
    let starts = seen
        .iter()
        .filter(|e| matches!(e, Event::SessionStarted))
        .count();
    assert_eq!(starts, 1, "one envelope per runtime run");
    assert!(matches!(seen.last(), Some(Event::SessionCompleted { .. })));

    // Status reflects the completed run.
    let status = runtime.status();
    assert_eq!(status.state, OrchestratorState::Completed);
    assert_eq!(status.loop_name, "standard");
    assert!(!status.paused);
}

/// A custom loop is dispatched without any runtime changes — this is
/// the seam `PlanningLoop` / `InteractiveLoop` will occupy.
#[test]
fn runtime_dispatches_to_custom_loop() {
    struct EchoLoop;
    impl AgentLoop for EchoLoop {
        fn name(&self) -> &str {
            "echo"
        }
        fn run(
            &self,
            _orchestrator: &Orchestrator,
            input: &str,
            _attachments: Vec<takka_core::Attachment>,
        ) -> Result<String, takka_core::AgenticError> {
            Ok(format!("echo: {input}"))
        }
        fn run_stream<'a>(
            &'a self,
            _orchestrator: &'a Orchestrator,
            input: &'a str,
            _attachments: Vec<takka_core::Attachment>,
            on_chunk: &'a mut (dyn FnMut(String) + Send),
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<String, takka_core::AgenticError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                on_chunk(format!("echo: {input}"));
                Ok(format!("echo: {input}"))
            })
        }
    }

    let orch = scripted_orchestrator(vec![]);
    let runtime = AgentRuntime::with_loop(Arc::new(orch), Box::new(EchoLoop));

    assert_eq!(runtime.status().loop_name, "echo");
    let answer = runtime.run("hello").unwrap();
    assert_eq!(answer, "echo: hello");
}

/// Runtime envelope also wraps streaming, and deltas reach the caller.
#[tokio::test]
async fn runtime_streams_deltas_through_envelope() {
    let provider: Arc<dyn LLMProvider> =
        Arc::new(StreamingScriptedProvider::new(vec![text_response(
            "streamed answer",
        )]));
    let orch = Orchestrator::new(provider, tools());
    orch.set_permission_mode(takka_core::safety::PermissionMode::Yolo);

    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    orch.on_event(move |e| sink.lock().unwrap().push(e));

    let runtime = AgentRuntime::new(Arc::new(orch));
    let chunks: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = chunks.clone();
    let answer = runtime
        .run_stream("go", move |delta| sink.lock().unwrap().push(delta))
        .await
        .unwrap();
    assert_eq!(answer, "streamed answer");
    assert!(!chunks.lock().unwrap().is_empty());
    assert!(matches!(
        events.lock().unwrap().last(),
        Some(Event::SessionCompleted { .. })
    ));
}

/// `pause()` parks the loop before the first iteration; `resume()`
/// releases it and the run completes normally.
#[test]
fn pause_parks_loop_until_resumed() {
    let orch = scripted_orchestrator(vec![text_response("after resume")]);
    let runtime = Arc::new(AgentRuntime::new(Arc::new(orch)));

    runtime.pause();
    assert!(runtime.status().paused);

    let rt = runtime.clone();
    let handle = std::thread::spawn(move || rt.run("go"));

    // Give the worker time to reach the park point; the loop must not
    // have entered an iteration yet (state still Created).
    std::thread::sleep(Duration::from_millis(300));
    assert!(runtime.status().paused, "still parked");
    assert_eq!(runtime.status().state, OrchestratorState::Created);

    runtime.resume();
    let answer = handle.join().unwrap().unwrap();
    assert_eq!(answer, "after resume");
    assert!(!runtime.status().paused);
    assert_eq!(runtime.status().state, OrchestratorState::Completed);
}

/// `runtime.cancel()` aborts a parked run at the next boundary.
#[test]
fn cancel_releases_parked_loop_with_cancelled_error() {
    let orch = scripted_orchestrator(vec![text_response("never reached")]);
    let runtime = Arc::new(AgentRuntime::new(Arc::new(orch)));

    runtime.pause();
    let rt = runtime.clone();
    let handle = std::thread::spawn(move || rt.run("go"));

    std::thread::sleep(Duration::from_millis(200));
    runtime.cancel();

    let result = handle.join().unwrap();
    assert!(
        matches!(result, Err(takka_core::AgenticError::Cancelled)),
        "expected Cancelled, got {:?}",
        result
    );
    assert_eq!(runtime.status().state, OrchestratorState::Cancelled);
}

/// The P1-2 `on_state_change` handler observes the transition sequence.
#[test]
fn on_state_change_observes_transitions() {
    let orch = scripted_orchestrator(vec![text_response("ok")]);

    let states: Arc<Mutex<Vec<OrchestratorState>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = states.clone();
    orch.on_state_change(move |s| sink.lock().unwrap().push(s));

    let runtime = AgentRuntime::new(Arc::new(orch));
    runtime.run("go").unwrap();

    let seen = states.lock().unwrap();
    assert_eq!(
        *seen,
        vec![
            OrchestratorState::WaitingForModel,
            OrchestratorState::Completed
        ],
        "legal transitions observed, got {:?}",
        *seen
    );
}
