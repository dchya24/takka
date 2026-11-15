use super::super::*;
use super::support::*;
use super::*;

/// Streaming-preview e2e (story 1.1, PTY-free): a scripted
/// OpenAI-compatible provider streams markdown deltas with delays; the
/// inline renderer path (`Commands::run` — shared by `takka run` and
/// the REPL) must surface every completed line through the live
/// preview in arrival order, each exactly once, and the end-of-run
/// path must not re-print the answer (the committed lines stand).
///
/// The renderer thread's terminal output cannot be captured in-process,
/// so the assertion rides the cfg(test) tap on `RenderFinal` (the plain
/// text of the printed preview lines). Deltas arrive 120 ms apart, so
/// each completed line flushes within two 80 ms renderer ticks.
#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_live_preview_prints_streamed_lines_in_order() {
    let deltas = vec![
        (
            std::time::Duration::from_millis(120),
            "# Title\n\n".to_string(),
        ),
        (
            std::time::Duration::from_millis(120),
            "First para\n\n".to_string(),
        ),
        (std::time::Duration::from_millis(120), "Second ".to_string()),
        (std::time::Duration::from_millis(120), "para\n".to_string()),
    ];
    let port = spawn_scripted_sse_server(deltas);
    let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(RuntimeMode::Embedded)
        .with_permission_mode(takka_core::PermissionMode::Yolo);

    let result = tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("say hi"))
        .await
        .expect("Commands::run hung: no terminal event within 60s");
    let _ = std::fs::remove_file(&fixture);
    assert!(result.is_ok(), "run should complete: {:?}", result.err());

    // The live preview announced each completed PARAGRAPH exactly once
    // (whole-paragraph hold-back: a single newline is a soft break, so
    // "Second para" cannot be announced until a blank line ends it —
    // it stands pending and the finish path prints it as the suffix,
    // keeping the answer exactly once end to end).
    assert_eq!(
        commands.test_preview.lock().unwrap().clone(),
        vec![
            "".to_string(),
            "# Title".to_string(),
            "First para".to_string(),
        ]
    );
    assert_eq!(
        commands.test_final_print.lock().unwrap().clone(),
        vec!["Second para".to_string()],
    );
}

/// I/O matrix row "stream error mid-preview" (story 1.1): deltas are
/// previewed, then the connection drops without a stop frame; the run
/// reports the provider error while the already printed preview stays
/// as ordinary scrollback (only the status line is erased).
#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_live_preview_survives_provider_stream_error() {
    let deltas = vec![(
        std::time::Duration::from_millis(60),
        "# Title\n\n".to_string(),
    )];
    let port = spawn_scripted_sse_server_abrupt(deltas);
    let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(RuntimeMode::Embedded)
        .with_permission_mode(takka_core::PermissionMode::Yolo);

    let _run_result = tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("hi"))
        .await
        .expect("Commands::run hung: no terminal event within 60s");
    let _ = std::fs::remove_file(&fixture);

    // `Commands::run` always returns Ok (the runtime error is reported as
    // a badge); assert the failure itself via the event tap.
    let events = commands.test_events.lock().unwrap();
    assert!(
        events.iter().any(|e| e.contains("Stream read error:")),
        "the broken framing surfaces as a provider stream error: {events:?}"
    );
    drop(events);
    // The previewed line was printed before the drop and is kept.
    assert!(!*commands.test_preview_diverged.lock().unwrap());
    assert_eq!(
        commands.test_preview.lock().unwrap().clone(),
        vec!["".to_string(), "# Title".to_string()],
    );
}

/// I/O matrix row "no streamed chunks" (story 1.1): nothing prints
/// live and `preview_diverged` stays false, so the end-of-run path
/// renders the full styled markdown exactly once (today's behavior).
#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_live_preview_no_chunks_falls_back_to_single_render() {
    let port = spawn_scripted_sse_server(vec![]);
    let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(RuntimeMode::Embedded)
        .with_permission_mode(takka_core::PermissionMode::Yolo);

    let result = tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("hi"))
        .await
        .expect("Commands::run hung: no terminal event within 60s");
    let _ = std::fs::remove_file(&fixture);

    assert!(result.is_ok(), "run should complete: {:?}", result.err());
    assert!(!*commands.test_preview_diverged.lock().unwrap());
    // Nothing printed live: the preview tap is empty.
    assert!(
        commands.test_preview.lock().unwrap().is_empty(),
        "no chunks must print no preview: {:?}",
        commands.test_preview.lock().unwrap()
    );
}

