# Steer & Queue (Fase 2b) Implementation Plan

>
> **Status:** LANDED — semua step selesai dieksekusi (checkbox disweep housekeeping 2026-11-12); fiturnya ada di CHANGELOG [0.6.0].
> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Agent bisa "dibimbing" mid-run — `Request::Steer` di engine (injeksi user message di iteration boundary, drain FIFO, gate-safe) + client-side `QueueMode` (`interrupt` via steer, `follow-up` via antrian lokal client yang mengirim `Run` berikutnya sendiri) + `AgentConnection::steer()` fire-and-forget + flag `--queue-mode` + slash `/queue`.

**Architecture:** Split ala pi: **steer = core** (orchestrator hook `set_steering_source` dipoll di guard cluster tiap iterasi; engine menampung antrian + reply `SteerQueued`; injeksi memancar via `on_event` yang ada sehingga otomatis ter-tag run request-id), **follow-up = frontend** (client menyimpan antrian lokal; setelah `Done`, client mengirim `Run` berikutnya — tanpa perubahan protokol di luar `Request::Steer` + 2 event). Semua additive, `PROTOCOL_VERSION` tetap 1.

**Tech Stack:** Rust (serde, std::sync::Mutex/VecDeque), JSON Lines protocol; tanpa dependensi baru. Reference spec: `docs/superpowers/specs/2026-11-03-steer-queue-design.md`.

## Global Constraints

- Semua protokol additive; `PROTOCOL_VERSION` tetap `1`; wire snake_case untuk `Request::Steer` (`"type":"steer"`), `Steered`/`SteerQueued` ikut konvensi enum `Event` yang ada.
- String error persis: `"not initialized"`, `"no active run"` (poli 2a yang lain tidak berubah).
- Allowlist opt-out Fase 2a TIDAK berubah: `Steered`/`SteerQueued` tidak boleh ditambah ke `NotificationKind` dan tidak boleh masuk jalur filter `EventSink` (selalu terkirim).
- Orchestrator tanpa `steering_source` ter-set = **no-op total** (hook `Option`), termasuk tidak ada alokasi/alokasi minimal di jalur panas.
- Injeksi selalu: drain **seluruh** antrian FIFO di satu boundary; tiap teks → memory sebagai **role user** → `Event::Steered { content }` (urutan event: Steered sebelum LLM call berikutnya).
- Run selalu frontend-issued: **tidak boleh** ada kode engine yang mengirim `Request::Run`-semacam auto-start.
- Gate-safety: steer yang tiba saat run thread menunggu confirmation/question tetap mengantre dan dikonsumsi di boundary setelah gate selesai — test eksplisit.
- Leftover steer saat run sebelum boundary terakhir: tersimpan di antrian (session-scope) dan dikonsumsi run berikutnya — perilaku deterministic, terdokumentasi di spec §4.
- Modul core yang berubah: `events.rs`, `orchestrator/**` (hook + poll), `runtime/{protocol,engine}.rs`, test engine. TIDAK: planner/tools/safety/memory-modul lain (memory dipakai lewat API add yang ada).
- CLI: renderer/event order tidak berubah; file: `client.rs`, `commands.rs`, `cli.rs`, `interactive.rs` (slash `/queue` minimal), test matriks.
- Semua test hermetic; smoke matriks lama tetap hijau; tanpa dependensi baru.
- CI gate: `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`, seluruh test pass.
- Branch kerja: `feature/steer-queue` dari `dev`.

---

### Task 1: Core — event variants + orchestrator steering hook + poll di boundary

**Files:**
- Modify: `core-agentic/src/events.rs` (2 varian `Event`)
- Modify: `core-agentic/src/orchestrator/**` (hook + poll; file persis mengikuti layout modul yang ada — grep `set_confirmation_handler` untuk menemukan tempat menaruh setter dan grep guard cluster cancel di run loop untuk titik poll)
- Test: test level orchestrator yang ada (pola harness `orchestrator_loop.rs`)

**Interfaces:**
- Produces (dipakai Task 2–3):
  - `Event::SteerQueued { content: String }`
  - `Event::Steered { content: String }`
  - `Orchestrator::set_steering_source(&mut self, source: Box<dyn Fn() -> Vec<String> + Send + Sync>)`
  - field `steering_source: Option<Box<dyn Fn() -> Vec<String> + Send + Sync>>` (default `None`)
