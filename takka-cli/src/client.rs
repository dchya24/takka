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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc as tokio_mpsc;

/// Single clearinghouse message for the platforms where daemon mode has
/// no transport. Windows named pipes are implemented (H-07), so this is
/// now reachable only on hosts that are neither unix nor windows; kept
/// platform-unconditional so its contract test compiles — and runs — on
/// every CI host (same pattern as `takka-runtime`'s
/// `not_supported_error`).
#[cfg_attr(any(unix, windows), allow(dead_code))]
pub(crate) const DAEMON_NOT_SUPPORTED_MESSAGE: &str =
    "daemon mode not supported on this platform — use --runtime embedded|spawn";

/// Actionable error when discovery finds no usable daemon and spawning
/// is not allowed (the `attach` path). Asserted verbatim by a unit test.
pub(crate) const NO_DAEMON_MESSAGE: &str =
    "no daemon running — start it with 'takka-runtime --daemon' or use --runtime embedded";

/// Upper bound for waiting until a freshly spawned daemon publishes its
/// discovery file (the broker writes it after bind, so its existence
/// means the listener is ready). 10s was too tight on loaded macOS CI
/// runners (observed: "daemon info file did not appear within 10s" in
/// run 42601781829); the bound is startup plumbing, not the thing
/// under test.
#[cfg(any(unix, windows))]
const DAEMON_SPAWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Upper bound for the shutdown retry loop against a busy broker (the
/// previous connection's slot clears within milliseconds of its EOF;
/// the bound only covers pathological cases).
#[cfg(any(unix, windows))]
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
/// (`daemon_token: None`) wait for it first; daemon callers
/// (`daemon_token: Some(..)`) go straight to `Init` with the discovery
/// file's shared secret, because the broker starts its engine before any
/// client attaches and must never replay that startup `Ready`.
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
    let init_id = io
        .raw_send(Request::Init {
            overrides,
            capabilities: caps,
            daemon_token,
        })
        .await?;
    // Only a frame carrying THIS Init's request id is the reply. A
    // broker may still emit an unrelated frame first (an engine
    // announcement, a replay for an earlier attach), and treating that
    // as the init result turned a stray frame into a failed attach
    // (issue #2: "unexpected initialization event: Ready"). Skip a
    // bounded number of foreign frames, then give up honestly.
    const MAX_FOREIGN_FRAMES: usize = 16;
    for _ in 0..MAX_FOREIGN_FRAMES {
        let initialized = io.raw_next_event().await?;
        let belongs = initialized.request_id.as_deref() == Some(init_id.as_str());
        // A broker-level refusal ("daemon in use", protocol errors found
        // before any request is decoded) is untagged: it answers the
        // attach attempt itself, so it must be surfaced rather than
        // skipped.
        let broker_level_error = initialized.request_id.is_none()
            && matches!(initialized.event, takka_core::Event::Error { .. });
        if !belongs && !broker_level_error {
            continue; // not our reply — not an answer either way
        }
        return match initialized.event {
            takka_core::Event::InitOk { .. } => Ok(()),
            takka_core::Event::Error { message } => Err(anyhow!(message)),
            other => Err(anyhow!("unexpected initialization event: {other:?}")),
        };
    }
    Err(anyhow!(
        "runtime did not answer Init within {MAX_FOREIGN_FRAMES} frames"
    ))
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_turn<C, E, R>(
    io: &mut impl ConnectionIo,
    task: String,
    attachments: Vec<takka_core::Attachment>,
    mut on_chunk: C,
    mut on_event: E,
    mut responder: R,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    mut steer_lines: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
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
        // Mid-run typed text (story 2.1, Interrupt mode): drain everything
        // the inline reader collected; each finished line goes to the
        // engine's steer queue as `Request::Steer`. The receiver is kept
        // for the run's lifetime — `.take()` once per pass would drop it
        // and starve later lines.
        if let Some(steer_lines) = &mut steer_lines {
            while let Ok(line) = steer_lines.try_recv() {
                io.raw_send(Request::Steer { content: line }).await?;
            }
        }
        let event = io.raw_next_event().await?;
        if event.request_id.as_deref() != Some(&request_id) {
            // Steer hand-over events (story 2.1): the engine tags them
            // with the STEER request's id, not the run's, so the id filter
            // would swallow them — forward these unconditionally; the UI
            // surfaces them wherever the run's other events land.
            if matches!(
                event.event,
                takka_core::Event::SteerQueued { .. } | takka_core::Event::Steered { .. }
            ) {
                on_event(event.event);
            }
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

    #[allow(clippy::too_many_arguments)]
    pub async fn run<C, E, R>(
        &mut self,
        task: String,
        attachments: Vec<takka_core::Attachment>,
        on_chunk: C,
        on_event: E,
        responder: R,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
        steer_lines: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
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
            steer_lines,
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

/// Async connection to the daemon broker (Fase 3a). Same line framing
/// and protocol-version check as the spawned-mode stdio client, but over
/// the local transport — a unix socket on unix, a named pipe on Windows
/// (the broker's one-JSON-per-line framing, see its `write_frame` /
/// `pump_requests`).
///
/// One duplex object rather than two halves: a Windows pipe handle is a
/// single `AsyncRead + AsyncWrite` value, and the connection only ever
/// alternates between writing a request and reading the next frame, so a
/// split buys nothing. Incoming bytes are buffered per line.
#[cfg(any(unix, windows))]
pub struct DaemonConnection {
    transport: Duplex,
    pending: Vec<u8>,
    sequence: u64,
}

#[cfg(any(unix, windows))]
impl DaemonConnection {
    /// Wrap an already-connected transport. Handshake is done by the
    /// caller (so [`AgentConnection::connect_daemon_at`] can reuse its
    /// liveness-probe connection instead of reconnecting).
    fn new(transport: Duplex) -> Self {
        Self {
            transport,
            pending: Vec::new(),
            sequence: 0,
        }
    }

    pub async fn send(&mut self, request: Request) -> Result<String> {
        self.sequence += 1;
        let id = format!("cli-{}", self.sequence);
        let envelope = ProtocolRequest::new(id.clone(), request);
        let mut bytes = serde_json::to_vec(&envelope)?;
        bytes.push(b'\n');
        self.transport.write_all(&bytes).await?;
        self.transport.flush().await?;
        Ok(id)
    }

    /// Next newline-delimited frame. Bytes the transport delivered past
    /// the frame boundary stay buffered for the following call.
    async fn next_line(&mut self) -> Result<Option<String>> {
        loop {
            if let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=newline).collect();
                let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
                return Ok(Some(text));
            }
            let mut chunk = [0u8; 8192];
            let read = self.transport.read(&mut chunk).await?;
            if read == 0 {
                // EOF with a partial frame left over is still EOF: the
                // broker never sends a frame without its newline.
                return Ok(None);
            }
            self.pending.extend_from_slice(&chunk[..read]);
        }
    }

    pub async fn next_event(&mut self) -> Result<ProtocolEvent> {
        let line = self
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
        self.transport.shutdown().await?;
        Ok(())
    }
}

#[cfg(any(unix, windows))]
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
    /// local socket — a unix socket on unix, a named pipe on Windows
    /// (discovery via `daemon.json`, auto-spawn on miss). Hosts with
    /// neither transport get [`DAEMON_NOT_SUPPORTED_MESSAGE`].
    Daemon,
}

/// One connection abstraction over both transports (codex's
/// `AppServerClient { Embedded, Remote }` pattern): embedded and spawned
/// are not two products — they are two transports behind one client.
pub enum AgentConnection {
    Spawned(Box<RuntimeClient>),
    InProcess(InProcessClient),
    /// Attached to the external daemon broker over its local socket
    /// (unix socket / Windows named pipe, see [`connect_daemon`]'s
    /// transport seam); other hosts get
    /// [`DAEMON_NOT_SUPPORTED_MESSAGE`].
    ///
    /// [`connect_daemon`]: AgentConnection::connect_daemon
    #[cfg(any(unix, windows))]
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
    /// daemon would be a run in disguise). Hosts with neither a unix
    /// socket nor named pipes return [`DAEMON_NOT_SUPPORTED_MESSAGE`],
    /// the single actionable guard asserted verbatim by a
    /// platform-independent unit test.
    #[cfg(any(unix, windows))]
    pub async fn connect_daemon(
        overrides: InitOverrides,
        caps: ConnectionCapabilities,
        spawn_if_missing: bool,
    ) -> Result<Self> {
        Self::connect_daemon_at(&daemon_info_path(), overrides, caps, spawn_if_missing).await
    }

    /// Stub for hosts with no daemon transport; the actionable message
    /// is the contract.
    #[cfg(not(any(unix, windows)))]
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
    #[cfg(any(unix, windows))]
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
        //    transport connect is the stale signal (portable, race-free;
        //    stale → unlink and respawn once when allowed).
        let transport = match connect_transport(&info.socket_path).await {
            Ok(transport) => transport,
            Err(_) if spawn_if_missing => {
                let _ = std::fs::remove_file(info_path); // stale info (best-effort)
                spawn_daemon_process(info_path)?;
                info = wait_for_daemon_info(info_path).await?;
                connect_transport(&info.socket_path)
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
        let mut connection = DaemonConnection::new(transport);
        perform_handshake(&mut connection, overrides, caps, Some(info.token)).await?;
        Ok(Self::Daemon(connection))
    }

    pub async fn send(&mut self, request: Request) -> Result<String> {
        match self {
            Self::Spawned(client) => client.send(request).await,
            Self::InProcess(client) => client.send(request).await,
            #[cfg(any(unix, windows))]
            Self::Daemon(client) => client.send(request).await,
        }
    }

    pub async fn next_event(&mut self) -> Result<ProtocolEvent> {
        match self {
            Self::Spawned(client) => client.next_event().await,
            Self::InProcess(client) => client.next_event().await,
            #[cfg(any(unix, windows))]
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

    #[allow(clippy::too_many_arguments)]
    pub async fn run<C, E, R>(
        &mut self,
        task: String,
        attachments: Vec<takka_core::Attachment>,
        on_chunk: C,
        on_event: E,
        responder: R,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
        steer_lines: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
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
            steer_lines,
        )
        .await
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        match self {
            Self::Spawned(client) => client.shutdown().await,
            Self::InProcess(client) => client.shutdown().await,
            #[cfg(any(unix, windows))]
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

// ---------------------------------------------------------------------
// Daemon transport seam (H-07): the only platform-specific client code.
// ---------------------------------------------------------------------
//
// Everything above is transport-agnostic (framing, handshake, shutdown
// retry). The seam is one async function — connect to the broker's
// endpoint and hand back a reader half plus a writer half — with a
// `unix` and a `windows` implementation.
//
// Windows uses real named pipes, never a TCP socket: a loopback port is
// reachable by every local user and by any process that can bind it,
// which would widen the local attack surface the shared-secret handshake
// exists to close.
//
// The handshake itself is NOT duplicated: `connect_daemon_at` and
// `shutdown_daemon_at` build a `DaemonConnection` from whatever halves
// the seam returns and run the same `perform_handshake` with the same
// discovery-file token on both platforms.

/// One connected duplex endpoint: a unix socket or a named-pipe handle.
/// A supertrait keeps both halves behind a single trait object (Rust
/// allows only one non-auto trait per `dyn`).
#[cfg(any(unix, windows))]
pub(crate) trait DuplexStream:
    tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send
{
}
#[cfg(unix)]
impl DuplexStream for tokio::net::UnixStream {}
#[cfg(windows)]
impl DuplexStream for tokio::fs::File {}

#[cfg(any(unix, windows))]
pub(crate) type Duplex = Box<dyn DuplexStream>;

/// Unix end of the seam: tokio's event-driven `UnixStream`, which is
/// already `AsyncRead + AsyncWrite`.
#[cfg(unix)]
pub(crate) mod transport {
    use super::*;

    pub async fn connect(endpoint: &str) -> Result<Duplex> {
        let stream = tokio::net::UnixStream::connect(endpoint).await?;
        Ok(Box::new(stream))
    }
}

/// Windows end of the seam: a named pipe opened with a duplex
/// `CreateFileW` handle and bridged into tokio by `tokio::fs::File`.
///
/// Why this shape:
/// - the runtime broker already creates the pipe with
///   `tokio::net::windows::named_pipe`, but the client crate only needs
///   to *open* one, and `ClientOptions` gives no way to obtain the
///   reader/writer halves the `DaemonConnection` wants while also
///   retrying on `ERROR_PIPE_BUSY` (`WaitNamedPipeW` semantics);
/// - `tokio::fs::File::from_std` drives any handle tokio's blocking pool
///   can block on, so a duplex pipe is a valid `AsyncRead + AsyncWrite`
///   with no overlapped-IOCP plumbing and no new dependency;
/// - the handle is opened with `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`
///   so a malicious program that renames the pipe cannot impersonate the
///   user through this connection (the caveat tokio's own
///   `OpenOptions::security_qos_flags` documents).
///
/// Deliberately minimal: compile-checked by
/// `cargo check --target x86_64-pc-windows-gnu` and never yet run on a
/// real Windows host (H-07 report).
#[cfg(windows)]
mod transport {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use std::time::Duration;

    const PIPE_PREFIX: &str = r"\\.\pipe\";
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const OPEN_EXISTING: u32 = 3;
    /// `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION` — the client can
    /// identify the server but never impersonate it.
    const SECURITY_IDENTIFICATION: u32 = 0x0010_0000 | 0x0001_0000;
    const INVALID_HANDLE_VALUE: isize = -1;
    /// `ERROR_PIPE_BUSY`: a server exists but every instance is serving.
    /// The only retryable failure — anything else (notably "file not
    /// found") means no daemon and must surface immediately.
    const ERROR_PIPE_BUSY: i32 = 231;
    /// Upper bound on waiting for a free instance. Comfortably inside the
    /// spawn budget, so a saturated pipe fails as "busy" rather than
    /// silently hanging the CLI.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
    const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(20);

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security_attributes: *mut core::ffi::c_void,
            creation: u32,
            flags: u32,
            template: *mut core::ffi::c_void,
        ) -> *mut core::ffi::c_void;
        fn WaitNamedPipeW(name: *const u16, timeout: u32) -> i32;
        fn GetLastError() -> u32;
    }

    /// Discovery files carry the endpoint as the broker was configured —
    /// a bare pipe name or a full `\\.\pipe\<name>` path. A bare name is
    /// qualified with the local pipe root; both forms then go through the
    /// same `CreateFileW`.
    fn pipe_path(endpoint: &str) -> Vec<u16> {
        let name = if endpoint.starts_with(PIPE_PREFIX) {
            endpoint.to_string()
        } else {
            format!("{PIPE_PREFIX}{endpoint}")
        };
        OsStr::new(&name).encode_wide().chain([0]).collect()
    }

    pub async fn connect(endpoint: &str) -> Result<Duplex> {
        let name = pipe_path(endpoint);
        let deadline = std::time::Instant::now() + CONNECT_TIMEOUT;
        loop {
            // SAFETY: NUL-terminated UTF-16 name that outlives the call;
            // no security attributes and no template, OPEN_EXISTING on a
            // server pipe returns the client end or INVALID_HANDLE_VALUE.
            let raw = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    SECURITY_IDENTIFICATION,
                    std::ptr::null_mut(),
                )
            };
            if raw as isize != INVALID_HANDLE_VALUE {
                // SAFETY: `raw` is a valid, exclusively owned handle (the
                // INVALID_HANDLE_VALUE case returned above).
                let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
                let file = tokio::fs::File::from_std(std::fs::File::from(handle));
                return Ok(Box::new(file));
            }
            // SAFETY: GetLastError reads this thread's last-error slot.
            let code = unsafe { GetLastError() } as i32;
            if code != ERROR_PIPE_BUSY {
                // No server at all (or a real open failure): fail NOW, so
                // the caller's stale-discovery-file handling still sees a
                // dead endpoint instead of a 5s busy wait.
                return Err(std::io::Error::from_raw_os_error(code).into());
            }
            if std::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "daemon pipe {endpoint} stayed busy (every instance in use)"
                ));
            }
            // SAFETY: WaitNamedPipeW only touches the name buffer.
            unsafe { WaitNamedPipeW(name.as_ptr(), 20) };
            tokio::time::sleep(CONNECT_RETRY_DELAY).await;
        }
    }
}

