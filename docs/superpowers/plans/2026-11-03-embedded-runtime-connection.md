# Embedded Runtime Connection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Mode runtime **embedded** (engine berjalan sebagai thread in-process via `MemoryTransport`) menjadi default CLI, dengan jalur spawn proses `agentic-runtime` dipertahankan sebagai escape hatch `--runtime spawn` — keduanya di belakang satu abstraction `AgentConnection` tanpa perubahan protokol.

**Architecture:** `agentic-cli/src/client.rs` menambahkan `trait ConnectionIo` + helper bersama (handshake, send_and_wait, run-loop) sehingga `RuntimeClient` dan `InProcessClient` baru berbagi satu implementasi logika. `InProcessClient` menjalankan `RuntimeEngine` (core, tidak diubah) di native thread + bridge thread ke tokio. `Commands` berganti field ke `Option<AgentConnection>`; flag global `--runtime` memilih mode.

**Tech Stack:** Rust (edition 2021, rustc ≥ 1.75 untuk RPITIT), tokio (features `full` — `sync::mpsc` tersedia), `std::sync::mpsc`, `std::thread`. Tanpa dependensi baru. Reference spec: `docs/superpowers/specs/2026-11-03-embedded-runtime-connection-design.md`.

## Global Constraints

- Protokol TIDAK berubah: `ProtocolRequest`/`ProtocolEvent`, `PROTOCOL_VERSION = 1`, handshake `Ready → Init → InitOk`.
- `core-agentic` tidak menerima perubahan apa pun di plan ini (engine + `MemoryTransport` sudah cukup).
- Tanpa dependensi baru di `agentic-cli/Cargo.toml`.
- Default mode = **embedded**; spawn tetap 100% perilaku lama.
- Nama thread: `agentic-runtime-embedded` (engine) dan `agentic-runtime-bridge` (bridge) — untuk debugging.
- Event error sintetis: `"embedded runtime engine panicked"` (catch_unwind) dan `"embedded runtime engine stopped"` (channel tertutup).
- CI gate: `cargo clippy --workspace --all-targets -- -D warnings` dan `cargo fmt --all --check` harus bersih.
- Branch kerja: `feature/embedded-runtime-connection` dari `dev`.
- Semua test hermetic: fixture config endpoint `http://127.0.0.1:9` (pola `hermetic_runtime_config()` di `agentic-cli/src/commands.rs:3303`) — tidak boleh menyentuh `~/.config/agentic/config.json`.

---

### Task 1: `InProcessClient` + `ConnectionIo` + helper bersama

**Files:**
- Modify: `agentic-cli/src/client.rs`
- Test: `agentic-cli/src/client.rs` (mod `#[cfg(test)]` baru di bagian bawah file)

**Interfaces:**
- Consumes: `core_agentic::runtime::engine::RuntimeEngine<T>` (`RuntimeEngine::new(T)`, `run(&mut self)`), `core_agentic::runtime::transport::MemoryTransport::new(req_rx: mpsc::Receiver<ProtocolRequest>, event_tx: mpsc::Sender<ProtocolEvent>)`, `ProtocolEvent::new(request_id: Option<String>, event: Event)`, `core_agentic::Event::{Ready, InitOk, Error, ToolList}`, `Request::{Init, ListTools, Shutdown}`.
- Produces (dipakai Task 2–3):
  - `pub trait ConnectionIo { fn raw_send(&mut self, request: Request) -> impl Future<Output = Result<String>> + Send; fn raw_next_event(&mut self) -> impl Future<Output = Result<ProtocolEvent>> + Send; }`
  - `pub(crate) async fn perform_handshake(io: &mut impl ConnectionIo, overrides: InitOverrides) -> Result<()>`
  - `pub(crate) async fn send_and_wait_io(io: &mut impl ConnectionIo, request: Request) -> Result<ProtocolEvent>`
  - `pub(crate) async fn run_turn<C, E, R>(io: &mut impl ConnectionIo, task: String, attachments: Vec<Attachment>, on_chunk: C, on_event: E, responder: R) -> Result<String>` (signature closure identik dengan `RuntimeClient::run` hari ini)
  - `pub struct InProcessClient` dengan `pub async fn start(overrides: InitOverrides) -> Result<Self>`, `pub async fn send(&mut self, request: Request) -> Result<String>`, `pub async fn next_event(&mut self) -> Result<ProtocolEvent>`, `pub async fn shutdown(&mut self) -> Result<()>`; mengimplementasikan `ConnectionIo`
  - `pub(crate) fn spawn_engine_thread<T, F>(make_engine: F, request_rx: std_mpsc::Receiver<ProtocolRequest>, event_tx: std_mpsc::Sender<ProtocolEvent>, panic_tx: std_mpsc::Sender<ProtocolEvent>) -> std::thread::JoinHandle<()>` — generik atas transport agar test bisa menyuntik transport panik

- [ ] **Step 1: Tulis test gagal untuk handshake + ListTools embedded**

