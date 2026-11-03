# Protocol Hardening (Fase 2a) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Harden protokol runtime gaya codex: `Init` wajib tepat sekali dengan error semantik (`not initialized` / `already initialized`), capability negotiation di handshake (`Request::Plan` menjadi API eksperimental pertama), notification opt-out via allowlist tertutup, dan pemisahan doc-contract tipe wire vs internal — semua additive di `PROTOCOL_VERSION = 1`.

**Architecture:** Tipe baru (`ConnectionCapabilities`, `NotificationKind`) di `protocol.rs`; engine (`engine.rs`) mendapat flag `initialized`, field `capabilities`, dan satu choke point `EventSink` yang mewarisi semua penulisan event (filter opt-out ada di satu tempat; gates/respon tidak pernah difilter karena di luar 4 varian enum). CLI (`agentic-cli/src/client.rs`) menambah parameter caps di connect/start/handshake — handshake tetap satu tempat (`perform_handshake`).

**Tech Stack:** Rust (serde, std::sync), JSON Lines protocol; tanpa dependensi baru. Reference spec: `docs/superpowers/specs/2026-11-03-protocol-hardening-design.md`.

## Global Constraints

- Semua perubahan protokol **additive**: field baru pakai `#[serde(default)]`, `PROTOCOL_VERSION` tetap `1`, bump hanya untuk breaking change.
- Error semantik persis: `"not initialized"`, `"already initialized"`, `"capability 'experimental_api' required"` (string literal ini ditulis verbatim; test assert persis/at least contains sesuai instruktur tiap test).
- `NotificationKind` adalah allowlist tertutup: hanya `Thinking`, `AssistantDelta`, `Warning`, `TodoChanged` yang boleh di-opt-out; semuanya di luar daftar (gates `ConfirmationRequest`/`QuestionRequest`/`PlanApprovalRequest`, respons `Done`/`Error`/`InitOk`/`ToolList`/`SkillActivatedResult`/`MemorySearchResult`/`SessionReset`/`Ready` dll.) **tidak boleh pernah difilter**.
- Opt-out dinegosiasikan sekali di `Init`, imutabel setelahnya.
- `Cancel`, `Shutdown`, `ConfirmResponse`, `QuestionResponse` tetap permisif terhadap init.
- Modul core yang berubah HANYA `core-agentic/src/runtime/{protocol,engine}.rs` + test core terkait (`core-agentic/tests/runtime_engine.rs`, inline tests `protocol.rs`). Orchestrator/tools/safety/memory tidak tersentuh.
- CLI: renderer/UI/event order tidak berubah; hanya signature connect/start + konstanta caps di `agentic-cli/src/client.rs` + satu pemanggilan di `agentic-cli/src/commands.rs`.
- Semua test hermetic (endpoint `http://127.0.0.1:9` bila butuh config fixture); matriks e2e CLI embedded/spawned yang ada tetap hijau.
- CI gate: `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`, seluruh test pass.
- Branch kerja: `feature/protocol-hardening` dari `dev`.

---

### Task 1: Tipe protokol — `ConnectionCapabilities`, `NotificationKind`, field `Init.capabilities`, doc contract

**Files:**
- Modify: `core-agentic/src/runtime/protocol.rs`
- Test: inline `#[cfg(test)] mod tests` di file yang sama

**Interfaces:**
- Produces (dipakai Task 2–3):
  - `#[derive(Debug, Clone, Default, Serialize, Deserialize)] #[serde(rename_all = "camelCase")] pub struct ConnectionCapabilities { #[serde(default)] pub experimental_api: bool, #[serde(default)] pub opt_out_notify: Vec<NotificationKind> }`
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)] #[serde(rename_all = "snake_case")] pub enum NotificationKind { Thinking, AssistantDelta, Warning, TodoChanged }`
  - `Request::Init { overrides: InitOverrides, capabilities: ConnectionCapabilities }` — dengan `#[serde(default)] capabilities`.

- [ ] **Step 1: Tulis test gagal untuk serde default + frame reject**

Tambahkan ke `mod tests` di `core-agentic/src/runtime/protocol.rs`:

