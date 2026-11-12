# Fase 1 — Tool Lifecycle & Live Output Implementation Plan

>
> **Status:** LANDED — semua step selesai dieksekusi (checkbox disweep housekeeping 2026-11-12); fiturnya ada di CHANGELOG [0.6.0].
> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Buat per-tool progress live di interactive/`run` mode: orchestrator emit `ToolStart`/`ToolDelta`/`ToolOutput`(diperkaya), dan `run_command`/`run_script` di-streaming output-kandaya, dirender inline.

**Architecture:** Tambah 3 variant ke `Event` enum + `execute_streaming` default method di `Tool` trait (default = `execute`, non-breaking). Orchestrator menyalurkan delta via throttled sink; renderer inline (single-writer 80ms loop yang sudah ada) menggambar tool card Opsi 2. Kontrak LLM/memory TIDAK berubah — delta hanya observability.

**Tech Stack:** Rust (workspace `core-agentic` library + `agentic-cli` binary), tokio, crossterm, ratatui. Cek semua langkah dengan `cargo test`.

## Global Constraints

- Semua `match` atas `Event` sudah punya wildcard `_ => {}` — menambah variant baru TIDAK memicu error kompilasi, tapi tetap perlu update matching di renderer/TUI.
- `run_command`/`run_script` adalah **mutating** (`is_read_only() == false`) → selalu dieksekusi sekuensial (batch-of-one) di kedua path `tool_exec.rs`. Artinya delta streaming hanya pernah untuk SATU tool aktif; tidak ada interleaving antar-tool di Fase 1.
- Kontrak output JSON `run_command`/`run_script` (`{success, exit_code, stdout, stderr, ...}`) TIDAK boleh berubah.
- Kontrak memory: hasil final tool tetap di-truncate oleh `tool_result_max_chars`. Delta tidak masuk memory.
- Rate-limit delta: maks 1 delta per ~80ms per tool, plus budget char (~8000) per tool untuk menghentikan live-streaming output yang sangat besar (hasil final tetap utuh di memory).
- Antonim: crate `core-agentic` (library) dan `agentic-cli` (binary). Test: `cargo test -p core-agentic` dan `cargo test -p agentic-cli`.

---

### Task 1: Extend `Event` enum + `EventType`

**Files:**
- Modify: `core-agentic/src/events.rs`
- Test: `core-agentic/src/events.rs` (mod tests)

**Justifications:**
- `Event::ToolDelta` dipakai sink streaming untuk menyampaikan delta.
- `ToolOutput` diperkaya `tool_call_id`, `duration_ms`, `success`, `truncated`.

**Exports:**
- Produces: `Event::ToolStart { tool_call_id: String, tool_name: String, arguments: Value }`, `Event::ToolDelta { tool_call_id: String, tool_name: String, delta: String }`, dan `Event::ToolOutput { tool_name, output, error, tool_call_id, duration_ms, success, truncated }`. `EventType` bertambah `ToolStart`, `ToolDelta`.

- [x] **Step 1: Write failing test**

```rust
// dibawah di #cfg[test)] mod tests di events.rs
#[test]
fn new_lifecycle_variants_serialize_and_map_to_type() {
    let start = Event::ToolStart {
        tool_call_id: "c1".into(),
        tool_name: "run_command".into(),
        arguments: serde_json::json!({"command": "echo hi"}),
    };
    assert_eq!(start.event_type(), EventType::ToolStart);
    let delta = Event::ToolDelta {
        tool_call_id: "c1".into(),
        tool_name: "run_command".into(),
        delta: "hi\n".to_string(),
    };
    assert_eq!(delta.event_type(), EventType::ToolDelta);
    // ToolOutput enriched fields.
    let out = Event::ToolOutput {
        tool_name: "run_command".into(),
        output: serde_json::json!({"stdout": "hi"}),
        error: None,
        tool_call_id: "c1".into(),
        duration_ms: 42,
        success: true,
        truncated: false,
    };
    assert_eq!(out.event_type(), EventType::ToolOutput);
}
```

- [x] **Step 2: Run to verify it fails**

Run: `cargo test -p core-agentic events::tests::new_lifecycle_variants_serialize_and_map_to_type`
Expected: compile error — `Event::ToolStart` / `EventType::ToolStart` tidak ada.

- [x] **Step 3: Add the variants**

Edit `core-agentic/src/events.rs`:

```rust
    // status
    #[serde(rename = "tool_start")]
    #[serde(rename_all = "camelCase")]
    ToolStart {
        tool_call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },

    /// Live output chunk from a streaming tool (run_command/run_script).
    #[serde(rename = "tool_delta")]
    #[serde(rename_all = "camelCase")]
    ToolDelta {
        tool_call_id: String,
        tool_name: String,
        delta: String,
    },

    #[serde(rename = "tool_output")]
    #[serde(rename_all = "camelCase")]
    ToolOutput {
        tool_name: String,
        output: serde_json::Value,
        error: Option<String>,
        tool_call_id: String,
        duration_ms: u64,
        success: bool,
        truncated: bool,
    },
```

