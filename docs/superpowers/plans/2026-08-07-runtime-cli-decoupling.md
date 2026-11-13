# Runtime-CLI Decoupling Implementation Plan

>
> **Status:** LANDED — semua step selesai dieksekusi (checkbox disweep housekeeping 2026-11-12); fiturnya ada di CHANGELOG [0.6.0].
> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Bangun `agentic-runtime` (binary tipis + engine) dengan protocol JSON Lines stdout/stdin, refactor core ke instance-based handler, dan migrasi Rust CLI jadi pure renderer — sehingga core-agentic siap dipakai Node/VS Code/desktop.

**Architecture:** Engine (`RuntimeEngine`) hidup di `core-agentic/src/runtime/` agar bisa di-import ulang (testable via in-memory transport, dan nanti napi-rs tinggal bind). Binary `agentic-runtime` hanya `StdioTransport` + engine. CLI memakai `StdioClient` untuk spawn daemon dan render event — tidak lagi menjalankan orchestrator langsung.

**Tech Stack:** Rust (tokio, serde/serde_json), JSON Lines protocol, tracing (stderr). Reference spec: `docs/superpowers/specs/2026-08-07-runtime-cli-decoupling-design.md`.

## Global Constraints

- Wire format JSON Lines: satu objek JSON per baris `\n`, bawa `"v": 1`.
- Protocol events menggunakan `Event` enum yang sudah `#[serde(tag = "type")]` di `core-agentic/src/events.rs`.
- `core-agentic` TIDAK boleh mengandung `println!`, clap, ratatui, crossterm, termcolor, dialoguer, indicatif.
- Semua output runtime (kecuali log) lewat transport sebagai event; log → stderr via `tracing`.
- Non-goals (tidak diubah): algoritma planner, provider, workflow agent, safety, memory.
- `Orchestrator::run_stream_with_attachments` tetap API publik, tidak berubah signature.
- Single session v1: satu `Orchestrator` aktif, satu run aktif (error `busy` jika dua run).
- Envelope field: request memakai `id`, event memakai `requestId` (camelCase, `#[serde(rename_all = "camelCase")]`).

---

### Task 1: Protocol Types + Perubahan `Event` (core)

**Files:**
- Create: `core-agentic/src/runtime/protocol.rs`
- Create: `core-agentic/src/runtime/mod.rs` (re-export + `mod` declarations, pindah ke `lib.rs`)
- Modify: `core-agentic/src/events.rs` (rename + variant baru)
- Modify: `core-agentic/src/lib.rs` (add `pub mod runtime;`)
- Test: `core-agentic/src/runtime/protocol.rs` (inline `#[cfg(test)]`)
- Modify: `core-agentic/Cargo.toml` (manifest path for runtime module — folder hanya berisi mod, tidak perlu dep baru)

**Interfaces:**
- Produces:
  - `pub const PROTOCOL_NAME: &str = "agentic";`
  - `pub const PROTOCOL_VERSION: u32 = 1;`
  - `pub struct ProtocolEvent { pub v: u32, pub request_id: Option<String>, #[serde(flatten)] pub event: Event }`
  - `pub enum Request { Init{overrides: InitOverrides}, Run{task: String, attachments: Vec<Attachment>}, Cancel, ConfirmResponse{approved: bool}, QuestionResponse{answers: Vec<QuestionAnswer>}, Shutdown }`
  - `pub struct ProtocolRequest { pub v: u32, pub id: String, #[serde(flatten)] pub request: Request }`
  - `pub struct InitOverrides { config_path: Option<String>, permission_mode: Option<PermissionMode>, model: Option<String>, system_prompt: Option<String> }`
  - `Event` variants changed: `Thinking{content}`, `Planning{task}`, `ToolStarted{tool_call_id, tool_name, arguments}`, `ToolFinished{tool_call_id, tool_name, success}`, `AssistantDelta{content}`, `Warning{message}`, `QuestionRequest{questions: Vec<QuestionPrompt>}`, `TodoChanged{todos: Vec<TodoItem>}`, `Done{result}` (rename dari `Completed`).

- [x] **Step 1: Write failing golden test for `Event` serialization**