Tambahkan di akhir `agentic-cli/src/client.rs`:

```rust
#[cfg(test)]
mod in_process_tests {
    use super::*;
    use core_agentic::runtime::protocol::InitOverrides;

    /// Fixture config hermetic yang sama dengan e2e smoke tests di
    /// `commands.rs`: endpoint discard port yang selalu gagal koneksi,
    /// jadi tidak pernah menyentuh provider nyata / config developer.
    fn hermetic_overrides() -> InitOverrides {
        let path = std::env::temp_dir().join(format!(
            "agentic-inproc-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
        .expect("write hermetic in-process config");
        InitOverrides {
            config_path: Some(path.to_string_lossy().into_owned()),
            permission_mode: Some(core_agentic::PermissionMode::Yolo),
            model: None,
            system_prompt: None,
        }
    }

    #[tokio::test]
    async fn in_process_handshake_and_list_tools_round_trip() {
        let mut client = InProcessClient::start(hermetic_overrides())
            .await
            .expect("embedded handshake");
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            client.send_and_wait(Request::ListTools),
        )
        .await
        .expect("ListTools timed out");
        assert!(matches!(event.event, core_agentic::Event::ToolList { .. }));
        client.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn shutdown_joins_engine_thread() {
        let mut client = InProcessClient::start(hermetic_overrides())
            .await
            .expect("embedded handshake");
        tokio::time::timeout(std::time::Duration::from_secs(10), client.shutdown())
            .await
            .expect("shutdown timed out")
            .expect("shutdown failed");
        // Engine + bridge thread harus sudah berakhir setelah join.
        assert!(client.engine_finished());
    }
}
```

- [ ] **Step 2: Jalankan test → verify gagal**

Run: `cargo test -p agentic-cli in_process -- --nocapture`
Expected: COMPILE ERROR — `InProcessClient` tidak ditemukan (`cannot find type InProcessClient`).

- [ ] **Step 3: Implementasi `ConnectionIo`, helper bersama, refactor `RuntimeClient`, dan `InProcessClient`**

Di `agentic-cli/src/client.rs`, tambahkan imports di bagian atas:

```rust
use std::future::Future;
use std::sync::mpsc as std_mpsc;
use tokio::sync::mpsc as tokio_mpsc;
```

Tambahkan trait + helper bersama (letakkan setelah imports, sebelum `RuntimeClient`):

```rust
/// Transport-agnostic IO surface shared by both connection modes, so the
/// handshake and the run loop exist in exactly one place (codex lesson:
/// duplicated handshake logic across clients is a bug waiting to happen).
pub trait ConnectionIo {
    fn raw_send(&mut self, request: Request) -> impl Future<Output = Result<String>> + Send;
    fn raw_next_event(&mut self) -> impl Future<Output = Result<ProtocolEvent>> + Send;
}

/// Identical handshake for both modes: wait `Ready`, send `Init`, wait
/// `InitOk` (or an `Error` explaining why initialization failed).
pub(crate) async fn perform_handshake(
    io: &mut impl ConnectionIo,
    overrides: InitOverrides,
) -> Result<()> {
    let ready = io.raw_next_event().await?;
    if !matches!(ready.event, core_agentic::Event::Ready { .. }) {
        return Err(anyhow!("runtime did not emit ready"));
    }
    io.raw_send(Request::Init { overrides }).await?;
    let initialized = io.raw_next_event().await?;
    match initialized.event {
        core_agentic::Event::InitOk { .. } => Ok(()),
        core_agentic::Event::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected initialization event: {other:?}")),
    }
}

pub(crate) async fn send_and_wait_io(
    io: &mut impl ConnectionIo,
    request: Request,
) -> Result<ProtocolEvent> {
    let id = io.raw_send(request).await?;
    loop {
        let event = io.raw_next_event().await?;
        if event.request_id.as_deref() == Some(&id) {
            return Ok(event);
        }
    }
}

/// Run loop shared by both modes: stream deltas, surface lifecycle events,
/// and answer interactive gates through the connection.
pub(crate) async fn run_turn<C, E, R>(
    io: &mut impl ConnectionIo,
    task: String,
    attachments: Vec<core_agentic::Attachment>,
    mut on_chunk: C,
    mut on_event: E,
    mut responder: R,
) -> Result<String>
where
    C: FnMut(&str),
    E: FnMut(core_agentic::Event),
    R: FnMut(&ProtocolEvent) -> Option<Request>,
{
    let request_id = io.raw_send(Request::Run { task, attachments }).await?;
    let mut streamed = String::new();
    loop {
        let event = io.raw_next_event().await?;
        if event.request_id.as_deref() != Some(&request_id) {
            continue;
        }
        if let Some(response) = responder(&event) {
            io.raw_send(response).await?;
        }
        match event.event {
            core_agentic::Event::AssistantDelta { content } => {
                streamed.push_str(&content);
                on_chunk(&content);
            }
            core_agentic::Event::Done { result } => {
                return Ok(if result.is_empty() { streamed } else { result });
            }
            core_agentic::Event::Error { message } => return Err(anyhow!(message)),
            event => on_event(event),
        }
    }
}
```

