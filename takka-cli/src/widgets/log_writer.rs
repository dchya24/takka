//! Terminal-safe writer for the console tracing layer.
//!
//! The inline CLI keeps a transient `Thinking…` spinner on the *last*
//! terminal line and redraws it in place with `MoveUp` + repaint. Any log
//! line written to stderr from another thread in the middle of that dance
//! lands on the spinner row: the next spinner redraw overwrites the log's
//! first columns (leaving the tail of the message stranded to the right),
//! and the end-of-run cleanup does `MoveUp(1)` on the wrong line, so the
//! progress bar is left behind after the answer is printed.
//!
//! The streaming renderer therefore installs a hook while its spinner is
//! on screen. This writer buffers stderr bytes per line and, when a hook is
//! installed, hands the finished line to it: the hook erases the spinner,
//! prints the log line, and re-arms the spinner — so neither the log nor the
//! progress line gets mangled.

use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock};

/// Callback invoked with one complete log line. It owns all terminal work:
/// erase the transient spinner, print the bytes, then repaint the spinner.
pub type SpinnerHook = Arc<dyn Fn(&[u8]) + Send + Sync>;

fn hook_slot() -> &'static Mutex<Option<SpinnerHook>> {
    static SLOT: OnceLock<Mutex<Option<SpinnerHook>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Install (or clear) the spinner guard used by [`ConsoleLogWriter`].
pub fn set_spinner_hook(hook: Option<SpinnerHook>) {
    *hook_slot().lock().unwrap() = hook;
}

fn current_hook() -> Option<SpinnerHook> {
    hook_slot().lock().unwrap().clone()
}

/// RAII guard that installs a [`SpinnerHook`] and clears it on drop, so a
/// panicking renderer thread can never leave a stale hook behind (which would
/// keep erasing a spinner line that no longer exists).
pub struct SpinnerHookGuard(());

impl SpinnerHookGuard {
    pub fn install(hook: SpinnerHook) -> Self {
        set_spinner_hook(Some(hook));
        Self(())
    }
}

impl Drop for SpinnerHookGuard {
    fn drop(&mut self) {
        set_spinner_hook(None);
    }
}

fn write_stderr(bytes: &[u8]) {
    let mut stderr = io::stderr();
    let _ = stderr.write_all(bytes);
    let _ = stderr.flush();
}

/// Write one finished log line to stderr in a way that survives raw mode.
///
/// The inline REPL keeps the terminal in raw mode while an agent run is in
/// flight, so a bare `\n` moves the cursor down without returning to column
/// 0. Terminating with `\r\n` (TTY only, so piped logs stay clean) keeps the
/// next spinner repaint starting at the left edge.
///
pub fn write_line_to_stderr(line: &[u8]) {
    use std::io::IsTerminal;

    let body = line.strip_suffix(b"\n").unwrap_or(line);
    let body = body.strip_suffix(b"\r").unwrap_or(body);

    let mut stderr = io::stderr();
    let _ = stderr.write_all(body);
    if stderr.is_terminal() {
        let _ = stderr.write_all(b"\r\n");
    } else {
        let _ = stderr.write_all(b"\n");
    }
    let _ = stderr.flush();
}

fn emit(line: &[u8]) {
    match current_hook() {
        Some(hook) => hook(line),
        None => write_stderr(line),
    }
}

/// Line-buffered, spinner-aware stderr writer for the console tracing layer.
///
/// A fresh instance is handed to the layer per event, so the buffered
/// remainder is emitted on drop (tracing does not always flush explicitly).
#[derive(Default)]
pub struct ConsoleLogWriter {
    buf: Vec<u8>,
}

impl ConsoleLogWriter {
    pub fn new() -> Self {
        Self::default()
    }

    fn drain_lines(&mut self) -> usize {
        let mut emitted = 0;
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            emit(&line);
            emitted += 1;
        }
        emitted
    }
}

impl Write for ConsoleLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        self.drain_lines();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            emit(&rest);
        }
        io::stderr().flush()
    }
}

impl Drop for ConsoleLogWriter {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            emit(&rest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines are only handed to the hook once a newline (or the final drop)
    /// completes them, so a multi-`write` event never gets split into
    /// several spinner erase/repaint cycles.
    #[test]
    fn buffers_until_newline_then_emits_complete_lines() {
        let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        set_spinner_hook(Some(Arc::new(move |bytes: &[u8]| {
            sink.lock().unwrap().push(bytes.to_vec());
        })));

        let mut writer = ConsoleLogWriter::new();
        writer.write_all(b"2026-05-11T10:00:00Z ").unwrap();
        assert!(seen.lock().unwrap().is_empty(), "partial line emitted");

        writer.write_all(b"WARN boom\n").unwrap();
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[b"2026-05-11T10:00:00Z WARN boom\n".to_vec()]
        );

        writer.write_all(b"tail without newline").unwrap();
        drop(writer);
        assert_eq!(seen.lock().unwrap().len(), 2);

        set_spinner_hook(None);
    }
}