```rust
    #[test]
    fn init_request_without_capabilities_field_defaults() {
        // Frame gaya lama (mis. protocol-demo.js): tidak membawa capabilities.
        let frame = r#"{"v":1,"id":"i1","type":"init"}"#;
        let req: ProtocolRequest = serde_json::from_str(frame).unwrap();
        match req.request {
            Request::Init { capabilities, .. } => {
                assert_eq!(capabilities, ConnectionCapabilities::default());
                assert!(!capabilities.experimental_api);
                assert!(capabilities.opt_out_notify.is_empty());
            }
            other => panic!("expected Init, got {other:?}"),
        }
    }

    #[test]
    fn init_request_with_capabilities_deserializes() {
        let frame = r#"{"v":1,"id":"i2","type":"init","capabilities":{"experimentalApi":true,"optOutNotify":["thinking","todo_changed"]}}"#;
        let req: ProtocolRequest = serde_json::from_str(frame).unwrap();
        match req.request {
            Request::Init { capabilities, .. } => {
                assert!(capabilities.experimental_api);
                assert!(capabilities.opt_out_notify.contains(&NotificationKind::Thinking));
                assert!(capabilities.opt_out_notify.contains(&NotificationKind::TodoChanged));
            }
            other => panic!("expected Init, got {other:?}"),
        }
    }

    #[test]
    fn unknown_notification_name_fails_frame_deserialization() {
        // Allowlist tertutup: nama di luar enum tidak bisa lewat jalur wire.
        let frame = r#"{"v":1,"id":"i3","type":"init","capabilities":{"experimentalApi":false,"optOutNotify":["done"]}}"#;
        assert!(serde_json::from_str::<ProtocolRequest>(frame).is_err());
    }
```

- [ ] **Step 2: Jalankan test → verify gagal**

Run: `cargo test -p core-agentic --lib protocol`
Expected: COMPILE ERROR — `ConnectionCapabilities` / `NotificationKind` tidak ditemukan.

- [ ] **Step 3: Implementasi tipe + field + doc contract**

Di `core-agentic/src/runtime/protocol.rs`:

1. Di atas file, tulis doc contract section (gantikan doc `//!` paling atas — jadikan dua bagian):

```rust
//! Transport-neutral runtime protocol types.
//!
//! # Wire contract (versioned, eksternal)
//!
//! Tipe di bawah adalah KONTRAK LINTAS-PROSES: `ProtocolRequest`,
//! `ProtocolEvent`, `Request`, `Event`, `InitOverrides`,
//! `ConnectionCapabilities`, `NotificationKind`. Perubahan harus additive
//! (`#[serde(default)]`) di dalam `PROTOCOL_VERSION` yang sama; rename,
//! penghapusan, atau perubahan semantik = bump versi. Frontend non-Rust
//! memakai kontrak ini mentah-mentah — jangan menambah field "sementara".
//!
//! # Internal
//!
//! Detail di luar kontrak wire (mis. helper dan modul `#[cfg(test)]`) tidak boleh
//! bocor ke tipe di atas.
```

2. Tambahkan tipe (letakkan setelah `InitOverrides`):

```rust
/// Capabilities yang dideklarasikan frontend saat `Init` — sekali per
/// koneksi, imutabel setelahnya (init kedua ditolak engine).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionCapabilities {
    /// Opt-in API eksperimental (saat ini: `Request::Plan`).
    #[serde(default)]
    pub experimental_api: bool,
    /// Notification yang TIDAK dikirim engine ke frontend ini. Selalu
    /// dalam allowlist `NotificationKind`; nilai lain tidak bisa
    /// direpresentasikan (embedded) / gagal deserialisasi (spawn).
    #[serde(default)]
    pub opt_out_notify: Vec<NotificationKind>,
}

/// Allowlist tertutup notification yang boleh di-opt-out. Event di luar
/// varian ini (per-request respons dan interaktif gate) TIDAK PERNAH
/// difilter — opt-out tidak bisa membunuh alur run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    Thinking,
    AssistantDelta,
    Warning,
    TodoChanged,
}
```

3. Ganti arm `Init` di `Request`:

```rust
    Init {
        #[serde(default)]
        overrides: InitOverrides,
        #[serde(default)]
        capabilities: ConnectionCapabilities,
    },
