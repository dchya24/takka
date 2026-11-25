//! Transport seam (H-07): the only platform-specific code in the broker.

#[cfg(unix)]
mod unix {
    use super::super::*;
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
        file.write_all(&super::super::info_json(config, token))?;
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

#[cfg(unix)]
pub use unix::*;

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
mod windows {
    use super::super::*;
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
    /// normalized path (see [`super::super::pipe_id`]); a value that already
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
        file.write_all(&super::super::info_json(config, token))?;
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

#[cfg(windows)]
pub use windows::*;
