use super::super::*;
use super::support::*;
use super::*;

/// I/O matrix row "cancel mid-preview" (story 1.4): the cancel flag
/// flips while streamed paragraphs are already printed; the run
/// returns gracefully, the preview lines stand as ordinary
/// scrollback, the final print stays empty (no re-render of the
/// answer), and "Cancelled by user" surfaces as the cancel notice —
/// not the red error badge. Hermetic scripted provider only.
#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_cancel_mid_preview_unwinds_with_notice() {
    // Multi-turn shape so the cancel has a boundary to win at: core
    // checks `AgenticError::Cancelled` at the START of every loop
    // iteration, and a single-turn run never loops — a mid-stream
    // cancel on a text-only turn would flag the request while the run
    // still completes normally (documented in the Implementation
    // Notes). Turns: text+tool → the flag lands mid-turn-1 streaming;
    // the tool call forces iteration 2, whose boundary check unwinds
    // with Cancelled.
    let turns = vec![
        ScriptedTurn::TextThenTool(
            vec![
                (
                    std::time::Duration::from_millis(120),
                    "# Title\n\n".to_string(),
                ),
                (
                    std::time::Duration::from_millis(120),
                    "First para\n\n".to_string(),
                ),
            ],
            "list_files".to_string(),
            r#"{"path": "."}"#.to_string(),
        ),
        ScriptedTurn::Text(vec![(
            std::time::Duration::from_millis(2000),
            "Second para\n\n".to_string(),
        )]),
    ];
    // flag lands mid-turn-1 streaming (the driver waits for the
    // second streamed paragraph's ModelChunk).
    let port = spawn_scripted_sse_server_turns(turns);
    let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

    let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(RuntimeMode::Embedded)
        .with_permission_mode(takka_core::PermissionMode::Yolo)
        .with_cancel_flag(cancel_flag.clone());

    // Drive the cancel from a plain thread while the run holds
    // `commands`: land the flag inside the inter-chunk gap so the
    // client's between-event cancel check sees it while turn-1 is
    // still streaming and the engine honours it at turn-1's boundary.
    // The scripted provider paces chunk-2 ~120 ms after chunk-1, so
    // 60 ms is safely inside that window; a later flip (e.g. past the
    // tool boundary) would be forwarded while the loop idles in
    // turn-2's scripted silence and never surface a Cancelled badge.
    let tap = commands.test_events.clone();
    let driver = std::thread::Builder::new()
        .name("cancel-driver".into())
        .spawn(move || {
            wait_for_tap_event(&tap, "ModelChunk");
            std::thread::sleep(std::time::Duration::from_millis(60));
            cancel_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        })
        .expect("cancel driver thread");

    // Commands::run always returns Ok — the unwind surfaces through
    // the outcome badge (test_notice), asserted below.
    let _outcome = tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("say hi"))
        .await
        .expect("Commands::run hung: no terminal event within 60s");
    driver.join().expect("cancel driver panicked");
    let _ = std::fs::remove_file(&fixture);
    // The Err unwind rendered the CANCEL notice; the red error badge
    // would have shown up here as RunOutcomeBadge::Error.
    // Lock once (a MutexGuard temporary in the assert would deadlock
    // the panic format's second lock on the same std Mutex).
    let notice = commands.test_notice.lock().unwrap().clone();
    assert_eq!(
        notice,
        vec![RunOutcomeBadge::Cancelled],
        "cancelled run must print the cancel notice: {notice:?}"
    );
    // Status line erased exactly once (the shared terminal-cleanup
    // block runs once per run, both outcomes).
    assert_eq!(
        commands.test_terminal_cleanup_count.load(Ordering::Relaxed),
        1,
        "terminal cleanup must run exactly once per run"
    );
    // Preview lines stand as ordinary scrollback (printed before the
    // cancel flipped; the run streams on briefly until the turn
    // boundary, which may only add more lines).
    let preview = commands.test_preview.lock().unwrap().clone();
    assert!(
        preview.iter().any(|l| l == "# Title"),
        "printed preview lines must survive the unwind: {preview:?}"
    );
    // No re-render of the answer on the cancel path.
    assert!(
        commands.test_final_print.lock().unwrap().is_empty(),
        "cancel path must not re-render the answer: {:?}",
        commands.test_final_print.lock().unwrap()
    );
}