Refactor `RuntimeClient` untuk memakai helper (hapus duplikasi): implement `ConnectionIo` yang mendelegasikan ke `send`/`next_event` yang ada, ganti body `spawn_binary` (bagian handshake) dengan `perform_handshake`, ganti body `send_and_wait` dengan `send_and_wait_io(self, request).await`, dan ganti body `run` dengan `run_turn(self, task, attachments, on_chunk, on_event, responder).await`:

```rust
impl ConnectionIo for RuntimeClient {
    fn raw_send(&mut self, request: Request) -> impl Future<Output = Result<String>> + Send {
        self.send(request)
    }

    fn raw_next_event(&mut self) -> impl Future<Output = Result<ProtocolEvent>> + Send {
        self.next_event()
    }
}
```

```rust
    // Di spawn_binary, GANTI blok handshake manual dengan:
    let mut client = Self {
        child,
        stdin,
        lines: BufReader::new(stdout).lines(),
        sequence: 0,
    };
    perform_handshake(&mut client, overrides).await?;
    Ok(client)
```

```rust
    pub async fn send_and_wait(&mut self, request: Request) -> Result<ProtocolEvent> {
        send_and_wait_io(self, request).await
    }

    pub async fn run<C, E, R>(
        &mut self,
        task: String,
        attachments: Vec<core_agentic::Attachment>,
        on_chunk: C,
        on_event: E,
        responder: R,
    ) -> Result<String>
    where
        C: FnMut(&str),
        E: FnMut(core_agentic::Event),
        R: FnMut(&ProtocolEvent) -> Option<Request>,
    {
        run_turn(self, task, attachments, on_chunk, on_event, responder).await
    }
```

Tambahkan `InProcessClient` + spawner thread engine yang generik (setelah impl `RuntimeClient`, sebelum `runtime_binary`):

```rust
/// Spawn the engine thread for an embedded connection. Generic over the
/// engine's transport so tests can inject a misbehaving (panicking)
/// transport; production uses [`MemoryTransport`].
pub(crate) fn spawn_engine_thread<T, F>(
    make_engine: F,
    request_rx: std_mpsc::Receiver<ProtocolRequest>,
    event_tx: std_mpsc::Sender<ProtocolEvent>,
    panic_tx: std_mpsc::Sender<ProtocolEvent>,
) -> std::thread::JoinHandle<()>
where
    T: core_agentic::runtime::transport::Transport,
    F: FnOnce(std_mpsc::Receiver<ProtocolRequest>, std_mpsc::Sender<ProtocolEvent>) -> RuntimeEngine<T>
        + Send
        + 'static,
{
    std::thread::Builder::new()
        .name("agentic-runtime-embedded".to_string())
        .spawn(move || {
            let mut engine = make_engine(request_rx, event_tx);
            // AssertUnwindSafe: the engine owns channels/Arcs; a panic
            // unwinds through them with no observable invariants to guard.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.run()));
            if result.is_err() {
                // The engine may have died mid-stream; deliver a terminal
                // error event so the frontend gets the same "runtime died"
                // UX as the spawned mode.
                let _ = panic_tx.send(ProtocolEvent::new(
                    None,
                    core_agentic::Event::Error {
                        message: "embedded runtime engine panicked".to_string(),
                    },
                ));
            }
        })
        .expect("spawn agentic-runtime-embedded thread")
}

/// In-process connection: the engine runs as a native thread inside this
/// process, talking over in-memory channels. No serialization, no runtime
/// binary discovery. Same protocol, same handshake, same event order as
/// the spawned daemon — embedded skips serialization, not the protocol.
pub struct InProcessClient {
    request_tx: std_mpsc::Sender<ProtocolRequest>,
    events: tokio_mpsc::UnboundedReceiver<ProtocolEvent>,
    engine_thread: Option<std::thread::JoinHandle<()>>,
    sequence: u64,
}

impl InProcessClient {
    pub async fn start(overrides: InitOverrides) -> Result<Self> {
        let (request_tx, request_rx) = std_mpsc::channel::<ProtocolRequest>();
        let (event_tx, event_rx) = std_mpsc::channel::<ProtocolEvent>();
        let (tokio_tx, events) = tokio_mpsc::unbounded_channel::<ProtocolEvent>();

        // Bridge thread: the MemoryTransport side is blocking
        // (std::sync::mpsc), so a dedicated native thread forwards events
        // into a tokio channel for the async client. Cheaper than
        // spawn_blocking-per-event for delta-heavy streams.
        std::thread::Builder::new()
            .name("agentic-runtime-bridge".to_string())
            .spawn({
                let event_rx = event_rx;
                move || {
                    while let Ok(event) = event_rx.recv() {
                        if tokio_tx.send(event).is_err() {
                            break;
                        }
                    }
                }
            })
            .expect("spawn agentic-runtime-bridge thread");

        let engine_thread = spawn_engine_thread(
            |request_rx, event_tx| RuntimeEngine::new(MemoryTransport::new(request_rx, event_tx)),
            request_rx,
            event_tx.clone(),
            event_tx,
        );

        let mut client = Self {
            request_tx,
            events,
            engine_thread: Some(engine_thread),
            sequence: 0,
        };
        perform_handshake(&mut client, overrides).await?;
        Ok(client)
    }

    pub async fn send(&mut self, request: Request) -> Result<String> {
        self.sequence += 1;
        let id = format!("cli-{}", self.sequence);
        self.request_tx
            .send(ProtocolRequest::new(id.clone(), request))
            .map_err(|_| anyhow!("embedded runtime engine stopped"))?;
        Ok(id)
    }

    pub async fn next_event(&mut self) -> Result<ProtocolEvent> {
        // Channel closed = every sender dropped (engine thread + bridge
        // thread both gone) without a terminal event — synthesize one so
        // callers get the same contract as the spawned mode.
        self.events
            .recv()
            .await
            .ok_or_else(|| anyhow!("embedded runtime engine stopped"))
    }

    pub async fn send_and_wait(&mut self, request: Request) -> Result<ProtocolEvent> {
        send_and_wait_io(self, request).await
    }

    /// Send `Shutdown` and join the engine thread (via `spawn_blocking`
    /// so the tokio runtime never blocks).
    ///
    /// `Request::Shutdown` makes `RuntimeEngine::run` break out of its
    /// dispatch loop (see `core-agentic/src/runtime/engine.rs`), so the
    /// join below never depends on drop timing of `request_tx`. The bridge
    /// thread ends on its own once the engine's event senders drop with it.
    pub async fn shutdown(&mut self) -> Result<()> {
        let _ = self.send(Request::Shutdown).await;
        if let Some(handle) = self.engine_thread.take() {
            tokio::task::spawn_blocking(move || {
                let _ = handle.join();
            })
            .await
            .map_err(|e| anyhow!("join engine thread failed: {e}"))?;
        }
        Ok(())
    }

    /// Test/teardown helper: has the engine thread finished?
    pub fn engine_finished(&self) -> bool {
        self.engine_thread
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(true)
    }
}

impl ConnectionIo for InProcessClient {
    fn raw_send(&mut self, request: Request) -> impl Future<Output = Result<String>> + Send {
        self.send(request)
    }

    fn raw_next_event(&mut self) -> impl Future<Output = Result<ProtocolEvent>> + Send {
        self.next_event()
    }
}
```

