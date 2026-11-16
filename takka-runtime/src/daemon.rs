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

// ---------------------------------------------------------------------
// Named-pipe naming (H-07 follow-up): portable so it is unit-tested on
// every host, Windows-only in effect.
// ---------------------------------------------------------------------

/// Derivation of a Windows pipe name from a configured endpoint.
///
/// The previous rule reduced the endpoint to its **file stem**
/// (`…/takka-da-123-456/test.sock` → `\\.\pipe\takka-test`). Every
/// concurrently running daemon smoke in one test binary therefore chose
/// the same pipe name, and `FILE_FLAG_FIRST_PIPE_INSTANCE` turned the
/// losers into `ERROR_ACCESS_DENIED` (os error 5) at bind time — which
/// the client then swallowed into "no daemon running". The name is now
/// derived from a hash of the *whole normalized* endpoint, so distinct
/// endpoints cannot collide and the same endpoint always yields the same
/// name on both sides.
///
/// The derivation is duplicated in `takka-cli`'s `pipe_id` module (the
/// runtime deliberately has no dependency on the CLI): the two copies
/// must stay identical — `pipe_name_matches_server_derivation` on the
/// CLI side pins the same vectors as
/// `pipe_name_hashing_is_unique_and_stable` here.
///
/// `DefaultHasher` is explicitly NOT used: it is not stable across
/// processes (`RandomState` is randomly seeded), so server and client
/// would disagree. FNV-1a over the normalized UTF-8 bytes is stable,
/// dependency-free and deterministic.
#[cfg(any(test, windows))]
pub(crate) mod pipe_id {
    /// Root every local named pipe lives under.
    pub(crate) const PIPE_PREFIX: &str = r"\\.\pipe\";

    /// Positional 64-bit FNV-1a (offset basis 0xcbf29ce484222325).
    pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    /// Normalize an endpoint into the string the hash is taken over.
    ///
    /// Textual only: `std::fs::canonicalize` would fail for a socket
    /// path that does not exist yet (the broker mints its endpoint
    /// *before* binding) and resolves 8.3 short names differently
    /// depending on the working directory. Both sides see the same
    /// string (the daemon records `config.socket_path` verbatim in the
    /// discovery file), so stripping trailing separators, folding
    /// `/` to `\` and lowercasing (NTFS is case-insensitive; pipe names
    /// are compared case-insensitively by the object manager) is enough
    /// to make the mapping stable.
    pub(crate) fn normalize(endpoint: &str) -> String {
        let mut text = endpoint.replace('/', r"\");
        while text.len() > 1 && text.ends_with('\\') {
            text.pop();
        }
        // ASCII-only lowercasing: a non-ASCII path component must hash
        // identically on both sides, and `to_lowercase`'s Unicode rules
        // (e.g. Turkish dotless i) would make that posture-dependent.
        text.make_ascii_lowercase();
        text
    }

    /// The pipe name (without the `\\.\pipe\` root) for an endpoint:
    /// `takka-<hex16>`, where `hex16` is the FNV-1a of the normalized
    /// endpoint. A caller that already passes a fully qualified
    /// `\\.\pipe\…` path keeps it verbatim (tests pin names that way).
    pub(crate) fn pipe_name_for(endpoint: &str) -> String {
        if endpoint.starts_with(PIPE_PREFIX) {
            return endpoint.to_string();
        }
        format!(
            "{PIPE_PREFIX}takka-{:016x}",
            fnv1a64(normalize(endpoint).as_bytes())
        )
    }
}

// ---------------------------------------------------------------------
// Transport seam (H-07): the only platform-specific code in the broker.
// ---------------------------------------------------------------------

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    /// Listening end of the local transport.
    pub struct LocalListener {
        listener: UnixListener,
    }

    impl LocalListener {
        pub fn bind(path: &Path) -> io::Result<Self> {
            Ok(Self {
                listener: UnixListener::bind(path)?,
            })
        }