- Kontrak poll: di awal setiap iterasi run loop, pada titik guard `cancel`/`pause` yang ada (SEBELUM `maybe_autocompact`/`build_messages` LLM call berikutnya):
  ```rust
  if let Some(source) = &self.steering_source {
      for message in source() {
          // 1. memory.add_message(role user) — pakai API add yang sudah ada
          // 2. emit Event::Steered { content: message } lewat jalur on_event yang ada
      }
  }
  ```
  Drain dilakukan SATU KALI per boundary — source mengembalikan seluruh antrian yang siap (FIFO); orchestrator tidak mem-pesan ulang/retain.

- [x] **Step 1: Tulis test gagal (orchestrator level)**

Kebutuhan test: provider ter-script yang MENGHARUSKAN satu tool call terlebih dahulu, lalu satu jawaban final — supaya run tak selesai di iterasi pertama dan ada boundary kedua untuk injeksi. Gunakan pola `support::ScriptedProvider` + `support::tool_call_response` dari `core-agentic/tests/support/` dan pola test yang ada di `orchestrator_loop.rs`. Tambahkan di `core-agentic/tests/orchestrator_loop.rs` (ikuti import yang ada):

```rust
/// Steering hook: pesan yang antri di source masuk sebagai user message
/// di boundary berikutnya dan memancar sebagai Event::Steered.
#[test]
fn steering_source_messages_injected_at_iteration_boundary() {
    // Provider: turn 1 = tool call (run masih belum selesai → ada boundary
    // berikutnya), turn 2 = jawaban final. Setelah tool eksekusi (turn 1
    // selesai), orchestrator memanggil `steering_source()` SEBELUM LLM call
    // turn 2 — saat itulah steer masuk.
    let steering = Arc::new(Mutex::new(vec!["use the lake theme instead".to_string()]));
    let steering_source = {
        let steering = steering.clone();
        move || steering.lock().unwrap().drain(..).collect::<Vec<_>>()
    };

    let provider = Arc::new(ScriptedProvider::new(vec![
        support::tool_call_response("t1", "grep", &serde_json::json!({"pattern": "x"})),
        final_response("done"),
    ]));
    let mut orchestrator = Orchestrator::new(provider, registry_with_grep());
    orchestrator.set_steering_source(Box::new(steering_source));

    let events = orchestrator.stream_events(|events| events); // pola emit capture yang ada di file
    orchestrator.run("pick a theme").expect("run ok");

    // Steered dipancarkan, konten benar.
    assert!(
        events.iter().any(
            |e| matches!(e, Event::Steered { content } if content == "use the lake theme instead")
        ),
        "expected Steered event, got {events:?}"
    );
    // ChatRequest LLM berikutnya memuat steer sebagai ROLE USER di konteks
    // (masuk sebelum jawaban final — tidak hilang).
    // (Provider capture: jika pola capturing-request belum ada di support,
    // tambahkan `CapturingScriptedProvider` di support/mod.rs — provider
    // yang mem-script jawaban DAN menyalin setiap ChatRequest.messages
    // ke Arc<Mutex<Vec<ChatRequest>>>; pola implementasi sama dengan
    // ScriptedProvider.)
}
```

> Sesuaikan helper-genlok dengan yang benar-benar ada di `orchestrator_loop.rs`/`support` (`final_response`, `registry_with_grep`, emit-capture — semua mengikuti file itu; nama di atas arti-kontraktual, bukan nama final). Yang TIDA boleh berubah: rumitnya 2-turn (tool lalu final), assertions Steered + konteks LLM role user.

Tambah juga test no-op:

```rust
/// Tanpa steering_source: run tak berubah, tidak ada Steered yang dipancarkan.
#[test]
fn run_without_steering_source_has_no_Steered_events() {
    // pola test happy-path existing; assert events tidak memuat Event::Steered
    // dan tidak mengubah behavior lain.
}
```

- [x] **Step 2: Jalankan test → verify gagal**

Run: `cargo test -p core-agentic --test orchestrator_loop`
Expected: COMPILE ERROR — `set_steering_source` / `Event::Steered` belum ada.

- [x] **Step 3: Implementasi (event + hook + poll)**

