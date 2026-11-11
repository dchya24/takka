//! Unix-socket daemon broker (Fase 3a).
//!
//! The daemon is a *broker*, not a new engine: one `RuntimeEngine`
//! talks over a `MemoryTransport`; the broker pumps wire frames between
//! the socket connection(s) and that engine's channels. Rules:
//!
//! - ONE active connection. A second attach is refused with a single
//!   `"daemon in use"` error frame; the active connection is never
//!   touched.
//! - Detach (client disconnect) never blocks the engine: the permanent
//!   event drain loop keeps consuming engine events and discards them
//!   while no writer is installed — the engine never blocks on a dead
//!   frontend and the orchestrator survives detach/reattach.
//! - Liveness guard: a live pid in the info file refuses startup with
//!   `"daemon already running (pid N)"`; anything else (dead pid,
//!   corrupt file) is stale and unlinked before bind.
//! - Shutdown comes from the engine (`Request::Shutdown`); SIGTERM and
//!   SIGINT are deliberately not handled — crash cleanup relies on the
//!   next daemon's stale cleanup (documented in the design spec).
//! - Publish order: the discovery file is the LAST startup step. Its
//!   existence is the contract "this daemon accepts clients", so every
//!   earlier step (engine thread, drain thread, non-blocking listener)
//!   must already be in place. Publishing earlier let a client attach to
//!   a half-started broker and observe EOF instead of a handshake reply
//!   (the macOS CI signature in issue #2).
//! - Visibility: the engine thread never dies silently. A panic is
//!   caught, reported on stderr and turned into an orderly broker
//!   shutdown, so a dead engine always looks like a shutdown — never
//!   like a connection that just vanished.

/// Single clearinghouse message for unsupported platforms (Windows:
/// named pipes are a documented TODO). Used by `main` on non-unix
/// hosts; kept platform-unconditional so the guard test compiles and
/// runs on every CI host.
#[cfg_attr(unix, allow(dead_code))]
pub fn not_supported_error() -> &'static str {
    "daemon mode not supported on Windows yet — use --runtime embedded|spawn"
}

// ---------------------------------------------------------------------
// Broker (unix only; the message helper above stays portable so the
// windows guard branch of `main` and its test compile everywhere).
// ---------------------------------------------------------------------

#[cfg(unix)]
use std::io::{self, BufRead, BufReader, BufWriter, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::sync::mpsc;
#[cfg(unix)]
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::time::{Duration, Instant};
#[cfg(unix)]
use takka_core::events::Event;
#[cfg(unix)]
use takka_core::runtime::engine::RuntimeEngine;
#[cfg(unix)]
use takka_core::runtime::protocol::{ProtocolEvent, ProtocolRequest};
#[cfg(unix)]
use takka_core::runtime::transport::MemoryTransport;

/// Daemon endpoints, both explicit (the CLI resolves defaults before
/// calling [`serve`]): the unix socket to listen on and the discovery
/// info file to publish.
#[cfg(unix)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub info_path: PathBuf,
}

#[cfg(unix)]
const DAEMON_IN_USE_MESSAGE: &str = "daemon in use";

/// Accept poll interval for the nonblocking listener and the shutdown
/// flag — bounds shutdown latency without burning CPU.
#[cfg(unix)]
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Bound on joining reader/drain/engine threads after shutdown.
/// Readers blocked on an idle-but-connected socket are left detached;
/// the process exits right after `serve` returns.
#[cfg(unix)]
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// The writer half of the active connection, tagged with a generation
/// counter: reader threads may outlive their connection's slot (EOF
/// detection races with the next attach), so they only clear the slot
/// when the generation still matches their own.
#[cfg(unix)]
type WriterSlot = Arc<Mutex<Option<(u64, BufWriter<UnixStream>)>>>;