```

- [ ] **Step 4: Jalankan test → verify pass**

Run: `cargo test -p core-agentic --lib protocol`
Expected: semua pass (termasuk 3 test baru).

- [ ] **Step 5: Commit**

```bash
git add core-agentic/src/runtime/protocol.rs
git commit -m "feat(protocol): ConnectionCapabilities + NotificationKind allowlist on Init"
```

---

### Task 2: Engine — strict init, capability gate, `EventSink` opt-out

**Files:**
- Modify: `core-agentic/src/runtime/engine.rs`
- Test: `core-agentic/tests/runtime_engine.rs` (update test lama + test baru)

**Interfaces:**
- Consumes: `ConnectionCapabilities`, `NotificationKind` (Task 1).
- Produces (dipakai Task 3, perilaku baru):
  - `Request::Init` arm baru (lihat Step 3) — `already initialized` untuk init kedua; caps+opt-outs tersimpan saat sukses.
  - 7 request resource wajib init; `Plan` khususnya juga gated `experimental_api`.
  - semua penulisan event lewat `EventSink` (filter opt-out).

- [ ] **Step 1: Tulis test gagal di `core-agentic/tests/runtime_engine.rs`**

Tambahkan di akhir file (pattern sama dengan test yang ada: `MemoryTransport` + thread worker + channel mpsc; likankan sesuaikan import — file sudah meng-import yang dibutuhkan):

```rust
/// Fresh engine (belum init): request resource wajib ditolak.
#[test]
fn resource_requests_before_init_are_rejected() {
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let transport = MemoryTransport::new(request_rx, event_tx);
    let mut engine = RuntimeEngine::new(transport);
    let worker = std::thread::spawn(move || engine.run());

    request_tx
        .send(ProtocolRequest::new(
            "run-1",
            Request::Run {
                task: "hello".into(),
                attachments: vec![],
            },
        ))
        .unwrap();
    request_tx
        .send(ProtocolRequest::new("tools-1", Request::ListTools))
        .unwrap();
    drop(request_tx);
    worker.join().unwrap();

    let events: Vec<_> = event_rx.try_iter().collect();
    let errors: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::Error { message } => Some(message.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 2, "expected exactly two not-initialized errors: {errors:?}");
    assert!(errors.iter().all(|m| *m == "not initialized"), "got: {errors:?}");
}

/// Init kedua kali (di sini: setelah constructor with_session yang berarti
/// sudah initialized) ditolak dengan "already initialized".
#[test]
fn second_init_is_rejected() {
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let transport = MemoryTransport::new(request_rx, event_tx);
    let mut engine = RuntimeEngine::with_session(
        transport,
        std::sync::Arc::new(TextProvider),
        ToolRegistry::new(),
    );
    let worker = std::thread::spawn(move || engine.run());

    request_tx
        .send(ProtocolRequest::new(
            "init-1",
            Request::Init {
                overrides: Default::default(),
                capabilities: Default::default(),
            },
        ))
        .unwrap();
    drop(request_tx);
    worker.join().unwrap();

    let events: Vec<_> = event_rx.try_iter().collect();
    assert!(events.iter().any(
        |e| matches!(&e.event, Event::Error { message } if message == "already initialized")
    ), "expected already-initialized error: {events:?}");
}

