//! Local-socket daemon broker (Fase 3a).
//!
//! The daemon is a *broker*, not a new engine: one `RuntimeEngine`
//! talks over a `MemoryTransport`; the broker pumps wire frames between
//! the connection(s) and that engine's channels. Rules:
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
//!
//! # Transport seam (H-07)
//!
//! Everything above the listener is transport-agnostic; the platform
//! surface is exactly four items, each with a `unix` and a `windows`
//! implementation in the `platform` module:
//!
//! - `LocalListener` — `UnixListener` / named-pipe server instances,
//! - `LocalStream` — `UnixStream` / an accepted named-pipe handle,
//! - `open_connection` — client-side connect for the broker's own
//!   transport tests,
//! - `write_info_file` — the discovery file (0600 mode / user-only ACL).
//!
//! Windows uses real named pipes (`\\.\pipe\<name>`), never a TCP
//! socket: a loopback TCP port is reachable by every local user and by
//! any process that can bind it, which would widen the local attack
//! surface the shared-secret handshake exists to close.

#[cfg(any(test, windows))]
pub(crate) mod pipe_id;
#[cfg(any(unix, windows))]
mod platform;
#[cfg(all(test, any(unix, windows)))]
mod tests;

/// Single clearinghouse message for platforms where daemon mode has no
/// transport. Windows named pipes are implemented (H-07), so this is
/// now reachable only on hosts that are neither unix nor windows; kept
/// platform-unconditional so its contract test compiles and runs on
/// every CI host.
#[cfg_attr(any(unix, windows), allow(dead_code))]
pub fn not_supported_error() -> &'static str {
    "daemon mode not supported on this platform — use --runtime embedded|spawn"
}

// ---------------------------------------------------------------------
// Broker (unix + windows; the message helper above stays portable so
// its guard branch in `main` and its test compile everywhere).
// ---------------------------------------------------------------------

#[cfg(any(unix, windows))]
use std::io::{self, BufRead, BufReader, BufWriter, Write};
#[cfg(any(unix, windows))]
use std::path::{Path, PathBuf};
#[cfg(any(unix, windows))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(any(unix, windows))]
use std::sync::mpsc;
#[cfg(any(unix, windows))]
use std::sync::{Arc, Mutex};
#[cfg(any(unix, windows))]
use std::time::{Duration, Instant};
#[cfg(any(unix, windows))]
use takka_core::events::Event;
#[cfg(any(unix, windows))]
use takka_core::runtime::engine::RuntimeEngine;
#[cfg(any(unix, windows))]
use takka_core::runtime::protocol::{ProtocolEvent, ProtocolRequest};
#[cfg(any(unix, windows))]
use takka_core::runtime::transport::MemoryTransport;
use takka_core::sync;

#[cfg(any(unix, windows))]
use platform::LocalStream;
#[cfg(any(unix, windows))]
use platform::{make_blocking, pid_is_alive, write_info_file, LocalListener};

/// Daemon endpoints, both explicit (the CLI resolves defaults before
/// calling [`serve`]): the local socket to listen on — a unix socket
/// path on unix, a pipe name or `\\.\pipe\<name>` path on Windows —
/// and the discovery info file to publish.
#[cfg(any(unix, windows))]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub info_path: PathBuf,
}

#[cfg(any(unix, windows))]
const DAEMON_IN_USE_MESSAGE: &str = "daemon in use";

/// Accept poll interval for the nonblocking listener and the shutdown
/// flag — bounds shutdown latency without burning CPU.
#[cfg(any(unix, windows))]
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Bound on joining reader/drain/engine threads after shutdown.
/// Readers blocked on an idle-but-connected socket are left detached;
/// the process exits right after `serve` returns.
#[cfg(any(unix, windows))]
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// The writer half of the active connection, tagged with a generation
/// counter: reader threads may outlive their connection's slot (EOF
/// detection races with the next attach), so they only clear the slot
/// when the generation still matches their own.
#[cfg(any(unix, windows))]
type WriterSlot = Arc<Mutex<Option<(u64, BufWriter<LocalStream>)>>>;

/// Serve the daemon until the engine reports shutdown.
/// Blocking; expected to run on `main`'s thread (tests spawn it).
#[cfg(any(unix, windows))]
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
    let listener = LocalListener::bind(&config.socket_path)?;
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
                    let mut guard = sync::lock(&slot);
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
            Ok(stream) => {
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
        // Windows: a second, unconnected server instance is armed before
        // the accepted one is served, so the next client always finds a
        // pipe to connect to (unix: no-op).
        listener.rearm();
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

/// Discovery-file body shared by both platforms (the on-disk schema is a
/// cross-crate contract pinned by `takka-cli`'s `daemon_info_tests`).
#[cfg(any(unix, windows))]
fn info_json(config: &DaemonConfig, token: &str) -> Vec<u8> {
    let info = serde_json::json!({
        "pid": std::process::id(),
        "socket_path": config.socket_path.to_string_lossy(),
        "token": token,
        "version": daemon_version(),
    });
    let mut bytes = serde_json::to_vec_pretty(&info).expect("info json");
    bytes.push(b'\n');
    bytes
}

/// First rule of `handle_connection`: ONE active connection. A second
/// attach gets a single `"daemon in use"` frame and a close — the
/// active connection is never touched. Otherwise installs the writer
/// slot and spawns the request pump; returns its join handle.
#[cfg(any(unix, windows))]
fn handle_connection(
    stream: LocalStream,
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

    if sync::lock(slot).is_some() {
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
    *sync::lock(slot) = Some((generation, writer));

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
#[cfg(any(unix, windows))]
fn pump_requests(
    stream: LocalStream,
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
                    let mut guard = sync::lock(&slot);
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
    let mut guard = sync::lock(&slot);
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
#[cfg(any(unix, windows))]
fn should_forward(event: &ProtocolEvent) -> bool {
    !matches!(event.event, Event::Ready { .. })
}

/// One JSON event frame per line (same framing as `StdioTransport`).
#[cfg(any(unix, windows))]
fn write_frame(writer: &mut BufWriter<LocalStream>, event: &ProtocolEvent) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, event)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// 32 hex chars (16 bytes) from the OS CSPRNG (`/dev/urandom`, or
/// `ProcessPrng` via getrandom on Windows). This is the shared secret of the auth
/// handshake, so it must never fall back to a userspace PRNG.
#[cfg(any(unix, windows))]
fn generate_token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    fill_random(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(unix)]
fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(bytes)
}

// getrandom reaches `ProcessPrng` without a hand-rolled `#[link]`, which has no
// import library and fails to link (`LNK1181: bcryptprimitives.lib`).
#[cfg(windows)]
fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
    getrandom::fill(bytes).map_err(|err| io::Error::other(err.to_string()))
}

/// Version reported in the info file. `takka_core` exposes no version
/// constant, so the broker reports the runtime binary's own version.
#[cfg(any(unix, windows))]
fn daemon_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// pid from a (possibly corrupt/absent) info file; `None` = no usable
/// pid, i.e. nothing to guard against.
#[cfg(any(unix, windows))]
fn live_info_pid(info_path: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(info_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    let pid = value["pid"].as_u64()?;
    u32::try_from(pid).ok()
}

/// Join with a deadline; stragglers are left detached (documented at
/// [`SHUTDOWN_JOIN_TIMEOUT`]).
#[cfg(any(unix, windows))]
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
