//! Daemon discovery, attachment, auto-spawn, and shutdown client plumbing.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::{Path, PathBuf};
use takka_core::runtime::protocol::{
    ConnectionCapabilities, InitOverrides, ProtocolEvent, ProtocolRequest, Request,
    PROTOCOL_VERSION,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

#[cfg(any(unix, windows))]
use super::transport::{connect as connect_transport, Duplex};
#[cfg(not(any(unix, windows)))]
use super::DAEMON_NOT_SUPPORTED_MESSAGE;
use super::{
    perform_handshake, runtime_binary, send_and_wait_io, AgentConnection, ConnectionIo,
    NO_DAEMON_MESSAGE,
};

/// The actionable "no daemon" error, with the transport failure kept as
/// its `source`.
#[cfg(any(unix, windows))]
pub(crate) fn no_daemon_error(cause: anyhow::Error) -> anyhow::Error {
    #[derive(Debug)]
    struct NoDaemon(Box<dyn std::error::Error + Send + Sync>);

    impl std::fmt::Display for NoDaemon {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(NO_DAEMON_MESSAGE)
        }
    }

    impl std::error::Error for NoDaemon {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&*self.0)
        }
    }

    anyhow::Error::new(NoDaemon(cause.into()))
}

/// Upper bound for waiting until a freshly spawned daemon publishes its
/// discovery file (the broker writes it after bind, so its existence
/// means the listener is ready).
#[cfg(any(unix, windows))]
const DAEMON_SPAWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Upper bound for the shutdown retry loop against a busy broker.
#[cfg(any(unix, windows))]
const DAEMON_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Discovery record the daemon broker publishes next to its socket
/// (`daemon.json`). Field names are pinned to the broker's writer
/// (`takka-runtime/src/daemon/mod.rs::write_info_file`): snake_case keys,
/// no serde renaming.
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

/// Read + parse the discovery file at `path`. Fails on missing,
/// unreadable, or corrupt files — callers treat any error as "no usable
/// info file".
pub fn parse_daemon_info(path: &Path) -> Result<DaemonInfo> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read daemon info {}", path.display()))?;
    parse_daemon_info_str(&content)
        .with_context(|| format!("invalid daemon info in {}", path.display()))
}

/// Default discovery location — the same path (and HOME-less fallback)
/// the runtime broker derives in `takka-runtime/src/main.rs`.
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

/// Async connection to the daemon broker (Fase 3a).
#[cfg(any(unix, windows))]
pub struct DaemonConnection {
    transport: Duplex,
    pending: Vec<u8>,
    sequence: u64,
}

#[cfg(any(unix, windows))]
impl DaemonConnection {
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

impl AgentConnection {
    /// Attach to the external daemon broker ([`super::RuntimeMode::Daemon`]
    /// and the `attach` command).
    #[cfg(any(unix, windows))]
    pub async fn connect_daemon(
        overrides: InitOverrides,
        caps: ConnectionCapabilities,
        spawn_if_missing: bool,
    ) -> Result<Self> {
        Self::connect_daemon_at(&daemon_info_path(), overrides, caps, spawn_if_missing).await
    }

    #[cfg(not(any(unix, windows)))]
    pub async fn connect_daemon(
        _overrides: InitOverrides,
        _caps: ConnectionCapabilities,
        _spawn_if_missing: bool,
    ) -> Result<Self> {
        Err(anyhow!(DAEMON_NOT_SUPPORTED_MESSAGE))
    }

    /// [`Self::connect_daemon`] against an explicit discovery file.
    #[cfg(any(unix, windows))]
    pub(crate) async fn connect_daemon_at(
        info_path: &Path,
        overrides: InitOverrides,
        caps: ConnectionCapabilities,
        spawn_if_missing: bool,
    ) -> Result<Self> {
        let mut info = parse_daemon_info(info_path).ok();

        if info.is_none() && spawn_if_missing {
            spawn_daemon_process(info_path)?;
            info = Some(wait_for_daemon_info(info_path).await?);
        }

        let mut info = info.ok_or_else(|| anyhow!(NO_DAEMON_MESSAGE))?;

        let transport = match connect_transport(&info.socket_path).await {
            Ok(transport) => transport,
            Err(_) if spawn_if_missing => {
                let _ = std::fs::remove_file(info_path);
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
            Err(error) => {
                let _ = std::fs::remove_file(info_path);
                return Err(no_daemon_error(error));
            }
        };

        let mut connection = DaemonConnection::new(transport);
        perform_handshake(&mut connection, overrides, caps, Some(info.token)).await?;
        Ok(Self::Daemon(connection))
    }
}

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
        .kill_on_drop(false)
        .spawn()
        .with_context(|| format!("failed to spawn daemon from {}", binary.display()))?;
    Ok(())
}

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

/// Ask the daemon broker recorded in `info_path` to shut down.
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
        if connection.send(Request::Shutdown).await.is_err() {
            if tokio::time::Instant::now() >= deadline {
                return Err(busy());
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            continue;
        }

        const REFUSAL_CHECK: std::time::Duration = std::time::Duration::from_millis(500);
        match tokio::time::timeout(REFUSAL_CHECK, connection.next_event()).await {
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
                _ => return Ok(()),
            },
            Ok(Err(_)) | Err(_) => return Ok(()),
        }
    }
}