/// Request::Plan tanpa opt-in experimental ditolak dengan capability error.
/// (Dengan `with_session` engine sudah initialized, jadi urutan cek:
/// capability langsung dievaluasi.)
#[test]
fn plan_without_experimental_capability_is_rejected() {
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let transport = MemoryTransport::new(request_rx, event_tx);
    let mut engine = RuntimeEngine::with_session(
        transport,
        std::sync::Arc::new(TextProvider),
        ToolRegistry::new(),
    );
    let worker = std::thread::spawn(move || engine.run());

    request_tx
        .send(ProtocolRequest::new(
            "plan-1",
            Request::Plan {
                goal: "fake goal".into(),
                require_approval: false,
            },
        ))
        .unwrap();
    drop(request_tx);
    worker.join().unwrap();

    let events: Vec<_> = event_rx.try_iter().collect();
    assert!(events.iter().any(
        |e| matches!(&e.event, Event::Error { message }
            if message == "capability 'experimental_api' required")
    ), "expected capability error: {events:?}");
}
```

Dan test EventSink (filter opt-out) — unit, langsung tanpa thread:

```rust
mod sink_tests {
    use core_agentic::events::QuestionPrompt;
    use core_agentic::runtime::protocol::{NotificationKind, ProtocolEvent};
    use core_agentic::runtime::engine::EventSink;
    use core_agentic::runtime::transport::MemoryTransport;

    fn make_sink() -> (
        std::sync::mpsc::Sender<ProtocolEvent>,
        std::sync::mpsc::Receiver<ProtocolEvent>,
        EventSink<MemoryTransport>,
    ) {
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let transport = MemoryTransport::new(request_rx, event_tx);
        let sink = EventSink::new(transport, Default::default());
        let _ = request_tx; // sink memegang salinan event_tx internal-nya sendiri
        (event_tx, event_rx, sink)
    }

    #[test]
    fn sink_filters_opted_out_notifications_only() {
        let (event_tx, event_rx, sink) = make_sink();
        let _ = event_tx; // dummy; test tidak mengirim via sini
        sink.set_opt_outs(vec![
            NotificationKind::Thinking,
            NotificationKind::AssistantDelta,
            NotificationKind::Warning,
            NotificationKind::TodoChanged,
        ]);

        // Diopt-out: tidak terkirim.
        sink.send(Some("r".into()), Event::Thinking { content: "t".into() });
        sink.send(Some("r".into()), Event::AssistantDelta { content: "d".into() });
        sink.send(Some("r".into()), Event::Warning { message: "w".into() });
        sink.send(Some("r".into()), Event::TodoChanged { todos: vec![] });

        // Tidak terseleksi: selalu terkirim — respons + gates.
        sink.send(Some("r".into()), Event::Done { result: "final".into() });
        sink.send(Some("r".into()), Event::Error { message: "boom".into() });
        sink.send(Some("r".into()), Event::ConfirmationRequest {
            action: "write".into(),
            description: "d".into(),
            risk_level: "medium".into(),
        });
        sink.send(Some("r".into()), Event::QuestionRequest { questions: vec![] });
        sink.send(None, Event::Ready { protocol: "agentic".into(), version: 1 });

        let seen: Vec<_> = event_rx.try_iter().collect();
        assert!(seen.iter().any(|e| matches!(e.event, Event::Done { .. })));
        assert!(seen.iter().any(|e| matches!(e.event, Event::Error { .. })));
        assert!(seen.iter().any(|e| matches!(e.event, Event::ConfirmationRequest { .. })));
        assert!(seen.iter().any(|e| matches!(e.event, Event::QuestionRequest { .. })));
        assert!(seen.iter().any(|e| matches!(e.event, Event::Ready { .. })));
        assert!(!seen.iter().any(|e| matches!(e.event, Event::Thinking { .. } | Event::AssistantDelta { .. } | Event::Warning { .. } | Event::TodoChanged { .. })));
        assert_eq!(seen.len(), 5, "exactly the 5 non-opted-out events: {seen:?}");
    }
}
```

> Catatan penyesuaian kecil yang boleh (dan diharapkan) dilakukan implementer: `EventSink::new(transport, opt_outs)` menerima `Arc<Mutex<HashSet<NotificationKind>>>`; helper `make_sink` di atas menulis `Default::default()` untuk itu. Jika field struct `Event` asli berbeda sedikit dari snippet (mis. `TodoChanged` field lain), sesuaikan literal test — yang WAJIB dijaga: 4 kind di-filter, request + gates tidak, dan jumlah event persis.

- [ ] **Step 2: Jalankan test → verify gagal**

Run: `cargo test -p core-agentic --test runtime_engine`
Expected: COMPILE ERROR atau FAIL — engine belum punya `EventSink` export, `Request::Init` masih 1 field, dan error belum sesuai.

- [ ] **Step 3: Implementasi engine**

Di `core-agentic/src/runtime/engine.rs`:

1. **Imports** — tambahkan `ConnectionCapabilities, NotificationKind` ke `use super::protocol::{...}` yang ada.

2. **`EventSink`** (letakkan di atas `RuntimeEngine`):

```rust
/// Satu choke point untuk semua penulisan event engine: filter opt-out
/// diterapkan di sini DAN hanya di sini. Allowlist `NotificationKind`
/// menjamin gate interaktif dan per-request response tidak pernah
/// difilter — opt-out tidak bisa membunuh alur run.
struct EventSink<T: Transport> {
    transport: Arc<T>,
    opt_outs: Arc<Mutex<std::collections::HashSet<NotificationKind>>>,
}

