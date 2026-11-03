use anyhow::{anyhow, Context, Result};
use core_agentic::runtime::engine::RuntimeEngine;
use core_agentic::runtime::protocol::{
    InitOverrides, ProtocolEvent, ProtocolRequest, Request, PROTOCOL_VERSION,
};
use core_agentic::runtime::transport::MemoryTransport;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc as tokio_mpsc;

/// Transport-agnostic IO surface shared by both connection modes, so the
/// handshake and the run loop exist in exactly one place (codex lesson:
/// duplicated handshake logic across clients is a bug waiting to happen).
pub trait ConnectionIo {
    fn raw_send(&mut self, request: Request) -> impl Future<Output = Result<String>> + Send;
    fn raw_next_event(&mut self) -> impl Future<Output = Result<ProtocolEvent>> + Send;
}

/// Identical handshake for both modes: wait `Ready`, send `Init`, wait
/// `InitOk` (or an `Error` explaining why initialization failed).
pub(crate) async fn perform_handshake(
    io: &mut impl ConnectionIo,
    overrides: InitOverrides,
) -> Result<()> {
    let ready = io.raw_next_event().await?;
    if !matches!(ready.event, core_agentic::Event::Ready { .. }) {
        return Err(anyhow!("runtime did not emit ready"));
    }
    io.raw_send(Request::Init { overrides }).await?;
    let initialized = io.raw_next_event().await?;
    match initialized.event {
        core_agentic::Event::InitOk { .. } => Ok(()),
        core_agentic::Event::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected initialization event: {other:?}")),
    }
}

pub(crate) async fn send_and_wait_io(
    io: &mut impl ConnectionIo,
    request: Request,
) -> Result<ProtocolEvent> {
    let id = io.raw_send(request).await?;
    loop {
        let event = io.raw_next_event().await?;
        if event.request_id.as_deref() == Some(&id) {
            return Ok(event);
        }
    }
}

/// Run loop shared by both modes: stream deltas, surface lifecycle events,
/// and answer interactive gates through the connection.
pub(crate) async fn run_turn<C, E, R>(
    io: &mut impl ConnectionIo,
    task: String,
    attachments: Vec<core_agentic::Attachment>,
    mut on_chunk: C,
    mut on_event: E,
    mut responder: R,
) -> Result<String>
where
    C: FnMut(&str),
    E: FnMut(core_agentic::Event),
    R: FnMut(&ProtocolEvent) -> Option<Request>,
{
    let request_id = io.raw_send(Request::Run { task, attachments }).await?;
    let mut streamed = String::new();
    loop {
        let event = io.raw_next_event().await?;
        if event.request_id.as_deref() != Some(&request_id) {
            continue;
        }
        if let Some(response) = responder(&event) {
            io.raw_send(response).await?;
        }
        match event.event {
            core_agentic::Event::AssistantDelta { content } => {
                streamed.push_str(&content);
                on_chunk(&content);
            }
            core_agentic::Event::Done { result } => {
                return Ok(if result.is_empty() { streamed } else { result });
            }
            core_agentic::Event::Error { message } => return Err(anyhow!(message)),
            event => on_event(event),
        }
    }
}

pub struct RuntimeClient {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    sequence: u64,
}

impl RuntimeClient {
    pub async fn spawn(overrides: InitOverrides) -> Result<Self> {
        let binary = runtime_binary()?;
        Self::spawn_binary(&binary, overrides).await
    }