        /// Armed BEFORE the discovery file is published: a client that
        /// sees the info file must never find a listener that is not
        /// accepting yet.
        pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
            self.listener.set_nonblocking(nonblocking)
        }

        pub fn accept(&self) -> io::Result<LocalStream> {
            let (stream, _addr) = self.listener.accept()?;
            Ok(stream)
        }

        /// No pre-instancing on unix: every connection finds the listener.
        pub fn rearm(&self) {}
    }

    /// Accepted connection end of the local transport.
    pub type LocalStream = UnixStream;

    /// Client-side connect, used by the broker's own transport tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open_connection(path: &Path) -> io::Result<LocalStream> {
        UnixStream::connect(path)
    }

    /// Accepted connections are explicitly made BLOCKING.
    ///
    /// `accept()` on a non-blocking listener returns a socket that
    /// INHERITS `O_NONBLOCK` on macOS/BSD, while Linux clears it. With
    /// the flag set, the reader gets `WouldBlock` instead of waiting for
    /// the client's first frame, treats a healthy client as a detach,
    /// and closes the connection while the daemon keeps running — the
    /// macOS CI signature of issue #2 (client sees EOF, daemon still
    /// alive, the engine never saw `Init`). Never rely on platform
    /// `accept` semantics.
    pub fn make_blocking(stream: LocalStream) -> io::Result<LocalStream> {
        stream.set_nonblocking(false)?;
        Ok(stream)
    }

    /// `{"pid":u32,"socket_path":String,"token":String,"version":String}`,
    /// permissions 0600 (mode applies at create; set explicitly too so a
    /// pre-existing file can never keep wider perms).
    pub fn write_info_file(config: &DaemonConfig, token: &str) -> io::Result<()> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&config.info_path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(&super::info_json(config, token))?;
        Ok(())
    }

    /// Best-effort liveness check: `/proc` on Linux (precise for reaped
    /// pids), `kill -0` elsewhere. PID reuse can false-positive — the
    /// token handshake, not this guard, is the real boundary.
    pub fn pid_is_alive(pid: u32) -> bool {
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
}