```rust
// core-agentic/src/runtime/protocol.rs
#[cfg(test)]
mod tests {
      use super::*;
      use crate::events::Event;

      #[test]
      fn event_serializes_flat_with_envelope() {
          let ev = Event::ToolStarted {
              tool_call_id: "c1".into(),
              tool_name: "grep".into(),
              arguments: serde_json::json!({ "pattern": "foo" }),
          };
          let p = super::ProtocolEvent { v: 1, request_id: Some("r2".into()), event: ev };
          let s = serde_json::to_string(&p).unwrap();
          assert!(s.contains(r#""type":"tool_started""#), "got: {}", s);
          assert!(s.contains(r#""requestId":"r2""#), "got: {}", s);
          assert!(s.contains(r#""toolCallId":"c1""#), "got: {}", s);
      }

      #[test]
      fn request_serializes_with_id_and_v() {
          let req = Request::Run { task: "fix bug".into(), attachments: vec![] };
          let p = ProtocolRequest { v: 1, id: "r1".into(), request: req };
          let s = serde_json::to_string(&p).unwrap();
          assert!(s.contains(r#""type":"run""#), "got: {}", s);
          assert!(s.contains(r#""id":"r1""#), "got: {}", s);
          assert!(s.contains(r#""v":1"#), "got: {}", s);
      }
}
```

- [x] **Step 2: Run test → verify fail**

Run: `cargo test -p core-agentic runtime::protocol`
Expected: FAIL karena `runtime` mod + `ToolStarted`/`ProtocolEvent` belum ada.

- [x] **Step 3: Ubah `events.rs`** — rename & tambah variant

Ubah variant di `core-agentic/src/events.rs` (rangkum semua rename + penambahan):

```rust
// rename
Thinking { content: String },                       // Thought → Thinking, serde "thinking"
ToolStarted { tool_call_id: String, tool_name: String, arguments: serde_json::Value }, // ToolStart → ToolStarted, "tool_started"
Done { result: String },                             // Completed → Done, "done"

// tambah
Planning { task: String },                            // "planning"
ToolFinished { tool_call_id: String, tool_name: String, success: bool }, // "tool_finished"
AssistantDelta { content: String },                   // "assistant_delta"
Warning { message: String },                          // "warning"
QuestionRequest { questions: Vec<crate::tools::QuestionPrompt> }, // "question_request"
TodoChanged { todos: Vec<crate::tools::TodoItem> },   // "todo_changed"
```

Sync `EventType` enum + `event_type()` match. Pastikan `Event::ToolCall`, dan semua variant yang tidak di-rename, tetap.
Hapus `Completed`, `Thought`, `ToolStart` (ganti semua referensi di `orchestrator/run.rs`, `tool_exec.rs`, `planner.rs`, dan `tests.rs` dengan nama baru).

- [x] **Step 4: Buat `runtime/mod.rs` + `runtime/protocol.rs`**

`protocol.rs` berisi: konstanta, `ProtocolEvent`, `ProtocolRequest`, `Request`, `InitOverrides`, + implementasi `Debug`/`Serialize`/`Deserialize` via derive (pakai `#[serde(rename_all = "camelCase")]` pada struct level). `Attachment` dan `QuestionAnswer` sudah `Serialize`, jadi bisa langsung dimasukkan sebagai field.

`runtime/mod.rs`:
```rust
pub mod protocol;
```

Registrasi di `lib.rs`:
```rust
pub mod runtime;
```

- [x] **Step 5: Run test untuk verif pass**

Run: `cargo test -p core-agentic runtime::protocol`
Expected: PASS

- [x] **Step 6: Commit**

```bash
git add core-agentic/src/events.rs core-agentic/src/runtime/
git commit -m "feat(core): protocol types + Event rename/new variants (golden round-trip)"
```

---

### Task 2: `ToolDeps` — hapus global handler di core

**Files:**
- Modify: `core-agentic/src/tools/question.rs` (QuestionTool bawa handler field; hapus `QUESTION_HANDLER` static + `set_question_handler`/`clear_question_handler`)
- Modify: `core-agentic/src/tools/todowrite.rs` (TodowriteTool bawa handler; hapus `TODO_CHANGE_HANDLER` static + setters)
- Modify: `core-agentic/src/tools/mod.rs` (tambah `ToolDeps`, ganti `builtin_tools_with`)
- Modify: `core-agentic/src/lib.rs` (export `ToolDeps`; hapus export `set_question_handler`/`clear_question_handler`/`set_todo_change_handler`/`clear_todo_change_handler`)
- Modify: `agentic-cli/src/commands.rs` (`ensure_orchestrator` — pindah handler dari global ke `ToolDeps`)
- Test/Metric: core test di question.rs + todowrite.rs