    pub async fn spawn_binary(binary: &Path, overrides: InitOverrides) -> Result<Self> {
        let mut child = Command::new(binary)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to spawn {}", binary.display()))?;
        let stdin = child.stdin.take().context("runtime stdin unavailable")?;
        let stdout = child.stdout.take().context("runtime stdout unavailable")?;
        let mut client = Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            sequence: 0,
        };
        perform_handshake(&mut client, overrides).await?;
        Ok(client)
    }

    pub async fn send(&mut self, request: Request) -> Result<String> {
        self.sequence += 1;
        let id = format!("cli-{}", self.sequence);
        let envelope = ProtocolRequest::new(id.clone(), request);
        let mut bytes = serde_json::to_vec(&envelope)?;
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await?;
        Ok(id)
    }

    pub async fn next_event(&mut self) -> Result<ProtocolEvent> {
        let line = self
            .lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("runtime stdout closed"))?;
        let event: ProtocolEvent = serde_json::from_str(&line)
            .with_context(|| format!("invalid runtime event: {line}"))?;
        if event.v != PROTOCOL_VERSION {
            return Err(anyhow!("unsupported runtime protocol version: {}", event.v));
        }
        Ok(event)
    }

    pub async fn send_and_wait(&mut self, request: Request) -> Result<ProtocolEvent> {
        send_and_wait_io(self, request).await
    }

    pub async fn run<C, E, R>(
        &mut self,
        task: String,
        attachments: Vec<core_agentic::Attachment>,
        on_chunk: C,
        on_event: E,
        responder: R,
    ) -> Result<String>
    where
        C: FnMut(&str),
        E: FnMut(core_agentic::Event),
        R: FnMut(&ProtocolEvent) -> Option<Request>,
    {
        run_turn(self, task, attachments, on_chunk, on_event, responder).await
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        let _ = self.send(Request::Shutdown).await?;
        self.stdin.shutdown().await?;
        let _ = self.child.wait().await?;
        Ok(())
    }
}

impl ConnectionIo for RuntimeClient {
    fn raw_send(&mut self, request: Request) -> impl Future<Output = Result<String>> + Send {
        self.send(request)
    }

    fn raw_next_event(&mut self) -> impl Future<Output = Result<ProtocolEvent>> + Send {
        self.next_event()
    }
}

/// Spawn the engine thread for an embedded connection. Generic over the
/// engine's transport so tests can inject a misbehaving (panicking)
/// transport; production uses [`MemoryTransport`].
pub(crate) fn spawn_engine_thread<T, F>(
    make_engine: F,
    request_rx: std_mpsc::Receiver<ProtocolRequest>,
    event_tx: std_mpsc::Sender<ProtocolEvent>,
    panic_tx: std_mpsc::Sender<ProtocolEvent>,
) -> std::thread::JoinHandle<()>
where
    T: core_agentic::runtime::transport::Transport,
    F: FnOnce(std_mpsc::Receiver<ProtocolRequest>, std_mpsc::Sender<ProtocolEvent>) -> RuntimeEngine<T>
        + Send
        + 'static,
{
    std::thread::Builder::new()
        .name("agentic-runtime-embedded".to_string())
        .spawn(move || {
            let mut engine = make_engine(request_rx, event_tx);
            // AssertUnwindSafe: the engine owns channels/Arcs; a panic
            // unwinds through them with no observable invariants to guard.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.run()));
            if result.is_err() {
                // The engine may have died mid-stream; deliver a terminal
                // error event so the frontend gets the same "runtime died"
                // UX as the spawned mode.
                let _ = panic_tx.send(ProtocolEvent::new(
                    None,
                    core_agentic::Event::Error {
                        message: "embedded runtime engine panicked".to_string(),
                    },
                ));
            }
        })
        .expect("spawn agentic-runtime-embedded thread")
}

/// In-process connection: the engine runs as a native thread inside this
/// process, talking over in-memory channels. No serialization, no runtime
/// binary discovery. Same protocol, same handshake, same event order as
/// the spawned daemon — embedded skips serialization, not the protocol.
pub struct InProcessClient {
    request_tx: std_mpsc::Sender<ProtocolRequest>,
    events: tokio_mpsc::UnboundedReceiver<ProtocolEvent>,
    engine_thread: Option<std::thread::JoinHandle<()>>,
    sequence: u64,
}

impl InProcessClient {
    pub async fn start(overrides: InitOverrides) -> Result<Self> {
        let (request_tx, request_rx) = std_mpsc::channel::<ProtocolRequest>();
        let (event_tx, event_rx) = std_mpsc::channel::<ProtocolEvent>();
        let (tokio_tx, events) = tokio_mpsc::unbounded_channel::<ProtocolEvent>();

        // Bridge thread: the MemoryTransport side is blocking
        // (std::sync::mpsc), so a dedicated native thread forwards events
        // into a tokio channel for the async client. Cheaper than
        // spawn_blocking-per-event for delta-heavy streams.
        std::thread::Builder::new()
            .name("agentic-runtime-bridge".to_string())
            .spawn({
                let event_rx = event_rx;
                move || {
                    while let Ok(event) = event_rx.recv() {
                        if tokio_tx.send(event).is_err() {
                            break;
                        }
                    }
                }
            })
            .expect("spawn agentic-runtime-bridge thread");

        let engine_thread = spawn_engine_thread(
            |request_rx, event_tx| RuntimeEngine::new(MemoryTransport::new(request_rx, event_tx)),
            request_rx,
            event_tx.clone(),
            event_tx,
        );

        let mut client = Self {
            request_tx,
            events,
            engine_thread: Some(engine_thread),
            sequence: 0,
        };
        perform_handshake(&mut client, overrides).await?;
        Ok(client)
    }