Dan `EventType`:

```rust
pub enum EventType {
    // ...
    ToolStart,
    ToolDelta,
    // ...
}
```

Update `Event::event_type()`:

```rust
        Event::ToolStart { .. } => EventType::ToolStart,
        Event::ToolDelta { .. } => EventType::ToolDelta,
```

Colokkan `#[serde(rename_all = "camelCase")]` di atas enum `ToolOutput` variant tersebut (jangan di enum induk karena `#[serde(tag = "type")]`).

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p core-agentic events::tests::new_lifecycle_variants_serialize_and_map_to_type`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add core-agentic/src/events.rs
git commit -m "feat(core): add ToolStart/ToolDelta events + enrich ToolOutput"
```

---

## Task 2: `execute_streaming` default method di `Tool` trait

**Files:**
- Modify: `core-agentic/src/tool.rs`
- Test: `core-agentic/src/tool.rs` (mod tests)

**Interfaces:**
- Produces: `Tool::execute_streaming(&self, args: serde_json::Value, on_progress: &dyn Fn(&str)) -> ToolResult<serde_json::Value>` dengan default impl memanggil `self.execute(args)` (mengabaikan `on_progress`).

- [x] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // Tool yang hanya mengimplementasikan `execute` (bukan meng-override
    // `execute_streaming`) tetap berfungsi: default mengembalikan hasil yang sama.
    #[test]
    fn execute_streaming_defaults_to_execute() {
        struct Basic;
        impl Tool for Basic {
            fn name(&self) -> &str { "basic" }
            fn description(&self) -> &str { "" }
            fn schema(&self) -> ToolSchema { ToolSchema::new("basic", "") }
            fn execute(&self, _: serde_json::Value) -> ToolResult<serde_json::Value> {
                Ok(serde_json::json!({ "ok": 1 }))
            }
        }
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let tool = Basic;
        let callbacks = Arc::new(AtomicUsize::new(0));
        let c = callbacks.clone();
        // Closure harus Fn (bukan FnMut) — pakai AtomicUsize, bukan `+=` ke
        // variabel local, supaya bisa di-coerce ke `&dyn Fn(&str)`.
        let result = tool
            .execute_streaming(serde_json::json!({}), &move |_| {
                c.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        assert_eq!(result, serde_json::json!({ "ok": 1 }));
        assert_eq!(callbacks.load(Ordering::SeqCst), 0, "fallback must not invoke on_progress");
    }
}
```

- [x] **Step 2: Run to verify it fails**

Run: `cargo test -p core-agentic tool::tests::execute_streaming_defaults_to_execute`
Expected: FAIL — method `execute_streaming` tidak ada di trait.

- [x] **Step 3: Implement**

Add to `core-agentic/src/tool.rs`, di dalam `pub trait Tool`:

```rust
    /// Stream progressive output to `on_progress` as the tool runs.
    ///
    /// Default: run [`Self::execute`] atomically and ignore the callback.
    /// Tools that produce long-running output (e.g. run_command) override
    /// this to report deltas live; non-streaming tools are untouched.
    #[allow(unused_mut)]
    fn execute_streaming(
        &self,
        args: serde_json::Value,
        _on_progress: &dyn Fn(&str),
    ) -> ToolResult<serde_json::Value> {
        self.execute(args)
    }
```

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p core-agentic tool::tests::execute_streaming_defaults_to_execute`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add core-agentic/src/tool.rs
git commit -m "feat(core): add Tool::execute_streaming with execute fallback; -p core-agentic"
```

---

## Task 3: `execute_streaming_by_name` di `ToolRegistry`

**Files:**
- Modify: `core-agentic/src/tool_registry.rs`
- Test: `core-agentic/src/tool_registry.rs` (mod tests)

**Justifications:**
- Later tasks (Task 6) call the orchestrator to run tools via the registry — needs a streaming variant beside `execute_by_name`.

**Interfaces:**
- Produces: `ToolRegistry::execute_streaming_by_name(&self, name: &str, args: &serde_json::Value, on_progress: &dyn Fn(&str)) -> Result<serde_json::Value, ToolError>`.

- [x] **Step 1: Write the failing test**

```rust
    // A tool that overrides execute_streaming to emit two deltas; the
    // registry must forward the callback untouched.
    struct Counter;
    impl Tool for Counter {
        fn name(&self) -> &str { "counter" }
        fn description(&self) -> &str { "dummy" }
        fn schema(&self) -> ToolSchema { ToolSchema::new("counter", "dummy") }
        fn execute(&self, _: serde_json::Value) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({ "ok": true }))
        }
        fn execute_streaming(
            &self,
            _: serde_json::Value,
            on_progress: &dyn Fn(&str),
        ) -> Result<serde_json::Value, ToolError> {
            on_progress("alpha");
            on_progress("beta");
            Ok(serde_json::json!({ "ok": true }))
        }
    }

    #[test]
    fn registry_forwards_streaming_deltas() {
        let reg = ToolRegistry::new();
        reg.register(Box::new(Counter));
        let deltas = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let d2 = deltas.clone();
        let result = reg
            .execute_streaming_by_name("counter", &serde_json::json!({}), &move |s| {
                d2.lock().unwrap().push(s.to_string());
            })
            .unwrap();
        assert_eq!(result, serde_json::json!({ "ok": true }));
        assert_eq!(*deltas.lock().unwrap(), vec!["alpha", "beta"]);
    }
