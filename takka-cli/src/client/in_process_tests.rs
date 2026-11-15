use super::*;
use takka_core::runtime::protocol::InitOverrides;

/// Fixture config hermetic yang sama dengan e2e smoke tests di
/// `commands.rs`: endpoint discard port yang selalu gagal koneksi,
/// jadi tidak pernah menyentuh provider nyata / config developer.
fn hermetic_overrides() -> InitOverrides {
    let path = std::env::temp_dir().join(format!(
        "agentic-inproc-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(
        &path,
        r#"{
  "providers": [
{
  "name": "hermetic-test",
  "type": "openai-compatible",
  "api_base": "http://127.0.0.1:9",
  "api_key": "unused",
  "models": [
    { "model": "hermetic-model", "temperature": 0.0, "max_tokens": 64 }
  ]
}
  ]
}"#,
    )
    .expect("write hermetic in-process config");
    InitOverrides {
        config_path: Some(path.to_string_lossy().into_owned()),
        permission_mode: Some(takka_core::PermissionMode::Yolo),
        model: None,
        session_id: None,
        system_prompt: None,
    }
}

#[tokio::test]
async fn in_process_handshake_and_list_tools_round_trip() {
    let mut client = InProcessClient::start(hermetic_overrides(), CLI_CAPABILITIES)
        .await
        .expect("embedded handshake");
    let event = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.send_and_wait(Request::ListTools),
    )
    .await
    .expect("ListTools timed out")
    .expect("ListTools failed");
    assert!(matches!(event.event, takka_core::Event::ToolList { .. }));
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn agent_connection_connects_in_both_modes() {
    // Embedded (default): tanpa binary takka-runtime sama sekali.
    let mut embedded = AgentConnection::connect(
        RuntimeMode::Embedded,
        hermetic_overrides(),
        CLI_CAPABILITIES,
    )
    .await
    .expect("embedded connect");
    let event = embedded
        .send_and_wait(Request::ListTools)
        .await
        .expect("list tools");
    assert!(matches!(event.event, takka_core::Event::ToolList { .. }));
    embedded.shutdown().await.expect("embedded shutdown");

    // Spawn: jalur lama lewat binary daemon (runtime_binary() menemukan
    // target/debug/takka-runtime hasil cargo test workspace).
    let mut spawned =
        AgentConnection::connect(RuntimeMode::Spawn, hermetic_overrides(), CLI_CAPABILITIES)
            .await
            .expect("spawned connect");
    let event = spawned
        .send_and_wait(Request::ListTools)
        .await
        .expect("list tools");
    assert!(matches!(event.event, takka_core::Event::ToolList { .. }));
    spawned.shutdown().await.expect("spawned shutdown");
}

/// Kontrak serialization caps CLI: Init frame yang dikirim handshake
/// harus membawa `experimentalApi: true` (opt-in eksplisit, bukan
/// default engine) — persis seperti yang diperiksa gating Task 2.
#[test]
fn cli_capabilities_serialize_experimental_opt_in() {
    let frame = serde_json::to_value(takka_core::runtime::protocol::ProtocolRequest::new(
        "i",
        Request::Init {
            overrides: Default::default(),
            capabilities: CLI_CAPABILITIES,
            daemon_token: None,
        },
    ))
    .unwrap();
    assert_eq!(frame["capabilities"]["experimentalApi"], true);
}

/// Wire-format pin (closes Task 2 defer #3): the Steer request must
/// serialize with the snake_case tag `steer` and carry `content`, and
/// must round-trip back to the same variant. This frame crosses the
/// JSONL protocol between CLI and daemon — a silent rename would break
/// spawned-mode steering.
#[test]
fn steer_request_serializes_snake_case() {
    let frame = serde_json::to_value(ProtocolRequest::new(
        "s1",
        Request::Steer {
            content: "hi".into(),
        },
    ))
    .unwrap();
    assert_eq!(frame["type"], "steer");
    assert_eq!(frame["content"], "hi");

    // Round-trip: daemon-side deserialization must see the same request.
    let parsed: ProtocolRequest = serde_json::from_value(frame).unwrap();
    match parsed.request {
        Request::Steer { content } => assert_eq!(content, "hi"),
        other => panic!("expected Request::Steer, got {other:?}"),
    }
}

