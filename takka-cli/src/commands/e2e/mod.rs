//! End-to-end smoke tests for the `Commands::run` pipeline.
//!
//! These exercise the full pipeline against a scripted mock provider so
//! refactors in the CLI wiring are caught before they reach a release.
//! The orchestrator integration tests in `takka-core` cover the loop
//! itself; here we validate the CLI layer (orchestrator init, tool
//! registration, event plumbing, output rendering) end to end.

mod cancel;
mod preview;
mod steer;
mod support;

use super::*;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use crate::client::RuntimeMode;

/// Write a minimal config file pinning the spawned `takka-runtime`
/// daemon to a provider endpoint that can never connect.
///
/// The daemon loads its own config (`RuntimeEngine::initialize`), so a
/// test must pin it explicitly — otherwise it reads the ambient
/// `~/.config/agentic/config.json` and these tests would call a real
/// LLM provider with the developer's credentials (and fail on CI, where
/// no config exists). `http://127.0.0.1:9` (discard port) fails fast
/// with a connection error and never leaves the machine, keeping the
/// tests hermetic.
pub(crate) fn hermetic_runtime_config() -> std::path::PathBuf {
    hermetic_runtime_config_at("http://127.0.0.1:9")
}

/// Same shape as [`hermetic_runtime_config`], pointed at an explicit
/// provider endpoint (the scripted SSE server of the streaming tests).
pub(crate) fn hermetic_runtime_config_at(api_base: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "agentic-e2e-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(
        &path,
        format!(
            r#"{{
  "providers": [
    {{
      "name": "hermetic-test",
      "type": "openai-compatible",
      "api_base": "{api_base}",
      "api_key": "unused",
      "models": [
        {{ "model": "hermetic-model", "temperature": 0.0, "max_tokens": 64 }}
      ]
    }}
  ]
}}"#
        ),
    )
    .expect("write hermetic runtime config");
    path
}

/// `run_with_callbacks` must stream the session lifecycle
/// events through to the caller's callback, in BOTH connection modes.
/// The embedded engine runs as a thread inside this process; the
/// spawned mode launches the real `takka-runtime` subprocess over
/// the wire protocol. The pinned unreachable provider makes the
/// session fail deterministically in both cases.
///
/// Tool-call / model-response semantics are covered by
/// `takka-core/tests/runtime_engine.rs` (in-process engine tests).
pub(crate) async fn smoke_lifecycle(mode: RuntimeMode) {
    let fixture = hermetic_runtime_config();

    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(mode)
        .with_permission_mode(takka_core::PermissionMode::Yolo);

    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        commands.run_with_callbacks(
            "list files",
            |_| {},
            move |evt| {
                captured.lock().unwrap().push(format!("{evt:?}"));
            },
        ),
    )
    .await
    .expect("run_with_callbacks hung: no terminal event within 60s");
    let _ = std::fs::remove_file(&fixture);

    // The terminal value depends on how the engine ends a failed
    // session (Done or Error) — only the streamed lifecycle is asserted.
    let _ = result;
    let logged = events.lock().unwrap();
    let all: String = logged.join(" ");
    assert!(
        all.contains("SessionStarted"),
        "Expected SessionStarted lifecycle event, got: {all}"
    );
    assert!(
        all.contains("SessionFailed"),
        "Expected SessionFailed after provider failure, got: {all}"
    );
}

#[tokio::test]
pub(crate) async fn smoke_lifecycle_embedded() {
    smoke_lifecycle(RuntimeMode::Embedded).await;
}

#[tokio::test]
pub(crate) async fn smoke_lifecycle_spawned() {
    smoke_lifecycle(RuntimeMode::Spawn).await;
}

/// Regression test for the frozen-"Thinking…" deadlock: `Commands::run`
/// (the inline renderer-thread path) used to leave `chunk_tx` alive in
/// its own stack frame while blocking on `renderer.join()`. The renderer
/// waits for `Disconnected` on the chunk channel, which only happens when
/// *every* `Sender` is dropped — so it never exited and the spinner
/// stayed on screen forever.
///
/// Driven through both connection modes with an unreachable provider so
/// the renderer must still join cleanly on the session-failure path.
/// Multi-thread runtime + timeout: without the fix this test fails with
/// a timeout instead of hanging the whole test binary.
pub(crate) async fn smoke_no_deadlock(mode: RuntimeMode) {
    let fixture = hermetic_runtime_config();

    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(mode)
        .with_permission_mode(takka_core::PermissionMode::Yolo);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        commands.run("who are you?"),
    )
    .await
    .expect("Commands::run deadlocked: renderer.join() never returned");
    let _ = std::fs::remove_file(&fixture);

    // `run` renders session failures as an error badge and returns Ok —
    // the regression here is returning at all, not the outcome.
    assert!(result.is_ok(), "run should complete: {:?}", result.err());
}