Tambahkan imports di bagian atas `client.rs` untuk `RuntimeEngine`/`MemoryTransport`:

```rust
use core_agentic::runtime::engine::RuntimeEngine;
use core_agentic::runtime::transport::MemoryTransport;
```

> Catatan: `shutdown` tidak me-drop `request_tx` secara eksplisit — pengirim itu drop alami bersama `self`. Engine keluar dari loop karena `Request::Shutdown => break` di `engine.rs`, bukan karena channel tertutup. Run yang sedang aktif tetap di-join oleh `engine.run()` sendiri (loop `run_threads.drain(..).join()`), sama seperti jalur spawn.

- [ ] **Step 4: Jalankan test → verify pass**

Run: `cargo test -p agentic-cli in_process`
Expected: 2 passed (`in_process_handshake_and_list_tools_round_trip`, `shutdown_joins_engine_thread`).

- [ ] **Step 5: Verify regresi jalur spawn (helper refactor tidak mengubah perilaku)**

Run: `cargo test -p agentic-cli` lalu `cargo test -p core-agentic --test runtime_engine`
Expected: semua pass — termasuk 2 smoke e2e spawn (`smoke_run_with_callbacks_streams_daemon_lifecycle_events`, `smoke_run_through_daemon_does_not_deadlock`) dan golden test engine core tanpa modifikasi.

- [ ] **Step 6: Commit**

```bash
git add agentic-cli/src/client.rs
git commit -m "feat(cli): add InProcessClient with shared ConnectionIo handshake/run helpers"
```

---

### Task 2: Panic isolation test (transport panik → `Event::Error`)

**Files:**
- Modify: `agentic-cli/src/client.rs` (mod test `in_process_tests` yang sudah ada dari Task 1)

**Interfaces:**
- Consumes: `spawn_engine_thread` (Task 1), `trait Transport` (`read_request`, `write_event`), `ProtocolEvent::new`, `tokio_mpsc::unbounded_channel`.
- Produces: bukti teruji bahwa panic di thread engine dikirim sebagai `Event::Error` berpesan `"embedded runtime engine panicked"` — kontrak yang diandalkan Task 3 (UX respawn).

- [ ] **Step 1: Tulis test untuk jalur panic**