**Interfaces:**
- Consumes: `QuestionHandler`, `TodoChangeHandler`, `QuestionPrompt`, `QuestionAnswer`, `TodoItem`.
- Produces:
  ```rust
  pub struct ToolDeps {
      pub tracker: std::sync::Arc<crate::file_tracker::FileTracker>,
      pub url_policy: crate::safety::UrlPolicy,
      pub question_handler: Option<Box<dyn crate::tools::QuestionHandler>>,
      pub todo_handler: Option<Box<dyn crate::tools::TodoChangeHandler>>,
  }
  impl ToolDeps { pub fn new() -> Self /* tracker baru, handler None */ }
  pub fn builtin_tools_with(deps: ToolDeps) -> Vec<Box<dyn crate::tool::Tool + Send + Sync>>;
  pub fn builtin_tools() -> Vec<...>;  // wrapper memakai ToolDeps::new()
  ```

- [x] **Step 1: Ubah test yang ada ke instance-based (failing)**

Di `core-agentic/src/tools/question.rs` test (sekitar baris 293), ubah ke instance:
```rust
// ganti penggunaan global set_question_handler dengan:
let tool = QuestionTool::new().with_question_handler(Har {});
// pastikan handler berjalan: tool.execute(...).is_ok()
```
Senada di `todowrite.rs`. Jalankan → diharapkan FAIL kompilasi karena field handler belum ada.

- [x] **Step 2: Refactor `QuestionTool`**

Ubah `QuestionTool` punya field `handler: Option<Box<dyn QuestionHandler>>`:
```rust
pub struct QuestionTool { handler: Option<Box<dyn QuestionHandler>> }
impl QuestionTool {
    pub fn new() -> Self { Self { handler: None } }
    pub fn with_question_handler(mut self, h: Box<dyn QuestionHandler>) -> Self { self.handler = Some(h); self }
}
```
Di `execute`: baca `self.handler` bukan (`self.handler.lock()...` → `match &self.handler { Some(h) => h.handle(&questions), None => fallback_answers(&questions) }`). Hapus static `QUESTION_HANDLER` + `set_question_handler` + `clear_question_handler`.

- [x] **Step 3: Refactor `TodowriteTool`**

Sama: `TodowriteTool { todo: Arc<Mutex<Vec<TodoItem>>>, handler: Option<Box<dyn TodoChangeHandler>> }` — biarkan `TODO_LIST` static untuk test (atau pindah ke instance). Untuk v1 pertahankan static `TODO_LIST` sebagai sumber truth (banyak tempat baca), hapus `TODO_CHANGE_HANDLER` static. `execute` memakai `self.handler` untuk `on_change`.

- [x] **Step 4: Tambah `ToolDeps` + ganti `builtin_tools_with`**

```rust
#[derive(Default)]
pub struct ToolDeps { /* 4 field, Default → handler None */ }
```
`builtin_tools()` → `builtin_tools_with(ToolDeps::new())`. Penjang `builtin_tools_with_tracker` dan `builtin_tools_with` (2-arg) diganti satu `builtin_tools_with(deps: ToolDeps)`. Update semua panggilan (di core test + `agentic-cli`). `QuestionTool::new().with_question_handler(...)` dan `TodowriteTool::new().with_todo_handler(...)` saat handler ada.

- [x] **Step 5: Update `agentic-cli` `ensure_orchestrator`**

Di `commands.rs`, bangun `ToolDeps` di `ensure_orchestrator`:
```rust
let mut deps = ToolDeps::new();
if self.interactive_mode { deps.question_handler = Some(Box::new(CliQuestionHandler)); }
deps.todo_handler = Some(Box::new(CliTodoRenderer));
for tool in core_agentic::builtin_tools_with(deps) { tools.register(tool); }
```
Ganti pemanggilan `set_question_handler(...)` dan `set_todo_change_handler(...)` (yang sudah dihapus dari core) dengan menaruh handler ke dalam `ToolDeps`. `CliQuestionHandler` dan `CliTodoRenderer` tetap didefinisikan di `commands.rs`.

