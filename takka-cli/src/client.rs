use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use takka_core::runtime::engine::RuntimeEngine;
use takka_core::runtime::protocol::{
    ConnectionCapabilities, InitOverrides, ProtocolEvent, ProtocolRequest, Request,
    PROTOCOL_VERSION,
};
use takka_core::runtime::transport::MemoryTransport;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc as tokio_mpsc;

#[cfg(unix)]
use tokio::net::UnixStream;

/// Single clearinghouse message for the unsupported-platform guard of
/// [`AgentConnection::connect_daemon`] (Windows: named pipes are a
/// documented TODO). Kept platform-unconditional so the guard branch
/// and its test compile — and run — on every CI host (same pattern as
/// `takka-runtime`'s `not_supported_error`).
#[cfg_attr(unix, allow(dead_code))]
pub(crate) const DAEMON_NOT_SUPPORTED_MESSAGE: &str =
    "daemon mode not supported on Windows yet — use --runtime embedded|spawn";

/// Actionable error when discovery finds no usable daemon and spawning
/// is not allowed (the `attach` path). Asserted verbatim by a unit test.
pub(crate) const NO_DAEMON_MESSAGE: &str =
    "no daemon running — start it with 'takka-runtime --daemon' or use --runtime embedded";

/// Upper bound for waiting until a freshly spawned daemon publishes its
/// discovery file (the broker writes it after bind, so its existence
/// means the listener is ready).
#[cfg(unix)]
const DAEMON_SPAWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Upper bound for the shutdown retry loop against a busy broker (the
/// previous connection's slot clears within milliseconds of its EOF;
/// the bound only covers pathological cases).
#[cfg(unix)]
const DAEMON_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Transport-agnostic IO surface shared by both connection modes, so the
/// handshake and the run loop exist in exactly one place (codex lesson:
/// duplicated handshake logic across clients is a bug waiting to happen).
pub trait ConnectionIo {
    fn raw_send(&mut self, request: Request) -> impl Future<Output = Result<String>> + Send;
    fn raw_next_event(&mut self) -> impl Future<Output = Result<ProtocolEvent>> + Send;
}

/// Capabilities yang dikirim CLI saat handshake: CLI memakai `/plan` /
/// `--plan`, jadi opt-in `experimental_api` itu jujur.
pub const CLI_CAPABILITIES: ConnectionCapabilities = ConnectionCapabilities {
    experimental_api: true,
    opt_out_notify: Vec::new(),
};

/// Discovery record the daemon broker publishes next to its socket
/// (`daemon.json`). Field names are pinned to the broker's writer
/// (`takka-runtime/src/daemon.rs::write_info_file`): snake_case keys,
/// no serde renaming — a silent mismatch here would break `RuntimeMode
/// ::Daemon` discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    /// Broker process id (liveness guard bookkeeping; the authoritative
    /// staleness signal is the socket connect result).
    pub pid: u32,
    /// Unix socket the broker listens on.
    pub socket_path: String,
    /// Shared secret required by the engine's Init gate.
    pub token: String,
    /// Runtime version that wrote the file (informational).
    pub version: String,
}

/// Parse a [`DaemonInfo`] from raw JSON (the exact schema the broker
/// writes). Exposed for tests pinning the wire format.
pub(crate) fn parse_daemon_info_str(json: &str) -> Result<DaemonInfo> {
    serde_json::from_str(json).context("invalid daemon info json")
}

/// Read + parse the discovery file at `path`. Fails on missing, unread
/// -able, or corrupt files — callers treat any error as "no usable info
/// file".
pub fn parse_daemon_info(path: &Path) -> Result<DaemonInfo> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read daemon info {}", path.display()))?;
    parse_daemon_info_str(&content)
        .with_context(|| format!("invalid daemon info in {}", path.display()))
}

/// Default discovery location — the same path (and HOME-less fallback)
/// the runtime broker derives in `takka-runtime/src/main.rs`, so
/// discovery and auto-spawn agree without configuration. Moved to
/// `~/.config/takka` during the rebrand, in lockstep with the broker.
pub fn daemon_info_path() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home)
            .join(".config")
            .join("takka")
            .join("daemon.json"),
        None => std::env::temp_dir()
            .join("takka-daemon")
            .join("daemon.json"),
    }
}