```

- [x] **Step 2: Run to verify it fails**

Run: `cargo test -p core-agentic registry_forwards_streaming_deltas`
Expected: compile error — method `execute_streaming_by_name` not found.

- [x] **Step 3: Implement**

```rust
    /// Execute a tool by name, streaming progress deltas through
    /// `on_progress`. For tools without a streaming override this routes
    /// to the default (atomic) execution and never calls `on_progress`.
    pub fn execute_streaming_by_name(
        &self,
        name: &str,
        args: &serde_json::Value,
        on_progress: &dyn Fn(&str),
    ) -> Result<serde_json::Value, ToolError> {
        let tools = self.tools.read().unwrap();
        let tool = tools
            .get(name)
            .ok_or_else(|| ToolError::new(format!("Tool not found: {}", name)))?;
        tool.execute_streaming(args.clone(), on_progress)
    }
```

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p core-agentic registry_forwards_streaming_deltas`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add core-agentic/src/tool_registry.rs
git commit -m "feat(core): add ToolRegistry::execute_streaming_by_name"
```

---

## Task 4: Stream `run_command`

**Files:**
- Modify: `core-agentic/src/tools/run_command.rs`
- Test: `core-agentic/src/tools/run_command.rs` (mod tests)

**Interfaces:**
- Consumes: `Tool::execute_streaming` (Task 2).
- Produces: `RunCommandTool` meng-override `execute_streaming`; JSON output TIDAK berubah.

- [x] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn run_command_streams_and_keeps_json() {
        let tool = RunCommandTool::new();
        let deltas: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let d2 = deltas.clone();
        // POSIX shell; 'sh' invoked with -c. Keep test portable across Unix.
        let result = tool.execute_streaming(serde_json::json!({
            "command": "printf 'hello\\nworld\\n'",
        }), &move |s| d2.lock().unwrap().push(s.to_string())).unwrap();
        assert_eq!(result["success"], true);
        let stdout = result["stdout"].as_str().unwrap();
        assert!(stdout.contains("hello"), "stdout missing 'hello': {}", stdout);
        // Deltas should carry the line(s) — at least one callback fired.
        assert!(!deltas.lock().unwrap().is_empty());
        // JSON shape conserved (exit_code present).
        assert!(result.get("exit_code").is_some());
    }
}
```

- [x] **Step 2: Run to verify it fails**

Run: `cargo test -p core-agentic run_command::tests::run_command_streams_and_keeps_json`
Expected: PASS of the OLD impl may already pass (because fallback path streams nothing?). The default calls execute: deltas stays empty → assert fails → must implement streaming. If it errors elsewhere fix accordingly imbalance.

Actually will produce as old behavior: result JSON same, deltas empty → assertion `!deltas...is_empty()` FAILS. Good, red.

- [x] **Step 3: Implement `execute_streaming`**

Replace `cmd.output()`-based body with a piped incremental read. Keep `execute` as the atomic path. Add:

```rust
    fn execute_streaming(
        &self,
        args: serde_json::Value,
        on_progress: &dyn Fn(&str),
    ) -> ToolResult<serde_json::Value> {
        let args_obj = args.as_object()
            .ok_or_else(|| ToolError::new("Invalid arguments: expected object"))?;
        let command = args_obj.get("command").and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: command"))?;
        let cwd = args_obj.get("cwd").and_then(|v| v.as_str());

        let mut child = if cfg!(target_os = "windows") {
            let mut cmd = std::process::Command::new("cmd");
            cmd.args(["/C", command]);
            if let Some(dir) = cwd { cmd.current_dir(dir); }
            cmd
        } else {
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", command]);
            if let Some(dir) = cwd { cmd.current_dir(dir); }
            cmd
        };
        child.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        let mut child = child.spawn()
            .map_err(|e| ToolError::new(format!("Failed to execute command: {}", e)))?;

        // Stream stdout and stderr line-by-line concurrently.
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        use std::io::{BufRead, BufReader};
        let stdout_reader = BufReader::new(stdout);
        let mut stdout_acc = String::new();
        for line in stdout_reader.lines() {
            match line {
                Ok(line) => { on_progress(&line); stdout_acc.push_str(&line); stdout_acc.push('\n'); }
                Err(_) => break,
            }
        }
        let mut stderr_acc = String::new();
        if let Some(stderr) = stderr {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) => { on_progress(&line); stderr_acc.push_str(&line); stderr_acc.push('\n'); }
                    Err(_) => break,
                }
            }
        }

        let status = child.wait()
            .map_err(|e| ToolError::new(format!("Failed to wait command: {}", e)))?;

        Ok(serde_json::json!({
            "success": status.success(),
            "exit_code": status.code().unwrap_or(-1),
            "stdout": stdout_acc,
            "stderr": stderr_acc,
        }))
    }
```