impl<T: Transport> EventSink<T> {
    fn new(transport: Arc<T>, opt_outs: Arc<Mutex<std::collections::HashSet<NotificationKind>>>) -> Self {
        Self { transport, opt_outs }
    }

    fn set_opt_outs(&self, kinds: &[NotificationKind]) {
        *self.opt_outs.lock().unwrap() = kinds.iter().copied().collect();
    }

    fn suppressed(&self, event: &Event) -> bool {
        let kind = match event {
            Event::Thinking { .. } => NotificationKind::Thinking,
            Event::AssistantDelta { .. } => NotificationKind::AssistantDelta,
            Event::Warning { .. } => NotificationKind::Warning,
            Event::TodoChanged { .. } => NotificationKind::TodoChanged,
            _ => return false,
        };
        self.opt_outs.lock().unwrap().contains(&kind)
    }

    fn send(&self, request_id: Option<String>, event: Event) {
        if self.suppressed(&event) {
            return;
        }
        if let Err(error) = self
            .transport
            .write_event(&ProtocolEvent::new(request_id, event))
        {
            tracing::warn!(%error, "failed to write runtime event");
        }
    }
}

impl<T: Transport> std::ops::Deref for EventSink<T> {
    type Target = T; // read_request masih butuh akses transport
    fn deref(&self) -> &T {
        &self.transport
    }
}
```

> Catatan: `Deref` ke transport diambil untuk loop utama membaca request dari transport lewat sink. Alternatif — simpan `transport: Arc<T>` terpisah di engine dan sink hanya untuk send. Pilih yang engine-modul paling bersih: jika `Deref` mengganggu (borrow conflict), pakai alternatif: sink field `sink: EventSink<T>` untuk send, dan engine tetap punya `transport: Arc<T>` untuk read. Implementer memutuskan internal-nya; kontrak test hanya butuh `EventSink::new`, `::set_opt_outs`, `::send` publik-terlihat dari test modul sink_tests.

3. **State struct** `RuntimeEngine<T>` — ganti `orchestrator: Option<Arc<Orchestrator>>` tetap (dipakai run path) tapi tambah:

```rust
    initialized: bool,
    capabilities: ConnectionCapabilities,
```

   dan field transport yang dipakai sink. Constructor `new`, `with_session`, `with_provider`: `initialized` = `true` untuk `with_session`/`with_provider` (orchestrator sudah disuntik), `false` untuk `new`. `capabilities` = `Default::default()`.
   Test yang sudah meng-hangs karena `run_threads` join behavior — tidak berubah perilaku loop; hanya dispatch arm.

4. **Dispatch arms** (per-arm; string error baru):

- `Init`:
```rust
                Request::Init { overrides, capabilities } => {
                    if self.initialized {
                        self.emit(
                            Some(request.id),
                            Event::Error { message: "already initialized".to_string() },
                        );
                    } else {
                        match self.initialize(overrides) {
                            Ok(()) => {
                                self.capabilities = capabilities.clone();
                                self.sink.set_opt_outs(&capabilities.opt_out_notify);
                                self.initialized = true;
                                self.emit(
                                    Some(request.id),
                                    Event::InitOk {
                                        protocol: PROTOCOL_NAME.to_string(),
                                        version: PROTOCOL_VERSION,
                                    },
                                );
                            }
                            Err(message) => self.emit(Some(request.id), Event::Error { message }),
                        }
                    }
                }