/// Windows end of the transport seam: real named pipes, NEVER TCP.
///
/// A loopback TCP listener would be reachable by every local user and by
/// any process that can bind the port, widening the local attack surface
/// the shared-secret handshake exists to close — hence the pipe.
///
/// The blocking `CreateNamedPipeW` / `ConnectNamedPipe` / `CreateFileW`
/// API set is reached through local `extern "system"` declarations, so
/// the runtime binary keeps its dependency set (no `windows-sys`).
/// The pipe gets a NULL security descriptor, which makes Windows derive
/// it from the creator's token — the same user-only protection the unix
/// socket file gets from its directory and the info file from mode 0600.
/// The token handshake is the second layer and is identical to unix.
///
/// Instances are created with `FILE_FLAG_OVERLAPPED` so
/// `ConnectNamedPipe` can be polled instead of blocking the accept loop;
/// reads and writes stay logically blocking by waiting on the same
/// overlapped operation immediately.
///
/// Deliberately minimal: this module is compile-checked by
/// `cargo check --target x86_64-pc-windows-gnu` and hosts the same
/// transport tests as unix. It is exercised by the Windows CI leg.
#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::OsStr;
    use std::io::{Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    /// Create a duplex pipe with blocking (synchronous) semantics.
    const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    /// Byte stream, byte read mode, no overlapped flag (zero constants).
    const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
    const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
    const PIPE_WAIT: u32 = 0x0000_0000;
    const PIPE_UNLIMITED_INSTANCES: u32 = 255;
    /// Refuse to attach to an existing pipe of the same name.
    const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
    /// Handle-level async flag. Only `ConnectNamedPipe` is issued
    /// asynchronously (so `accept()` can poll); the accepted instance
    /// then does blocking `ReadFile` / `WriteFile` through an
    /// `OVERLAPPED` with an immediate wait, which Win32 defines as
    /// "wait for this operation to finish".
    const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
    /// `ConnectNamedPipe` on an overlapped handle: no client yet.
    const ERROR_IO_PENDING: i32 = 997;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 258;
    const INVALID_HANDLE_VALUE: isize = -1;
    /// `ConnectNamedPipe` reports a client that connected first this way.
    const ERROR_PIPE_CONNECTED: i32 = 535;
    /// "All pipe instances are busy" — `WaitNamedPipeW` failed to find one.
    const ERROR_PIPE_BUSY: i32 = 231;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const OPEN_EXISTING: u32 = 3;
    /// Upper bound on waiting for a free instance (well under the
    /// broker's spawn budget).
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
    const CONNECT_RETRY_SLEEP: Duration = Duration::from_millis(20);

    /// `OVERLAPPED` as Win32 lays it out on 64-bit hosts:
    /// `ULONG_PTR Internal; ULONG_PTR InternalHigh;` then the
    /// offset/offset-high union, then `HANDLE hEvent`. The extra
    /// status block in some declarations belongs to the 32-bit variant,
    /// so there is nothing after `hEvent` here.
    #[repr(C)]
    struct OVERLAPPED {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut core::ffi::c_void,
    }

    /// Owns the `OVERLAPPED`'s event handle: one event per operation, so
    /// it must be closed or a long-lived daemon leaks a handle per
    /// connection.
    struct OverlappedOp {
        overlapped: OVERLAPPED,
    }

    impl OverlappedOp {
        /// A fresh overlapped op with its own manual-reset event, so a
        /// pending connect (or a read) can be waited on.
        fn new() -> io::Result<Self> {
            // SAFETY: NULL attributes/name, manual reset, initially
            // non-signaled; returns an owned handle or NULL.
            let event = unsafe { CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null()) };
            if event.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                overlapped: OVERLAPPED {
                    internal: 0,
                    internal_high: 0,
                    offset: 0,
                    offset_high: 0,
                    event,
                },
            })
        }

        fn as_mut_ptr(&mut self) -> *mut core::ffi::c_void {
            std::ptr::from_mut(&mut self.overlapped).cast()
        }

        fn as_ptr(&self) -> *const OVERLAPPED {
            std::ptr::from_ref(&self.overlapped)
        }

        fn event(&self) -> *mut core::ffi::c_void {
            self.overlapped.event
        }
    }

    impl Drop for OverlappedOp {
        fn drop(&mut self) {
            // SAFETY: `event` came from CreateEventW and is closed once.
            unsafe { CloseHandle(self.overlapped.event) };
        }
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buffer_size: u32,
            in_buffer_size: u32,
            default_timeout: u32,
            security_attributes: *mut core::ffi::c_void,
        ) -> *mut core::ffi::c_void;
        fn ConnectNamedPipe(
            pipe: *mut core::ffi::c_void,
            overlapped: *mut core::ffi::c_void,
        ) -> i32;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security_attributes: *mut core::ffi::c_void,
            creation: u32,
            flags: u32,
            template: *mut core::ffi::c_void,
        ) -> *mut core::ffi::c_void;
        fn CreateEventW(
            attributes: *mut core::ffi::c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> *mut core::ffi::c_void;
        fn WaitForSingleObject(handle: *mut core::ffi::c_void, milliseconds: u32) -> u32;
        fn GetOverlappedResult(
            file: *mut core::ffi::c_void,
            overlapped: *const OVERLAPPED,
            transferred: *mut u32,
            wait: i32,
        ) -> i32;
        fn WaitNamedPipeW(name: *const u16, timeout: u32) -> i32;
        fn ReadFile(
            file: *mut core::ffi::c_void,
            buffer: *mut u8,
            to_read: u32,
            read: *mut u32,
            overlapped: *mut core::ffi::c_void,
        ) -> i32;
        fn WriteFile(
            file: *mut core::ffi::c_void,
            buffer: *const u8,
            to_write: u32,
            written: *mut u32,
            overlapped: *mut core::ffi::c_void,
        ) -> i32;
        fn GetLastError() -> u32;
        fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
    }

    /// `…/x/test.sock` → `\\.\pipe\takka-<hex16>`, a hash of the whole
    /// normalized path (see [`super::pipe_id`]); a value that already
    /// looks like a pipe path is used verbatim, so a caller (or a test)
    /// can pin a fully qualified name.
    pub fn pipe_name(path: &Path) -> Vec<u16> {
        let text = path.as_os_str().to_string_lossy();
        let name = pipe_id::pipe_name_for(&text);
        OsStr::new(&name).encode_wide().chain([0]).collect()
    }

    /// Wrap a raw handle from `CreateNamedPipeW` / `CreateFileW`,
    /// rejecting INVALID_HANDLE_VALUE (ownership stays with the OS).
    fn adopt(raw: *mut core::ffi::c_void) -> io::Result<OwnedHandle> {
        if raw as isize == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a valid, exclusively owned handle as checked.
        Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
    }

    fn create_instance(name: &[u16], first: bool) -> io::Result<OwnedHandle> {
        // `FILE_FLAG_OVERLAPPED` is what makes `ConnectNamedPipe` not
        // block: without it `accept()` parked inside the call and the
        // broker's shutdown flag was never re-read, so a Windows daemon
        // could not be stopped once it went idle (the four smoke tests
        // that hung past 60s in run 43797687559).
        let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        // SAFETY: NULL security descriptor → the ACL is derived from the
        // creating process's token (user-only, like mode 0600); the name
        // is a NUL-terminated UTF-16 buffer that outlives the call.
        let raw = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                std::ptr::null_mut(),
            )
        };
        adopt(raw)
    }

    /// Listening end: the broker keeps ONE extra unconnected instance
    /// armed at all times. A named pipe whose every instance has served a
    /// client is *closed* to new clients ("file not found") until another
    /// instance is created, so a refused or churning client would
    /// otherwise see connect errors instead of the broker's
    /// `"daemon in use"` frame.
    pub struct LocalListener {
        name: Vec<u16>,
        next: std::sync::Mutex<Armed>,
    }

    /// The instance waiting for the next client, plus its pending
    /// overlapped connect (if one has already been issued).
    struct Armed {
        handle: OwnedHandle,
        /// Kept across polls: re-issuing `ConnectNamedPipe` on an
        /// instance whose connect is already pending is an error.
        pending: Option<OverlappedOp>,
    }

    impl LocalListener {
        pub fn bind(path: &Path) -> io::Result<Self> {
            let name = pipe_name(path);
            let handle = create_instance(&name, true)?;
            Ok(Self {
                name,
                next: std::sync::Mutex::new(Armed {
                    handle,
                    pending: None,
                }),
            })
        }

        /// Nothing to arm: `accept()` is already non-blocking (the
        /// armed instance's `ConnectNamedPipe` is issued overlapped and
        /// polled), which is what the accept loop's shutdown check needs.
        pub fn set_nonblocking(&self, _nonblocking: bool) -> io::Result<()> {
            Ok(())
        }

        /// Accept the next client, never blocking: the armed instance's
        /// `ConnectNamedPipe` runs overlapped, so a client that has not
        /// connected yet yields `WouldBlock` and the accept loop keeps
        /// polling its shutdown flag (the unix listener's contract).
        ///
        /// The replacement instance is armed BEFORE the accepted one is
        /// handed out, so a connecting client always finds a server.
        pub fn accept(&self) -> io::Result<LocalStream> {
            let mut guard = sync::lock(&self.next);
            if !connect_pending(&mut guard)? {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let replacement = create_instance(&self.name, false)?;
            let current = std::mem::replace(
                &mut *guard,
                Armed {
                    handle: replacement,
                    pending: None,
                },
            );
            drop(guard);
            Ok(LocalStream {
                handle: current.handle,
            })
        }

        /// Between accept polls the replacement instance is already in
        /// place (see [`Self::accept`]).
        pub fn rearm(&self) {}
    }

    /// Advance the armed instance's connect without blocking.
    ///
    /// Returns `Ok(true)` once a client is connected, `Ok(false)` while
    /// none has arrived (the caller reports `WouldBlock`), and `Err` for
    /// a real failure. The pending `OVERLAPPED` is kept on the instance
    /// between polls — issuing `ConnectNamedPipe` twice on the same
    /// instance is an error.
    fn connect_pending(armed: &mut Armed) -> io::Result<bool> {
        if armed.pending.is_none() {
            // First poll for this instance: issue the overlapped connect
            // and keep the op, so later polls only inspect its event.
            let mut op = OverlappedOp::new()?;
            // SAFETY: overlapped `ConnectNamedPipe` on a handle this
            // struct exclusively owns; the op is moved onto the instance
            // and lives until the operation completes.
            let connected =
                unsafe { ConnectNamedPipe(armed.handle.as_raw_handle(), op.as_mut_ptr()) };
            if connected != 0 {
                return Ok(true);
            }
            // SAFETY: GetLastError reads this thread's last-error slot.
            let code = unsafe { GetLastError() } as i32;
            if code == ERROR_PIPE_CONNECTED {
                // A client connected before the call; Windows reports
                // that as an error even though it succeeded.
                return Ok(true);
            }
            if code != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(code));
            }
            armed.pending = Some(op);
            return Ok(false);
        }

        let op = armed.pending.as_ref().expect("checked above");
        // SAFETY: `op` is the stored, still-pending connect on this
        // instance; a zero timeout only inspects its event.
        match unsafe { WaitForSingleObject(op.event(), 0) } {
            WAIT_TIMEOUT => return Ok(false),
            WAIT_OBJECT_0 => {}
            _ => return Err(io::Error::last_os_error()),
        }
        let mut transferred: u32 = 0;
        // SAFETY: the operation is signalled, so `wait = 1` returns at
        // once with the byte count (0 for a connect).
        let ok = unsafe {
            GetOverlappedResult(
                armed.handle.as_raw_handle(),
                op.as_ptr(),
                &mut transferred,
                1,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        armed.pending = None;
        Ok(true)
    }

    /// Accepted connection end of the local transport: a byte-mode duplex
    /// pipe handle, a plain `Read + Write` source.
    pub struct LocalStream {
        handle: OwnedHandle,
    }

    impl LocalStream {
        /// Second handle onto the same pipe instance, so the broker can
        /// hold a reader half and a writer half at once (mirrors
        /// `UnixStream::try_clone`).
        pub fn try_clone(&self) -> io::Result<Self> {
            Ok(Self {
                handle: self.handle.try_clone()?,
            })
        }

        /// Accepted named pipes are blocking and have no synchronous
        /// read timeout (`SO_RCVTIMEO` equivalent), so the broker's
        /// bounded shutdown relies on the `SHUTDOWN_JOIN_TIMEOUT`
        /// detach rule. Provided so the transport tests share one
        /// client shape with unix.
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn set_read_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
    }

    /// Run one overlapped pipe operation to completion.
    ///
    /// Instances carry `FILE_FLAG_OVERLAPPED` (needed so `accept()` can
    /// poll), and Win32 requires an `OVERLAPPED` for every `ReadFile` /
    /// `WriteFile` on such a handle. Passing one with an immediate
    /// `GetOverlappedResult(wait = 1)` restores exactly the blocking
    /// semantics the synchronous handles had.
    fn run_overlapped(
        handle: *mut core::ffi::c_void,
        issue: impl FnOnce(*mut core::ffi::c_void) -> i32,
    ) -> io::Result<usize> {
        let mut op = OverlappedOp::new()?;
        // SAFETY: `op` lives for the whole operation.
        let issued = issue(op.as_mut_ptr());
        if issued == 0 {
            // SAFETY: GetLastError reads this thread's last-error slot.
            let code = unsafe { GetLastError() } as i32;
            if code != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(code));
            }
        }
        let mut done: u32 = 0;
        // SAFETY: `wait = 1` blocks until the operation completes and
        // fills in the transferred byte count.
        let finished = unsafe { GetOverlappedResult(handle, op.as_ptr(), &mut done, 1) };
        if finished == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(done as usize)
    }

    impl Read for LocalStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let len = u32::try_from(buf.len().min(u32::MAX as usize))
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            let handle = self.handle.as_raw_handle();
            // SAFETY: `buf` is valid for `len` writable bytes until the
            // operation completes synchronously inside `run_overlapped`.
            run_overlapped(handle, |overlapped| unsafe {
                ReadFile(
                    handle,
                    buf.as_mut_ptr(),
                    len,
                    std::ptr::null_mut(),
                    overlapped,
                )
            })
        }
    }

    impl Write for LocalStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let len = u32::try_from(buf.len().min(u32::MAX as usize))
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            let handle = self.handle.as_raw_handle();
            // SAFETY: `buf` is valid for `len` readable bytes until the
            // operation completes synchronously inside `run_overlapped`.
            run_overlapped(handle, |overlapped| unsafe {
                WriteFile(handle, buf.as_ptr(), len, std::ptr::null_mut(), overlapped)
            })
        }

        /// Byte-mode pipes are unbuffered: every `write` reaches the
        /// pipe, so there is nothing to flush.
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Pipe handles are blocking by construction.
    pub fn make_blocking(stream: LocalStream) -> io::Result<LocalStream> {
        Ok(stream)
    }

    /// Client-side connect (mirrors `UnixStream::connect`): wait for a
    /// free instance, then open it with GENERIC_READ|GENERIC_WRITE.
    ///
    /// A pipe that does not exist at all fails immediately (the caller's
    /// stale-discovery-file handling depends on that); only
    /// `ERROR_PIPE_BUSY` — every instance already serving — is retried.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open_connection(path: &Path) -> io::Result<LocalStream> {
        let name = pipe_name(path);
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            // SAFETY: NUL-terminated UTF-16 name, no security attributes,
            // no template; OPEN_EXISTING on a server pipe returns the
            // client end or INVALID_HANDLE_VALUE.
            let raw = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            match adopt(raw) {
                Ok(handle) => return Ok(LocalStream { handle }),
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    if Instant::now() >= deadline {
                        return Err(error);
                    }
                    // SAFETY: WaitNamedPipeW only touches the name buffer.
                    unsafe { WaitNamedPipeW(name.as_ptr(), 20) };
                    std::thread::sleep(CONNECT_RETRY_SLEEP);
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Discovery file. Windows has no mode bits: the ACL is inherited
    /// from the parent directory (`%USERPROFILE%\.config\takka` or the
    /// user's temp dir), which is user-only by default.
    pub fn write_info_file(config: &DaemonConfig, token: &str) -> io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&config.info_path)?;
        file.write_all(&super::info_json(config, token))?;
        Ok(())
    }

    /// Best-effort liveness check with the tool Windows always has.
    /// PID reuse can false-positive — the token handshake, not this
    /// guard, is the real boundary.
    pub fn pid_is_alive(pid: u32) -> bool {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|text| text.contains(&format!(",\"{pid}\",")))
            .unwrap_or(false)
    }
}