    pub async fn send(&mut self, request: Request) -> Result<String> {
        self.sequence += 1;
        let id = format!("cli-{}", self.sequence);
        self.request_tx
            .send(ProtocolRequest::new(id.clone(), request))
            .map_err(|_| anyhow!("embedded runtime engine stopped"))?;
        Ok(id)
    }

    pub async fn next_event(&mut self) -> Result<ProtocolEvent> {
        // Channel closed = every sender dropped (engine thread + bridge
        // thread both gone) without a terminal event — synthesize one so
        // callers get the same contract as the spawned mode.
        self.events
            .recv()
            .await
            .ok_or_else(|| anyhow!("embedded runtime engine stopped"))
    }

    pub async fn send_and_wait(&mut self, request: Request) -> Result<ProtocolEvent> {
        send_and_wait_io(self, request).await
    }

    /// Send `Shutdown` and join the engine thread (via `spawn_blocking`
    /// so the tokio runtime never blocks).
    ///
    /// `Request::Shutdown` makes `RuntimeEngine::run` break out of its
    /// dispatch loop (see `core-agentic/src/runtime/engine.rs`), so the
    /// join below never depends on drop timing of `request_tx`. The bridge
    /// thread ends on its own once the engine's event senders drop with it.
    pub async fn shutdown(&mut self) -> Result<()> {
        let _ = self.send(Request::Shutdown).await;
        if let Some(handle) = self.engine_thread.take() {
            tokio::task::spawn_blocking(move || {
                let _ = handle.join();
            })
            .await
            .map_err(|e| anyhow!("join engine thread failed: {e}"))?;
        }
        Ok(())
    }

    /// Test/teardown helper: has the engine thread finished?
    pub fn engine_finished(&self) -> bool {
        self.engine_thread
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(true)
    }
}

impl ConnectionIo for InProcessClient {
    fn raw_send(&mut self, request: Request) -> impl Future<Output = Result<String>> + Send {
        self.send(request)
    }

    fn raw_next_event(&mut self) -> impl Future<Output = Result<ProtocolEvent>> + Send {
        self.next_event()
    }
}

/// Which connection mode the CLI uses to reach the agent engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeMode {
    /// Engine runs as a thread inside this process (default; no runtime
    /// binary needed).
    #[default]
    Embedded,
    /// Spawn the `agentic-runtime` daemon subprocess (JSONL over stdio).
    Spawn,
}

/// One connection abstraction over both transports (codex's
/// `AppServerClient { Embedded, Remote }` pattern): embedded and spawned
/// are not two products — they are two transports behind one client.
pub enum AgentConnection {
    Spawned(RuntimeClient),
    InProcess(InProcessClient),
}

impl AgentConnection {
    pub async fn connect(mode: RuntimeMode, overrides: InitOverrides) -> Result<Self> {
        match mode {
            RuntimeMode::Embedded => {
                Ok(Self::InProcess(InProcessClient::start(overrides).await?))
            }
            RuntimeMode::Spawn => Ok(Self::Spawned(RuntimeClient::spawn(overrides).await?)),
        }
    }

    pub async fn send(&mut self, request: Request) -> Result<String> {
        match self {
            Self::Spawned(client) => client.send(request).await,
            Self::InProcess(client) => client.send(request).await,
        }
    }

    pub async fn next_event(&mut self) -> Result<ProtocolEvent> {
        match self {
            Self::Spawned(client) => client.next_event().await,
            Self::InProcess(client) => client.next_event().await,
        }
    }

    pub async fn send_and_wait(&mut self, request: Request) -> Result<ProtocolEvent> {
        send_and_wait_io(self, request).await
    }

