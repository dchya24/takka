mod daemon;
#[cfg(any(unix, windows))]
pub(crate) mod transport;

#[cfg(test)]
pub(crate) mod daemon_info_tests;
#[cfg(test)]
mod handshake_tests;
#[cfg(test)]
mod in_process_tests;

pub use daemon::daemon_info_path;
#[cfg(all(test, any(unix, windows)))]
pub(crate) use daemon::no_daemon_error;
#[cfg(test)]
pub(crate) use daemon::parse_daemon_info_str;
#[cfg(any(unix, windows))]
pub(crate) use daemon::shutdown_daemon_at;
#[cfg(any(unix, windows))]
pub use daemon::DaemonConnection;

use anyhow::{anyhow, Context, Result};
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
        if !cancel_sent && cancel.load(std::sync::atomic::Ordering::Relaxed) {
            io.raw_send(Request::Cancel).await?;
            cancel_sent = true;
        }
        if let Some(steer_lines) = &mut steer_lines {
            while let Ok(line) = steer_lines.try_recv() {
                io.raw_send(Request::Steer { content: line }).await?;
            }
        }
        let event = io.raw_next_event().await?;
        if event.request_id.as_deref() != Some(&request_id) {
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
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.run()));
            if result.is_err() {
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
        self.events
            .recv()
            .await
            .ok_or_else(|| anyhow!("embedded runtime engine stopped"))
    }

    pub async fn send_and_wait(&mut self, request: Request) -> Result<ProtocolEvent> {
        send_and_wait_io(self, request).await
    }

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
            RuntimeMode::Daemon => Self::connect_daemon(overrides, caps, true).await,
        }
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

#[cfg(test)]
mod pipe_id_tests {
    use takka_core::runtime::pipe_id::{fnv1a64, normalize, pipe_name_for};

    #[test]
    fn pipe_name_matches_server_derivation() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(normalize("/tmp/x.SOCK/"), r"\tmp\x.sock");

        assert_eq!(
            pipe_name_for("/tmp/takka-da-123-456/ep-456.sock"),
            r"\\.\pipe\takka-0013c643e0315b28"
        );
        assert_eq!(
            pipe_name_for(r"C:\Temp\takka-da-1-2\ep-3.sock"),
            r"\\.\pipe\takka-2dd13866d5f634ea"
        );
        assert_eq!(
            pipe_name_for(r"\\.\pipe\takka-pinned"),
            r"\\.\pipe\takka-pinned"
        );
    }

    #[test]
    fn no_daemon_error_keeps_the_cause() {
        let cause = anyhow::anyhow!("Access is denied. (os error 5)");
        let error = super::no_daemon_error(cause);
        assert_eq!(error.to_string(), super::NO_DAEMON_MESSAGE);
        let chain: Vec<String> = error.chain().map(|link| link.to_string()).collect();
        assert!(
            chain.iter().any(|link| link.contains("os error 5")),
            "cause must survive in the chain, got {chain:?}"
        );
    }

    #[test]
    fn distinct_endpoints_never_collide() {
        let a = pipe_name_for("/tmp/takka-da-123-456/ep-456.sock");
        let b = pipe_name_for("/tmp/takka-da-123-457/ep-457.sock");
        assert_ne!(a, b);
        assert_eq!(a, pipe_name_for("/tmp/takka-da-123-456/ep-456.sock"));
    }
}