1. `events.rs`: tambahkan dua varian ke `Event` (letakkan dekat grup lifecycle events):
```rust
    /// Dipancarkan engine saat menerima Request::Steer (ack segera; reply).
    SteerQueued { content: String },
    /// Dipancarkan orchestrator saat steer dikonsumsi di iteration boundary.
    Steered { content: String },
```
   Serde mengikuti enum yang ada (`#[serde(tag="type", rename_all="snake_case")]` → wire `steer_queued`/`steered`).

2. Orchestrator:
   - Field: `steering_source: Option<Box<dyn Fn() -> Vec<String> + Send + Sync>>` + `set_steering_source` setter (pola setter lain).
   - Run-loop poll: di guard cluster cancel/pause (temukan dengan grep) — SEBELUM call model berikutnya; drain SATU kali; untuk tiap teks: add user message (pakai API memory yang ada) + emit `Steered`.

- [x] **Step 4: Jalankan test → verify pass + workspace green**

Run: `cargo test -p core-agentic` lalu `cargo build --workspace && cargo test -p agentic-cli`
Expected: semua pass.

- [x] **Step 5: Commit**

```bash
git add core-agentic/src/events.rs core-agentic/src/orchestrator core-agentic/tests
git commit -m "feat(core): steering source hook + Steered event at iteration boundary"
```

---

### Task 2: Runtime — `Request::Steer` + engine antrian + reply/gating

**Files:**
- Modify: `core-agentic/src/runtime/protocol.rs` (variant Request)
- Modify: `core-agentic/src/runtime/engine.rs` (arm + queue + wiring hook)
- Test: `core-agentic/tests/runtime_engine.rs`

**Interfaces:**
- Consumes: `Event::{SteerQueued, Steered}`, `set_steering_source` (Task 1).
- Produces (dipakai Task 3):
  - `Request::Steer { content: String }` — engine arm mengirim SteerQueued reply + error strict.
  - Engine otomatis menghubungkan antrian ke orchestrator pada semua constructor yang membuat orchestrator (`initialize`, `with_provider`, `with_session`): closure source drain dari `steer_queue` engine; satu `Arc` queue digunakan bersama.

- [x] **Step 1: Tulis test gagal di `runtime_engine.rs`**

Skenario utama (end-to-end engine, run + steer + boundary):

```rust
/// Steer saat run aktif: SteerQueued reply segera (request id steer),
/// Steered di-emmit dengan request id RUN, dan konteks model berikutnya
/// memuat steer sebagai user message.
#[test]
fn steer_during_run_injects_at_next_boundary() {
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let transport = MemoryTransport::new(request_rx, event_tx);
    let provider = Arc::new(support::ScriptedProvider::new(vec![
        // turn 1: tool call (run belum selesai → ada boundary berikut)
        support::tool_call_response("t1", "grep", &serde_json::json!({"pattern":"x"})),
        // turn 2: final
        support::ChatResponse { /* final content, pola support */ },
    ]));
    let engine = RuntimeEngine::with_session(
        transport,
        provider,
        registry_with_grep_tool(), // ToolRegistry dari testsupport
    );
    let worker = std::thread::spawn(move || engine.run());
    // engine run harus sudah auto-Init (with_session) → langsung Run
    // (impl note: kirim request di titik yang tepat agar steer tiba SAAT
    // run aktif: kirim Run dulu, sleep singkat/loop-until ToolStarted lalu
    // kirim Steer — style polling dengan try_recv di event channel.)
```

> Detail run flow test: di engine, `with_session` means `initialized = true`, tapi `Request::Run` tetap diperlukan untuk memulai run. Steer dikirim SETelah run aktif; to complement, capture provider nevote konteks; assertions:
> 1. Ada event `SteerQueued` / its event request_id sama dengan id request kirim ("steer-1").
> 2. Ada event `Steered { running request id }` — ter-tag dengan run-id, BUKAN id steer.
> 3. Captured provider request ke-2 memuat `"use the lake theme instead"` sebagai role user.
> 4. Test tetap hermetic dan finite (lock/join di timeout).

Test tambahan (pola yang sama, hermetic):

