use super::super::*;
use super::support::*;
use super::*;

/// Story 2.1 tracer e2e: the run is in Interrupt queue mode; typed
/// keys injected mid-run (turn 2's provider window) echo visually,
/// Enter submits through the steer contract (Request::Steer), the
/// engine's SteerQueued/Steered events render their acks, and the
/// next provider turn's request body carries the steered text in the
/// LLM context.
#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_typed_mid_run_steers_into_the_context() {
    let turns = vec![
        ScriptedTurn::TextThenTool(
            vec![(
                std::time::Duration::from_millis(120),
                "First para\n\n".to_string(),
            )],
            "list_files".to_string(),
            r#"{"path": "."}"#.to_string(),
        ),
        // Turn 2: long think (2 s behind its first delta) — the keys
        // land inside this window; the tool call forces the
        // iteration-3 boundary that drains the steer.
        ScriptedTurn::TextThenTool(
            vec![(
                std::time::Duration::from_millis(2000),
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
    let (port, captured) = spawn_scripted_sse_server_turns_with_capture(turns, Default::default());
    let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

    let (key_tx, key_rx) = std::sync::mpsc::channel::<MidRunKey>();
    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(RuntimeMode::Embedded)
        .with_permission_mode(takka_core::PermissionMode::Yolo)
        .with_queue_mode(QueueMode::Interrupt)
        .with_mid_run_keys(key_rx);

    // Inject keys inside turn 2's provider window (the 2nd ModelRequest
    // emitted = the boundary check ran, provider streaming).
    let tap = commands.test_events.clone();
    let driver = std::thread::Builder::new()
        .name("steer-driver".into())
        .spawn(move || {
            wait_for_tap_event_count(&tap, "ModelRequest", 2);
            for ch in "check the wix".chars() {
                key_tx.send(MidRunKey::Char(ch)).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            key_tx.send(MidRunKey::Enter).unwrap();
        })
        .expect("steer driver thread");

    let _outcome = tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("say hi"))
        .await
        .expect("Commands::run hung: no terminal event within 60s");
    driver.join().expect("steer driver panicked");
    let _ = std::fs::remove_file(&fixture);

    // Engine events: SteerQueued on submit, Steered at the boundary.
    let order = commands.test_render_order.lock().unwrap().clone();
    assert!(
        order
            .iter()
            .any(|m| m.starts_with("steer-queued:check the wix")),
        "the submit must steer with an on-screen acknowledgement: {order:?}"
    );
    assert!(
        order.iter().any(|m| m == "steered:check the wix"),
        "the boundary drain must apply the steer: {order:?}"
    );

    // The typed echo was visible while typing.
    let echoes = commands.test_input_echoes.lock().unwrap().clone();
    assert!(
        echoes.iter().any(|e| e.contains("check the wix")),
        "the typed text must echo above the status line: {echoes:?}"
    );

    // The steered text reaches the next provider turn as user context.
    let bodies = captured.lock().unwrap().clone();
    assert!(
        bodies
            .iter()
            .filter(|b| b.contains("check the wix"))
            .count()
            >= 1,
        "a later provider request must carry the steered text: {bodies:?}"
    );
    assert!(
        commands.test_final_print.lock().unwrap().is_empty(),
        "answer fully previewed; final print must be suffix-free"
    );
}

/// Story 2.3, edge 1 (prompt gate): keys injected while a prompt
/// owns stdin must NOT enter the steer buffer — the reader pauses on
/// `INTERACTIVE_PROMPT_ACTIVE`; after the gate clears, the reader
/// resumes accepting text mid-run (asserted by a later steer).
#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_prompt_gate_keeps_keys_away_from_steers() {
    let turns = vec![
        // Turn 1: long think with a tool call afterwards (the gate
        // window lives here).
        ScriptedTurn::TextThenTool(
            vec![(
                std::time::Duration::from_millis(300),
                "First para\n\n".to_string(),
            )],
            "list_files".to_string(),
            r#"{"path": "."}"#.to_string(),
        ),
        // Turn 2: a steer lands here after the gate clears.
        ScriptedTurn::TextThenTool(
            vec![(
                std::time::Duration::from_millis(2000),
                "after the gate\n\n".to_string(),
            )],
            "list_files".to_string(),
            r#"{"path": "."}"#.to_string(),
        ),
        ScriptedTurn::Text(vec![(
            std::time::Duration::from_millis(120),
            "Third para\n\n".to_string(),
        )]),
    ];
    let (port, captured) = spawn_scripted_sse_server_turns_with_capture(turns, Default::default());
    let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

    let prompt_gate = std::sync::Arc::new(AtomicBool::new(false));
    prompt_gate.store(true, Ordering::SeqCst);
    let (key_tx, key_rx) = std::sync::mpsc::channel::<MidRunKey>();
    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(RuntimeMode::Embedded)
        .with_permission_mode(takka_core::PermissionMode::Yolo)
        .with_queue_mode(QueueMode::Interrupt)
        .with_mid_run_keys(key_rx)
        .with_mid_run_notice_sink(Arc::new(std::sync::Mutex::new(Vec::new())))
        .with_mid_run_prompt_gate(prompt_gate.clone());

    // Driver: keys land BEHIND the gate (the flag set before the
    // run's first provider call and cleared just before the keys in
    // turn 2's window), then a REAL steer after the clear.
    let tap = commands.test_events.clone();
    let driver = std::thread::Builder::new()
        .name("gate-driver".into())
        .spawn(move || {
            wait_for_tap_event_count(&tap, "ModelRequest", 1);
            // Behind the gate:
            for ch in "behind the gate".chars() {
                key_tx.send(MidRunKey::Char(ch)).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            key_tx.send(MidRunKey::Enter).unwrap();
            // The gate clears mid-run:
            wait_for_tap_event_count(&tap, "ModelRequest", 2);
            prompt_gate.store(false, Ordering::SeqCst);
            for ch in "after the gate".chars() {
                key_tx.send(MidRunKey::Char(ch)).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            key_tx.send(MidRunKey::Enter).unwrap();
        })
        .expect("gate driver thread");

    let _outcome = tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("say hi"))
        .await
        .expect("Commands::run hung: no terminal event within 60s");
    driver.join().expect("gate driver panicked");
    let _ = std::fs::remove_file(&fixture);

    // The keys behind the gate never steered (no "behind the gate"
    // traffic in the steer history at all).
    let order = commands.test_render_order.lock().unwrap().clone();
    assert!(
        !order.iter().any(|m| m.contains("behind the gate")),
        "keys behind a prompt gate must not steer: {order:?}"
    );
    // The post-gate steer did land (the reader resumed).
    assert!(
        order.iter().any(|m| m.contains("after the gate")),
        "the resumed reader must steer through mid-run text: {order:?}"
    );

    // The provider capture carries the post-gate steer only.
    let bodies = captured.lock().unwrap().clone();
    assert!(
        bodies
            .iter()
            .filter(|b| b.contains("after the gate"))
            .count()
            >= 1,
        "the post-gate steer must reach the provider turn: {bodies:?}"
    );
    assert!(
        !bodies.iter().any(|b| b.contains("behind the gate")),
        "the gated keys must never carry context: {bodies:?}"
    );
}

/// Story 2.3, edge 2 (post-run steer): the steering queue persists
/// across runs — a steer arriving AFTER the first run's terminal
/// event applies as the FIRST user message of the follow-up run,
/// seen in the provider's captured request body.
#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_post_run_steer_lands_in_the_next_run() {
    // Turn 1: text+tool; turn 2: a 2 s think then a tool call (so a
    // TURN 3 request follows, which is where the steer text arrives);
    // turn 3: text only (its completion closes the run).
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
                std::time::Duration::from_millis(2000),
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
    let (port, captured) = spawn_scripted_sse_server_turns_with_capture(turns, Default::default());
    let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

    let (key_tx, key_rx) = std::sync::mpsc::channel::<MidRunKey>();
    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(RuntimeMode::Embedded)
        .with_permission_mode(takka_core::PermissionMode::Yolo)
        .with_queue_mode(QueueMode::Interrupt)
        .with_mid_run_keys(key_rx);

    // Inject the steer text inside TURN 3's provider window: the
    // first run's last tool boundary is behind, so the text arrives
    // in the engine's steering queue AFTER the run's last turn — its
    // next boundary is whatever run/follow-up consumes it next.
    let tap = commands.test_events.clone();
    let driver = std::thread::Builder::new()
        .name("steer-driver".into())
        .spawn(move || {
            // Inject INSIDE the turn-2 provider window (16 s long);
            // the steer request ships while turn 2 streams, and the
            // engine drains it at the boundary right before turn 3's
            // provider call — with the text in turn 3's request body
            // (the carry-over semantics at the next boundary).
            wait_for_tap_event_count(&tap, "ModelRequest", 2);
            for ch in "carry into run two".chars() {
                key_tx.send(MidRunKey::Char(ch)).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            key_tx.send(MidRunKey::Enter).unwrap();
        })
        .expect("steer driver thread");

    let _outcome = tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("say hi"))
        .await
        .expect("Commands::run hung: no terminal event within 60s");
    driver.join().expect("steer driver panicked");
    let _ = std::fs::remove_file(&fixture);

    // The steer carries forward: run 2's next provider request
    // contains "carry into run two" as USER context.
    let bodies = captured.lock().unwrap().clone();
    let last = bodies.last().expect("run 2 must post a request");
    assert!(
        last.contains("carry into run two"),
        "the post-run steer must carry into the next run: {last:?}"
    );
    // The run-2 flow completes with the steer visible in the run tap
    // (3rd ModelRequest and the steered text marker).
    let order = commands.test_render_order.lock().unwrap().clone();
    assert!(
        order.iter().any(|m| m.contains("carry into run two")),
        "the carry-over steer must surface to the UI: {order:?}"
    );
    assert!(
        commands.test_final_print.lock().unwrap().is_empty(),
        "previewed answers stay suffix-free"
    );
}

/// Story 2.2 e2e: FollowUp mode (the CLI default) — typed keys mid-run
/// route into the client-side follow-up queue (visible via the hook
/// notice tap), and the existing depth-guarded drain executes the
/// queued line as the next run; the provider captures run 2's request
/// body carrying the queued text as its task.
#[tokio::test(flavor = "multi_thread")]
pub(crate) async fn smoke_followup_mode_queues_and_drains_visibly() {
    let turns = vec![
        ScriptedTurn::TextThenTool(
            vec![(
                std::time::Duration::from_millis(120),
                "First para\n\n".to_string(),
            )],
            "list_files".to_string(),
            r#"{"path": "."}"#.to_string(),
        ),
        ScriptedTurn::Text(vec![(
            std::time::Duration::from_millis(2000),
            "Second para\n\n".to_string(),
        )]),
        // Run 2 (the drained follow-up): a fresh single turn.
        ScriptedTurn::Text(vec![(
            std::time::Duration::from_millis(120),
            "Follow-up done\n\n".to_string(),
        )]),
    ];
    let (port, captured) = spawn_scripted_sse_server_turns_with_capture(turns, Default::default());
    let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

    let notice_sink = Arc::new(std::sync::Mutex::new(Vec::new()));

    let (key_tx, key_rx) = std::sync::mpsc::channel::<MidRunKey>();
    let mut commands = Commands::new(Config::fallback())
        .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
        .with_runtime_mode(RuntimeMode::Embedded)
        .with_permission_mode(takka_core::PermissionMode::Yolo)
        // the DEFAULT mode — explicit for clarity
        .with_queue_mode(QueueMode::FollowUp)
        .with_mid_run_notice_sink(notice_sink.clone())
        .with_mid_run_keys(key_rx);

    // Inject keys inside turn 2's provider window; the queue's notice
    // lands then, and the drain runs the line after the terminal event.
    let tap = commands.test_events.clone();
    let driver = std::thread::Builder::new()
        .name("followup-driver".into())
        .spawn(move || {
            wait_for_tap_event_count(&tap, "ModelRequest", 2);
            for ch in "later text more".chars() {
                key_tx.send(MidRunKey::Char(ch)).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            key_tx.send(MidRunKey::Enter).unwrap();
        })
        .expect("followup driver thread");

    let _outcome = tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("say hi"))
        .await
        .expect("Commands::run hung: no terminal event within 60s");
    driver.join().expect("followup driver panicked");
    let _ = std::fs::remove_file(&fixture);

    // The queue notice was visible once (hook notice tap).
    let notices = notice_sink.lock().unwrap().clone();
    assert_eq!(
        notices.len(),
        1,
        "queueing must report once via the hook notice: {notices:?}"
    );
    assert!(
        notices[0].contains("queued for the next run")
            && notices[0].contains("\"later text more\""),
        "the notice must name the queued line: {notices:?}"
    );
    // Clear the notice sink (no check: the setter returns nothing).

    // The queue drained: the follow-up run executed as run 2 — its
    // provider request carried the queued line as the task.
    let bodies = captured.lock().unwrap().clone();
    let last = bodies.last().expect("run 2 must post a request");
    assert!(
        last.contains("later text more"),
        "the follow-up run's provider request must carry the queued task: {last:?}"
    );
    assert!(
        commands.test_final_print.lock().unwrap().is_empty(),
        "run 2's answer fully previewed"
    );
}