/// Serve the daemon until the engine reports shutdown.
/// Blocking; expected to run on `main`'s thread (tests spawn it).
#[cfg(unix)]
pub fn serve(config: DaemonConfig) -> io::Result<()> {
    // 1) Stale guard: a live pid in the info file means another daemon
    //    owns this socket — refuse. Anything else is stale and removed.
    if let Some(pid) = live_info_pid(&config.info_path) {
        if pid_is_alive(pid) {
            return Err(io::Error::other(format!(
                "daemon already running (pid {pid})"
            )));
        }
    }
    let _ = std::fs::remove_file(&config.info_path); // stale info (best-effort)
    let _ = std::fs::remove_file(&config.socket_path); // stale socket file

    for path in [&config.socket_path, &config.info_path] {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
    }

    // 2) Bind + shared secret.
    let listener = UnixListener::bind(&config.socket_path)?;
    let token = generate_token()?;

    // 3) One engine behind a MemoryTransport; the broker pumps frames
    //    between sockets and these channels. The engine exits only on
    //    `Request::Shutdown` (or total sender drop) — client detaches
    //    do not stop it.
    let (req_tx, req_rx) = mpsc::channel::<ProtocolRequest>();
    let (evt_tx, evt_rx) = mpsc::channel::<ProtocolEvent>();
    let shutdown = Arc::new(AtomicBool::new(false));
    let engine_shutdown = shutdown.clone();
    let engine_token = token.clone();
    let engine_handle = std::thread::Builder::new()
        .name("daemon-engine".into())
        .spawn(move || {
            // A panicking engine must never look like a vanished
            // connection: catch it, report it, and stop the broker the
            // same way a clean engine exit does.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut engine = RuntimeEngine::new(MemoryTransport::new(req_rx, evt_tx));
                engine.set_daemon_token(Some(engine_token));
                engine.set_adopt_on_reinit(true);
                engine.run();
            }));
            if let Err(payload) = outcome {
                let detail = payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "non-string panic payload".to_string());
                tracing::error!(panic = %detail, "daemon: engine thread panicked");
            }
            tracing::info!("daemon: engine thread finished");
            engine_shutdown.store(true, Ordering::SeqCst);
        })?;

    // 4) Permanent event drain: forwards engine events to whichever
    //    writer is installed, discards while detached — the engine
    //    never blocks on a dead frontend. Exits when the engine (and
    //    its `MemoryTransport`) is dropped.
    let slot: WriterSlot = Arc::new(Mutex::new(None));
    let drain_handle = std::thread::Builder::new()
        .name("daemon-event-drain".into())
        .spawn({
            let slot = slot.clone();
            move || {
                while let Ok(event) = evt_rx.recv() {
                    // The engine's startup `Ready` announces "the engine
                    // loop started". No client can have seen it (it is
                    // emitted before anyone attaches), so forwarding it
                    // would only confuse the first client's handshake:
                    // the drain loop usually discards it while the slot
                    // is empty, but a client attaching in that window
                    // used to receive it instead of `InitOk` (issue #2,
                    // "unexpected initialization event: Ready"). Always
                    // drop it here so the outcome never depends on
                    // timing.
                    if !should_forward(&event) {
                        continue;
                    }
                    let mut guard = slot.lock().unwrap();
                    if let Some((_, writer)) = guard.as_mut() {
                        if write_frame(writer, &event).is_err() {
                            *guard = None; // client vanished mid-stream
                        }
                    }
                    // None (detached): discard — engine never blocks.
                }
            }
        })?;

    // 5) Non-blocking accept loop is armed BEFORE the discovery file is
    //    published: a client that sees the file must never find a
    //    listener that is not accepting yet.
    listener.set_nonblocking(true)?;

    // 6) Discovery info LAST — its existence means the whole broker
    //    (engine thread, drain thread, armed listener) is up. Perms
    //    0600: the token is a shared secret.
    write_info_file(&config, &token)?;
    tracing::info!(socket = %config.socket_path.display(), "daemon: listening");

    let mut readers: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let mut connection_generation: u64 = 0;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                connection_generation += 1;
                if let Some(handle) =
                    handle_connection(stream, connection_generation, &req_tx, &slot)
                {
                    readers.retain(|handle| !handle.is_finished());
                    readers.push(handle);
                }
            }
            Err(ref error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => tracing::warn!(%error, "daemon: accept failed"),
        }
        std::thread::sleep(ACCEPT_POLL_INTERVAL);
    }

    // 7) Bounded join + cleanup. Dropping our request sender lets any
    //    reader still pumping fail its `send` and exit.
    drop(req_tx);
    join_bounded(readers);
    join_bounded(vec![engine_handle]);
    join_bounded(vec![drain_handle]);
    let _ = std::fs::remove_file(&config.socket_path);
    let _ = std::fs::remove_file(&config.info_path);
    Ok(())
}