Tambahkan ke mod `in_process_tests` di `agentic-cli/src/client.rs`. Kontraknya: `RuntimeEngine::run()` mengirim `Ready` via `write_event` **sebelum** membaca request pertama, jadi transport yang `write_event`-nya `Ok` dan `read_request`-nya panic menghasilkan urutan event `Ready` lalu `Error` (via `panic_tx` dari `catch_unwind`):

```rust
    /// Transport test yang panic pada pembacaan request pertama — simulasi
    /// bug engine. `write_event` tetap sukses agar `Ready` sempat terkirim.
    struct PanickyTransport;

    impl core_agentic::runtime::transport::Transport for PanickyTransport {
        fn read_request(&self) -> std::io::Result<Option<ProtocolRequest>> {
            panic!("boom: injected engine bug");
        }
        fn write_event(&self, _event: &ProtocolEvent) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn engine_panic_becomes_error_event() {
        let (request_tx, request_rx) = std_mpsc::channel::<ProtocolRequest>();
        let (event_tx, event_rx) = std_mpsc::channel::<ProtocolEvent>();
        let (tokio_tx, mut events) = tokio_mpsc::unbounded_channel::<ProtocolEvent>();

        std::thread::Builder::new()
            .name("agentic-runtime-bridge".to_string())
            .spawn(move || {
                while let Ok(event) = event_rx.recv() {
                    if tokio_tx.send(event).is_err() {
                        break;
                    }
                }
            })
            .expect("bridge thread");

        // Factory mengabaikan channel — transport panik tidak butuh keduanya.
        let _engine = spawn_engine_thread(
            |_request_rx, _event_tx| RuntimeEngine::new(PanickyTransport),
            request_rx,
            event_tx.clone(),
            event_tx,
        );

        // Event pertama: Ready (terkirim sebelum panic).
        let first = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .expect("no event after panic window")
            .expect("event channel closed before terminal event");
        assert!(
            matches!(first.event, core_agentic::Event::Ready { .. }),
            "expected Ready first, got {first:?}"
        );

        // Event kedua: Error hasil catch_unwind (bukan crash test process).
        let second = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .expect("no terminal event after panic")
            .expect("event channel closed before terminal event");
        match second.event {
            core_agentic::Event::Error { message } => {
                assert!(
                    message.contains("panicked"),
                    "expected panic message, got: {message}"
                );
            }
            other => panic!("expected Error event after panic, got {other:?}"),
        }
        drop(request_tx);
    }
```

> `AtomicBool`/state tidak diperlukan pada transport ini: `read_request` dipanggil tepat satu kali sebelum engine unwind, dan panic pertama langsung memicu jalur `catch_unwind` → `panic_tx`.

- [ ] **Step 2: Jalankan test → verify pass**

Run: `cargo test -p agentic-cli engine_panic -- --nocapture`
Expected: 1 passed — `Error` berpesan panic diterima lewat channel, test process tetap hidup (catch_unwind bekerja).

- [ ] **Step 3: Commit**

```bash
git add agentic-cli/src/client.rs
git commit -m "test(cli): embedded engine panic surfaces as terminal Error event"
```

---

### Task 3: `AgentConnection` + flag `--runtime` + wiring `Commands`

**Files:**
- Modify: `agentic-cli/src/client.rs` (tambah `RuntimeMode`, `AgentConnection`)
- Modify: `agentic-cli/src/cli.rs` (tambah `RuntimeModeArg` + arg root `--runtime`)
- Modify: `agentic-cli/src/main.rs` (wire `.with_runtime_mode(...)`)
- Modify: `agentic-cli/src/commands.rs` (field `connection`, `ensure_connection`, rename 8 call site)
- Modify: `agentic-cli/src/commands.rs` (mod `e2e_tests` → matriks dua mode)
- Test: `agentic-cli/src/commands.rs` (e2e matriks), `agentic-cli/src/client.rs` (unit `connect`)