```

- `Run` (auto-init DIHAPUS):
```rust
                Request::Run { task, attachments } => {
                    if !self.initialized {
                        self.emit(Some(request.id), Event::Error { message: "not initialized".to_string() });
                        continue;
                    }
                    self.start_run(request.id, task, attachments);
                }
```

- `ResetSession`, `AddSystemMessage`, `SkillActivate` (yang sudah memeriksa): ganti pesan error jadi persis `"not initialized"`.
- `ListTools`, `SearchMemory`: hapus `.unwrap_or_default()`; wajib init:
```rust
                Request::ListTools => {
                    if !self.initialized {
                        self.emit(Some(request.id), Event::Error { message: "not initialized".to_string() });
                        continue;
                    }
                    let tools = /* kode list yang ada */;
                    self.emit(Some(request.id), Event::ToolList { tools });
                }
```
(mirip untuk SearchMemory; Return error bukan data kosong dan bukan silent).
- `Plan`:
```rust
                Request::Plan { goal, require_approval } => {
                    if !self.initialized {
                        self.emit(Some(request.id), Event::Error { message: "not initialized".to_string() });
                        continue;
                    }
                    if !self.capabilities.experimental_api {
                        self.emit(Some(request.id),
                            Event::Error { message: "capability 'experimental_api' required".to_string() });
                        continue;
                    }
                    self.handle_plan(request.id, goal, require_approval);
                }
```

5. **Semua penulisan event lewat sink** — ganti `self.emit(...)` internal `emit()` helper jadi delegasi ke `self.sink.send(...)`, dan ganti SEMUA `transport.write_event(&ProtocolEvent::new(...))` di handler (runtime_tool_deps, with_provider confirmation handler, install_confirmation_handler, runtime question/todo handler, handle_plan forwarder + approval callback, start_run `on_event` closure + delta closure) memakai kloning sink yang sama sehingga opt-out berlaku global. Detail per site:
   - `emit()` helper jadi: `self.sink.send(request_id, event)`.
   - `RuntimeQuestionHandler`/`RuntimeTodoHandler` menerima kloning `Arc<Mutex<HashSet<...>>>` + `Arc<T>` (atau langsung `EventSink<T>` kloning — jika `EventSink` clone: derive `Clone` manually (Arc field)) — implementer memilih bentuk yang compile bersih; kontrak: SEMUA filter opt-out via HashSet yang sama.
   - `handle_plan` + `start_run` closure menerima `sink.clone()`-semacamnya.

6. **Update test lama di `runtime_engine.rs`:**
   - `runtime_emits_ready_init_ok_delta_and_done`: engine `with_session` sudah initialized → **hapus pengiriman Init** dan rename test jadi `runtime_emits_ready_delta_and_done_without_init`; assertion InitOk DIHAPUS (iya, karena tidak ada Init). (Dokumen perubahan ini di report.)
   - Test lain yang mengirim `Request::Init { overrides }` (grep) → tambahkan `capabilities: Default::default()`.
   - Test `protocol.rs` lama yang membuat `Request::Init` → idem.

- [ ] **Step 4: Jalankan test core → verify pass**

Run: `cargo test -p core-agentic`
Expected: semua pass. (Ada 3 test baru + 3 test sink + engine tests lama.)

- [ ] **Step 5: Commit**

```bash
git add core-agentic/src/runtime/engine.rs core-agentic/tests/runtime_engine.rs core-agentic/src/runtime/protocol.rs
git commit -m "feat(engine): strict init, experimental_api gate for Plan, opt-out sink"
```

---

### Task 3: CLI — caps threading di client + konstanta edilen + demo.js

**Files:**
- Modify: `agentic-cli/src/client.rs`
- Modify: `agentic-cli/src/commands.rs` (satu call site: `ensure_connection`)
- Modify: `scripts/protocol-demo.js` (HANYA bila memakai Plan)
- Test: `agentic-cli/src/client.rs` mod test

**Interfaces:**
- Consumes: semantik engine Task 2 (CLI wajib Init sebelum request lain — sudah begitu), `ConnectionCapabilities` (Task 1).
- Produces:
  - `pub const CLI_CAPABILITIES: ConnectionCapabilities = ConnectionCapabilities { experimental_api: true, opt_out_notify: Vec::new() };` — tapi `Vec::new()` di konst! PAKAI `serde_json` tidak; konst tidak bisa `vec![]`? `Vec::new()` di konst VALID (const fn). OK — pakai `Vec::new()`.
  - `AgentConnection::connect(mode, overrides, caps)`, `InProcessClient::start(overrides, caps)`, `RuntimeClient::spawn(overrides, caps)` / `spawn_binary(binary, overrides, caps)`.
  - `perform_handshake(io, overrides, caps)` — Init frame membawa caps ini.

- [ ] **Step 1: Update handshake + client constructors**

Di `agentic-cli/src/client.rs`:

1. `perform_handshake` signature: `pub(crate) async fn perform_handshake(io: &mut impl ConnectionIo, overrides: InitOverrides, caps: ConnectionCapabilities) -> Result<()>` dan body kirim:
```rust
    io.raw_send(Request::Init { overrides, capabilities: caps }).await?;