Catatan: pendekatan ini streaming stdout, lalu stderr (berurutan, tidak interleave dua stream). Di Fase 1 cukup (ke-2 jarang dipakai bersamaan); interleave stdout/stderr bisa disempurnakan nanti via `read_line` pada dua thread. Jangan ubah `execute` — biarkan sebagai path atomik yang lama.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p core-agentic run_command::tests::run_command_streams_and_keeps_json`
Expected: PASS.

- [x] **Step 5: Confirm existing atomic tests still pass**

Run: `cargo test -p core-agentic -- run_command`
Expected: PASS (seluruh test module run_command, lama + baru).

- [x] **Step 6: Commit**

```bash
git add core-agentic/src/tools/run_command.rs
git commit -m "feat(core): stream run_command output via execute_streaming"
```

---

## Task 5: Stream `run_script`

**Files:**
- Modify: `core-agentic/src/tools/run_script.rs`
- Test: `core-agentic/src/tools/run_script.rs` (mod tests)

**Interfaces:**
- Consumes: `Tool::execute_streaming` default.
- Produces: `RunScriptTool` overrides `execute_streaming`; contraction also honors its 64KB truncation for stdout/stderr (jangan diregressi).

- [x] **Step 1: Write the failing test**

```rust
    #[test]
    fn run_script_streams_echo_lines() {
        use std::sync::{Arc, Mutex};
        let tool = RunScriptTool::new();
        let deltas: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let d = deltas.clone();
        let result = tool.execute_streaming(
            serde_json::json!({"script": "echo one\necho two", "interpreter": "sh"}),
            &move |line| d.lock().unwrap().push(line.to_string()),
        ).unwrap();
        assert_eq!(result["success"], true);
        let stdout = result["stdout"].as_str().unwrap();
        assert!(stdout.contains("one") && stdout.contains("two"), "bad stdout: {}", stdout);
        assert!(!deltas.lock().unwrap().is_empty(), "expected live deltas");
    }
```

- [x] **Step 2: Run to verify fail** (`cargo test -p core-agentic run_script_streams_lines`) — deltas empty in default.

- [x] **Step 3: Implement `execute_streaming`**

Refactor: extract the interpreter-spawn + read into a shared helper that takes an `on_progress` option. Turn `run_script` atomic `execute` path calls with `None`; `execute_streaming` calls with `Some(on_progress)`.

```rust
    fn run_interpreter_streaming(
        &self,
        interpreter: &str,
        script_path: &std::path::Path,
        cwd: Option<&str>,
        _timeout_secs: u64,
        on_progress: Option<&dyn Fn(&str)>,
    ) -> ToolResult<serde_json::Value> {
        // same spawn, but:
        // stdout/stderr piped + BufReader lines; call on_progress per line
        // if present; accumulate into stdout_acc/stderr_acc, applying the
        // existing 64KB truncation before returning.
        ...
        // reuse the truncation logic from the existing code.
    }
```

Update `execute` to call `run_interpreter_streaming(..., None)`; add:

```rust
    fn execute_streaming(&self, args, on_progress) -> ToolResult<Value> {
        // duplicate arg parsing (same as execute); write temp file;
        // then run_interpreter_streaming(..., Some(on_progress))
    }
```

Keep the 64KB truncation for stdout/stderr intact in `run_interpreter_streaming` (extract `truncate_64k(&str) -> String`).

- [x] **Step 4: Verify new test passes + old tests** (`cargo test -p core-agentic run_script`)
- [x] **Step 5: Commit**

```bash
git add core-agentic/src/tools/run_script.rs
git commit -m "feat(core): stream run_script via execute_streaming"
```

---

## Task 6: Orchestrator emits lifecycle events + wires streaming

**Files:**
- Modify: `core-agentic/src/orchestrator/mod.rs` (change `events` to `Arc`?)
- Modify: `core-agentic/src/orchestrator/tool_exec.rs`
- Modify: `core-agentic/src/orchestrator/messages.rs` (optional small helper)
- New: `core-agentic/src/orchestrator/progress.rs` (DeltaThrottler)
- Test: `core-agentic/tests/orchestrator_loop.rs`

**Justifications:**
- Sync path (`handle_tool_calls`) executes in scoped threads, can borrow `&self.events` for direct emit.
- Async path (`handle_tool_calls_parallel`) uses `spawn_blocking` ('static) → needs an `Arc<EventEmitter>` to a forwarder. Only batch-of-one streams (run_command/run_script), so forwarder trivial.

- [x] **Step 1: Small helper — DeltaThrottler**

New module `core-agentic/src/orchestrator/progress.rs`:

```rust
/// Limit live tool deltas: emit at most one delta per tool per ~80ms and
/// cap total live-displayed chars per tool, so a noisy stream (e.g. tail -f)
/// can't drown the channel or the terminal. The full result still returns in
/// the final ToolOutput, so this only affects the live preview.
pub struct DeltaThrottler {
    last_emit: std::sync::Mutex<std::time::Instant>,
    budget_chars: usize,
}