#[cfg(any(unix, windows))]
use transport::connect as connect_transport;

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
#[cfg(any(unix, windows))]
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
#[cfg(any(unix, windows))]
async fn wait_for_daemon_info(info_path: &Path) -> Result<DaemonInfo> {
    let deadline = tokio::time::Instant::now() + DAEMON_SPAWN_TIMEOUT;
    loop {
        if let Ok(info) = parse_daemon_info(info_path) {
            return Ok(info);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "spawned daemon did not publish {} within 60s",
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
#[cfg(any(unix, windows))]
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
        let transport = match connect_transport(&info.socket_path).await {
            Ok(transport) => transport,
            Err(_) => {
                return Err(anyhow!(
                    "{} (stale discovery file at {} — its daemon is gone)",
                    NO_DAEMON_MESSAGE,
                    info_path.display()
                ))
            }
        };
        let mut connection = DaemonConnection::new(transport);
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
mod in_process_tests;

#[cfg(test)]
pub(crate) mod daemon_info_tests;

/// Handshake regression tests (issue #2): a broker frame that is NOT the
/// reply to our `Init` must never be mistaken for the init result.
///
/// These drive `perform_handshake` through a scripted `ConnectionIo`, so
/// the frame a real daemon sent by accident (`Ready`, a replay for an
/// earlier attach) is reproducible deterministically.
#[cfg(test)]
#[cfg(test)]
mod handshake_tests;