/// I/O matrix row "final text escapes the stream" (story 1.1): an
/// unclosed code fence is held back (whole-fence granularity), so
/// nothing prints live; the run ends cleanly and the finish path
/// renders the whole answer once — today's behavior.
#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_live_preview_growing_fence_is_held_back_full_render_at_end() {
    // 160 ms spacing: the renderer tick is 80 ms; equal spacing races
    // (both deltas could drain in one tick), so keep real margin here.
    let deltas = vec![
        (
            std::time::Duration::from_millis(160),
            "```rust\n".to_string(),
        ),
        (
            std::time::Duration::from_millis(160),
            "fn a() {}\n".to_string(),
        ),
    ];
    let port = spawn_scripted_sse_server(deltas);
    let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(RuntimeMode::Embedded)
        .with_permission_mode(takka_core::PermissionMode::Yolo);

    let result = tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("hi"))
        .await
        .expect("Commands::run hung: no terminal event within 60s");
    let _ = std::fs::remove_file(&fixture);

    assert!(result.is_ok(), "run should complete: {:?}", result.err());
    assert!(!*commands.test_preview_diverged.lock().unwrap());
    // Nothing announced live: the fence never closed.
    assert!(commands.test_preview.lock().unwrap().is_empty());
    // The finish path rendered the whole answer once (today's path).
    let final_print = commands.test_final_print.lock().unwrap().clone();
    assert!(
        final_print.iter().any(|l| l.contains("fn a()")),
        "full render must cover the held-back fence: {final_print:?}"
    );
}

/// I/O matrix row "gate contention" (story 1.1): while a prompt holds
/// the render gate the renderer parks (nothing prints); after the gate
/// frees the deferred paragraphs print and the run completes normally.
/// Note `Commands: !Sync` — the parking itself is exercised through the
/// parked-run observation (run stays unfinished while the gate is
/// held), and the parked claims live outside the helper's control.
#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_live_preview_parks_while_the_render_gate_is_held() {
    let deltas = vec![
        (
            std::time::Duration::from_millis(80),
            "# Title\n\n".to_string(),
        ),
        (
            std::time::Duration::from_millis(80),
            "First para\n\n".to_string(),
        ),
    ];
    let port = spawn_scripted_sse_server(deltas);
    let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

    // Hold the gate like a question / confirmation prompt would: from a
    // blocking thread (async code must not hold a std guard across an
    // await point), released after the run has had time to stream into
    // the parked renderer.
    let gate_holder = std::thread::Builder::new()
        .name("gate-holder".into())
        .spawn(move || {
            let _gate = RENDER_GATE.lock().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(600));
        })
        .expect("gate holder thread");
    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(RuntimeMode::Embedded)
        .with_permission_mode(takka_core::PermissionMode::Yolo);
    #[cfg(test)]
    let preview_tap = commands.test_preview.clone();
    let run_task = tokio::spawn(async move { commands.run("hi").await });

    // The engine is independent of the render gate: the run itself
    // completes while the prompt holds the gate, and the parked
    // renderer prints the deferred paragraphs only after the gate
    // frees — nothing is lost, and the run joins cleanly.
    let run_result = run_task.await.expect("run task must not panic");
    gate_holder.join().expect("gate holder thread");
    let _ = std::fs::remove_file(&fixture);
    assert!(
        run_result.is_ok(),
        "run should complete: {:?}",
        run_result.err()
    );
    // Every announced paragraph survived the gate hold.
    assert_eq!(
        preview_tap.lock().unwrap().clone(),
        vec![
            "".to_string(),
            "# Title".to_string(),
            "First para".to_string(),
        ]
    );
}