**Interfaces:**
- Consumes: `InProcessClient::start`, `RuntimeClient::spawn`, helper `send_and_wait_io`/`run_turn` (Task 1).
- Produces:
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)] pub enum RuntimeMode { #[default] Embedded, Spawn }`
  - `impl AgentConnection { pub async fn connect(mode: RuntimeMode, overrides: InitOverrides) -> Result<Self> }` dengan metode publik `send`, `next_event`, `send_and_wait`, `run`, `shutdown` — signature identik `RuntimeClient` hari ini
  - `Commands::with_runtime_mode(self, mode: RuntimeMode) -> Self`
- Kontrak call site (dipakai renderer/REPL/TUI, tidak berubah perilaku): `send_and_wait(Request)` → `ProtocolEvent`, `send(Request)` → `String`, `next_event()` → `ProtocolEvent`, `run(task, attachments, on_chunk, on_event, responder)` → `String`, `shutdown()`.

- [ ] **Step 1: Tulis test gagal untuk `AgentConnection::connect` dua mode**

Tambahkan ke mod `in_process_tests` di `client.rs`:

```rust
    #[tokio::test]
    async fn agent_connection_connects_in_both_modes() {
        // Embedded (default): tanpa binary agentic-runtime sama sekali.
        let mut embedded = AgentConnection::connect(
            RuntimeMode::Embedded,
            hermetic_overrides(),
        )
        .await
        .expect("embedded connect");
        let event = embedded.send_and_wait(Request::ListTools).await.expect("list tools");
        assert!(matches!(event.event, core_agentic::Event::ToolList { .. }));
        embedded.shutdown().await.expect("embedded shutdown");

        // Spawn: jalur lama lewat binary daemon (runtime_binary() menemukan
        // target/debug/agentic-runtime hasil cargo test workspace).
        let mut spawned = AgentConnection::connect(
            RuntimeMode::Spawn,
            hermetic_overrides(),
        )
        .await
        .expect("spawned connect");
        let event = spawned.send_and_wait(Request::ListTools).await.expect("list tools");
        assert!(matches!(event.event, core_agentic::Event::ToolList { .. }));
        spawned.shutdown().await.expect("spawned shutdown");
    }
```

- [ ] **Step 2: Jalankan test → verify gagal**

Run: `cargo test -p agentic-cli agent_connection -- --nocapture`
Expected: COMPILE ERROR — `AgentConnection`/`RuntimeMode` tidak ditemukan.

- [ ] **Step 3: Implementasi `RuntimeMode` + `AgentConnection`**

Di `client.rs` (setelah impl `InProcessClient`):

```rust
/// Which connection mode the CLI uses to reach the agent engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeMode {
    /// Engine runs as a thread inside this process (default; no runtime
    /// binary needed).
    #[default]
    Embedded,
    /// Spawn the `agentic-runtime` daemon subprocess (JSONL over stdio).
    Spawn,
}

/// One connection abstraction over both transports (codex's
/// `AppServerClient { Embedded, Remote }` pattern): embedded and spawned
/// are not two products — they are two transports behind one client.
pub enum AgentConnection {
    Spawned(RuntimeClient),
    InProcess(InProcessClient),
}

impl AgentConnection {
    pub async fn connect(mode: RuntimeMode, overrides: InitOverrides) -> Result<Self> {
        match mode {
            RuntimeMode::Embedded => {
                Ok(Self::InProcess(InProcessClient::start(overrides).await?))
            }
            RuntimeMode::Spawn => Ok(Self::Spawned(RuntimeClient::spawn(overrides).await?)),
        }
    }

    pub async fn send(&mut self, request: Request) -> Result<String> {
        match self {
            Self::Spawned(client) => client.send(request).await,
            Self::InProcess(client) => client.send(request).await,
        }
    }

    pub async fn next_event(&mut self) -> Result<ProtocolEvent> {
        match self {
            Self::Spawned(client) => client.next_event().await,
            Self::InProcess(client) => client.next_event().await,
        }
    }

    pub async fn send_and_wait(&mut self, request: Request) -> Result<ProtocolEvent> {
        send_and_wait_io(self, request).await
    }

    pub async fn run<C, E, R>(
        &mut self,
        task: String,
        attachments: Vec<core_agentic::Attachment>,
        on_chunk: C,
        on_event: E,
        responder: R,
    ) -> Result<String>
    where
        C: FnMut(&str),
        E: FnMut(core_agentic::Event),
        R: FnMut(&ProtocolEvent) -> Option<Request>,
    {
        run_turn(self, task, attachments, on_chunk, on_event, responder).await
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        match self {
            Self::Spawned(client) => client.shutdown().await,
            Self::InProcess(client) => client.shutdown().await,
        }
    }
}

impl ConnectionIo for AgentConnection {
    fn raw_send(&mut self, request: Request) -> impl Future<Output = Result<String>> + Send {
        self.send(request)
    }

    fn raw_next_event(&mut self) -> impl Future<Output = Result<ProtocolEvent>> + Send {
        self.next_event()
    }
}
```

- [ ] **Step 4: Jalankan test `agent_connection` → verify pass**

Run: `cargo test -p agentic-cli agent_connection`
Expected: 1 passed (dua mode keduanya berhasil handshake + ListTools).

- [ ] **Step 5: Flag CLI `--runtime`**

Di `cli.rs`, tambahkan enum + arg root (pola persis `PermissionModeArg`/`ColorChoice`):

```rust
/// Which runtime connection mode to use
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum RuntimeModeArg {
    /// Run the agent engine in-process (default; single binary)
    #[default]
    Embedded,
    /// Spawn the agentic-runtime daemon subprocess
    Spawn,
}

impl From<RuntimeModeArg> for crate::client::RuntimeMode {
    fn from(arg: RuntimeModeArg) -> Self {
        match arg {
            RuntimeModeArg::Embedded => crate::client::RuntimeMode::Embedded,
            RuntimeModeArg::Spawn => crate::client::RuntimeMode::Spawn,
        }
    }
}
```

Di struct `Cli`, sebelum field `command`:

```rust
    /// Runtime connection mode
    ///
    /// - embedded: run the agent engine in-process (default; no separate
    ///   agentic-runtime binary needed)
    /// - spawn:    spawn the agentic-runtime daemon subprocess (JSONL over
    ///   stdio; honors AGENTIC_RUNTIME_BIN)
    #[arg(long, global = true, value_name = "MODE", default_value = "embedded")]
    pub runtime: RuntimeModeArg,
```

- [ ] **Step 6: Wiring `Commands` (field + builder + ensure_connection + rename call site)**

Di `commands.rs`:

1. Ganti field (sekitar baris 465):

```rust
    connection: Option<crate::client::AgentConnection>,
```

2. Tambah field + default + builder (di dekat field `runtime_config_path` / builder `with_runtime_config_path`):

```rust
    runtime_mode: crate::client::RuntimeMode,
```

Default di konstruktor: `runtime_mode: crate::client::RuntimeMode::Embedded,` — dan:

```rust
    pub fn with_runtime_mode(mut self, mode: crate::client::RuntimeMode) -> Self {
        self.runtime_mode = mode;
        self
    }
```

3. Ganti `ensure_runtime` (baris ~520) menjadi:

```rust
    async fn ensure_connection(&mut self) -> Result<()> {
        if self.connection.is_some() {
            return Ok(());
        }
        let overrides = core_agentic::runtime::protocol::InitOverrides {
            config_path: self.runtime_config_path.clone(),
            permission_mode: Some(self.permission_mode),
            model: self.config.active_model().map(|model| model.model.clone()),
            system_prompt: self.config.system_prompt.clone(),
        };
        self.connection = Some(
            crate::client::AgentConnection::connect(self.runtime_mode, overrides).await?,
        );
        Ok(())
    }
```

4. Rename semua pemakaian `self.runtime_client` → `self.connection` dan panggilan `ensure_runtime()` → `ensure_connection()` (8 call site: baris ~835, 851, 916, 1221, 1306, 1695, 1809, 2087; plus pemanggil `ensure_runtime` — cari dengan `grep -n "runtime_client\|ensure_runtime" agentic-cli/src/commands.rs`). Metode yang dipanggil (`send_and_wait`, `send`, `next_event`, `run`) **ada semua** di `AgentConnection` — body call site tidak berubah.

5. `switch_model` (baris ~916): `self.runtime_client = None;` → `self.connection = None;` (drop `InProcessClient` menghentikan engine thread via drop `request_tx` — proses tetap bersih tanpa join eksplisit).

6. Commit sementara belum — lanjut Step 7.

- [ ] **Step 7: Wire `main.rs`**

Di `main.rs` sekitar baris 254, tambahkan builder setelah `.with_permission_mode(...)`:

```rust
        .with_runtime_mode(cli.runtime.into());
```

(ubah baris berantai yang ada, jangan duplikasi statement `let mut commands`).

- [ ] **Step 8: Build + seluruh test CLI**

Run: `cargo test -p agentic-cli`
Expected: semua pass. Perhatikan: dua smoke e2e lama kini berjalan lewat **embedded** (default baru) — keduanya tetap harus hijau karena jalur event-nya identik.

- [ ] **Step 9: Ubah smoke e2e menjadi matriks dua mode**

Di `commands.rs` mod `e2e_tests`: ekstrak body kedua test menjadi helper privat yang menerima `RuntimeMode`, lalu duplikasi wrapper `#[tokio::test]` per mode. Pola:

```rust
    use crate::client::RuntimeMode;

    async fn smoke_lifecycle(mode: RuntimeMode) {
        let fixture = hermetic_runtime_config();
        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(mode)
            .with_permission_mode(core_agentic::PermissionMode::Yolo);
        // ... body persis `smoke_run_with_callbacks_streams_daemon_lifecycle_events`
        // (events Arc, timeout 60s, assertions SessionStarted + SessionFailed)
    }

    #[tokio::test]
    async fn smoke_lifecycle_embedded() {
        smoke_lifecycle(RuntimeMode::Embedded).await;
    }

    #[tokio::test]
    async fn smoke_lifecycle_spawned() {
        smoke_lifecycle(RuntimeMode::Spawn).await;
    }

    async fn smoke_no_deadlock(mode: RuntimeMode) {
        // ... body persis `smoke_run_through_daemon_does_not_deadlock`,
        // dengan .with_runtime_mode(mode) ditambahkan ke builder.
        // Catatan: wrapper #[tokio::test(flavor = "multi_thread")] pindah ke
        // wrapper per-mode di bawah.
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_no_deadlock_embedded() {
        smoke_no_deadlock(RuntimeMode::Embedded).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_no_deadlock_spawned() {
        smoke_no_deadlock(RuntimeMode::Spawn).await;
    }
```

Hapus kedua test lama (`smoke_run_with_callbacks_streams_daemon_lifecycle_events`, `smoke_run_through_daemon_does_not_deadlock`) — body-nya sudah pindah ke helper.

- [ ] **Step 10: Jalankan matriks e2e → verify pass**

Run: `cargo test -p agentic-cli smoke`
Expected: 4 passed — lifecycle × {embedded, spawned}, deadlock × {embedded, spawned}.

- [ ] **Step 11: Commit**

```bash
git add agentic-cli/src/client.rs agentic-cli/src/cli.rs agentic-cli/src/main.rs agentic-cli/src/commands.rs
git commit -m "feat(cli): embedded runtime default via AgentConnection + --runtime flag"
```

---

### Task 4: Dokumentasi + gate CI (fmt, clippy, full test)

**Files:**
- Modify: `CHANGELOG.md` (section `[Unreleased]`)
- Modify: `README.md` (bagian Installation — catatan mode runtime)
- Modify: `docs/BENCHMARK_CODEX.md` (tandai Fase 1 selesai)

**Interfaces:**
- Consumes: perilaku final dari Task 1–3.
- Produces: dokumentasi yang akurat + workspace lolos gate CI (`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, seluruh test).

- [ ] **Step 1: CHANGELOG — entri `[Unreleased]`**

Tambahkan di bawah `## [Unreleased]` (sebelum `### Changed` yang ada):

```markdown
### Added
- CLI: **embedded runtime mode** (default) — the agent engine now runs as
  a thread inside the `agentic` process over in-memory channels, removing
  the need for a separate `agentic-runtime` binary on PATH. The spawned
  daemon mode is still available via `agentic --runtime spawn` and honors
  `AGENTIC_RUNTIME_BIN`. Both modes share the same protocol, handshake,
  and interactive gates; an engine panic in embedded mode surfaces as a
  normal terminal `Error` event (same "runtime died" UX as the spawned
  mode).
```

- [ ] **Step 2: README — perbarui narasi dua binary**

Di `README.md`, bagian "Installation" paragraf yang menjelaskan dua binary, tambahkan penjelasan setelah daftar binary:

```markdown
Since v0.6.0, the CLI defaults to **embedded mode**: the agent engine runs
as a thread inside the `agentic` process, so `agentic-runtime` does not need
to be on PATH at all. Use `agentic --runtime spawn` to run the agent in a
separate daemon process instead (the `agentic-runtime` binary is discovered
next to `agentic`, or via the `AGENTIC_RUNTIME_BIN` environment variable).
```

> Catatan: nomor versi (`v0.6.0`) adalah asumsi rilis berikutnya — sesuaikan saat rilis.

- [ ] **Step 3: Tandai Fase 1 selesai di BENCHMARK_CODEX.md**

Di `docs/BENCHMARK_CODEX.md`, pada tabel "Perbandingan Side-by-Side" ubah baris headless runner dan client abstraction:

```markdown
| Headless runner | `codex-exec` via `InProcessAppServerClient` | embedded (default) + `agentic-runtime` spawn | ✅ dual-mode |
```

```markdown
| Client abstraction | `AppServerClient { InProcess, Remote }` | `AgentConnection { InProcess, Spawned }` | ✅ setara |
```

Dan di section "Fase 1 — Dual-mode connection di CLI *(gap utama, effort kecil)*" tambahkan tandasi selesai, contoh: ubah judul menjadi `### Fase 1 — Dual-mode connection di CLI ✅ (landed 2026-11)` dan sisakan Fase 2–4 sebagai backlog.

- [ ] **Step 4: fmt + clippy + full test**

Run:
```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Expected: fmt bersih (atau hanya menyusun ulang import — commit hasilnya), clippy 0 warning, semua test pass. Jika clippy menemukan pelanggaran di kode baru (mis. `field_reassign_with_default`, `redundant_clone`), perbaiki di file terkait sampai bersih.

- [ ] **Step 5: Commit**

```bash
git add CHANGELOG.md README.md docs/BENCHMARK_CODEX.md agentic-cli/src
git commit -m "docs: embedded runtime mode notes + mark codex-benchmark Fase 1 landed"
```

---

## Self-Review (dijalankan penulis plan — sudah dilakukan)

1. **Spec coverage**: handshake bersama + InProcessClient + panic isolation + shutdown (Task 1–2); AgentConnection + flag + wiring + test matrix e2e (Task 3); docs + gate (Task 4). Success criteria spec: "tanpa binary agentic-runtime" tercakup oleh test `agent_connection_connects_in_both_modes` (embedded connect tanpa spawn) + smoke embedded; "spawn 100% perilaku lama" tercakup wrapper spawned. ✓
2. **Placeholder scan**: tidak ada TBD/TODO; semua step berisi kode konkret atau command exact. Transport panik di Task 2 ditulis dalam satu versi final.
3. **Type consistency**: `ConnectionIo::raw_send/raw_next_event` konsisten di semua impl (RuntimeClient, InProcessClient, AgentConnection); `send_and_wait_io`/`run_turn` dipakai nama sama di Task 1 & 3; `RuntimeMode::{Embedded, Spawn}` konsisten dengan `RuntimeModeArg`. ✓