```
2. `RuntimeClient::spawn(overrides, caps)` + `spawn_binary(binary, overrides, caps)` → pass ke handshake.
3. `InProcessClient::start(overrides, caps)` → `perform_handshake(&mut client, overrides, caps).await?`.
4. `AgentConnection::connect(mode, overrides, caps)` → pass kedu arm.
5. Konstanta:
```rust
/// Capabilities yang dikirim CLI saat handshake: CLI memakai `/plan` /
/// `--plan`, jadi opt-in `experimental_api` itu jujur.
pub const CLI_CAPABILITIES: ConnectionCapabilities = ConnectionCapabilities {
    experimental_api: true,
    opt_out_notify: Vec::new(),
};
```

- [ ] **Step 2: Wiring `ensure_connection`**

`agentic-cli/src/commands.rs` — `ensure_connection` (grep `AgentConnection::connect`):

```rust
        self.connection = Some(
            crate::client::AgentConnection::connect(
                self.runtime_mode,
                overrides,
                crate::client::CLI_CAPABILITIES,
            )
            .await?,
        );
```

- [ ] **Step 3: Update test client yang memanggil constructor**

Grep mod test `client.rs` yang memanggil `InProcessClient::start(...)` / `AgentConnection::connect(...)` dengan satu argumen → tambah `CLI_CAPABILITIES` sebagai argumen terakhir (test `agent_connection_connects_in_both_modes`, `in_process_handshake_and_list_tools_round_trip`, dan lainnya bila ada).

Tambah test baru (assert caps benar-benar di-serialize di Init frame — hanya bisa diverifikasi di jalur spawn; buat helper capture):

Pola (di mod `in_process_tests`): frame handshake pada jalur spawned tidak bisa di-introspect dari sisi client — jadi test konsistensi caps cukup di level unit: `Request::Init` dengan `CLI_CAPABILITIES` di-serialize membawa `experimentalApi: true`:

```rust
    #[test]
    fn cli_capabilities_serialize_experimental_opt_in() {
        let frame = serde_json::to_value(core_agentic::runtime::protocol::ProtocolRequest::new(
            "i",
            Request::Init {
                overrides: Default::default(),
                capabilities: CLI_CAPABILITIES,
            },
        ))
        .unwrap();
        assert_eq!(frame["capabilities"]["experimentalApi"], true);
    }