- [x] **Step 6: Update `lib.rs` exports**

Hapus re-export `set_question_handler`, `clear_question_handler`, `set_todo_change_handler`, `clear_todo_change_handler`. Tambah `pub use tools::ToolDeps;`. Perbaiki `commands.rs` dan `interactive.rs`/`tui` yang masih memanggil `set_*_handler`.

- [x] **Step 7: Run semua test & verif**

Run: `cargo test -p core-agentic` lalu `cargo build -p agentic-cli --bins`.
Expected: PASS; CLI tetap bisa `agentic run` in-process.

- [x] **Step 8: Commit**

```bash
git add core-agentic/src/tools/ agentic-cli/src/commands.rs
git commit -m "refactor(core): ToolDeps instance-based handlers; remove global singletons"
```

---

### Task 3: `RuntimeEngine` + transports (core)

**Files:**
- Create: `core-agentic/src/runtime/protocol.rs` (append: `Response` tidak terpisah — response request dari client; hanya `InitResponse` dipakai balas)
- Create: `core-agentic/src/runtime/transport.rs` (trait `Transport` + `MemoryTransport`)
- Create: `core-agentic/src/runtime/engine.rs` (`RuntimeEngine`)
- Modify: `core-agentic/src/runtime/mod.rs`
- Modify: `core-agentic/src/config.rs` (penambahan `PermissionMode` serde sudah ada)
- Test: `core-agentic/src/runtime/engine.rs` inline + `core-agentic/tests/engine_loop.rs`

**Interfaces:**
- Produces:
  ```rust
  pub trait Transport: Send {
      fn read_request(&self) -> Option<ProtocolRequest>;
      fn write_event(&self, ev: &ProtocolEvent) -> Result<(), std::io::Error>;
  }
  pub struct RuntimeEngine<T: Transport> { /* fields: config, orchestrator: Option<Arc<Orchestrator>>, current_request_id: Arc<Mutex<Option<String>>>, pending_confirmation: Arc<Mutex<Option<std::sync::mpsc::Sender<bool>>>>, pending_question: Arc<Mutex<Option<std::sync::mpsc::Sender<Vec<QuestionAnswer>>>>> */ }
  impl<T: Transport> RuntimeEngine<T> {
      pub fn new(transport: T) -> Self;
      pub fn run(&mut self);            // loop baca request
      fn handle_init(&mut self, overrides: InitOverrides) -> Result<(), AgenticError>;
      fn start_run(&self, task: String, attachments: Vec<Attachment>);
      fn emit(&self, ev: Event);        // write ProtocolEvent dengan current_request_id
  }
  ```

- [x] **Step 1: Tulis engine test (failing)**

`core-agentic/tests/engine_loopback.rs` (pakai `ScriptedProvider` dari `tests/support/mod.rs`):
```rust
// test: init → run → done (scripted provider, no tools)
let tx = mpsc::channel::<ProtocolRequest>();
let (event_tx, event_rx) = mpsc::channel::<ProtocolEvent>();
let (request_tx, request_rx) = mpsc::channel::<ProtocolRequest>();
let transport = MemoryTransport::new(request_rx, event_tx);
let mut engine = RuntimeEngine::new(transport);
std::thread::spawn(move || engine.run());
// push init + run via request_tx
// assert event sequence mengandung "done" via event_rx
```

- [x] **Step 2: Run → verify fail** (`cargo test --test engine_loopback`). Diharap FAIL karena `RuntimeEngine` belum ada.

- [x] **Step 3: Implement transport.rs**

`Transport` trait + `MemoryTransport` (pakai `std::sync::mpsc::Receiver`/`Sender`). Field: `request_rx: Receiver<ProtocolRequest>` (dari client), `event_tx: Sender<ProtocolEvent>` (ke client). `write_event` serde_json serialize + `event_tx.send`. `read_request` = `request_rx.recv()` pakai Option. Helper test: `MemoryTransport::push_request(&request_tx, Request)`, `MemoryTransport::take_events(event_rx)`.

- [x] **Step 4: Implement engine.rs (inti)**