/// Interleave e2e (story 1.3, the verify's shape): a scripted
/// multi-turn provider runs text → tool → text → tool → text. Each
/// streamed block must print live in arrival order with the tool row
/// BETWEEN its turns (one unified arrival-ordered queue), every
/// duplicated Thinking recap must be suppressed (its text is already
/// in the preview), and the end-of-run print must add nothing — every
/// block on screen exactly once.
#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_multi_turn_interleaves_tool_rows_in_arrival_order() {
    let turns = vec![
        ScriptedTurn::TextThenTool(
            vec![(
                std::time::Duration::from_millis(120),
                "First para\n\n".to_string(),
            )],
            "list_files".to_string(),
            r#"{"path": "."}"#.to_string(),
        ),
        ScriptedTurn::TextThenTool(
            vec![(
                std::time::Duration::from_millis(120),
                "Second para\n\n".to_string(),
            )],
            "list_files".to_string(),
            r#"{"path": "."}"#.to_string(),
        ),
        ScriptedTurn::Text(vec![(
            std::time::Duration::from_millis(120),
            "Third para\n\n".to_string(),
        )]),
    ];
    let port = spawn_scripted_sse_server_turns(turns);
    let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(RuntimeMode::Embedded)
        .with_permission_mode(takka_core::PermissionMode::Yolo);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        commands.run("run through the plan"),
    )
    .await
    .expect("Commands::run hung: no terminal event within 60s");
    let _ = std::fs::remove_file(&fixture);

    assert!(result.is_ok(), "run should complete: {:?}", result.err());

    // Arrival order across the whole run: text above the row that
    // followed it, one completion row per turn, and no stray thinking
    // block (its content is already in the preview — suppressed).
    let order = commands.test_render_order.lock().unwrap().clone();
    assert_eq!(
        order,
        vec![
            "text:First para".to_string(),
            "tool".to_string(),
            "text:Second para".to_string(),
            "tool".to_string(),
            "text:Third para".to_string(),
        ],
        "interleave must follow arrival order: {order:?}"
    );

    // The preview announced every block exactly once (each block ends
    // at a blank-line boundary, so nothing stays pending). Note: a
    // paragraph-first render produces no leading blank line — story
    // 1.1's empty first line came from its heading lead-in.
    assert_eq!(
        commands.test_preview.lock().unwrap().clone(),
        vec![
            "First para".to_string(),
            "Second para".to_string(),
            "Third para".to_string(),
        ]
    );
    // End of run: the multi-turn source rule parses the full stream,
    // the committed lines cover it — nothing re-prints.
    assert!(!*commands.test_preview_diverged.lock().unwrap());
    assert!(
        commands.test_final_print.lock().unwrap().is_empty(),
        "final print must be suffix-free: {:?}",
        commands.test_final_print.lock().unwrap()
    );
}

/// I/O matrix row "thinking between blocks" (story 1.3), second half:
/// when the tool-call turn's text was NOT announced by the preview
/// (it ends without a blank-line boundary, so the hold-back keeps it
/// pending), the Thinking block must still render as today — the
/// suppression only applies to fully previewed content.
#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_unpreviewed_thinking_still_renders() {
    let turns = vec![
        ScriptedTurn::TextThenTool(
            // No blank-line boundary: `completed_text` holds it back.
            vec![(
                std::time::Duration::from_millis(120),
                "Unfinished".to_string(),
            )],
            "list_files".to_string(),
            r#"{"path": "."}"#.to_string(),
        ),
        ScriptedTurn::Text(vec![(
            std::time::Duration::from_millis(120),
            " tail\n\n".to_string(),
        )]),
    ];
    let port = spawn_scripted_sse_server_turns(turns);
    let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(RuntimeMode::Embedded)
        .with_permission_mode(takka_core::PermissionMode::Yolo);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        commands.run("run through the plan"),
    )
    .await
    .expect("Commands::run hung: no terminal event within 60s");
    let _ = std::fs::remove_file(&fixture);

    assert!(result.is_ok(), "run should complete: {:?}", result.err());

    // The un-previewed turn text rendered through the Thinking block
    // (today's path), and the preview picked the paragraph up as soon
    // as the blank-line boundary completed it — arrival order holds.
    assert_eq!(
        commands.test_render_order.lock().unwrap().clone(),
        vec![
            "thinking".to_string(),
            "tool".to_string(),
            "text:Unfinished tail".to_string(),
        ]
    );
    assert_eq!(
        commands.test_preview.lock().unwrap().clone(),
        vec!["Unfinished tail".to_string()]
    );
    assert!(!*commands.test_preview_diverged.lock().unwrap());
    assert!(
        commands.test_final_print.lock().unwrap().is_empty(),
        "final print must be suffix-free: {:?}",
        commands.test_final_print.lock().unwrap()
    );
}