impl DeltaThrottler {
    pub fn new(budget: usize) -> Self {
        Self { last_emit: Mutex::new(Instant::now() - Duration::from_millis(100)), budget_chars: budget }
    }
    /// Returns true if we should emit now; if the budget is exhausted
    /// (total delta passed) returns false thereafter.
    pub fn should_emit(&self, delta_len: usize) -> bool {
        budget.. or guard; true only if elapsed >= 80ms && !exhausted
    }
}
```

**Actual simplified thumbtbumin:** keep a running counter of emitted chars; `should_emit(n)` returns true only when running_total stays ≤ self.budget_chars. Splitting cadence is up to the caller-bi=-nothing. (80ms coalescing already provided by renderer's 80ms drain; orchestrator only caps budget.) — Keep both. Provide:

```rust
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct DeltaThrottler {
    state: Mutex<ThrottleState>,
}
struct ThrottleState {
    last_emit: Instant,
    budget_remaining: usize,
}
const DELTA_INTERVAL: Duration = Duration::from_millis(80);

impl DeltaThrottler {
    pub fn new(budget: usize) -> Self {
        Self { state: Mutex::new(ThrottleState {
            last_emit: Instant::now(),
            budget_remaining: budget,
        })}
    }
    /// Call for each incoming delta piece. Returns true when it should be
    /// surfaced to the EventEmitter as a ToolDelta.
    pub fn accept(&self, delta: &str) -> bool {
        let mut s = self.state.lock().unwrap();
        if delta.len() >= s.budget_remaining {
            // Take the final chunk; exhaust the budget.
            s.budget_remaining = 0;
            return true;
        }
        if s.last_emit.elapsed() < DELTA_INTERVAL {
            return false;
        }
        s.budget_remaining -= delta.len();
        s.last_emit = Instant::now();
        true
    }
}
```

Export from `orchestrator/mod.rs` (`mod progress;`).

- [x] **Step 2: Make `events` shared (`Arc<EventEmitter>`)**

In `orchestrator/mod.rs` — change the field type:

```rust
    events: Arc<EventEmitter>,
```

and initialization `events: Arc::new(EventEmitter::new())`. Rely on `Deref` for existing `self.events.on/emit` calls (no call-site changes needed elsewhere). Add a note in the struct doc. Run `cargo build -p core-agentic` to ensure nothing breaks.

- [x] **Step 3: Add execution helper in tool_exec.rs**

Add a private method:

```rust
    /// Execute a tool with a streaming progress sink. Returns the truncated
    /// result string plus duration/success/truncated flags for ToolOutput.
    struct ToolExecOutcome { result: String, duration_ms: u64, success: bool, truncated: bool }

    fn execute_tool_streaming(
        &self,
        name: &str,
        args: &serde_json::Value,
        on_progress: &dyn Fn(&str),
    ) -> ExecutedOutcome {
        let start = std::time::Instant::now();
        let raw = match self.tools.execute_streaming_by_name(name, args, on_progress) {
            Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()),
            Err(e) => format!("Tool error: {}", e),
        };
        let duration_ms = start.elapsed().as_millis() as u64;
        let truncated = raw.len() > self.tool_result_max_chars;
        let result = crate::orchestrator::messages::truncate_tool_result(&raw, self.tool_result_max_chars);
        let success = !result.starts_with("Tool error");
        ExecutedOutcome { result, duration_ms, success, truncated }
    }