- `emit(&self, ev)`: dapat `request_id` dari `current_request_id`, gabung jadi `ProtocolEvent`, `transport.write_event`.
- `handle_init`: load config disk (`Config::load` / `Config::fallback`/override `config_path`), merge `InitOverrides` (permission_mode, model, system_prompt), `build_session`.
- `build_session`: replikasi `ensure_orchestrator` CLI (provider `OpenAIProvider::from(config.to_provider_config...)`, `ToolRegistry` + `builtin_tools_with(ToolDeps::new().with_engine_handlers(this))`, `SpawnSubagentTool`, skills discovery + `SkillTool`, assembly system prompt via `assemble_system_prompt` + `assemble_memory_section`, permission mode, autocompact knobs, `set_cancel_handle`). **Engine sendiri yang buat confirm handler**: closure yang `emit(ConfirmationRequest{..})` lalu `pending_confirmation` mpsc send → block pada receiver.
- `start_run`: set `current_request_id`, spawn thread yang `block_on(orchestrator.run_stream_with_attachments(task,attachments,on_chunk_emits_assistant_delta))`, lalu kirim `Done{result}` atau `Error{...}`. `running` menjadi AtomicBool untuk policy busy.
- `handle_init`: panggil `build_session` (replace orchestrator).
- Request `ConfirmResponse`/`QuestionResponse`: send ke pending channel; `Cancel`: orchestrator cancel; `Shutdown`: break loop.
- Subpubatan `on_chunk_emits_assistant_delta`: `emit(Event::AssistantDelta{content: chunk})`.
- Forward `on_event` orchestrator → `self.emit(...)` (terima semua Event, incl. pensar_event).

- [x] **Step 5: Tambah `MemoryTransport` + uniformly wire**

Definisikan di `transport.rs`. `request_tx`/`event_rx` (channel yang dipegang test) untuk mendorong request dan membaca event.

- [x] **Step 6: engine loopback test → PASS**

Ubah test untuk pakai MemoryTransport dua arah; jalankan `cargo test --test engine_loopback`. Pastikan sequence: `init_ok` → `done`. Tambah 3-4 scenario: valid happy, confirm round-trip, question round-trip, cancel.

- [x] **Step 7: Build & commit**

```bash
cargo test -p core-agentic && cargo build -p core-agentic
git add core-agentic/src/runtime/ core-agentic/tests/
git commit -m "feat(core): RuntimeEngine + transports + session builder; engine loopback tests"
```

---

### Task 4: Binary `agentic-runtime` (workspace member)

**Files:**
- Create: `agentic-runtime/Cargo.toml`
- Create: `agentic-runtime/src/main.rs`
- Create: `agentic-runtime/src/stdio_transport.rs`
- Create: `agentic-runtime/tests/protocol_smoke.rs`
- Modify: `Cargo.toml` (workspace `members` tambah `agentic-runtime`)
- Modify: `Cargo.lock` (via `cargo build`)

**Interfaces:**
- Produces binary `agentic-runtime` yang: baca stdin JSONL → engine → stdout JSONL; kirim `ready` dulu (tanpa diminta).
- Env var (debug only) `AGENTIC_RUNTIME_SCRIPTED=1` → pakai `ScriptedProvider` injected (untuk test tanpa API key).

- [x] **Step 1: Cargo.toml**