```rust
#[test]
fn steer_before_init_is_rejected() { /* fresh engine → Steer → Error "not initialized" */ }

#[test]
fn steer_when_no_run_active_is_rejected() {
    /* with_session engine, tidak ada Run di-kirim (atau Run sudah selesai) →
       Steer → Error "no active run" */
}

#[test]
fn steers_consume_fifo_order() { /* 3 steer saat boundary → Steered berurutan a,b,c */ }
```

- [x] **Step 2: Jalankan test → verify gagal**

Run: `cargo test -p core-agentic --test runtime_engine`
Expected: COMPILE ERROR — `Request::Steer` tidak ada.

- [x] **Step 3: Implementasi**

1. `protocol.rs` — variant baru:
```rust
    /// Steer the currently-running agent: content masuk sebagai user
    /// message di iteration boundary berikutnya (reply: SteerQueued).
    Steer {
        content: String,
    },
```

2. `engine.rs`:
   - Field: `steer_queue: Arc<Mutex<std::collections::VecDeque<String>>>` (default).
   - Di titik konstruksi orchestrator yang ada (`initialize(...)`, `with_provider`, `with_session`) — bind hook:
   ```rust
   orchestrator.set_steering_source({
       let queue = self.steer_queue.clone();
       move || queue.lock().unwrap().drain(..).collect()
   });
   ```
   (di `with_session`/`with_provider` orchestrator di-clone sebelum `run`; hook ditaruh sekali — implementer mengikuti struktur engine yang ada, hook cukup dipasang di semua constructor yang membuat orchestrator; efektifnya satu `Arc` queue yang pasta dari engine field.)
   - Arm baru di dispatch:
   ```rust
                Request::Steer { content } => {
                    if !self.initialized {
                        self.emit(Some(request.id), Event::Error { message: "not initialized".to_string() });
                    } else if !self.running.load(Ordering::SeqCst) {
                        self.emit(Some(request.id), Event::Error { message: "no active run".to_string() });
                    } else {
                        self.steer_queue.lock().unwrap().push_back(content.clone());
                        self.emit(
                            Some(request.id),
                            Event::SteerQueued { content },
                        );
                    }
                }
   ```
   - `Steered` mengalir lewat jalur on_event orchestrator yang sudah menulis ke sink — TIDAK perlu perubahan di sink (bukan event gate/response).

3. Catat: string error persis mengikuti Global Constraints; allowlist `NotificationKind` TIDAK ditambah.

- [x] **Step 4: Jalankan test → verify pass 2 kali (workspace)**

Run: `cargo test -p core-agentic` lalu `cargo build --workspace && cargo test -p agentic-cli`
Expected: semua pass; smoke CLI matriks (embedded+spawned) tetap hijau.

- [x] **Step 5: Commit**

```bash
git add core-agentic/src/runtime core-agentic/tests/runtime_engine.rs
git commit -m "feat(runtime): Request::Steer with engine queue, SteerQueued reply, strict gating"
```

---

### Task 3: CLI — `steer()`, QueueMode, follow-up queue, `/queue`

**Files:**
- Modify: `agentic-cli/src/client.rs` (`AgentConnection::steer`)
- Modify: `agentic-cli/src/commands.rs` (QueueMode + ongoing queue + drain-after-Done)
- Modify: `agentic-cli/src/cli.rs` (`--queue-mode` global flag)
- Modify: `agentic-cli/src/interactive.rs` (slash `/queue`)
- Test: `agentic-cli/src/commands.rs` e2e mod + inline tests yang sesuai