/// Identical handshake for every mode: send `Init`, wait `InitOk` (or an
/// `Error` explaining why initialization failed). Stdio/in-process
/// engines announce `Ready` before the first request, so those callers
/// (`daemon_token: None`) wait for it first; the daemon broker never
/// replays the engine's startup `Ready` (it is emitted before any client
/// attaches and the broker's drain loop discards detached events), so
/// daemon callers (`daemon_token: Some(..)`) go straight to `Init` with
/// the discovery file's shared secret.
pub(crate) async fn perform_handshake(
    io: &mut impl ConnectionIo,
    overrides: InitOverrides,
    caps: ConnectionCapabilities,
    daemon_token: Option<String>,
) -> Result<()> {
    if daemon_token.is_none() {
        let ready = io.raw_next_event().await?;
        if !matches!(ready.event, takka_core::Event::Ready { .. }) {
            return Err(anyhow!("runtime did not emit ready"));
        }
    }
    io.raw_send(Request::Init {
        overrides,
        capabilities: caps,
        daemon_token,
    })
    .await?;
    let initialized = io.raw_next_event().await?;
    match initialized.event {
        takka_core::Event::InitOk { .. } => Ok(()),
        takka_core::Event::Error { message } => Err(anyhow!(message)),
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
/// answer interactive gates through the connection, and watch the cancel
/// flag between events (story 1.4): one cheap flag load per loop pass;
/// when the flag flips, `Request::Cancel` is sent exactly once and the
/// engine's cancel handle unwinds the run at its next turn boundary
/// (`AgenticError::Cancelled` → the run's normal Err path — the cleanup
/// the caller already owns).
pub(crate) async fn run_turn<C, E, R>(
    io: &mut impl ConnectionIo,
    task: String,
    attachments: Vec<takka_core::Attachment>,
    mut on_chunk: C,
    mut on_event: E,
    mut responder: R,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<String>
where
    C: FnMut(&str),
    E: FnMut(takka_core::Event),
    R: FnMut(&ProtocolEvent) -> Option<Request>,
{
    let request_id = io.raw_send(Request::Run { task, attachments }).await?;
    let mut streamed = String::new();
    let mut cancel_sent = false;
    loop {
        // Cooperative cancel watch (story 1.4): checked between events,
        // forwarded to the engine exactly once. The orchestrator honors it
        // at the next turn boundary — deltas may continue briefly (an
        // in-flight LLM call is not aborted); that latency is the
        // documented contract, bounded by the boundary.
        if !cancel_sent && cancel.load(std::sync::atomic::Ordering::Relaxed) {
            io.raw_send(Request::Cancel).await?;
            cancel_sent = true;
        }
        let event = io.raw_next_event().await?;
        if event.request_id.as_deref() != Some(&request_id) {
            continue;
        }
        if let Some(response) = responder(&event) {
            io.raw_send(response).await?;
        }
        match event.event {
            takka_core::Event::AssistantDelta { content } => {
                streamed.push_str(&content);
                on_chunk(&content);
            }
            takka_core::Event::Done { result } => {
                return Ok(if result.is_empty() { streamed } else { result });
            }
            takka_core::Event::Error { message } => return Err(anyhow!(message)),
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
    pub async fn spawn(overrides: InitOverrides, caps: ConnectionCapabilities) -> Result<Self> {
        let binary = runtime_binary()?;
        Self::spawn_binary(&binary, overrides, caps).await
    }

    pub async fn spawn_binary(
        binary: &Path,
        overrides: InitOverrides,
        caps: ConnectionCapabilities,
    ) -> Result<Self> {
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
        perform_handshake(&mut client, overrides, caps, None).await?;
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
        attachments: Vec<takka_core::Attachment>,
        on_chunk: C,
        on_event: E,
        responder: R,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<String>
    where
        C: FnMut(&str),
        E: FnMut(takka_core::Event),
        R: FnMut(&ProtocolEvent) -> Option<Request>,
    {
        run_turn(
            self,
            task,
            attachments,
            on_chunk,
            on_event,
            responder,
            cancel,
        )
        .await
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
    T: takka_core::runtime::transport::Transport,
    F: FnOnce(
            std_mpsc::Receiver<ProtocolRequest>,
            std_mpsc::Sender<ProtocolEvent>,
        ) -> RuntimeEngine<T>
        + Send
        + 'static,
{
    std::thread::Builder::new()
        .name("takka-runtime-embedded".to_string())
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
                    takka_core::Event::Error {
                        message: "embedded runtime engine panicked".to_string(),
                    },
                ));
            }
        })
        .expect("spawn takka-runtime-embedded thread")
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
    pub async fn start(overrides: InitOverrides, caps: ConnectionCapabilities) -> Result<Self> {
        let (request_tx, request_rx) = std_mpsc::channel::<ProtocolRequest>();
        let (event_tx, event_rx) = std_mpsc::channel::<ProtocolEvent>();
        let (tokio_tx, events) = tokio_mpsc::unbounded_channel::<ProtocolEvent>();

        // Bridge thread: the MemoryTransport side is blocking
        // (std::sync::mpsc), so a dedicated native thread forwards events
        // into a tokio channel for the async client. Cheaper than
        // spawn_blocking-per-event for delta-heavy streams.
        std::thread::Builder::new()
            .name("takka-runtime-bridge".to_string())
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
            .expect("spawn takka-runtime-bridge thread");

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
        perform_handshake(&mut client, overrides, caps, None).await?;
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
    /// dispatch loop (see `takka-core/src/runtime/engine.rs`), so the
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

/// Async-native unix-socket connection to the daemon broker (Fase 3a).
/// Same line framing and protocol-version check as the spawned-mode
/// stdio client, but over the split halves of a `tokio::net::UnixStream`
/// (the broker's one-JSON-per-line framing, see its `write_frame` /
/// `pump_requests`).
#[cfg(unix)]
pub struct DaemonConnection {
    lines: Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: tokio::net::unix::OwnedWriteHalf,
    sequence: u64,
}

#[cfg(unix)]
impl DaemonConnection {
    /// Wrap an already-connected socket. Handshake is done by the caller
    /// (so [`AgentConnection::connect_daemon_at`] can reuse its
    /// liveness-probe stream instead of reconnecting).
    fn from_stream(stream: UnixStream) -> Self {
        let (read_half, write_half) = stream.into_split();
        Self {
            lines: BufReader::new(read_half).lines(),
            writer: write_half,
            sequence: 0,
        }
    }

    pub async fn send(&mut self, request: Request) -> Result<String> {
        self.sequence += 1;
        let id = format!("cli-{}", self.sequence);
        let envelope = ProtocolRequest::new(id.clone(), request);
        let mut bytes = serde_json::to_vec(&envelope)?;
        bytes.push(b'\n');
        self.writer.write_all(&bytes).await?;
        self.writer.flush().await?;
        Ok(id)
    }

    pub async fn next_event(&mut self) -> Result<ProtocolEvent> {
        let line = self
            .lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("daemon closed the connection"))?;
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

    /// Ask the broker's engine to shut down, then close our end. The
    /// engine breaks its dispatch loop without acking Shutdown (see
    /// `RuntimeEngine::run`'s `Request::Shutdown => break`), so this is
    /// deliberately fire-and-forget — waiting for a reply would hang
    /// forever. The broker notices EOF and its `serve` cleanup unlinks
    /// the socket + info files.
    pub async fn shutdown(&mut self) -> Result<()> {
        let _ = self.send(Request::Shutdown).await?;
        self.writer.shutdown().await?;
        Ok(())
    }
}

#[cfg(unix)]
impl ConnectionIo for DaemonConnection {
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
    /// Spawn the `takka-runtime` daemon subprocess (JSONL over stdio).
    Spawn,
    /// Attach to an external `takka-runtime --daemon` broker over its
    /// unix socket (discovery via `daemon.json`, auto-spawn on miss).
    /// Unix only — other hosts get [`DAEMON_NOT_SUPPORTED_MESSAGE`].
    Daemon,
}

/// One connection abstraction over both transports (codex's
/// `AppServerClient { Embedded, Remote }` pattern): embedded and spawned
/// are not two products — they are two transports behind one client.
pub enum AgentConnection {
    Spawned(Box<RuntimeClient>),
    InProcess(InProcessClient),
    /// Attached to the external daemon broker over its unix socket
    /// (unix only; Windows named pipes are a documented TODO —
    /// [`AgentConnection::connect_daemon`] guards that path).
    #[cfg(unix)]
    Daemon(DaemonConnection),
}

impl AgentConnection {
    pub async fn connect(
        mode: RuntimeMode,
        overrides: InitOverrides,
        caps: ConnectionCapabilities,
    ) -> Result<Self> {
        match mode {
            RuntimeMode::Embedded => Ok(Self::InProcess(
                InProcessClient::start(overrides, caps).await?,
            )),
            RuntimeMode::Spawn => Ok(Self::Spawned(Box::new(
                RuntimeClient::spawn(overrides, caps).await?,
            ))),
            // Auto-spawn is the default for the mode flag: `--runtime
            // daemon` on a machine without a live daemon should just work.
            RuntimeMode::Daemon => Self::connect_daemon(overrides, caps, true).await,
        }
    }

    /// Attach to the external daemon broker ([`RuntimeMode::Daemon`] and
    /// the `attach` command). `spawn_if_missing` auto-launches the
    /// runtime binary as a daemon when discovery finds no live one — the
    /// attach command passes `false` (an attach that starts its own
    /// daemon would be a run in disguise). Unix-only: on other hosts this
    /// returns [`DAEMON_NOT_SUPPORTED_MESSAGE`], the single actionable
    /// guard asserted verbatim by a platform-independent unit test.
    #[cfg(unix)]
    pub async fn connect_daemon(
        overrides: InitOverrides,
        caps: ConnectionCapabilities,
        spawn_if_missing: bool,
    ) -> Result<Self> {
        Self::connect_daemon_at(&daemon_info_path(), overrides, caps, spawn_if_missing).await
    }

    /// Non-unix stub for [`Self::connect_daemon`]: daemon mode is
    /// Unix-only; the actionable message is the contract.
    #[cfg(not(unix))]
    pub async fn connect_daemon(
        _overrides: InitOverrides,
        _caps: ConnectionCapabilities,
        _spawn_if_missing: bool,
    ) -> Result<Self> {
        Err(anyhow!(DAEMON_NOT_SUPPORTED_MESSAGE))
    }

    /// [`Self::connect_daemon`] against an explicit discovery file — the
    /// seam that keeps the daemon path hermetically testable (end-to-end
    /// smokes point it at a tempdir fixture) and lets `attach` honor a
    /// per-Commands override.
    #[cfg(unix)]
    pub(crate) async fn connect_daemon_at(
        info_path: &Path,
        overrides: InitOverrides,
        caps: ConnectionCapabilities,
        spawn_if_missing: bool,
    ) -> Result<Self> {
        // 1) Discovery: read the broker's info file (missing / corrupt
        //    / unreadable all collapse to "no usable info").
        let mut info = parse_daemon_info(info_path).ok();

        // 2) Nothing usable && auto-spawn: launch the runtime binary as
        //    a background daemon, then poll for the info file (the
        //    broker writes it after bind — existence means listening).
        if info.is_none() && spawn_if_missing {
            spawn_daemon_process(info_path)?;
            info = Some(wait_for_daemon_info(info_path).await?);
        }

        // 3) Still nothing → single actionable error (the attach path
        //    runs with spawn_if_missing=false and lands here directly).
        let mut info = info.ok_or_else(|| anyhow!(NO_DAEMON_MESSAGE))?;

        // 4) Liveness: the info file can outlive its daemon, so a failed
        //    socket connect is the stale signal (portable, race-free;
        //    stale → unlink and respawn once when allowed).
        let stream = match UnixStream::connect(&info.socket_path).await {
            Ok(stream) => stream,
            Err(_) if spawn_if_missing => {
                let _ = std::fs::remove_file(info_path); // stale info (best-effort)
                spawn_daemon_process(info_path)?;
                info = wait_for_daemon_info(info_path).await?;
                UnixStream::connect(&info.socket_path)
                    .await
                    .with_context(|| {
                        format!(
                            "daemon socket {} did not come up after respawn",
                            info.socket_path
                        )
                    })?
            }
            Err(_) => {
                let _ = std::fs::remove_file(info_path); // stale info (best-effort)
                return Err(anyhow!(NO_DAEMON_MESSAGE));
            }
        };

        // 5) Token handshake: Init carries the info file's shared
        //    secret; the engine rejects mismatches with `unauthorized`
        //    before touching any session state.
        let mut connection = DaemonConnection::from_stream(stream);
        perform_handshake(&mut connection, overrides, caps, Some(info.token)).await?;
        Ok(Self::Daemon(connection))
    }

    pub async fn send(&mut self, request: Request) -> Result<String> {
        match self {
            Self::Spawned(client) => client.send(request).await,
            Self::InProcess(client) => client.send(request).await,
            #[cfg(unix)]
            Self::Daemon(client) => client.send(request).await,
        }
    }

    pub async fn next_event(&mut self) -> Result<ProtocolEvent> {
        match self {
            Self::Spawned(client) => client.next_event().await,
            Self::InProcess(client) => client.next_event().await,
            #[cfg(unix)]
            Self::Daemon(client) => client.next_event().await,
        }
    }

    pub async fn send_and_wait(&mut self, request: Request) -> Result<ProtocolEvent> {
        send_and_wait_io(self, request).await
    }

    /// Fire-and-forget steer: kirim request, tidak menunggu reply
    /// (reply `SteerQueued` tiba di stream event biasa).
    pub async fn steer(&mut self, content: String) -> Result<String> {
        self.send(Request::Steer { content }).await
    }

    pub async fn run<C, E, R>(
        &mut self,
        task: String,
        attachments: Vec<takka_core::Attachment>,
        on_chunk: C,
        on_event: E,
        responder: R,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<String>
    where
        C: FnMut(&str),
        E: FnMut(takka_core::Event),
        R: FnMut(&ProtocolEvent) -> Option<Request>,
    {
        run_turn(
            self,
            task,
            attachments,
            on_chunk,
            on_event,
            responder,
            cancel,
        )
        .await
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        match self {
            Self::Spawned(client) => client.shutdown().await,
            Self::InProcess(client) => client.shutdown().await,
            #[cfg(unix)]
            Self::Daemon(client) => client.shutdown().await,
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
    let (value, warned) =
        takka_core::config::resolve_env("TAKKA_RUNTIME_BIN", "AGENTIC_RUNTIME_BIN");
    if warned {
        static LEGACY_RUNTIME_BIN_WARNED: std::sync::Once = std::sync::Once::new();
        LEGACY_RUNTIME_BIN_WARNED.call_once(|| {
            eprintln!(
                "{}",
                takka_core::config::legacy_env_warning("TAKKA_RUNTIME_BIN", "AGENTIC_RUNTIME_BIN")
            );
        });
    }
    if let Some(path) = value {
        return Ok(PathBuf::from(path));
    }
    let current = std::env::current_exe()?;
    let sibling = current.with_file_name(if cfg!(windows) {
        "takka-runtime.exe"
    } else {
        "takka-runtime"
    });
    // is_file(), not exists(): during development the ancestor scan walks up
    // to the workspace root where `takka-runtime/` is the *source crate
    // directory* — spawning it fails with a cryptic "Permission denied".
    if sibling.is_file() {
        return Ok(sibling);
    }
    for ancestor in current.ancestors() {
        let candidate = ancestor.join(if cfg!(windows) {
            "takka-runtime.exe"
        } else {
            "takka-runtime"
        });
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "takka-runtime not found next to {}; set TAKKA_RUNTIME_BIN",
        current.display()
    ))
}

/// Spawn the `takka-runtime` binary as a background daemon publishing
/// its discovery info to `info_path`. Both endpoints are passed
/// explicitly (re-derived from the discovery file's directory) because
/// this runs BEFORE the info file exists, so the socket path cannot be
/// read from it. The socket name carries OUR pid — unique among
/// concurrently running CLI processes; nothing downstream depends on the
/// guess anyway, since the broker records the real path in the info
/// file. The child is deliberately detached: the daemon must outlive
/// this process (tokio's orphan reaper collects it).
#[cfg(unix)]
fn spawn_daemon_process(info_path: &Path) -> Result<()> {
    let binary = runtime_binary()?;
    let socket_path = info_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("takka-daemon-{}.sock", std::process::id()));
    Command::new(&binary)
        .arg("--daemon")
        .arg("--socket-path")
        .arg(&socket_path)
        .arg("--info-path")
        .arg(info_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(false) // daemon outlives the CLI process
        .spawn()
        .with_context(|| format!("failed to spawn daemon from {}", binary.display()))?;
    Ok(())
}

/// Poll for the discovery file of a freshly spawned daemon. The broker
/// writes it after binding the listener, so a parsed info file means the
/// socket is ready to accept. Bounded by [`DAEMON_SPAWN_TIMEOUT`].
#[cfg(unix)]
async fn wait_for_daemon_info(info_path: &Path) -> Result<DaemonInfo> {
    let deadline = tokio::time::Instant::now() + DAEMON_SPAWN_TIMEOUT;
    loop {
        if let Ok(info) = parse_daemon_info(info_path) {
            return Ok(info);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "spawned daemon did not publish {} within 10s",
                info_path.display()
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Ask the daemon broker recorded in `info_path` to shut down. No
/// session handshake: `Request::Shutdown` needs no init (the engine
/// breaks its dispatch loop on receipt — see `RuntimeEngine::run` — and
/// the broker's cleanup unlinks socket + info files), so stopping a
/// daemon never requires matching its config.
///
/// The request is delivery-confirmed, not fire-and-forget: a busy broker
/// answers an attach with a single `"daemon in use"` error frame and
/// closes WITHOUT reading anything (no reader pump is spawned for a
/// refused connection), so a blind write would silently lose the
/// request. We detect that frame and retry until the previous
/// connection's slot clears (bounded by [`DAEMON_SHUTDOWN_TIMEOUT`]).
#[cfg(unix)]
pub(crate) async fn shutdown_daemon_at(info_path: &Path) -> Result<()> {
    let info = match parse_daemon_info(info_path) {
        Ok(info) => info,
        Err(_) => {
            return Err(anyhow!(
                "{} (no readable discovery file at {})",
                NO_DAEMON_MESSAGE,
                info_path.display()
            ))
        }
    };
    let deadline = tokio::time::Instant::now() + DAEMON_SHUTDOWN_TIMEOUT;
    // Emitted both when a refusal frame arrives and when the race below
    // turns into a write error. Kept identical so the terminal message is
    // stable regardless of which path the broker/OS takes.
    let busy = || anyhow!("daemon busy: another client is attached; detach it and retry shutdown");
    loop {
        let stream = match UnixStream::connect(&info.socket_path).await {
            Ok(stream) => stream,
            Err(_) => {
                return Err(anyhow!(
                    "{} (stale discovery file at {} — its daemon is gone)",
                    NO_DAEMON_MESSAGE,
                    info_path.display()
                ))
            }
        };
        let mut connection = DaemonConnection::from_stream(stream);
        // The refusing broker writes its `daemon in use` frame and closes the
        // socket without spawning a reader. A fast writer can race that close
        // and observe EPIPE instead of the frame; the request was not
        // delivered either way, so retry until the deadline like a refusal.
        if connection.send(Request::Shutdown).await.is_err() {
            if tokio::time::Instant::now() >= deadline {
                return Err(busy());
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            continue;
        }

        /// Only the refusal case loses the request: the refusing broker
        /// never reads it. Any other outcome means a live reader holds
        /// our (buffered) frame or the daemon is already gone.
        const REFUSAL_CHECK: std::time::Duration = std::time::Duration::from_millis(500);
        match tokio::time::timeout(REFUSAL_CHECK, connection.next_event()).await {
            // Refused: slot still held by another client — retry.
            Ok(Ok(event)) => match event.event {
                takka_core::Event::Error { message } if message.trim() == "daemon in use" => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(busy());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                takka_core::Event::Error { message } => {
                    return Err(anyhow!("daemon refused shutdown: {message}"));
                }
                // Any other frame proves a live reader accepted us; the
                // buffered shutdown request will be consumed.
                _ => return Ok(()),
            },
            // EOF/closed without a frame (daemon exited or consumed the
            // buffered request), or no frame within the check window —
            // in both cases the request sits in the socket buffer ahead
            // of our FIN and a live reader will read it.
            Ok(Err(_)) | Err(_) => return Ok(()),
        }
    }
}

#[cfg(test)]
mod in_process_tests {
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
}

#[cfg(test)]
pub(crate) mod daemon_info_tests {
    use super::*;

    /// Serializes env mutation between the env-compat tests in this
    /// module and the daemon-smoke fixtures in `commands.rs` (which
    /// resolve `runtime_binary()` and must not observe a transient
    /// env value).
    pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Wire-format pin for the discovery file the daemon broker writes
    /// (`takka-runtime/src/daemon.rs::write_info_file`): snake_case
    /// keys, no renaming — a mismatch silently breaks `RuntimeMode::
    /// Daemon` discovery.
    #[test]
    fn daemon_info_parses_fields() {
        let json =
            r#"{"pid":4242,"socket_path":"/tmp/x.sock","token":"tok","version":"0.0.0-test"}"#;
        let info = parse_daemon_info_str(json).unwrap();
        assert_eq!(info.pid, 4242);
        assert_eq!(info.socket_path, "/tmp/x.sock");
        assert_eq!(info.token, "tok");
        assert_eq!(info.version, "0.0.0-test");
    }

    #[test]
    fn daemon_info_rejects_corrupt_or_incomplete_json() {
        assert!(parse_daemon_info_str("not json at all").is_err());
        // Missing the token (the shared secret) → unusable for attach.
        let no_token = r#"{"pid":1,"socket_path":"/tmp/x.sock","version":"0"}"#;
        assert!(parse_daemon_info_str(no_token).is_err());
        // pid must be a number.
        let bad_pid = r#"{"pid":"4242","socket_path":"/tmp/x.sock","token":"t","version":"0"}"#;
        assert!(parse_daemon_info_str(bad_pid).is_err());
    }

    /// Default discovery location must agree with the runtime broker's
    /// own default (`daemon_config_dir()` in
    /// `takka-runtime/src/main.rs`), including the HOME-less fallback.
    /// Both sides moved to `~/.config/takka` in lockstep during the
    /// rebrand — this pin keeps them honest.
    #[test]
    fn daemon_info_path_matches_broker_default() {
        match std::env::var_os("HOME") {
            Some(home) => {
                assert_eq!(
                    daemon_info_path(),
                    PathBuf::from(home)
                        .join(".config")
                        .join("takka")
                        .join("daemon.json")
                );
            }
            None => {
                assert_eq!(
                    daemon_info_path(),
                    std::env::temp_dir()
                        .join("takka-daemon")
                        .join("daemon.json")
                );
            }
        }
    }

    /// Env-compat shim: `TAKKA_RUNTIME_BIN` is the primary name and
    /// wins when both are set (the legacy name must never shadow it).
    #[test]
    fn runtime_binary_env_prefers_takka_name() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("TAKKA_RUNTIME_BIN", "/tmp/takka-runtime-new");
        std::env::set_var("AGENTIC_RUNTIME_BIN", "/tmp/takka-runtime-legacy");
        assert_eq!(
            runtime_binary().unwrap(),
            PathBuf::from("/tmp/takka-runtime-new")
        );
        std::env::remove_var("TAKKA_RUNTIME_BIN");
        std::env::remove_var("AGENTIC_RUNTIME_BIN");
    }

    /// The pre-rebrand `AGENTIC_RUNTIME_BIN` name still resolves so
    /// existing setups keep working until they switch (with a stderr
    /// warning emitted by `runtime_binary`).
    #[test]
    fn runtime_binary_env_falls_back_to_agentic_name() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TAKKA_RUNTIME_BIN");
        std::env::set_var("AGENTIC_RUNTIME_BIN", "/tmp/takka-runtime-legacy");
        assert_eq!(
            runtime_binary().unwrap(),
            PathBuf::from("/tmp/takka-runtime-legacy")
        );
        std::env::remove_var("AGENTIC_RUNTIME_BIN");
    }

    /// Windows-level guard contract, compiled on every host (same
    /// pattern as `takka-runtime`'s `not_supported_error` test): the
    /// message `connect` produces on a non-unix host is exactly one
    /// actionable line pointing at the modes that DO work.
    #[test]
    fn daemon_mode_windows_guard_message_is_actionable() {
        assert_eq!(
            DAEMON_NOT_SUPPORTED_MESSAGE,
            "daemon mode not supported on Windows yet — use --runtime embedded|spawn"
        );
        assert!(!DAEMON_NOT_SUPPORTED_MESSAGE.contains('\n'));
    }

    /// The no-daemon attach error must tell the user how to get a
    /// daemon (or pick a mode that does not need one).
    #[test]
    fn no_daemon_message_is_actionable() {
        assert_eq!(
            NO_DAEMON_MESSAGE,
            "no daemon running — start it with 'takka-runtime --daemon' or use --runtime embedded"
        );
        assert!(!NO_DAEMON_MESSAGE.contains('\n'));
    }
}