    pub async fn run<C, E, R>(
        &mut self,
        task: String,
        attachments: Vec<core_agentic::Attachment>,
        on_chunk: C,
        on_event: E,
        responder: R,
    ) -> Result<String>
    where
        C: FnMut(&str),
        E: FnMut(core_agentic::Event),
        R: FnMut(&ProtocolEvent) -> Option<Request>,
    {
        run_turn(self, task, attachments, on_chunk, on_event, responder).await
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        match self {
            Self::Spawned(client) => client.shutdown().await,
            Self::InProcess(client) => client.shutdown().await,
        }
    }
}

impl ConnectionIo for AgentConnection {
    fn raw_send(&mut self, request: Request) -> impl Future<Output = Result<String>> + Send {
        self.send(request)
    }

    fn raw_next_event(&mut self) -> impl Future<Output = Result<ProtocolEvent>> + Send {
        self.next_event()
    }
}

pub fn runtime_binary() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("AGENTIC_RUNTIME_BIN") {
        return Ok(PathBuf::from(path));
    }
    let current = std::env::current_exe()?;
    let sibling = current.with_file_name(if cfg!(windows) {
        "agentic-runtime.exe"
    } else {
        "agentic-runtime"
    });
    // is_file(), not exists(): during development the ancestor scan walks up
    // to the workspace root where `agentic-runtime/` is the *source crate
    // directory* — spawning it fails with a cryptic "Permission denied".
    if sibling.is_file() {
        return Ok(sibling);
    }
    for ancestor in current.ancestors() {
        let candidate = ancestor.join(if cfg!(windows) {
            "agentic-runtime.exe"
        } else {
            "agentic-runtime"
        });
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "agentic-runtime not found next to {}; set AGENTIC_RUNTIME_BIN",
        current.display()
    ))
}

#[cfg(test)]
mod in_process_tests {
    use super::*;
    use core_agentic::runtime::protocol::InitOverrides;

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
            permission_mode: Some(core_agentic::PermissionMode::Yolo),
            model: None,
            system_prompt: None,
        }
    }

    #[tokio::test]
    async fn in_process_handshake_and_list_tools_round_trip() {
        let mut client = InProcessClient::start(hermetic_overrides())
            .await
            .expect("embedded handshake");
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            client.send_and_wait(Request::ListTools),
        )
        .await
        .expect("ListTools timed out")
        .expect("ListTools failed");
        assert!(matches!(event.event, core_agentic::Event::ToolList { .. }));
        client.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn agent_connection_connects_in_both_modes() {
        // Embedded (default): tanpa binary agentic-runtime sama sekali.
        let mut embedded = AgentConnection::connect(
            RuntimeMode::Embedded,
            hermetic_overrides(),
        )
        .await
        .expect("embedded connect");
        let event = embedded.send_and_wait(Request::ListTools).await.expect("list tools");
        assert!(matches!(event.event, core_agentic::Event::ToolList { .. }));
        embedded.shutdown().await.expect("embedded shutdown");

        // Spawn: jalur lama lewat binary daemon (runtime_binary() menemukan
        // target/debug/agentic-runtime hasil cargo test workspace).
        let mut spawned = AgentConnection::connect(
            RuntimeMode::Spawn,
            hermetic_overrides(),
        )
        .await
        .expect("spawned connect");
        let event = spawned.send_and_wait(Request::ListTools).await.expect("list tools");
        assert!(matches!(event.event, core_agentic::Event::ToolList { .. }));
        spawned.shutdown().await.expect("spawned shutdown");
    }

    #[tokio::test]
    async fn shutdown_joins_engine_thread() {
        let mut client = InProcessClient::start(hermetic_overrides())
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

    impl core_agentic::runtime::transport::Transport for PanickyTransport {
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
            .name("agentic-runtime-bridge".to_string())
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
            matches!(first.event, core_agentic::Event::Ready { .. }),
            "expected Ready first, got {first:?}"
        );

        // Event kedua: Error hasil catch_unwind (bukan crash test process).
        let second = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .expect("no terminal event after panic")
            .expect("event channel closed before terminal event");
        match second.event {
            core_agentic::Event::Error { message } => {
                assert!(
                    message.contains("panicked"),
                    "expected panic message, got: {message}"
                );
            }
            other => panic!("expected Error event after panic, got {other:?}"),
        }
        drop(request_tx);
    }
}