**Interfaces:**
- Consumes: `Request::Steer` (Task 2).
- Produces:
  - `pub impl AgentConnection { pub async fn steer(&mut self, content: String) -> Result<()> }` (send-only, no wait)
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)] pub enum QueueMode { #[default] FollowUp, Interrupt }` di commands.rs (PREFER: didefinisikan di commands.rs sebagai tipe runtime CLI; arg clap di pisah di cli.rs sebagai `QueueModeArg`)
  - `Commands::with_queue_mode(...)`, `Commands::queue_mode_getter`; state `pending_follow_ups: VecDeque<String>`
  - Slash: `/queue` (tampilkan mode + isi antrian), `/queue interrupt` / `/queue follow-up` (ganti mode; isi antrian tampil) — pola handler slash yang ada di interactive.rs.

- [x] **Step 1: client `steer()` (TDD kecil)**

Test + implementasi di client.rs:
```rust
    /// Fire-and-forget steer: kirim request, tidak menunggu reply
    /// (reply `SteerQueued` tiba di stream event biasa).
    pub async fn steer(&mut self, content: String) -> Result<String> {
        self.send(Request::Steer { content }).await
    }
```
   (di `AgentConnection` — delegasi arms; tambah test hermetic yang memanggil steer sesudah run aktif via mocked/run flow yang sudah ada pada test modul: pola run_connection smoke; untuk unit sederhana cukup assert frame `{"type":"steer",...}` serialized benar:
```rust
    #[test]
    fn steer_request_serializes_snake_case() {
        let frame = serde_json::to_value(ProtocolRequest::new("s1", Request::Steer { content: "hi".into() })).unwrap();
        assert_eq!(frame["type"], "steer");
        assert_eq!(frame["content"], "hi");
    }
```
)

- [x] **Step 2: QueueMode + follow-up queue di Commands**

1. Type + state + builder di commands.rs:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueMode {
    /// Pesan selama run aktif diantrikan lokal; dikirim sebagai Run berikutnya.
    #[default]
    FollowUp,
    /// Pesan selama run aktif dikirim langsung sebagai steer ke engine.
    Interrupt,
}
```
   - Field `queue_mode: QueueMode` (default FollowUp) + `with_queue_mode`.
   - Field `pending_follow_ups: std::collections::VecDeque<String>`.
   - API untuk jalur input saat run aktif (yang dipakai UI nanti):
   ```rust
   pub(crate) async fn submit_user_text(&mut self, text: String) -> Result<bool> {
       // Fungsi ini adalah entrypoint programatik (unit-test + TUI usai
       // fase berikut); input-during-run inline REPL out of scope.
       match self.queue_mode {
           QueueMode::Interrupt => {
               let conn = self.connection.as_mut().ok_or_else(|| anyhow::anyhow!("not connected"))?;
               conn.steer(text).await?;
               Ok(true)
           }
           QueueMode::FollowUp => {
               self.pending_follow_ups.push_back(text);
               Ok(true)
           }
       }
   }

   /// Setelah run selesai: drain antrian follow-up — kirim Run berikutnya
   /// (client-issued, loop, guard depth).
   async fn drain_follow_ups(&mut self) -> Result<()> {
       while let Some(next) = self.pending_follow_ups.pop_front() {
           self.run(&next).await?; // jalur run ada yang sudah diuji
       }
       Ok(())
   }
   ```
   Integrasi drain: di `run_with_callbacks` (dan padanan jalur `run`) SETELAH hasil akhir (Ok/Err) sekali, sebelum return; call sites yang memakai structure final: grep `Done` handler or return-point di run path. Simpan rekursi minimal: drain loop di command layer, bukan recursive call menjauh run-with-run (mengunana run methods yang ada).

2. `--queue-mode` di cli.rs (pola `PermissionModeArg`):
```rust
    /// How user text typed while a run is active is handled
    ///
    /// - follow-up: locally queued; sent as the next run after the
    ///   current one finishes (default)
    /// - interrupt: sent to the engine as a steer; injected at the next
    ///   tool boundary of the running task
    #[arg(long, global = true, value_name = "MODE", default_value = "follow-up")]
    pub queue_mode: QueueModeArg,
```
   + `QueueModeArg` konversi; wiring di main.rs `.with_queue_mode(cli.queue_mode.into())`.

- [x] **Step 3: `/queue` slash command minimal di interactive.rs**

- `/queue` (tanpa arg) → tampilkan mode aktif + isi antrian (index + potongan 80 char).
- `/queue interrupt` / `/queue follow-up` → switch mode runtime (setter di Commands) + konfirmasi single line.
- Bangun dengan pola handler slash `/mode`/`/provider` yang sudah ada. (Input-during-run tetap out of scope: command dipanggil saat REPL idle.)

- [x] **Step 4: e2e smoke untuk flow follow-up (both transports)**

Tambah ke e2e matrix mod di commands.rs: dari runtime mode embedded/spawned × QueueMode:
- Jalankan run pertama (mock hermetic unreachable provider → run selesai gagal dengan 1 tour).
- Panggil `submit_user_text("second")` setelahnya (FollowUp mode) → antrian bertambah; hubungkan `drain_follow_ups()` → run kedua dikirim via connection (driver terjatuh adalah request id kedua; test assert: run kedua menghasilkan SessionStarted lagi — pola assertions yang ada di smoke_lifecycle).
- galat: client-issued garanti: Total request Run count == jumlah item antrian + 1.
Dalam praktik yang dipinjamkan dari smoke e2e hermetic provider fixture yang persis (127.0.0.1:9).

- [x] **Step 5: Full test + gates**

Run: `cargo test --workspace`, `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`
Expected: semua hijau.

- [x] **Step 6: Commit**

```bash
git add agentic-cli/src
git commit -m "feat(cli): QueueMode (interrupt steer / follow-up queue) + steer() + /queue"
```

---

### Task 4: Docs + testing matrix + gate CI

**Files:**
- Modify: `CHANGELOG.md`
- Modify: `docs/BENCHMARK_CODEX.md` (Fase 2b landed: item 4 antrian; Tabel "Antrian saat streaming" ✅; COMPARISON doc tak disentuh)
- Modify: `docs/ROADMAP.md` (Fase 2b landed; berikutnya = Fase 3 daemon)

- [x] **Step 1: CHANGELOG** — di `## [Unreleased]` → `### Added`:

```markdown
- Runtime: **steer & queue** (Fase 2b) — the agent can now be steered
  mid-run: `Request::Steer { content }` queues user text engine-side;
  at the next tool boundary it is injected as a real user message and
  announced with `Steered` (ack via `SteerQueued`). Errors: `not
  initialized` / `no active run`. Pending gates never lose user input.
  CLI adds `--queue-mode interrupt|follow-up` and the `/queue` slash
  command: `interrupt` sends live steers, `follow-up` (default) queues
  text client-side and issues the next run itself. Additive; protocol
  version stays 1.
```

- [x] **Step 2: BENCHMARK_CODEX.md** — tandai Fase 2b landed:

- Judul: `### Fase 2 — Protocol hardening gaya codex ✅ (landed 2026-11)` — titip setelah Fase 2a: tambahkan sub-catatan `#### Fase 2b — Antrian steer/follow-up ✅ (landed 2026-11)` yang menjelaskan split ala pi (steer di engine, follow-up client-side).
- Tabel: baris `Antrian saat streaming | ... | 🔜 backlogged` → `Request::Steer antrian engine + follow-up client-side | ✅ ala pi (split shell/core)`.

- [x] **Step 3: ROADMAP.md** — update open-work catatan: Fase 2b landed; berikutnya Fase 3 (daemon long-lived). Tambahkan catatan: input-during-run inline REPL (Fase UX) masih open.

- [x] **Step 4: Gates + full test**

Run:
```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Perbaiki sampai bersih; commit hasil (jika failing code fixes diperlukan, buat satu commit "fix: polish steering wiring" dan laporkan).

- [x] **Step 5: Commit**

```bash
git add CHANGELOG.md docs/BENCHMARK_CODEX.md docs/ROADMAP.md agentic-cli/src core-agentic/src core-agentic/tests
git commit -m "docs: steer & queue (Fase 2b) landed notes"
```

---

## Self-Review (dijalankan penulis plan — sudah dilakukan)

1. **Spec coverage**: engine steer (Task 1+2), QueueMode interrupt/follow-up + client queue + drain (Task 3), gate-safety test (Task 2 step 1 catatan), error strings exact (Task 2), idle/pre-init reject (Task 2), FIFO drain (Task 1 poll + Task 2 test), docs & gates (Task 4). Leftover session-scope behavior terdokumentasi di spec §4 dan tidak butuh task khusus (engine queue policy yang menghasilkannya alami). ✓
2. **Placeholder scan**: tidak ada TBD/TODO. Snippet orchestrator-test memakai nama temporer dari harness yang ada — diinstruksikan tegas "sesuaikan ke pola file, jangan ubah kontrak assertions". Implementer yang menyentuh lebih dari adaptasi pola akan tertangkap review. ✓
3. **Type consistency**: `Request::Steer { content: String }` konsisten Task 2–3; `Event::{SteerQueued, Steered { content }}` konsisten; `steer(content) -> Result<String>` konsisten; `QueueMode-{FollowUp, Interrupt}` konsisten dgn `QueueModeArg`; error strings dua buah konsisten dg Global Constraints. ✓