```

- [ ] **Step 4: Run seluruh test CLI**

Run: `cargo test -p agentic-cli`
Expected: semua pass — termasuk smoke matriks 4 test (client kini mengirim caps; engine gating kini aktif).

- [ ] **Step 5: `scripts/protocol-demo.js`**

Baca file. Jika memakai `Request::Plan` → tambahkan `capabilities: { experimentalApi: true }` ke object Init-nya. Jika tidak → tidak ada perubahan (Init tanpa caps = serde default, tetap valid). Laporkan keputusannya.

- [ ] **Step 6: Commit**

```bash
git add agentic-cli/src/client.rs agentic-cli/src/commands.rs scripts/protocol-demo.js
git commit -m "feat(cli): send ConnectionCapabilities (experimental opt-in) at handshake"
```

---

### Task 4: Docs + gate CI (fmt, clippy, full test)

**Files:**
- Modify: `CHANGELOG.md`
- Modify: `docs/BENCHMARK_CODEX.md`
- Modify: `docs/ROADMAP.md` (open work: tandai Fase 2a landed, Fase 2b = steer queue open)

- [ ] **Step 1: CHANGELOG**

Di bawah `## [Unreleased]` → `### Added`:

```markdown
- Runtime: **protocol hardening** (Fase 2a) — `Init` wajib tepat sekali
  (`not initialized` / `already initialized` errors), capability
  negotiation (`experimental_api` opt-in, `Request::Plan` kini API
  eksperimental), dan notification opt-out (`ConnectionCapabilities.
  optOutNotify`, allowlist: thinking/assistant_delta/warning/todo_changed).
  Semua additive; `PROTOCOL_VERSION` tetap 1.
```

- [ ] **Step 2: BENCHMARK_CODEX.md — tandai Fase 2a**

Ubah judul section `### Fase 2 — Protocol hardening gaya codex` → tambahkan `✅ (landed 2026-11, kecuali antrian steer → Fase 2b)` dan tandai item 1–3 done, item 4 (antrian steer) tetap open → Fase 2b. Update juga baris tabel `Handshake | ... | cek tunggal | ⚠️ cukup untuk v1` → `capabilities + opt-out | ✅ setara`.

- [ ] **Step 3: ROADMAP.md**

Di bagian "Open work" (atau section atas): tambah catatan satu baris — Fase 2a (capabilities+strict init+opt-out) landed; **Fase 2b: steer/follow-up queue** (terdokumentasi di BENCHMARK_CODEX + COMPARISON_PI_VS_AGENTIC) adalah item protocol berikutnya.

- [ ] **Step 4: fmt + clippy + full test**

Run:
```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Expected: fmt bersih, clippy 0 warning, semua test pass (~800). Perbaiki hingga bersih.

- [ ] **Step 5: Commit**

```bash
git add CHANGELOG.md docs/BENCHMARK_CODEX.md docs/ROADMAP.md agentic-cli/src core-agentic/src
git commit -m "docs: protocol hardening (Fase 2a) landed notes"
```

---

## Self-Review (dijalankan penulis plan — sudah dilakukan)

1. **Spec coverage**: capabilities negotiation (Task 1–3), strict init + semantic errors (Task 2), opt-out allowlist via sink satu choke point (Task 2), doc-contract pemisahan tipe (Task 1 Step 3.1), versi tetap 1 + additive (Task 1, Global Constraints), CLI tak berubah perilaku (Task 3, smoke matrix tetap hijau), demo.js check (Task 3 Step 5). Steer queue eksplisit keluar scope → Fase 2b. ✓
2. **Placeholder scan**: tidak ada TBD/TODO; semua step berisi kode konkret / command exact. Snippet test diizinkan disesuaikan implementer untuk literal field `Event` yang beda — instruksi eksplisit, bukan placeholder. ✓
3. **Type consistency**: `ConnectionCapabilities` / `NotificationKind` / field `capabilities` di `Request::Init` konsisten di Task 1–3; error string literal `"not initialized"` / `"already initialized"` / `"capability 'experimental_api' required"` konsisten; signature handshake `(io, overrides, caps)` konsisten di Task 2 vs 3; `EventSink::{new, set_opt_outs, send}` konsisten. Satu pure-bug kecil di draf sebelumnya (pemanggilan `single_request` payload lama tanpa caps) sudah diganti dengan instruksi grep-update eksplisit. ✓