/// Review fix (story 1.4 pass 1): the shared cancel flag must RESET at
/// each run start — the two-stage semantics are per-run. After a
/// mid-run cancel, a second run on the same `Commands` (as the inline
/// REPL does) completes normally: no insta-cancel, empty notice, and
/// the per-run reset makes the next Ctrl+C the cooperative one.
#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_cancel_then_next_run_starts_fresh() {
    let turns = vec![
        // Run 1: turned cancellation (mid-turn-2 provider call → the
        // iteration 3 boundary unwinds with Cancelled).
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
                std::time::Duration::from_millis(2000),
                "never reached\n\n".to_string(),
            )],
            "list_files".to_string(),
            r#"{"path": "."}"#.to_string(),
        ),
        // Run 2: a fresh single turn that completes normally.
        ScriptedTurn::Text(vec![(
            std::time::Duration::from_millis(120),
            "Second run\n\n".to_string(),
        )]),
    ];
    let port = spawn_scripted_sse_server_turns(turns);
    let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

    let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(RuntimeMode::Embedded)
        .with_permission_mode(takka_core::PermissionMode::Yolo)
        .with_cancel_flag(cancel_flag.clone());

    // Cancel run 1 once its second provider call is underway.
    let tap = commands.test_events.clone();
    let cancel_flag_for_driver = cancel_flag.clone();
    let driver = std::thread::Builder::new()
        .name("cancel-driver".into())
        .spawn(move || {
            wait_for_tap_event_count(&tap, "ModelRequest", 2);
            cancel_flag_for_driver.store(true, std::sync::atomic::Ordering::Relaxed);
            cancel_flag_for_driver.store(true, std::sync::atomic::Ordering::Relaxed);
        })
        .expect("cancel driver thread");

    let _outcome = tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("say hi"))
        .await
        .expect("Commands::run hung: no terminal event within 60s");
    driver.join().expect("cancel driver panicked");

    // Run 1 unwind recorded as the cancel notice.
    let first_notice = commands.test_notice.lock().unwrap().clone();
    assert_eq!(first_notice, vec![RunOutcomeBadge::Cancelled]);
    // The flag survived the unwind (the review's stale-flag defect):
    // the run-2 start reset is what must clear it.
    assert!(cancel_flag.load(std::sync::atomic::Ordering::Relaxed));

    // Run 2 on the same Commands completes normally — the per-run
    // reset cleared the stale flag before the watcher's first read.
    let _outcome = tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("again"))
        .await
        .expect("Commands::run hung: no terminal event within 60s");

    // Run 2 completed Ok: the badge list gains nothing (the tap only
    // records Err-branch badges) — the stale flag did not insta-cancel
    // the follow-up run.
    let notices = commands.test_notice.lock().unwrap().clone();
    assert_eq!(
        notices,
        vec![RunOutcomeBadge::Cancelled],
        "run 2 must complete normally (no second badge): {notices:?}"
    );
    // Run 2's preview printed normally and the final render is empty
    // (suffix-only, the answer printed ahead of the run ending).
    let preview = commands.test_preview.lock().unwrap().clone();
    assert!(
        preview.iter().any(|l| l == "Second run"),
        "run 2 must complete with its own preview: {preview:?}"
    );
    assert!(commands.test_final_print.lock().unwrap().is_empty());
}

/// I/O matrix row "cancel during thinking" (story 1.4): the flag
/// flips after the session started but before the first delta
/// arrives; the run unwinds cleanly at the boundary, nothing prints
/// live, and the notice prints once.
#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_cancel_during_thinking_prints_notice_without_live_lines() {
    // Multi-turn shape (see smoke_cancel_mid_preview): the flag flips
    // inside turn 1's think window (first delta lands 2s in; the
    // paragraph never reaches a blank-line boundary, so even a
    // late-arriving delta cannot announce a live line), and the tool
    // call forces an iteration 2 whose boundary check unwinds with
    // Cancelled.
    let turns = vec![
        ScriptedTurn::TextThenTool(
            vec![
                (
                    std::time::Duration::from_millis(2000),
                    "Unfinished ".to_string(),
                ),
                (std::time::Duration::from_millis(120), " tail\n".to_string()),
            ],
            "list_files".to_string(),
            r#"{"path": "."}"#.to_string(),
        ),
        ScriptedTurn::Text(vec![(
            std::time::Duration::from_millis(2000),
            "never reached\n\n".to_string(),
        )]),
    ];
    let port = spawn_scripted_sse_server_turns(turns);
    let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

    let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(RuntimeMode::Embedded)
        .with_permission_mode(takka_core::PermissionMode::Yolo)
        .with_cancel_flag(cancel_flag.clone());

    // Flip the flag as soon as the run has started (turn 1's
    // provider call is inside its 2s think window — no delta has
    // arrived).
    let tap = commands.test_events.clone();
    let driver = std::thread::Builder::new()
        .name("cancel-driver".into())
        .spawn(move || {
            wait_for_tap_event(&tap, "SessionStarted");
            cancel_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        })
        .expect("cancel driver thread");

    // Commands::run always returns Ok — the unwind surfaces through
    // the outcome badge (test_notice), asserted below.
    let _outcome = tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("hi"))
        .await
        .expect("Commands::run hung: no terminal event within 60s");
    driver.join().expect("cancel driver panicked");
    let _ = std::fs::remove_file(&fixture);

    // Lock once (a MutexGuard temporary in the assert would deadlock
    // the panic format's second lock on the same std Mutex).
    let notice = commands.test_notice.lock().unwrap().clone();
    assert_eq!(
        notice,
        vec![RunOutcomeBadge::Cancelled],
        "notice must print exactly once on the thinking-phase cancel: {notice:?}"
    );
    assert_eq!(
        commands.test_terminal_cleanup_count.load(Ordering::Relaxed),
        1,
        "terminal cleanup must run exactly once per run"
    );
    // No live lines: nothing was announced while the run unwound.
    let preview = commands.test_preview.lock().unwrap().clone();
    assert!(
        preview.is_empty(),
        "no paragraph completed before the cancel: {preview:?}"
    );
    assert!(commands.test_final_print.lock().unwrap().is_empty());
}