/// Accepted connections are explicitly made BLOCKING.
///
/// `accept()` on a non-blocking listener returns a socket that INHERITS
/// `O_NONBLOCK` on macOS/BSD, while Linux clears it. With the flag set,
/// the reader gets `WouldBlock` instead of waiting for the client's first
/// frame, treats a healthy client as a detach, and closes the connection
/// while the daemon keeps running — exactly the macOS CI signature of
/// issue #2 (client sees EOF, daemon still alive, the engine never saw
/// `Init`). Never rely on platform `accept` semantics.
#[cfg(unix)]
fn make_blocking(stream: UnixStream) -> io::Result<UnixStream> {
    stream.set_nonblocking(false)?;
    Ok(stream)
}

/// First rule of `handle_connection`: ONE active connection. A second
/// attach gets a single `"daemon in use"` frame and a close — the
/// active connection is never touched. Otherwise installs the writer
/// slot and spawns the request pump; returns its join handle.
#[cfg(unix)]
fn handle_connection(
    stream: UnixStream,
    generation: u64,
    req_tx: &mpsc::Sender<ProtocolRequest>,
    slot: &WriterSlot,
) -> Option<std::thread::JoinHandle<()>> {
    // BSD/macOS: the accepted socket may have inherited the listener's
    // non-blocking flag. Clear it before anyone reads or writes.
    let stream = match make_blocking(stream) {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(%error, "daemon: cannot make the connection blocking");
            return None;
        }
    };

    if slot.lock().unwrap().is_some() {
        let mut writer = BufWriter::new(stream);
        let _ = write_frame(
            &mut writer,
            &ProtocolEvent::new(
                None,
                Event::Error {
                    message: DAEMON_IN_USE_MESSAGE.to_string(),
                },
            ),
        );
        return None; // stream dropped → connection closed
    }

    let writer = match stream.try_clone() {
        Ok(writer) => BufWriter::new(writer),
        Err(error) => {
            tracing::warn!(%error, "daemon: cannot wrap connection");
            return None;
        }
    };
    *slot.lock().unwrap() = Some((generation, writer));

    let pump_slot = slot.clone();
    let pump_tx = req_tx.clone();
    std::thread::Builder::new()
        .name("daemon-reader".into())
        .spawn(move || pump_requests(stream, generation, pump_tx, pump_slot))
        .ok()
}

/// Reader thread for the active connection: decodes request frames and
/// forwards them into the engine channel. EOF/error or engine shutdown
/// ends the pump; the slot is released only if this connection still
/// owns it (a newer attach may already have replaced it).
#[cfg(unix)]
fn pump_requests(
    stream: UnixStream,
    generation: u64,
    req_tx: mpsc::Sender<ProtocolRequest>,
    slot: WriterSlot,
) {
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break, // client detached
            Ok(_) if line.trim().is_empty() => continue,
            Ok(_) => match serde_json::from_str::<ProtocolRequest>(line.trim()) {
                Ok(request) => {
                    if req_tx.send(request).is_err() {
                        break; // engine gone — daemon is shutting down
                    }
                }
                // Mirror the engine's transport-level error shape so
                // malformed frames surface to the frontend too.
                Err(error) => {
                    let mut guard = slot.lock().unwrap();
                    if let Some((_, writer)) = guard.as_mut() {
                        let _ = write_frame(
                            writer,
                            &ProtocolEvent::new(
                                None,
                                Event::Error {
                                    message: format!("protocol_error: {error}"),
                                },
                            ),
                        );
                    }
                }
            },
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::Interrupted =>
            {
                // Never a detach: a client that has not spoken yet is
                // healthy. (Should be unreachable now that accepted
                // streams are explicitly blocking, but a spin is worse
                // than a short wait if a platform surprises us again.)
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(_) => break, // read error → treat as detach
        }
    }
    let mut guard = slot.lock().unwrap();
    if matches!(guard.as_ref(), Some((owner, _)) if *owner == generation) {
        *guard = None;
    }
}

