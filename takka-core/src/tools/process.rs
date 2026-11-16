//! Bounded subprocess execution shared by the shell tools.

use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::time::{Duration, Instant};

/// Per-stream cap applied while reading, not after buffering.
pub const OUTPUT_CAP_BYTES: usize = 64 * 1024;
/// Timeout advertised by the shell tool schemas.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Upper bound accepted for a model-supplied timeout.
pub const MAX_TIMEOUT_SECS: u64 = 600;
/// Chunks the readers may buffer ahead of the drain loop (backpressure).
const READ_CHANNEL_DEPTH: usize = 32;

/// Clamp a model-supplied timeout into the documented 1..=600 s range.
pub fn clamp_timeout(secs: u64) -> Duration {
    Duration::from_secs(secs.clamp(1, MAX_TIMEOUT_SECS))
}

/// Why a child was killed instead of exiting on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillReason {
    Timeout,
    Cancelled,
}

/// Result of [`run_bounded`]; mirrors the shell tools' JSON contract.
#[derive(Debug)]
pub struct ProcessOutput {
    pub success: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// `Some` when the child was killed rather than exiting by itself.
    pub killed: Option<KillReason>,
}

#[derive(Debug, Clone, Copy)]
enum Stream {
    Out,
    Err,
}

/// Run `cmd` with both pipes drained concurrently, a wall-clock deadline, a
/// per-stream cap and optional cooperative cancellation.
pub fn run_bounded(
    cmd: &mut Command,
    deadline: Duration,
    cap_bytes: usize,
    cancel: Option<&AtomicBool>,
    on_progress: Option<&dyn Fn(&str)>,
) -> std::io::Result<ProcessOutput> {
    let started = Instant::now();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        // Put the child in its own process group so a kill can reach its
        // descendants too: `sh /tmp/x.sh` does not exec the script body, so
        // killing the interpreter alone leaves grandchildren (e.g. a
        // backgrounded `sleep`) running.
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn()?;
    #[cfg(unix)]
    let pgid = child.id(); // == pgid: the child leads its new group

    let (tx, rx) = mpsc::sync_channel(READ_CHANNEL_DEPTH);
    if let Some(stdout) = child.stdout.take() {
        spawn_reader(stdout, Stream::Out, tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_reader(stderr, Stream::Err, tx.clone());
    }
    drop(tx);

    let mut stdout = Acc::new(cap_bytes);
    let mut stderr = Acc::new(cap_bytes);
    let mut killed = None;

    loop {
        // Deadline and cancel are checked on every iteration, including the
        // ones that consumed a chunk: a child spewing output must not keep
        // the loop too busy to enforce them.
        if let Ok((stream, chunk)) = rx.recv_timeout(Duration::from_millis(25)) {
            absorb(stream, &chunk, &mut stdout, &mut stderr, on_progress);
        }
        if child.try_wait()?.is_some() {
            break;
        }
        if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            killed = Some(KillReason::Cancelled);
            break;
        }
        if started.elapsed() >= deadline {
            killed = Some(KillReason::Timeout);
            break;
        }
    }

    if killed.is_some() {
        #[cfg(unix)]
        kill_process_group(pgid);
        // Windows is out of scope for group kills (no Job Objects here), so
        // only the direct child is killed and any grandchildren survive.
        let _ = child.kill();
    }
    let status: ExitStatus = child.wait()?;

    // Drain whatever the readers already buffered; bounded so a surviving
    // grandchild holding the pipe open cannot block the return.
    let drain_until = Instant::now() + Duration::from_millis(500);
    while let Ok((stream, chunk)) = rx.recv_timeout(
        drain_until
            .checked_duration_since(Instant::now())
            .unwrap_or_default(),
    ) {
        absorb(stream, &chunk, &mut stdout, &mut stderr, on_progress);
    }

    let mut stderr_text = stderr.finish();
    if let Some(reason) = killed {
        let label = match reason {
            KillReason::Timeout => format!("timed out after {:.1}s", deadline.as_secs_f64()),
            KillReason::Cancelled => "cancelled".to_string(),
        };
        stderr_text = format!("command killed: {}\n{}", label, stderr_text);
    }

    Ok(ProcessOutput {
        success: killed.is_none() && status.success(),
        exit_code: status.code().unwrap_or(-1),
        stdout: stdout.finish(),
        stderr: stderr_text,
        killed,
    })
}

/// SIGKILL the whole process group led by `pgid`. Best effort: a group that
/// is already gone reports `ESRCH` and is ignored.
#[cfg(unix)]
fn kill_process_group(pgid: u32) {
    // SAFETY: `kill` has no memory-safety preconditions; a negative pid
    // addresses the process group `pgid`.
    unsafe {
        libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
    }
}