```

Update existing `execute_tool` usages (the sync one) to call a shared path.

- [x] **Step 4: Emit ToolStart/ToolDelta/ToolOutput in `handle_tool_calls` (sync)**

**Struktur data bersama untuk kedua path.** Ganti `Vec<Option<(String, String, String)>>` menjadi `Vec<Option<SlotOutcome>>` dengan struct (tambahkan di atas `impl Orchestrator` dalam `tool_exec.rs`):

```rust
/// Hasil eksekusi satu tool + metadata untuk ToolOutput.
struct SlotOutcome {
    name: String,
    id: String,
    result: String,     // sudah di-truncate
    duration_ms: u64,
    success: bool,
    truncated: bool,
}
```

Helper eksekusi (ganti `execute_tool` lama; hapus `execute_tool` bila tidak terpakai lagi):

```rust
fn execute_tool_streaming(
    &self,
    name: &str,
    args: &serde_json::Value,
    on_progress: &dyn Fn(&str),
) -> SlotOutcome {
    let start = std::time::Instant::now();
    let raw = match self.tools.execute_streaming_by_name(name, args, on_progress) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()),
        Err(e) => format!("Tool error: {}", e),
    };
    let truncated = raw.len() > self.tool_result_max_chars;
    let result = super::messages::truncate_tool_result(&raw, self.tool_result_max_chars);
    SlotOutcome {
        name: name.to_string(),
        id: String::new(), // diisi pemanggil
        result,
        duration_ms: start.elapsed().as_millis() as u64,
        success: !result.starts_with("Tool error"),
        truncated,
    }
}
```

> Catatan: `truncate_tool_result` sudah `pub(crate)` di `orchestrator/messages.rs` — akses via `super::messages::truncate_tool_result`.

**Sync path (`handle_tool_calls`):**

1. **PreResolved** (denied/skipped): `results[i] = Some(SlotOutcome { name, id, result: message, duration_ms: 0, success: false, truncated: false })` — event `ToolOutput`-nya sudah di-emit di pre-pass, jadi final loop tidak emit ulang untuk slot ini.
2. **Sebelum eksekusi setiap Pending** (single-element inline DAN state-changing batch-of-one): emit `ToolStart`, lalu jalankan dengan throttler:

```rust
self.events.emit(Event::ToolStart {
    tool_call_id: id.clone(),
    tool_name: name.clone(),
    arguments: args.clone(),
});
let throttler = DeltaThrottler::new(8_000);
let oid = id.clone();
let oname = name.clone();
let mut outcome = self.execute_tool_streaming(name, args, &|delta| {
    if throttler.accept(delta) {
        self.events.emit(Event::ToolDelta {
            tool_call_id: oid.clone(),
            tool_name: oname.clone(),
            delta: delta.to_string(),
        });
    }
});
outcome.id = id.clone();
results[i] = Some(outcome);
```

3. **Multi-element read-only batch** (tool tidak streaming — callback tak pernah dipanggil):
   - Emit `ToolStart` per tool di thread utama **sebelum spawn**.
   - Dalam thread: ukur durasi (`Instant::now()` sebelum/`elapsed()` sesudah `execute_by_name`), hitung `truncated` sebelum truncate. Kembalikan tuple 6-field `(local_idx, name, id, result, duration_ms, truncated)` dan isi `SlotOutcome` saat join (`success = !result.starts_with("Tool error")`).
4. **Final loop** — ganti emit plain menjadi enriched (hanya untuk `Slot::Pending`):

```rust
if matches!(slots[idx], Slot::Pending { .. }) {
    self.events.emit(Event::ToolOutput {
        tool_name: outcome.name.clone(),
        output: serde_json::Value::String(outcome.result.clone()),
        error: None,
        tool_call_id: outcome.id.clone(),
        duration_ms: outcome.duration_ms,
        success: outcome.success,
        truncated: outcome.truncated,
    });
}
```

- [x] **Step 5: Async path `handle_tool_calls_parallel`**

Pola sama; bedanya `spawn_blocking` butuh `'static`, jadi delta dikirim lewat channel + forwarder thread:

```rust
// Per batch (sebelum spawn):
let (tx, rx) = std::sync::mpsc::channel::<String>();
let emitter = self.events.clone();
let throttler = Arc::new(DeltaThrottler::new(8_000));
let t2 = throttler.clone();
let fwd = std::thread::Builder::new()
    .name("agentic-delta-fwd".into())
    .spawn(move || {
        for del in rx {
            if t2.accept(&del) {
                emitter.emit(Event::ToolDelta {
                    tool_call_id: /* diisi: butuh id per tool */,
                    tool_name: /* diisi */,
                    delta: del,
                });
            }
        }
    })
    .expect("spawn delta forwarder");
```

- Emit `ToolStart` per tool di thread utama sebelum spawn (id/name/args).
- Dalam closure `spawn_blocking`: `let tx = tx.clone();` lalu `execute_streaming_by_name(name, args, &|d| { let _ = tx.send(d.to_string()); })`; ukur durasi; hitung `truncated`; kembalikan tuple 6-field seperti sync path. Tool read-only tidak pernah memanggil callback (default `execute`), jadi `tx` tidak pernah terkirim apa pun.
- Setelah semua handle batch di-await: `drop(tx)` (Sender asli), lalu `fwd.join()` — memastikan semua delta ter-flush **sebelum** `ToolOutput` final di-emit.
- Emit enriched `ToolOutput` di final loop (sama dengan sync path).

Perhatikan: `EventEmitter` sudah `Sync` (handler di dalam `Mutex`), jadi `emitter.emit` aman dari thread forwarder. `Event::ToolDelta` memerlukan `tool_call_id`/`tool_name` — untuk batch-of-one (run_command/run_script) nilainya tunggal; simpan di variabel luar closure sebelum spawn dan clone ke forwarder.

- [x] **Step 6: Integration test**

Append to `core-agentic/tests/orchestrator_loop.rs`:

```rust
// New helper: a mock command tool that exercises streaming (or just use the
// real RunCommandTool with a shell command that prints lines then exits).
#[test]
fn tool_lifecycle_events_emitted_in_order() {
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        support::tool_call_response("call-1", "run_command",
            &serde_json::json!({"command": "printf 'a\\nb\\n'",})),
        support::text_response("done"),
    ]));
    let tools = ToolRegistry::new();
    for t in builtin_tools_with_tracker(Arc::new(FileTracker::new())) { tools.register(t); }
    let orchestrator = Orchestrator::new(provider, tools);

    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<core_agentic::Event>::new()));
    let s2 = seen.clone();
    orchestrator.on_event(move |e| s2.lock().unwrap().push(e));

    let out = orchestrator.run("run it").unwrap();   // sync path -> handle_tool_calls
    assert_eq!(out, "done");

    let events = seen.lock().unwrap();
    // sanity: we saw ToolStart then a ToolOutput for run_command with a
    // duration_ms > 0.
    let starts = events.iter().filter(|e| matches!(e, Event::ToolStart{..})).count();
    let outputs = events.iter().filter(|e| matches!(e, Event::ToolOutput{..})).count();
    assert!(starts >= 1, "expected ToolStart events");
    assert!(outputs >= 1, "expected ToolOutput events");
    if let Some(Event::ToolOutput{ duration_ms, .. }) =
        events.iter().find(|e| matches!(e, Event::ToolOutput{ tool_name, .. } if tool_name=="run_command"))
    {
        assert!(*duration_ms > 0);
        let _ = duration_ms;
    }
}
```

- [x] **Step 7: Run the test suite**

Run: `cargo test -p core-agentic`
Expected: seluruh suite core-agentic pass, termasuk test lifecycle baru dan semua test `orchestrator_loop.rs` existing.

- [x] **Step 8: Commit**

```bash
git add core-agentic/src/orchestrator/mod.rs core-agentic/src/orchestrator/tool_exec.rs core-agentic/src/orchestrator/progress.rs core-agentic/src/orchestrator/messages.rs core-agentic/tests/orchestrator_loop.rs
git commit -m "feat(core): emit tool lifecycle events + wire streaming execution"
```

---

## Task 7: Renderer — live tool card (inline + TUI)

**Files:**
- Modify: `agentic-cli/src/commands.rs` (`render_event` + the event forwarding at `~1650`)
- Modify: `agentic-cli/src/tui/app.rs` (two event match sites)
- Test: `agentic-cli/src/commands.rs` (a unit test extracted from render logic)

**Justifications:**
- Interactive + `agentic run` use `commands.rs::run` → `render_event`.
- Full-screen TUI (`app.rs`) has its own events-to-log; keep it rendering the new events so mode doesn't silently regress.

**Interfaces:**
- Consumes: `Event::ToolStart` / `ToolDelta` / enriched `ToolOutput` from Task 1.

**Catatan testing:** renderer menulis ke stdout; untuk unit-test, pindahkan logika format delta ke fungsi murni `fn render_tool_delta(tool_name: &str, delta: &str) -> Vec<RLine>` dan test fungsi itu (bukan stdout capture).

- [x] **Step 1: Render `ToolStart` / `ToolDelta` di `render_event`**

Di `render_event`, tambahkan match arm:

```rust
core_agentic::Event::ToolStart { tool_name, .. } => {
    // "⟳ run_command" running header (reuse tool_call make running line/simple)
    inline::print_line(&RLine::from(RSpan::styled(
        format!("  ⟳ {}", tool_name),
        RStyle::default().fg(RColor::Indexed(247)),
    )));
}
core_agentic::Event::ToolDelta { delta, .. } => {
    for line in delta.lines() {
        if line.trim().is_empty() { continue; }
        inline::print_line(&RLine::from(RSpan::styled(
            format!("    {}", line),
            RStyle::default().fg(RColor::Indexed(244)).add_modifier(RModifier::DIM),
        )));
    }
}
```

And for enriched `Event::ToolOutput`, extend the existing match arm to read `duration_ms` / `success` / `truncated`:

```rust
core_agentic::Event::ToolOutput {
    tool_name,
    output,
    duration_ms,
    truncated,
    ..
} => {
    // ...logika existing (is_error, diff, dst)...

    // Tambah durasi sebagai span DIM di baris terpisah (hindari refactor
    // `render_result_compact`):
    if *duration_ms > 0 {
        inline::print_line(&RLine::from(RSpan::styled(
            format!("   ({:.1}s)", *duration_ms as f64 / 1000.0),
            RStyle::default().add_modifier(RModifier::DIM),
        )));
    }
    if *truncated {
        inline::print_line(&RLine::from(RSpan::styled(
            "   … output dipotong (lihat hasil penuh di memory model)",
            RStyle::default().add_modifier(RModifier::DIM),
        )));
    }
}
```

- [x] **Step 2: Forward `ToolDelta` in the interrupt coordinator**