/// Whether an engine event may reach a socket client.
///
/// `Ready` is the engine's own startup announcement. In the daemon the
/// engine starts before any client attaches, so `Ready` describes a
/// moment no client can have observed — replaying it only breaks the
/// first handshake (it lands where `InitOk` is expected). Everything
/// else is forwarded as-is (detach is handled by the absent writer).
#[cfg(unix)]
fn should_forward(event: &ProtocolEvent) -> bool {
    !matches!(event.event, Event::Ready { .. })
}

/// One JSON event frame per line (same framing as `StdioTransport`).
#[cfg(unix)]
fn write_frame(writer: &mut BufWriter<UnixStream>, event: &ProtocolEvent) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, event)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// 32 hex chars (16 bytes) from `/dev/urandom`.
#[cfg(unix)]
fn generate_token() -> io::Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// `{"pid":u32,"socket_path":String,"token":String,"version":String}`,
/// permissions 0600 (mode applies at create; set explicitly too so a
/// pre-existing file can never keep wider perms).
#[cfg(unix)]
fn write_info_file(config: &DaemonConfig, token: &str) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let info = serde_json::json!({
        "pid": std::process::id(),
        "socket_path": config.socket_path.to_string_lossy(),
        "token": token,
        "version": daemon_version(),
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&config.info_path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(
        serde_json::to_string_pretty(&info)
            .expect("info json")
            .as_bytes(),
    )?;
    file.write_all(b"\n")?;
    Ok(())
}

/// Version reported in the info file. `takka_core` exposes no version
/// constant, so the broker reports the runtime binary's own version.
#[cfg(unix)]
fn daemon_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// pid from a (possibly corrupt/absent) info file; `None` = no usable
/// pid, i.e. nothing to guard against.
#[cfg(unix)]
fn live_info_pid(info_path: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(info_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    let pid = value["pid"].as_u64()?;
    u32::try_from(pid).ok()
}

/// Best-effort liveness check: `/proc` on Linux (precise for reaped
/// pids), `kill -0` elsewhere. PID reuse can false-positive — the token
/// handshake, not this guard, is the real boundary.
#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    if Path::new(&format!("/proc/{pid}")).exists() {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        // No /proc entry on Linux ⇒ pid gone (post-reap).
        false
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

/// Join with a deadline; stragglers are left detached (documented at
/// [`SHUTDOWN_JOIN_TIMEOUT`]).
#[cfg(unix)]
fn join_bounded(handles: Vec<std::thread::JoinHandle<()>>) {
    let deadline = Instant::now() + SHUTDOWN_JOIN_TIMEOUT;
    for handle in handles {
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                tracing::warn!("daemon: thread did not finish in time; left detached");
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = handle.join();
    }
}

#[cfg(test)]
mod windows_guard_tests {
    use super::not_supported_error;

    /// Windows-level guard contract, compiled on every host: the
    /// message is exactly one actionable line.
    #[test]
    fn not_supported_error_is_the_single_line_message() {
        assert_eq!(
            not_supported_error(),
            "daemon mode not supported on Windows yet — use --runtime embedded|spawn"
        );
        assert!(!not_supported_error().contains('\n'));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, BufWriter, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use takka_core::events::Event;
    use takka_core::runtime::protocol::ProtocolEvent;

    /// Every poll/read in these tests is bounded by this deadline so a
    /// regression can never hang the suite.
    const POLL_TIMEOUT: Duration = Duration::from_secs(10);

    fn deadline() -> Instant {
        Instant::now() + POLL_TIMEOUT
    }

    /// Local tempdir (the `support` helpers live in takka-core's test
    /// tree; this crate has no cross-crate test dependency).
    fn tempdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "takka-daemon-test-{}-{}-{}",
            std::process::id(),
            nanos,
            tag
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Hermetic provider config pinned to the discard port
    /// (`127.0.0.1:9`): `Init` succeeds, a `Run` fails fast with a
    /// connection error and never leaves the machine — no ambient
    /// `~/.config/agentic/config.json` is ever read.
    fn write_hermetic_config(dir: &Path) -> String {
        let path = dir.join("config.json");
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
        .unwrap();
        path.to_string_lossy().into_owned()
    }

    fn spawn_serve(
        socket_path: PathBuf,
        info_path: PathBuf,
    ) -> std::thread::JoinHandle<std::io::Result<()>> {
        std::thread::spawn(move || {
            serve(DaemonConfig {
                socket_path,
                info_path,
            })
        })
    }

    /// The info file is written after bind and before the accept loop,
    /// so its existence means the listener is ready.
    fn wait_for_info_file(info_path: &Path) {
        let deadline = deadline();
        while !info_path.exists() {
            assert!(
                Instant::now() < deadline,
                "daemon info file did not appear within 10s"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn info_value(info_path: &Path) -> serde_json::Value {
        let text = std::fs::read_to_string(info_path).expect("read info file");
        serde_json::from_str(&text).expect("parse info file")
    }

    fn wait_serve_exit(handle: std::thread::JoinHandle<std::io::Result<()>>) {
        let deadline = deadline();
        while !handle.is_finished() {
            assert!(Instant::now() < deadline, "serve did not exit within 10s");
            std::thread::sleep(Duration::from_millis(10));
        }
        handle
            .join()
            .expect("serve thread did not panic")
            .expect("serve returned Ok");
    }

    /// Minimal test client: raw framed JSON over the socket (the
    /// public `UnixSocketTransport` has no event-reading API and its
    /// line reader lives in another crate — this is the same framing).
    struct TestConn {
        sock: UnixStream,
        reader: BufReader<UnixStream>,
        writer: BufWriter<UnixStream>,
    }

    impl TestConn {
        fn connect(socket_path: &Path, deadline: Instant) -> Self {
            loop {
                match UnixStream::connect(socket_path) {
                    Ok(stream) => {
                        let reader = BufReader::new(stream.try_clone().expect("clone reader half"));
                        let sock = stream.try_clone().expect("clone timeout handle");
                        return Self {
                            sock,
                            reader,
                            writer: BufWriter::new(stream),
                        };
                    }
                    Err(_) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("connect to {socket_path:?} failed: {error}"),
                }
            }
        }

        fn send(&mut self, frame: &serde_json::Value) {
            serde_json::to_writer(&mut self.writer, frame).expect("encode frame");
            self.writer.write_all(b"\n").expect("write frame newline");
            self.writer.flush().expect("flush frame");
        }

        fn next_event(&mut self, deadline: Instant) -> ProtocolEvent {
            let remaining = deadline.saturating_duration_since(Instant::now());
            self.sock
                .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
                .expect("set read timeout");
            loop {
                let mut line = String::new();
                match self.reader.read_line(&mut line) {
                    Ok(0) => panic!("daemon closed the connection while waiting for an event"),
                    Ok(_) if line.trim().is_empty() => continue,
                    Ok(_) => {
                        return serde_json::from_str(line.trim())
                            .unwrap_or_else(|error| panic!("decode event frame: {error}"))
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        panic!("timed out waiting for an event frame")
                    }
                    Err(error) => panic!("read event frame failed: {error}"),
                }
            }
        }

        /// The broker must never forward the engine's startup `Ready`:
        /// a client's first frame is its `Init` reply (`InitOk`). Any
        /// `Ready` here is the timing-dependent leak from issue #2, so
        /// it is a hard failure rather than a skip.
        fn expect_init_ok(&mut self, deadline: Instant, context: &str) -> ProtocolEvent {
            let event = self.next_event(deadline);
            match &event.event {
                Event::Ready { .. } => panic!(
                    "{context}: broker forwarded the engine's startup Ready \
                     (clients must only see InitOk) — the drain filter regressed"
                ),
                Event::InitOk { .. } => event,
                Event::Error { message } => panic!("{context}: got error frame: {message}"),
                other => panic!("{context}: unexpected frame: {other:?}"),
            }
        }

        fn expect_terminal(&mut self, deadline: Instant) -> ProtocolEvent {
            loop {
                let event = self.next_event(deadline);
                match &event.event {
                    Event::Done { .. } | Event::Error { .. } => return event,
                    _ => continue,
                }
            }
        }

        fn expect_eof(&mut self, deadline: Instant) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            self.sock
                .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
                .expect("set read timeout");
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => {}
                Ok(_) => panic!("expected EOF, got another frame: {line}"),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    panic!("timed out waiting for EOF")
                }
                // Closing a socket that still holds unread input (our
                // init frame was never read by the refusing daemon)
                // signals RST rather than FIN — both mean "closed".
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                Err(error) => panic!("read after refusal failed: {error}"),
            }
        }
    }

    fn init_frame(id: &str, token: &str, config_path: &str) -> serde_json::Value {
        serde_json::json!({
            "v": 2,
            "id": id,
            "type": "init",
            "overrides": { "configPath": config_path },
            "daemonToken": token,
        })
    }

    fn run_frame(id: &str, task: &str) -> serde_json::Value {
        serde_json::json!({ "v": 2, "id": id, "type": "run", "task": task })
    }

    fn shutdown_frame(id: &str) -> serde_json::Value {
        serde_json::json!({ "v": 2, "id": id, "type": "shutdown" })
    }

    fn list_tools_frame(id: &str) -> serde_json::Value {
        serde_json::json!({ "v": 2, "id": id, "type": "list_tools" })
    }

    fn spawn_daemon(
        tag: &str,
    ) -> (
        PathBuf,
        PathBuf,
        String,
        std::thread::JoinHandle<std::io::Result<()>>,
    ) {
        let dir = tempdir(tag);
        let config_path = write_hermetic_config(&dir);
        let socket_path = dir.join(format!("{tag}.sock"));
        let info_path = dir.join("daemon.json");
        let handle = spawn_serve(socket_path.clone(), info_path.clone());
        wait_for_info_file(&info_path);
        let token = info_value(&info_path)["token"]
            .as_str()
            .expect("info file carries the token")
            .to_string();
        (socket_path, PathBuf::from(config_path), token, handle)
    }

    /// The broker's forwarding rule, unit-tested without timing: the
    /// engine's `Ready` is the only event a socket client must never
    /// see; everything else keeps flowing.
    #[test]
    fn drain_filter_drops_only_the_startup_ready() {
        let ready = ProtocolEvent::new(
            None,
            Event::Ready {
                protocol: "takka".into(),
                version: 2,
            },
        );
        assert!(!should_forward(&ready));

        let init_ok = ProtocolEvent::new(
            Some("init-1".into()),
            Event::InitOk {
                protocol: "takka".into(),
                version: 2,
            },
        );
        assert!(should_forward(&init_ok));

        let error = ProtocolEvent::new(
            None,
            Event::Error {
                message: "boom".into(),
            },
        );
        assert!(should_forward(&error));
    }

    /// The accept-mode invariant behind issue #2: whatever `accept()`
    /// inherits from a non-blocking listener (macOS/BSD inherit
    /// `O_NONBLOCK`, Linux clears it), a served connection must end up
    /// blocking — otherwise the reader mistakes a silent client for a
    /// detach and closes a healthy connection.
    #[test]
    fn accepted_connections_are_forced_blocking() {
        let dir = tempdir("accept-mode");
        let path = dir.join("mode.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");

        let client = UnixStream::connect(&path).expect("connect");
        let deadline = deadline();
        let accepted = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "accept did not complete");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        };
        let mut accepted = make_blocking(accepted).expect("clear the non-blocking flag");
        // No getter for the flag, so prove it by behaviour: a blocking
        // stream with a read timeout WAITS for the timeout, while one
        // that still carries O_NONBLOCK returns instantly.
        accepted
            .set_read_timeout(Some(Duration::from_millis(400)))
            .expect("set read timeout");
        let started = Instant::now();
        let mut probe = [0u8; 1];
        assert!(
            accepted.read(&mut probe).is_err(),
            "an idle peer must not deliver a frame"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(350),
            "a served connection must block, not return WouldBlock instantly \
             (it still carries the listener's non-blocking flag); elapsed {:?}",
            started.elapsed()
        );
        drop(client);
    }

    /// A client that connects and stays silent is normal (it may be slow
    /// to send its `Init`). The broker must keep the connection open;
    /// closing it here is the issue #2 failure mode verbatim.
    #[test]
    fn an_idle_connection_is_not_mistaken_for_a_detach() {
        let (socket_path, config_path, token, serve_handle) = spawn_daemon("idle");

        let mut client = TestConn::connect(&socket_path, deadline());
        std::thread::sleep(Duration::from_millis(300)); // deliberately quiet

        client.send(&init_frame(
            "init-idle",
            &token,
            &config_path.to_string_lossy(),
        ));
        let event = client.expect_init_ok(deadline(), "init after an idle period");
        assert_eq!(event.request_id.as_deref(), Some("init-idle"));

        client.send(&shutdown_frame("down"));
        drop(client);
        wait_serve_exit(serve_handle);
    }

    /// Happy path: connect → Init (token + hermetic config) → InitOk →
    /// Run → a terminal frame (Done or the provider's Error — both are
    /// legal terminals for an unreachable provider) → Shutdown stops
    /// serve cleanly and unlinks its files.
    #[test]
    fn serve_round_trips_basic_run() {
        let (socket_path, config_path, token, serve_handle) = spawn_daemon("roundtrip");

        let mut client = TestConn::connect(&socket_path, deadline());
        client.send(&init_frame(
            "init-1",
            &token,
            &config_path.to_string_lossy(),
        ));
        let event = client.expect_init_ok(deadline(), "init");
        assert_eq!(event.request_id.as_deref(), Some("init-1"));

        client.send(&run_frame("run-1", "say hi"));
        let event = client.expect_terminal(deadline());
        assert_eq!(event.request_id.as_deref(), Some("run-1"));

        client.send(&shutdown_frame("down-1"));
        drop(client);
        wait_serve_exit(serve_handle);
        assert!(!socket_path.exists(), "socket file must be unlinked");
        assert!(!config_path.parent().unwrap().join("daemon.json").exists());
    }

    /// While connection A is active, B gets exactly one
    /// `"daemon in use"` error frame and a close; A keeps working.
    #[test]
    fn second_connection_is_refused() {
        let (socket_path, config_path, token, serve_handle) = spawn_daemon("refusal");

        let mut a = TestConn::connect(&socket_path, deadline());
        a.send(&init_frame(
            "init-a",
            &token,
            &config_path.to_string_lossy(),
        ));
        a.expect_init_ok(deadline(), "init A");

        let mut b = TestConn::connect(&socket_path, deadline());
        b.send(&init_frame(
            "init-b",
            &token,
            &config_path.to_string_lossy(),
        ));
        let event = b.next_event(deadline());
        match &event.event {
            Event::Error { message } => assert_eq!(message, "daemon in use"),
            other => panic!("expected refusal frame, got {other:?}"),
        }
        b.expect_eof(deadline()); // refused connection is closed by the daemon

        // A is unaffected by B's refused attach.
        a.send(&list_tools_frame("lt-1"));
        let event = a.next_event(deadline());
        assert!(
            matches!(event.event, Event::ToolList { .. }),
            "expected ToolList on the active connection, got {:?}",
            event.event
        );
        assert_eq!(event.request_id.as_deref(), Some("lt-1"));

        a.send(&shutdown_frame("down"));
        drop(a);
        drop(b);
        wait_serve_exit(serve_handle);
    }

    /// A inits, detaches; B re-inits with the same token/caps/config
    /// and the engine adopts the session (`InitOk` + `SessionAdopted`).
    #[test]
    fn reattach_adopts_session() {
        let (socket_path, config_path, token, serve_handle) = spawn_daemon("adopt");

        let mut a = TestConn::connect(&socket_path, deadline());
        a.send(&init_frame(
            "init-a",
            &token,
            &config_path.to_string_lossy(),
        ));
        a.expect_init_ok(deadline(), "init A");
        drop(a); // detach

        // If A's reader thread has not noticed the EOF yet, the broker
        // still holds the slot and refuses B — poll until adoption.
        let deadline = Instant::now() + POLL_TIMEOUT;
        loop {
            assert!(
                Instant::now() < deadline,
                "reattach did not succeed within 10s"
            );
            let mut b = TestConn::connect(&socket_path, deadline);
            b.send(&init_frame(
                "init-b",
                &token,
                &config_path.to_string_lossy(),
            ));
            let event = b.next_event(deadline);
            match &event.event {
                Event::Error { message } if message == "daemon in use" => {
                    drop(b);
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Event::Error { message } => {
                    panic!("unexpected error on reattach: {message}")
                }
                Event::InitOk { .. } => {
                    assert_eq!(event.request_id.as_deref(), Some("init-b"));
                    let adopted = b.next_event(deadline);
                    assert_eq!(adopted.request_id.as_deref(), Some("init-b"));
                    assert!(
                        matches!(adopted.event, Event::SessionAdopted { .. }),
                        "expected SessionAdopted, got {:?}",
                        adopted.event
                    );
                    b.send(&shutdown_frame("down"));
                    drop(b);
                    break;
                }
                other => panic!("unexpected frame on reattach: {other:?}"),
            }
        }
        wait_serve_exit(serve_handle);
    }

    /// Shutdown over an initialized connection: serve returns cleanly
    /// and the socket + info files are gone.
    #[test]
    fn shutdown_unlinks_files() {
        let (socket_path, config_path, token, serve_handle) = spawn_daemon("shutdown");
        assert!(socket_path.exists() && config_path.parent().unwrap().join("daemon.json").exists());

        let mut client = TestConn::connect(&socket_path, deadline());
        client.send(&init_frame(
            "init-1",
            &token,
            &config_path.to_string_lossy(),
        ));
        client.expect_init_ok(deadline(), "init");

        client.send(&shutdown_frame("down-1"));
        drop(client);
        wait_serve_exit(serve_handle);
        assert!(!socket_path.exists(), "socket file must be unlinked");
        assert!(
            !config_path.parent().unwrap().join("daemon.json").exists(),
            "info file must be unlinked"
        );
    }

    /// A stale info file with a dead pid (plus a garbage socket file)
    /// must NOT block startup: serve overwrites the info file with its
    /// own pid, replaces the socket file with a live listener, and
    /// answers a shutdown.
    #[test]
    fn stale_info_file_is_cleaned() {
        let dir = tempdir("stale");
        let socket_path = dir.join("stale.sock");
        let info_path = dir.join("daemon.json");

        // Guaranteed-dead pid: spawn, then reap via wait().
        let mut child = std::process::Command::new("sleep")
            .arg("0")
            .spawn()
            .expect("spawn sleep 0");
        let dead_pid = child.id();
        child.wait().expect("reap sleep 0");

        std::fs::write(
            &info_path,
            format!(
                r#"{{"pid": {dead_pid}, "socket_path": "{}", "token": "stale-token", "version": "0.0.0"}}"#,
                socket_path.display()
            ),
        )
        .unwrap();
        std::fs::write(&socket_path, b"garbage").unwrap(); // bind must survive this

        let serve_handle = spawn_serve(socket_path.clone(), info_path.clone());

        // If serve mistook the dead pid for a live daemon it would have
        // exited with "daemon already running" instead of rewriting the
        // file with its own pid.
        let deadline = Instant::now() + POLL_TIMEOUT;
        loop {
            assert!(
                !serve_handle.is_finished(),
                "serve exited early: {:?}",
                serve_handle.join()
            );
            let overwritten = std::fs::read_to_string(&info_path)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .and_then(|value| {
                    value["pid"]
                        .as_u64()
                        .map(|pid| pid == u64::from(std::process::id()))
                })
                .unwrap_or(false);
            if overwritten {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "stale info file was not rewritten within 10s"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        // The garbage socket file was replaced by a real listener.
        let mut client = TestConn::connect(&socket_path, deadline);
        client.send(&shutdown_frame("down"));
        drop(client);
        wait_serve_exit(serve_handle);
        assert!(!socket_path.exists());
        assert!(!info_path.exists());
    }
}