fn spawn_reader(
    mut reader: impl Read + Send + 'static,
    stream: Stream,
    tx: SyncSender<(Stream, Vec<u8>)>,
) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send((stream, buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
}

fn absorb(
    stream: Stream,
    chunk: &[u8],
    stdout: &mut Acc,
    stderr: &mut Acc,
    on_progress: Option<&dyn Fn(&str)>,
) {
    match stream {
        Stream::Out => stdout.absorb(chunk, on_progress),
        Stream::Err => stderr.absorb(chunk, on_progress),
    }
}

/// Stream accumulator that keeps at most `cap` bytes but counts them all.
struct Acc {
    bytes: Vec<u8>,
    total: usize,
    cap: usize,
    pending: Vec<u8>,
}

impl Acc {
    fn new(cap: usize) -> Self {
        Self {
            bytes: Vec::new(),
            total: 0,
            cap,
            pending: Vec::new(),
        }
    }

    fn absorb(&mut self, chunk: &[u8], on_progress: Option<&dyn Fn(&str)>) {
        self.total += chunk.len();
        let room = self.cap.saturating_sub(self.bytes.len());
        if room > 0 {
            let take = room.min(chunk.len());
            self.bytes.extend_from_slice(&chunk[..take]);
        }

        let Some(on) = on_progress else { return };
        self.pending.extend_from_slice(chunk);
        while let Some(pos) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=pos).collect();
            if self.bytes.len() < self.cap {
                on(&String::from_utf8_lossy(&line[..line.len() - 1]));
            }
        }
        if self.pending.len() > 8192 {
            self.pending.clear();
        }
    }

    fn finish(self) -> String {
        let text = String::from_utf8_lossy(&self.bytes).to_string();
        if self.total > self.bytes.len() {
            format!("{}... (truncated, {} bytes total)", text, self.total)
        } else {
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    /// Run `run_bounded` on a watchdog so a deadlock fails the test instead
    /// of hanging the suite.
    fn run_with_watchdog(
        mut cmd: Command,
        deadline: Duration,
        cancel: Option<Arc<AtomicBool>>,
    ) -> ProcessOutput {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let out = run_bounded(
                &mut cmd,
                deadline,
                OUTPUT_CAP_BYTES,
                cancel.as_deref(),
                None,
            )
            .expect("run_bounded failed");
            let _ = tx.send(out);
        });
        rx.recv_timeout(Duration::from_secs(15))
            .expect("run_bounded deadlocked (watchdog timed out)")
    }

    #[test]
    fn timeout_kills_child_and_reports() {
        let started = Instant::now();
        let out = run_with_watchdog(sh("sleep 5"), Duration::from_millis(200), None);
        let elapsed = started.elapsed();
        assert_eq!(out.killed, Some(KillReason::Timeout));
        assert!(!out.success);
        assert_eq!(out.exit_code, -1);
        assert!(
            out.stderr.contains("killed"),
            "stderr should explain the kill: {:?}",
            out.stderr
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "timeout was not enforced, took {:?}",
            elapsed
        );
    }

    #[test]
    fn timeout_kills_chatty_child() {
        // Always-ready pipe: the deadline must still be enforced.
        let started = Instant::now();
        let out = run_with_watchdog(
            sh("while :; do echo spam; done"),
            Duration::from_millis(200),
            None,
        );
        let elapsed = started.elapsed();
        assert_eq!(out.killed, Some(KillReason::Timeout));
        assert!(
            elapsed < Duration::from_secs(3),
            "deadline not enforced on a chatty child, took {:?}",
            elapsed
        );
    }

    #[test]
    fn large_stderr_does_not_deadlock() {
        // ~1 MiB to stderr, then sleeps: a serial stdout-then-stderr reader
        // never reaches stderr and hangs, because the child blocks on a full
        // stderr pipe and never closes stdout.
        let started = Instant::now();
        let out = run_with_watchdog(
            sh("head -c 1048576 /dev/zero >&2; sleep 5"),
            Duration::from_millis(2000),
            None,
        );
        let elapsed = started.elapsed();
        assert_eq!(out.killed, Some(KillReason::Timeout));
        assert!(
            elapsed < Duration::from_secs(5),
            "drain deadlocked, took {:?}",
            elapsed
        );
        assert!(
            out.stderr.len() <= OUTPUT_CAP_BYTES + 128,
            "stderr not capped while streaming: {} bytes",
            out.stderr.len()
        );
    }

    #[test]
    fn output_capped_while_streaming() {
        // Far more than the cap on both streams, then exit on its own.
        let out = run_with_watchdog(
            sh("head -c 1048576 /dev/zero | tr '\\0' o; head -c 1048576 /dev/zero | tr '\\0' e >&2"),
            Duration::from_secs(10),
            None,
        );
        assert!(out.killed.is_none(), "child should exit on its own");
        assert!(out.success);
        assert!(
            out.stdout.len() <= OUTPUT_CAP_BYTES + 128,
            "stdout not capped: {} bytes",
            out.stdout.len()
        );
        assert!(
            out.stderr.len() <= OUTPUT_CAP_BYTES + 128,
            "stderr not capped: {} bytes",
            out.stderr.len()
        );
        assert!(
            out.stdout.contains("truncated") && out.stdout.contains("1048576"),
            "stdout should report the real total, got: {:?}",
            &out.stdout[out.stdout.len().saturating_sub(120)..]
        );
    }

    #[test]
    fn cancel_kills_running_child() {
        let cancel = Arc::new(AtomicBool::new(false));
        let flipper = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            flipper.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let out = run_with_watchdog(sh("sleep 5"), Duration::from_secs(30), Some(cancel));
        let elapsed = started.elapsed();
        assert_eq!(out.killed, Some(KillReason::Cancelled));
        assert!(!out.success);
        assert!(
            elapsed < Duration::from_secs(3),
            "cancel was not honoured, took {:?}",
            elapsed
        );
        assert!(
            out.stderr.contains("cancelled"),
            "stderr should explain the cancel: {:?}",
            out.stderr
        );
    }
}