#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_no_deadlock_embedded() {
    smoke_no_deadlock(RuntimeMode::Embedded).await;
}

#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_no_deadlock_spawned() {
    smoke_no_deadlock(RuntimeMode::Spawn).await;
}

/// Follow-up queue flow (both transports), per the steer-queue Task 3
/// brief: first run against the hermetic unreachable provider, then
/// `submit_user_text` in FollowUp mode queues locally, and
/// `drain_follow_ups` issues the queued text as a NEW client-issued
/// run — its SessionStarted arrives again, and total client-issued
/// Runs == queue items + 1.
///
/// The drained run travels through `Commands::run` (the renderer
/// path), so its lifecycle is observed through the cfg(test) event tap
/// rather than the caller callback used for the first run.
pub(crate) async fn smoke_follow_up_queue(mode: RuntimeMode) {
    let fixture = hermetic_runtime_config();

    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(mode)
        .with_permission_mode(takka_core::PermissionMode::Yolo);
    assert_eq!(commands.queue_mode_getter(), QueueMode::FollowUp);

    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();

    let first = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        commands.run_with_callbacks(
            "first run",
            |_| {},
            move |evt| {
                captured.lock().unwrap().push(format!("{evt:?}"));
            },
        ),
    )
    .await
    .expect("first run hung: no terminal event within 60s");
    // The provider is unreachable; the session fails deterministically.
    // The outcome value itself is not the assertion here.
    let _ = first;

    // FollowUp mode: text submitted once the run is over queues
    // locally instead of steering.
    commands
        .submit_user_text("second run".to_string())
        .await
        .expect("submit_user_text should queue in FollowUp mode");
    assert_eq!(commands.pending_follow_ups().len(), 1);

    // Drain after Done: the queued text is sent as the next run.
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        commands.drain_follow_ups(),
    )
    .await
    .expect("drain hung")
    .expect("drain failed");

    let _ = std::fs::remove_file(&fixture);

    // Client-issued guarantee: exactly one Run per queue item, on top
    // of the first one.
    assert_eq!(
        commands.test_runs.load(Ordering::Relaxed),
        2,
        "expected first run + 1 drained follow-up run"
    );

    // The drained run started its own session (SessionStarted again).
    let drained: String = commands.test_events.lock().unwrap().join(" ");
    assert!(
        drained.contains("SessionStarted"),
        "drained follow-up run should emit SessionStarted, got: {drained}"
    );

    // …and the first run streamed its lifecycle to the caller callback.
    let all: String = events.lock().unwrap().join(" ");
    assert!(
        all.contains("SessionStarted"),
        "Expected SessionStarted on first run, got: {all}"
    );
}

#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_follow_up_queue_embedded() {
    smoke_follow_up_queue(RuntimeMode::Embedded).await;
}

#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_follow_up_queue_spawned() {
    smoke_follow_up_queue(RuntimeMode::Spawn).await;
}

/// Interrupt mode (both transports): text submitted outside an active
/// run (the only case this task wires) is sent fire-and-forget as a
/// steer — never queued locally. The engine's strict gating answers
/// `Error { "no active run" }` on the event stream (Task 2); the stray
/// reply is skipped by the next run's request-id filter.
pub(crate) async fn smoke_interrupt_mode_does_not_queue(mode: RuntimeMode) {
    let fixture = hermetic_runtime_config();

    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(mode)
        .with_permission_mode(takka_core::PermissionMode::Yolo)
        .with_queue_mode(QueueMode::Interrupt);

    // Establish the connection with a first run (hermetic fixture).
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        commands.run_with_callbacks(
            "first run",
            |_| {},
            move |evt| {
                captured.lock().unwrap().push(format!("{evt:?}"));
            },
        ),
    )
    .await
    .expect("first run hung");

    // Interrupt mode: fire-and-forget steer, local queue untouched.
    commands
        .submit_user_text("steer with no active run".to_string())
        .await
        .expect("submit_user_text should send the steer");
    assert!(
        commands.pending_follow_ups().is_empty(),
        "interrupt mode must not queue locally"
    );

    let _ = std::fs::remove_file(&fixture);
}

#[tokio::test]
pub(crate) async fn smoke_interrupt_mode_does_not_queue_embedded() {
    smoke_interrupt_mode_does_not_queue(RuntimeMode::Embedded).await;
}

#[tokio::test]
pub(crate) async fn smoke_interrupt_mode_does_not_queue_spawned() {
    smoke_interrupt_mode_does_not_queue(RuntimeMode::Spawn).await;
}