The event forwarding at `commands.rs:1650` currently skips `Thought` and passes everything else. Add same pass-through (no filter) because `ToolDelta` must reach renderer. Verify it already passes through (only Thought filtered). Leave as-is; confirm.

- [x] **Step 3: TUI app.rs render new events**

At `tui/app.rs:877` and `:1399`, add arms in both `match`es to append ToolStart (running marker) + latest ToolDelta into the tool activity log (reuse an existing render helper if present; otherwise append wrapped text `[⟳ tool] delta-line`). Enriched ToolOutput already handled; add a duration maybe later — minimal now:

```rust
core_agentic::Event::ToolStart { tool_name, .. } => { /* push log line "⟳ tool started" */ }
core_agentic::Event::ToolDelta { delta, .. } => {
    for line in delta.lines() { if !line.trim().is_empty() { /* push log line indented */ } }
}
```

Keep it small, gated by the same `show_tool_calls` toggle if present.

- [x] **Step 4+: Build + manual smoke test**

Run: `cargo build -p agentic-cli`
Run a manual smoke test:
```
agentic run "run `printf 'line1\nline2'\n` then echo done"
```
Expected: a `⟳` header, the `line1`/`line2` deltas dim, then `✓ run_command — 0.0s`, then final answer.

Also test interactive quickly: `agentic interactive`, submit a prompt that uses `run_command` with a grep/ls to see the header + evidence before vs after.

- [x] **Step 5: Commit**

```bash
git add agentic-cli/src/commands.rs agentic-cli/src/tui/app.rs
git commit -m "feat(cli): render live tool deltas + enriched output inline & TUI"
```

---

## Task 8: Docs + task-file updates

**Files:**
- Modify: `docs/ROADMAP.md`
- Modify: `tasks/tasks-core-agentic.md`
- Modify: `tasks/tasks-agentic-cli.md`
- Modify: `docs/COMPARISON_PI_VS_AGENTIC.md`

- [x] **Step 1: Update docs**: mark Fase 1 landed (mark Fasev1 done; note lingering open items for Fase 2 steering + Fase 3).
- [x] **Step 2: Update task files**: add/complete Fase 1 subtasks with checkboxes `- [x]`.
- [x] **Step 3: Update the comparison doc**: strike the "event granularity" and "per-tool progress" entries to done-in-fase-1; note steering still pending (Fase 2).
- [x] **Step 4: Commit**

```bash
git add docs/ROADMAP.md tasks/tasks-core-agentic.md tasks/tasks-agentic-cli.md docs/COMPARISON_PI_VS_AGENTIC.md
git commit -m "docs: mark Fase 1 tool-lifecycle + live output done"
```

---

## Self-Review

### 1. Spec coverage
Spec (Fase 1) menuntut: Tool lifecycle events (`ToolStart`/`ToolDelta`/`ToolOutput` kaya) → Task 1; hook `execute_streaming` default non-breaking + registry → Tasks 2–3; streaming `run_command`/`run_script` → Tasks 4/5; orchestrator emit + throttler + integration test → Task 6; renderer inline + TUI → Task 7; dropout docs → Task 8. ✅ Semua tercakup.

### 2. Placeholder scan
Tidak ada "TODO/TBD". Task 6/7 punya deskripsi konkret dengan kode. Test yang awalnya kabur (`registry_forwards_streaming_callback`) sudah diganti jadi test lengkap di Task 3 Step 1. ✅

### 3. Type consistency
- `Event::ToolStart{..}/ToolDelta{..}` names and fields match Task 1 definition and are used identically in Task 6/7. ✅
- `Tool::execute_streaming(&self, Value, &dyn Fn(&str)) -> ToolResult<Value>` matches Task 2, 3, 4, 5. ✅
- `ToolRegistry::execute_streaming_by_name(&self, &str, &Value, &dyn Fn(&str)) -> Result<Value, ToolError>` matches Task 3/6. ✅
- `DeltaThrottler` returned by Task 6 Step1, used in same Task. ✅
- `Event::ToolOutput` new fields used by both Task 6 & 7. ✅

### 4. Contradictions
- Task 4 reads stdout fully, then stderr (not interleaved). Noted as intentional Fase-1 limitation; doesn't violate spec (spec only says "output live", not streaming-interleaved). ✅
- Task 6 only streams for batch-on-one (run_command/run_script); read-only batches no-op. Consistent with Global Constraint. ✅

---

## Execution Handoff

Plan Fase 1 selesai. Pilihan eksekusi:

**1. Subagent-Driven (rekomendasi)** — dispatch subagent fresh per task, review di antara task, iterasi cepat.

**2. Inline Execution** — eksekusi task di sesi ini dengan `executing-plans`, batch + checkpoint review.

Catatan: Fase 2 (steering) dan Fase 3 (paralel) punya plan sendiri setelah Fase 1 land (lihat spec `docs/superpowers/specs/2026-08-06-interactive-live-progress-and-steering-design.md`).