//! Daemon transport seam (H-07): the only platform-specific client code.
//!
//! Unix uses `tokio::net::UnixStream`; Windows opens a real named pipe
//! with `CreateFileW` (`SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`)
//! and wraps it in `tokio::fs::File`, using `takka_core::runtime::pipe_id`
//! for endpoint-to-pipe-name derivation.

use anyhow::Result;

/// One connected duplex endpoint: a unix socket or a named-pipe handle.
/// A supertrait keeps both halves behind a single trait object (Rust
/// allows only one non-auto trait per `dyn`).
pub(crate) trait DuplexStream:
    tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send
{
}
#[cfg(unix)]
impl DuplexStream for tokio::net::UnixStream {}
#[cfg(windows)]
impl DuplexStream for tokio::fs::File {}

pub(crate) type Duplex = Box<dyn DuplexStream>;

#[cfg(unix)]
pub async fn connect(endpoint: &str) -> Result<Duplex> {
    let stream = tokio::net::UnixStream::connect(endpoint).await?;
    Ok(Box::new(stream))
}

#[cfg(windows)]
mod windows {
    use super::*;
    use anyhow::anyhow;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use std::time::Duration;
    use takka_core::runtime::pipe_id;

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const OPEN_EXISTING: u32 = 3;
    /// `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION` — the client can
    /// identify the server but never impersonate it.
    const SECURITY_IDENTIFICATION: u32 = 0x0010_0000 | 0x0001_0000;
    const INVALID_HANDLE_VALUE: isize = -1;
    /// `ERROR_PIPE_BUSY`: a server exists but every instance is serving.
    const ERROR_PIPE_BUSY: i32 = 231;
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

    fn pipe_path(endpoint: &str) -> Vec<u16> {
        let name = pipe_id::pipe_name_for(endpoint);
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
                // SAFETY: `raw` is a valid, exclusively owned handle.
                let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
                let file = tokio::fs::File::from_std(std::fs::File::from(handle));
                return Ok(Box::new(file));
            }
            // SAFETY: GetLastError reads this thread's last-error slot.
            let code = unsafe { GetLastError() } as i32;
            if code != ERROR_PIPE_BUSY {
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

#[cfg(windows)]
pub use windows::connect;
