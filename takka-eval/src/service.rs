//! Background service for tasks that need one (see `task::SetupSpec`).

use crate::task::SetupSpec;
use anyhow::{bail, Context, Result};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A running service. Killed and reaped when dropped.
pub struct Service {
    child: Child,
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Ports this process has already handed out. The listener is dropped before
/// the service binds its port, so without this two callers could be given the
/// same number in that window.
static HANDED_OUT: Mutex<Vec<u16>> = Mutex::new(Vec::new());

/// Ask the OS for a free loopback port, never the same one twice in this
/// process. Another process could still take it before the service binds;
/// the readiness check turns that rare case into a clear failure.
pub fn free_port() -> Result<u16> {
    for _ in 0..64 {
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("finding a free port")?;
        let port = listener.local_addr()?.port();
        let mut seen = HANDED_OUT.lock().unwrap_or_else(|e| e.into_inner());
        if !seen.contains(&port) {
            seen.push(port);
            return Ok(port);
        }
    }
    bail!("could not find an unused loopback port")
}

/// Replace `{port}` in a task prompt.
pub fn resolve_prompt(prompt: &str, port: u16) -> String {
    prompt.replace("{port}", &port.to_string())
}

/// Start the service and wait until `port` accepts connections.
pub fn start(spec: &SetupSpec, task_dir: &Path, workdir: &Path, port: u16) -> Result<Service> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("exec {}", spec.command))
        .current_dir(task_dir)
        .env("EVAL_PORT", port.to_string())
        .env("EVAL_TASK_DIR", task_dir)
        .env("EVAL_WORKDIR", workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting service `{}`", spec.command))?;

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let deadline = Instant::now() + Duration::from_secs(spec.ready_timeout_secs);
    loop {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return Ok(Service { child });
        }
        if let Some(status) = child.try_wait()? {
            bail!("service `{}` exited early with {status}", spec.command);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "service `{}` not ready on port {port} after {}s",
                spec.command,
                spec.ready_timeout_secs
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn have_python() -> bool {
        Command::new("python3").arg("--version").output().is_ok()
    }

    fn spec(command: &str, secs: u64) -> SetupSpec {
        SetupSpec {
            command: command.to_string(),
            ready_timeout_secs: secs,
        }
    }

    #[test]
    fn prompt_port_is_substituted() {
        assert_eq!(
            resolve_prompt("GET http://127.0.0.1:{port}/x then {port}", 4242),
            "GET http://127.0.0.1:4242/x then 4242"
        );
        assert_eq!(resolve_prompt("no placeholder", 1), "no placeholder");
    }

    #[test]
    fn starts_waits_for_ready_and_kills_on_drop() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let port = free_port().unwrap();
        let svc = start(
            &spec("python3 -m http.server $EVAL_PORT --bind 127.0.0.1", 10),
            dir.path(),
            dir.path(),
            port,
        )
        .expect("service becomes ready");
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        assert!(TcpStream::connect(addr).is_ok());
        drop(svc);
        // After the drop nothing listens any more.
        assert!(TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_err());
    }

    #[test]
    fn never_ready_service_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port().unwrap();
        let err = start(&spec("sleep 30", 1), dir.path(), dir.path(), port)
            .err()
            .expect("must fail")
            .to_string();
        assert!(err.contains("not ready"), "{err}");
    }

    #[test]
    fn service_that_exits_early_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port().unwrap();
        let err = start(&spec("false", 5), dir.path(), dir.path(), port)
            .err()
            .expect("must fail")
            .to_string();
        assert!(err.contains("exited early"), "{err}");
    }
}