/// Hermetic `AgentConnection::steer` (fire-and-forget) against the
/// embedded engine's strict gating (Task 2): with no active run the
/// engine answers on the normal event stream with
/// `Error { "no active run" }` — the reply the caller deliberately
/// does not wait for.
#[tokio::test]
async fn steer_without_active_run_gets_error_on_event_stream() {
    let mut conn = AgentConnection::connect(
        RuntimeMode::Embedded,
        hermetic_overrides(),
        CLI_CAPABILITIES,
    )
    .await
    .expect("embedded connect");

    // Handshake consumed `cli-1` (Init); this steer is `cli-2`.
    let id = conn
        .steer("hello".to_string())
        .await
        .expect("steer send failed");
    assert_eq!(id, "cli-2");

    // The reply arrives as a regular stream event, tagged with the
    // steer request id.
    let event = tokio::time::timeout(std::time::Duration::from_secs(10), conn.next_event())
        .await
        .expect("timed out waiting for steer reply")
        .expect("event stream closed");
    assert_eq!(event.request_id.as_deref(), Some("cli-2"));
    match event.event {
        takka_core::Event::Error { message } => assert_eq!(message, "no active run"),
        other => panic!("expected Error(no active run), got {other:?}"),
    }
    conn.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn shutdown_joins_engine_thread() {
    let mut client = InProcessClient::start(hermetic_overrides(), CLI_CAPABILITIES)
        .await
        .expect("embedded handshake");
    tokio::time::timeout(std::time::Duration::from_secs(10), client.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown failed");
    // Engine + bridge thread harus sudah berakhir setelah join.
    assert!(client.engine_finished());
}

/// Transport test yang panic pada pembacaan request pertama — simulasi
/// bug engine. `write_event` tetap mengalirkan event (mirip
/// `MemoryTransport`) agar `Ready` benar-benar terkirim sebelum panic.
/// FIX(brief): brief asli memakai unit struct yang membuang `Ready`,
/// bertentangan dengan kontrak "Ready lalu Error" di komentarnya,
/// sehingga assertion event pertama selalu gagal.
struct PanickyTransport {
    event_tx: std_mpsc::Sender<ProtocolEvent>,
}

impl takka_core::runtime::transport::Transport for PanickyTransport {
    fn read_request(&self) -> std::io::Result<Option<ProtocolRequest>> {
        panic!("boom: injected engine bug");
    }
    fn write_event(&self, event: &ProtocolEvent) -> std::io::Result<()> {
        self.event_tx.send(event.clone()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "event receiver closed")
        })
    }
}

#[tokio::test]
async fn engine_panic_becomes_error_event() {
    let (request_tx, request_rx) = std_mpsc::channel::<ProtocolRequest>();
    let (event_tx, event_rx) = std_mpsc::channel::<ProtocolEvent>();
    let (tokio_tx, mut events) = tokio_mpsc::unbounded_channel::<ProtocolEvent>();

    std::thread::Builder::new()
        .name("takka-runtime-bridge".to_string())
        .spawn(move || {
            while let Ok(event) = event_rx.recv() {
                if tokio_tx.send(event).is_err() {
                    break;
                }
            }
        })
        .expect("bridge thread");

    // FIX(brief): pakai `event_tx` dari factory agar `Ready` sampai ke
    // bridge/channel; brief asli mengabaikannya sehingga `Ready` hilang.
    let _engine = spawn_engine_thread(
        |_request_rx, event_tx| RuntimeEngine::new(PanickyTransport { event_tx }),
        request_rx,
        event_tx.clone(),
        event_tx,
    );

    // Event pertama: Ready (terkirim sebelum panic).
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
        .await
        .expect("no event after panic window")
        .expect("event channel closed before terminal event");
    assert!(
        matches!(first.event, takka_core::Event::Ready { .. }),
        "expected Ready first, got {first:?}"
    );

    // Event kedua: Error hasil catch_unwind (bukan crash test process).
    let second = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
        .await
        .expect("no terminal event after panic")
        .expect("event channel closed before terminal event");
    match second.event {
        takka_core::Event::Error { message } => {
            assert!(
                message.contains("panicked"),
                "expected panic message, got: {message}"
            );
        }
        other => panic!("expected Error event after panic, got {other:?}"),
    }
    drop(request_tx);
}