#[cfg(any(unix, windows))]
use platform::LocalStream;
#[cfg(any(unix, windows))]
use platform::{make_blocking, pid_is_alive, write_info_file, LocalListener};

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

/// Broker-level transport tests. They drive the real `serve()` over the
/// same seam the broker uses (`platform::open_connection`), so unix and
/// Windows run the identical assertions — the named-pipe path gets the
/// same coverage as the unix socket, not a weaker stand-in.
#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::platform::open_connection;
    use super::*;
    use std::io::{BufRead, BufReader, BufWriter, Write};
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
    ///
    /// The name is deliberately SHORT: `temp_dir()` on macOS runners is
    /// already ~48 chars (`/var/folders/h5/…/T/`), and a unix socket path
    /// must stay under `SUN_LEN` (104). A descriptive name plus a
    /// `{tag}.sock` sibling blew that limit, so every daemon test in this
    /// module failed with "path must be shorter than SUN_LEN" (the same
    /// trap the CLI fixture hit; its fix never reached here).
    fn tempdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Two tag characters keep parallel instances readable without
        // spending the path budget.
        let short: String = tag
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(2)
            .collect();
        let dir = std::env::temp_dir().join(format!(
            "td{}{}{}",
            std::process::id() % 1000,
            nanos % 1_000_000,
            short
        ));
        assert!(
            dir.join("s.sock").as_os_str().len() < 100,
            "socket path budget exceeded: {:?}",
            dir.join("s.sock")
        );
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

    /// Minimal test client: raw framed JSON over the broker's transport
    /// (the public client transports have no event-reading API from this
    /// crate — this is the same one-JSON-per-line framing).
    struct TestConn {
        reader: BufReader<LocalStream>,
        writer: BufWriter<LocalStream>,
    }

    impl TestConn {
        fn connect(transport_path: &Path, deadline: Instant) -> Self {
            loop {
                match open_connection(transport_path) {
                    Ok(stream) => {
                        // Set the read bound ONCE: every wait in this
                        // helper is covered by it, and repeating
                        // SO_RCVTIMEO mid-test is a platform quirk magnet
                        // (macOS returned EINVAL for an in-test value).
                        // Named pipes have no synchronous read timeout;
                        // there `set_read_timeout` is a no-op and the
                        // bounded assertions carry the guarantee.
                        stream
                            .set_read_timeout(Some(POLL_TIMEOUT))
                            .expect("set read timeout at connect");
                        let reader = BufReader::new(stream.try_clone().expect("clone reader half"));
                        return Self {
                            reader,
                            writer: BufWriter::new(stream),
                        };
                    }
                    Err(_) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("connect to {transport_path:?} failed: {error}"),
                }
            }
        }

        fn send(&mut self, frame: &serde_json::Value) {
            serde_json::to_writer(&mut self.writer, frame).expect("encode frame");
            self.writer.write_all(b"\n").expect("write frame newline");
            self.writer.flush().expect("flush frame");
        }

        fn next_event(&mut self, deadline: Instant) -> ProtocolEvent {
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
                        panic!("timed out waiting for an event frame (deadline {deadline:?})")
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
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => {}
                Ok(_) => panic!("expected EOF, got another frame: {line}"),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    panic!("timed out waiting for EOF (deadline {deadline:?})")
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
        let socket_path = dir.join("s.sock");
        let info_path = dir.join("daemon.json");
        let handle = spawn_serve(socket_path.clone(), info_path.clone());
        wait_for_info_file(&info_path);
        let token = info_value(&info_path)["token"]
            .as_str()
            .expect("info file carries the token")
            .to_string();
        (socket_path, PathBuf::from(config_path), token, handle)
    }

    /// The transport seam under the broker, asserted for real: a client
    /// connects, the listener accepts, and bytes flow both ways. This is
    /// the test that replaces the old "unsupported platform" guard — on
    /// every host it exercises the actual transport `serve` uses.
    #[test]
    fn transport_round_trips_bytes_both_ways() {
        use std::io::Read;
        let dir = tempdir("seam");
        let path = dir.join("s.sock");
        let listener = LocalListener::bind(&path).expect("bind");
        listener.set_nonblocking(true).expect("arm the listener");

        let mut client = open_connection(&path).expect("connect");
        let accepted = loop {
            match listener.accept() {
                Ok(stream) => break stream,
                Err(ref error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline(), "accept did not complete");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        };
        listener.rearm();
        let mut server = make_blocking(accepted).expect("served connection");

        client.write_all(b"ping\n").expect("client write");
        client.flush().expect("client flush");
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while !line.ends_with(b"\n") {
            let read = server.read(&mut byte).expect("server read");
            assert_eq!(read, 1, "server read returned 0 before the frame");
            line.push(byte[0]);
        }
        assert_eq!(line, b"ping\n");

        server.write_all(b"pong\n").expect("server write");
        server.flush().expect("server flush");
        let mut reply = Vec::new();
        while !reply.ends_with(b"\n") {
            let read = client.read(&mut byte).expect("client read");
            assert_eq!(read, 1, "client read returned 0 before the frame");
            reply.push(byte[0]);
        }
        assert_eq!(reply, b"pong\n");
    }

    /// The pipe-name derivation must be (a) collision-free across the
    /// endpoints the smokes actually generate and (b) byte-identical to
    /// the CLI's copy of the rule
    /// (`takka-cli/src/client.rs` `mod transport`). These vectors are
    /// duplicated verbatim there — change one, change both.
    #[test]
    fn pipe_name_hashing_is_unique_and_stable() {
        use super::pipe_id::{fnv1a64, normalize, pipe_name_for};

        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(normalize("/tmp/x.SOCK/"), r"\tmp\x.sock");

        // Two fixtures that differ ONLY in their unique dir (the exact
        // shape the smokes produce) must not share a pipe name.
        let a = pipe_name_for("/tmp/takka-da-123-456/ep-456.sock");
        let b = pipe_name_for("/tmp/takka-da-123-457/ep-457.sock");
        assert_ne!(a, b, "distinct endpoints must not collide");

        // Same endpoint, different spelling (case, separator style,
        // trailing slash): one pipe name, so client and server always
        // agree.
        assert_eq!(
            pipe_name_for(r"C:\Temp\takka-da-1-2\ep-3.sock"),
            pipe_name_for(r"c:/temp/TAKKA-DA-1-2/ep-3.sock\"),
        );

        // Pinned vectors: the CLI copy asserts the same strings.
        assert_eq!(
            pipe_name_for("/tmp/takka-da-123-456/ep-456.sock"),
            r"\\.\pipe\takka-0013c643e0315b28"
        );
        assert_eq!(
            pipe_name_for(r"C:\Temp\takka-da-1-2\ep-3.sock"),
            r"\\.\pipe\takka-2dd13866d5f634ea"
        );

        // A fully qualified name is pinned verbatim (tests and callers
        // can choose their own pipe).
        assert_eq!(
            pipe_name_for(r"\\.\pipe\takka-pinned"),
            r"\\.\pipe\takka-pinned"
        );
    }

    /// The broker's client half refuses a path nobody listens on, so the
    /// CLI's "stale discovery file" error stays reachable on both
    /// transports.
    #[test]
    fn connecting_to_a_dead_endpoint_fails() {
        let dir = tempdir("dead");
        let path = dir.join("nobody-home.sock");
        assert!(
            open_connection(&path).is_err(),
            "connect to a non-existent endpoint must fail"
        );
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
    ///
    /// Unix-only in practice: it asserts the *inherited* flag the BSD
    /// `accept` returns. Named-pipe handles are synchronous by
    /// construction (there is no non-blocking flag to inherit), so on
    /// Windows the seam promise is checked by
    /// [`served_connection_waits_for_a_silent_client`] instead.
    #[cfg(unix)]
    #[test]
    fn accepted_connections_are_forced_blocking() {
        use std::io::Read;
        let dir = tempdir("accept-mode");
        let path = dir.join("s.sock");
        let listener = LocalListener::bind(&path).expect("bind");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");

        let client = open_connection(&path).expect("connect");
        let deadline = deadline();
        let accepted = loop {
            match listener.accept() {
                Ok(stream) => break stream,
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