/// I/O matrix row "cancel during tool execution" (story 1.4): the
/// flag flips after the tool executed; the tool row and preview
/// lines stand, the run unwinds at the next turn boundary, and the
/// notice (not the red badge) prints.
#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_cancel_during_tool_execution_keeps_rows_and_unwinds() {
    let turns = vec![
        ScriptedTurn::TextThenTool(
            vec![(
                std::time::Duration::from_millis(120),
                "First para\n\n".to_string(),
            )],
            "list_files".to_string(),
            r#"{"path": "."}"#.to_string(),
        ),
        // Tool-phase cancel, deterministically: the flag lands inside
        // turn 2's provider call — which runs for 2s behind its first
        // SSE delta — and turn 2 carries its own tool call, creating
        // the iteration 3 boundary the unwind needs.
        ScriptedTurn::TextThenTool(
            vec![(
                std::time::Duration::from_millis(2000),
                "Third para\n\n".to_string(),
            )],
            "list_files".to_string(),
            r#"{"path": "."}"#.to_string(),
        ),
        ScriptedTurn::Text(vec![(
            std::time::Duration::from_millis(120),
            "never reached\n\n".to_string(),
        )]),
    ];
    let port = spawn_scripted_sse_server_turns(turns);
    let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

    let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(RuntimeMode::Embedded)
        .with_permission_mode(takka_core::PermissionMode::Yolo)
        .with_cancel_flag(cancel_flag.clone());

    // Flip inside turn 2's provider call: `ModelRequest` for
    // iteration 2 is emitted AFTER that iteration's boundary check
    // (so the flag cannot race ahead of it) and while the provider
    // call streams for 2s — plenty of room for the 10 ms tap poll.
    // The iteration 3 boundary (turn 2's tool call) then unwinds with
    // Cancelled. The tool row from turn 1 (already printed) stands.
    let tap = commands.test_events.clone();
    let driver = std::thread::Builder::new()
        .name("cancel-driver".into())
        .spawn(move || {
            wait_for_tap_event_count(&tap, "ModelRequest", 2);
            cancel_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        })
        .expect("cancel driver thread");

    // Commands::run always returns Ok — the unwind surfaces through
    // the outcome badge (test_notice), asserted below.
    let _outcome = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        commands.run("run through the plan"),
    )
    .await
    .expect("Commands::run hung: no terminal event within 60s");
    driver.join().expect("cancel driver panicked");
    let _ = std::fs::remove_file(&fixture);

    // Lock once (a MutexGuard temporary in the assert would deadlock
    // the panic format's second lock on the same std Mutex).
    let notice = commands.test_notice.lock().unwrap().clone();
    assert_eq!(
        notice,
        vec![RunOutcomeBadge::Cancelled],
        "tool-phase cancel must unwind through the cancel notice: {notice:?}"
    );
    assert_eq!(
        commands.test_terminal_cleanup_count.load(Ordering::Relaxed),
        1,
        "terminal cleanup must run exactly once per run"
    );
    // Tool row + preview lines stand.
    let order = commands.test_render_order.lock().unwrap().clone();
    assert!(
        order.contains(&"tool".to_string()),
        "tool row must have rendered before the cancel: {order:?}"
    );
    let preview = commands.test_preview.lock().unwrap().clone();
    assert!(
        preview.iter().any(|l| l == "First para"),
        "preview lines must survive the tool-phase cancel: {preview:?}"
    );
    // No re-render on the cancel path.
    assert!(commands.test_final_print.lock().unwrap().is_empty());
}