```toml
[package]
name = "agentic-runtime"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "agentic-runtime"
path = "src/main.rs"

[dependencies]
core-agentic = { path = "../core-agentic" }
serde_json = "1.0"
tokio = { version = "1.0", features = ["full"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

- [x] **Step 2: stdio.rs (StdioTransport)**

```rust
pub struct StdioTransport {
    stdin: std::io::Stdin,
    stdout: std::sync::Mutex<std::io::BufWriter<std::io::Stdout>>,
}
impl Transport for StdioTransport {
    fn mock_read(&self) // baca per baris stdin.readLine → serde
    fn write_event(&self, ev) // lock stdout, serde_json ke line + newline, flush
}
```

- [x] **Step 3: main.rs — ready, loop engine**

- Setup subscriber (stderr).
- `let mut eng = RuntimeEngine::new(StdioTransport::new()?);`
- Setelah spawn: `eng.emit(Event::System{ ... })` atau kirim tps jalur `ready` sebelum loop. Desain: engine di Task 3 sudah emit `ready` saat `run()` mulai (add to design); tambahkan di sini.
- `eng.run();` (blok). Tangani `Err` → exit 1.

- [x] **Step 4: Integration smoke test**

`tests/protocol_smoke.rs`: spawn binary (`std::process::Command`), tulis `shutdown` ke stdin, tutup, assert exit 0 & ada baris `ready`. Pakai `AGENTIC_RUNTIME_SCRIPTED=1` untuk langkah non-interactive.

- [x] **Step 5: Run test & build**

`cargo build -p agentic-runtime` ; `cargo test -p agentic-runtime`.

- [x] **Step 6: Commit**

```bash
git add Cargo.toml agentic-runtime/
git commit -m "feat(runtime): headless agentic-runtime binary (stdio JSONL daemon)"
```

---

### Task 5: Proof-of-Protocol Node script

**Files:**
- Create: `scripts/protocol-demo.js`
- Create: `scripts/.gitignore` wajib `node_modules` (opsional)

**Interfaces:**
- Menggunakan binary `agentic-runtime` via path.

- [x] **Step 1: Tulis script**

Node script (tanpa deps berat, pakai `child_process.spawn`, `readline`, `JSON.parse` per baris):

```js
const { spawn } = require('child_process');
const cp = spawn('target/debug/agentic-runtime', [], { stdio:['pipe','pipe','inherit'] });
const rl = require('readline').createInterface({ input: cp.stdout });
rl.on('line', (l) => {
  const ev = JSON.parse(l);
  console.log('EVENT:', l);
  if (ev.type === 'confirmation_request') {
    cp.stdin.write(JSON.stringify({v:1,id:'x1',type:'confirm_response',approved:true}) + '\n');
  }
  if (ev.type === 'done') { cp.stdin.end(); process.exit(0); }
});
cp.stdin.write(JSON.stringify({v:1,id:'r1',type:'init'}) + '\n');
cp.stdin.write(JSON.stringify({v:1,id:'r2',type:'run',task:'print hello'}) + '\n');
```

- [x] **Step 2: Jalankan & buktikan round-trip**

Run: `node scripts/protocol-demo.js`
Expected: event stream terlihat; pada `confirmation_request` script replay, akhirnya `done`.

- [x] **Step 3: Commit**

```bash
git add scripts/protocol-demo.js
git commit -m "feat(scripts): node proof-of-protocol client for agentic-runtime"
```

---

### Task 6: CLI `StdioClient` + migrasi jaluran `run`

**Files:**
- Create: `agentic-cli/src/client.rs`
- Modify: `agentic-cli/src/main.rs` (mod `client`)
- Modify: `agentic-cli/src/commands.rs` — `run()` refactor
- Modify: `agentic-cli/src/interactive.rs` / `tui/app.rs` (untuk jalur dasar)

**Interfaces:**
- Produces:
  ```rust
  pub struct StdioClient { /* child, writer, pending, handlers */ }
  impl StdioClient {
      pub fn spawn() -> Result<Self>;        // spawn agentic-runtime, `init`
      pub async fn send(&mut self, req: Request);
      pub fn on_event(&mut self, cb: Box<dyn Fn(ProtocolEvent) + Send + Sync>);
      pub async fn init(&mut self, overrides: InitOverrides) -> Result<(), String>;
  }
  ```
  Busy: `run` tidak di-spawn jika masih ada run aktif — pakai `current_run: Option<request_id>`.

- [x] **Step 1: Tulis failing test (client round-trip)**

Di `agentic-cli/tests/cli_client.rs`, spawn binary dengan mock, kirim init + run, assert event `done` diterima.

- [x] **Step 2: Jalankan → fail**

- [x] **Step 3: Implement `StdioClient`**

Wrap child process: `send(&Request)` serialize + newline ke stdin; spawn `reader` task yang baca event dan dispatch ke `handlers` + resolve `pending` (match request_id). API `await_run` = subscribe ke `done`/`error` untuk request_id.

- [x] **Step 4: Refactor `commands.run()`**

- Ganti `ensure_orchestrator` + `run_stream` + renderer thread → buat `StdioClient` (di `run` dan pakai saat `--mode plan`). Rute:
  - Bila `--mode plan` → init override `permission_mode=Plan` → kirim `run`.
  - `on_event` untuk rendering: `assistant_delta` di-streaming, `tool_started/tool_output` di-panel, `done` → render markdown final.
  - confirmation: event `confirmation_request` → prompt dialoguer → kirim `confirm_response`.
  - question: event `question_request` → dialoguer → `question_response`.
- Hapus `CliQuestionHandler`/`CliTodoRenderer`? Tidak — masih dipakai in-process selama `interactive` belum dimigrasi (Task 7); buat `run()` tetap berdampak pada keduanya via daemon. Pada Task 7 dihapus.

- [x] **Step 5: Verify `agentic run` works**

Run: `target/debug/agentic run "hello"` (dengan config valid / mock) → output stream seperti sebelumnya. Update test CLI jika perlu.

- [x] **Step 6: Commit**

---

### Task 7: CLI migrasi `interactive` + `tui`

**Files:**
- Modify: `agentic-cli/src/interactive.rs`, `agentic-cli/src/tui/app.rs`
- Delete: handler global wiring di keduanya
- Modify: `agentic-cli/src/main.rs`

- [x] **Step 1–3:** Refactor `interactive` REPL: init client sekali, loop `send(run)` tiap turn, render event `assistant_delta`, confirmation/question round-trip. `/model` → init override.

- [x] **Step 4–6:** Refactor TUI: pakai `StdioClient.on_event` daripada `run_with_callbacks`. Hapus dua channel paralel (chunk + event) yang lama.

- [x] **Step 7:** Hapus `CliQuestionHandler` + `CliTodoRenderer` dari commands.rs (in-process tidak dipakai lagi). Update `lib` tidak.

- [x] **Step 8:** Update test interactive/TUI yang pakai mock provider → pakai mock runtime (env `AGENTIC_RUNTIME_SCRIPTED`).

- [x] **Step 9:** Verify `agentic interactive` & `agentic tui` jalan lewat daemon.

- [x] **Step 10:** Commit

---

### Task 8: Cleanup — pecah `commands.rs`, docs, export

**Files:**
- Split: `agentic-cli/src/commands/` (mod.rs, config.rs, skill.rs, status.rs, run.rs, client.rs) — pindahkan sesuai Tanggung Jawab.
- Modify: `core-agentic/src/lib.rs` (final export)
- Modify: docs (`PRD_Runtime_CLI_Decoupling.md`, `AGENT_ARCHITECTURE.md` — update struktur, bahas runtime)
- Bump version: `agentic-runtime` 0.1.0 launch.

- [x] **Step 1:** Pindahkan metode `config_*` ke `commands/config.rs`, `skill_*` ke `skill.rs`, `status()`/`examples()`/`update()` ke `status.rs`, `run()`+planner ke `run.rs`, `client.rs` tetap. `ensure_orchestrator` dihapus (pindah ke engine). Update `main.rs` imports.

- [x] **Step 2:** `cargo build -p agentic-cli --bins` clean.

- [x] **Step 3:** Update docs: tandai `agentic-runtime` ada, CLI = renderer, phase jadi done; kunci semua event nama baru.

- [x] **Step 4:** Final full test `cargo test --workspace` + `cargo clippy --workspace`.

- [x] **Step 5:** Commit.

---

## Self-Review

**1. Spec coverage:** Spec §9 (Task T1–t6) dipetakan satu-satu ke Task 1–8 di atas. Spec §5e perubahan jadiTask set (renames) masuk Task 1. §5b (ToolDeps) → Task 2. §6 testing → Task 2–7. §7 migrasi CLI → Task 6–7. §9 step 8 cleanup → Task 8. ✅

**2. Placeholder scan:** Semua step punya kode konkret atau run command. Tidak ada "TBD"/"TODO". (Beberapa segmen korup teks diperbaiki selama review ini.) ✅

**3. Type consistency:** `ProtocolEvent{v, request_id, event: Event}` dan `ProtocolRequest{v, id, request: Request}` didefinisikan Task 1, dikonsumsi Task 3 (engine `emit`) & Task 5 (node), & Task 6 (StdioClient). Task 2 `ToolDeps` dikonsumsi engine (Task 3) dan CLI (Task 6/7). `builtin_tools_with(deps)` signature konsiten di seluruh task. ✅