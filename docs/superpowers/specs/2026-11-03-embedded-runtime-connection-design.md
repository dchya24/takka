# Design: Embedded Runtime Connection (Dual-Mode AgentConnection)

**Status:** Draft v1 — disetujui brainstorming 2026-11-03
**Scope:** Fase 1 dari roadmap adopsi codex-rs (`docs/BENCHMARK_CODEX.md`): satu abstraction client, dua transport.
**Acuan:** `docs/BENCHMARK_CODEX.md` (benchmark + aturan disiplin), `docs/PRD_Runtime_CLI_Decoupling.md` (arsitektur hasil v0.5.0).

---

## 1. Konteks & Tujuan

Sejak v0.5.0, seluruh logika agent berjalan di `agentic-runtime` (daemon headless) dan CLI adalah pure renderer. Namun `RuntimeClient` (`agentic-cli/src/client.rs`) **hard-wired spawn proses**: setiap sesi harus menemukan binary `agentic-runtime` (env `AGENTIC_RUNTIME_BIN` atau sibling executable) dan berkomunikasi via JSONL stdio.

Biaya konkret dari desain ini sudah pernah terjadi: bug v0.5.1 (source directory `agentic-runtime/` di workspace root keliru dianggap binary → spawn gagal "Permission denied"), plus risiko version skew CLI ↔ runtime.

Goal Fase 1: mode **embedded** — engine berjalan sebagai thread di proses CLI yang sama via `MemoryTransport` (sudah ada di core, belum pernah dipakai CLI) — di belakang satu abstraction yang sama dengan jalur spawn, tanpa mengubah protokol sedikit pun.

## 2. Keputusan Hasil Brainstorming

| Keputusan | Pilihan | Alasan |
|---|---|---|
| Default mode saat landed | **Embedded**; `--runtime spawn` sebagai escape hatch | target akhir benchmark doc dicapai sekali jalan; test matrix dua transport mengelola risikonya (keputusan user, opsi B) |
| Penempatan `InProcessClient` | `agentic-cli/src/client.rs`, sebelah `RuntimeClient` | engine + transport tetap reusable di core; yang in-process-specific (bridging tokio, thread supervision) memang urusan frontend Rust. Node/Bun tetap jalur spawn |
| Bridging async→blocking | Satu *bridge thread* per koneksi → `tokio::sync::mpsc` unbounded | lebih ringan daripada `spawn_blocking` per event untuk delta stream yang intensif |
| Duplikasi handshake/run-loop | Diekstrak ke helper generik (`ConnectionIo` trait + fungsi bebas) | aturan disiplin #2 benchmark doc: duplikasi handshake = bug menunggu waktu |
| Flag | `--runtime <embedded\|spawn>`, global, default `embedded` | pola sama dengan `--mode`; tanpa setting `config.toml` di fase ini (YAGNI, bisa menyusul saat daemon mode Fase 3) |
| Panic isolation | `catch_unwind` di thread engine + klon `evt_tx` untuk mengirim `Event::Error`; channel tertutup juga disintesis jadi `Error` | tanpa isolasi proses, panic engine harus tetap menghasilkan UX "runtime died" yang sama seperti jalur spawn |
| Protokol | **Tidak berubah sama sekali** | embedded hanya skip serialisasi, bukan skip protokol (aturan disiplin #1) |

## 3. Arsitektur Target

```text
agentic-cli/src/client.rs
├── trait ConnectionIo          ← BARU: raw_send + raw_next_event (RPITIT, tanpa dep async-trait)
├── async fn perform_handshake  ← BARU: Ready → Init → InitOk (dipakai kedua mode)
├── async fn send_and_wait_io   ← BARU: loop korelasi request_id (dipakai kedua mode)
├── async fn run_turn           ← BARU: loop run + gate interaktif (dipakai kedua mode)
├── RuntimeClient               ← refactor: implement ConnectionIo, delegasi ke helper di atas
├── InProcessClient             ← BARU: channel mpsc + thread engine + bridge thread
├── enum AgentConnection        ← BARU: { Spawned(RuntimeClient), InProcess(InProcessClient) }
└── enum RuntimeMode            ← BARU: { Embedded, Spawn }

agentic-cli/src/cli.rs
└── RuntimeModeArg + --runtime  ← BARU: global flag, default embedded (pola PermissionModeArg)

agentic-cli/src/commands.rs
└── Commands                    ← refactor: runtime_client: Option<RuntimeClient>
                                    → connection: Option<AgentConnection>
                                    ensure_runtime() → ensure_connection()

core-agentic                    ← TIDAK BERUBAH (engine + MemoryTransport sudah cukup)
```

### 3.1 Komponen kunci

**`ConnectionIo` + helper bersama.** Kedua client mengimplementasikan IO mentah; handshake, `send_and_wait`, dan run-loop tinggal ada di satu tempat. Versi protokol tetap divalidasi di `raw_next_event` masing-masing client (perilaku identik jalur lama).

**`InProcessClient`.** Topologi channel:

```text
req_tx (client) ──► [mpsc] ──► req_rx ──► MemoryTransport ◄── Arc ──► RuntimeEngine (thread "agentic-runtime-embedded")
evt_rx (bridge) ◄── [mpsc] ◄── evt_tx ◄──┘                                │ catch_unwind(engine.run())
evt_rx ──► tokio unbounded ──► events (client .recv().await)              └─ panic → Event::Error via evt_tx.clone()
```

- Thread engine native (`std::thread::Builder`, nama `agentic-runtime-embedded`) — bukan task tokio, karena `MemoryTransport` blocking.
- Bridge thread (`agentic-runtime-bridge`) meneruskan event blocking → tokio unbounded; `next_event` cukup `recv().await`.
- `send()` tidak pernah block (`mpsc::channel` unbounded), aman dipanggil dari async tanpa bridging.
- Handshake identik jalur spawn: `Ready` → `Request::Init` → `InitOk`.
- Shutdown: kirim `Request::Shutdown` + drop `req_tx` → loop engine menerima `None` → berakhir → join via `spawn_blocking`.
- Panic: `catch_unwind(AssertUnwindSafe(engine.run()))` → kirim `Event::Error` ("embedded runtime engine panicked") via klon `evt_tx` yang dipegang thread engine. Belt-and-braces: jika event channel tertutup tanpa event (thread mati tanpa sempat mengirim), `next_event` mensintesis `Error` ("embedded runtime engine stopped"). Renderer/TUI sudah punya UX "runtime died" — tidak perlu perubahan di sana.

**`AgentConnection`.** Enum dengan delegasi metode publik yang sama persis dengan `RuntimeClient` hari ini: `send`, `next_event`, `send_and_wait`, `run`, `shutdown`, plus konstruktor `connect(mode, overrides)`. `AgentConnection` sendiri mengimplementasikan `ConnectionIo` sehingga helper generik bisa berjalan di atasnya.

### 3.2 Flag & wiring

- `--runtime embedded|spawn`, global (root `Cli`), default `embedded`; konversi ke `RuntimeMode` via `Into`, pola `PermissionModeArg`.
- `main.rs`: `.with_runtime_mode(cli.runtime.into())`.
- `AGENTIC_RUNTIME_BIN` tetap dihormati **hanya** di jalur spawn; embedded tidak menyentuh filesystem binary.
- `switch_model` yang selama ini `self.runtime_client = None` menjadi `self.connection = None`; drop `AgentConnection::InProcess` otomatis menghentikan engine thread (req_tx drop → loop engine berakhir), tanpa join eksplisit — deterministik saat proses berakhir.

## 4. Data Flow (identik di kedua mode)

1. `ensure_connection()` → `AgentConnection::connect(mode, overrides)` → handshake.
2. Slash commands / run / TUI memanggil `send_and_wait` / `run` — tipe `ProtocolRequest`/`ProtocolEvent` sama, hanya beda: jalur spawn lewat serialisasi JSONL; embedded lewat channel memori (tanpa serialisasi).
3. Gate interaktif (`ConfirmationRequest`, `QuestionRequest`, `PlanApprovalRequest`) tetap bolak-balik request/response yang sama.
4. Panic/crash engine → `Event::Error` di kedua mode (proses mati terdeteksi via stdout tertutup pada jalur spawn; thread panic tertangkap pada jalur embedded).

## 5. Testing Strategy (test matrix)

Skenario identik dijalankan terhadap kedua koneksi — regresi serialisasi hanya muncul di jalur spawn, jadi keduanya wajib diuji:

| Skenario | Spawned | InProcess |
|---|---|---|
| Handshake Ready → InitOk | ✅ (via smoke e2e) | ✅ unit test client |
| Run + lifecycle events (SessionStarted → … → terminal) | ✅ smoke e2e ada | ✅ smoke e2e diadaptasi |
| Run + renderer-thread tidak deadlock | ✅ smoke e2e ada | ✅ smoke e2e diadaptasi |
| ListTools via `send_and_wait` | — (tercakup core) | ✅ unit test client |
| Panic engine → `Event::Error` | n/a (proses mati, terdeteksi stdout) | ✅ unit test dengan transport panik |
| Shutdown bersih (thread join) | ✅ ada | ✅ unit test client |

- Hermetic seperti e2e yang sudah diperbaiki: fixture config endpoint `http://127.0.0.1:9` via `InitOverrides.config_path` — embedded memakai trik yang sama persis.
- Smoke e2e yang ada diubah menjadi matriks: tiap perilaku dijalankan dua kali (embedded default + spawn eksplisit).
- Panic isolation diuji dengan transport palsu yang `read_request()`-nya panic — pola factory thread engine dibuat generik agar test bisa menyuntik transport apa pun.

## 6. Out of Scope (Fase 2+)

- Capabilities negotiation pada handshake, notification opt-out, pemisahan tipe protokol internal/external (kandidat crate `agentic-protocol`) — Fase 2.
- Daemon long-lived (Unix socket / TCP), `agentic attach`, session multiplexing — Fase 3.
- Setting `runtime.mode` di `config.toml` — menyusul saat dibutuhkan.
- Node/Bun CLI, VS Code extension — Fase 5 PRD; tetap jalur spawn.
- Perubahan protokol, engine, planner, safety — tidak ada.

## 7. Success Criteria

- `agentic run` / `interactive` / `tui` berjalan **tanpa binary `agentic-runtime` terpasang** (mode default embedded) — menghapus kelas bug binary discovery.
- `--runtime spawn` mempertahankan perilaku lama 100% (e2e spawn tetap hijau).
- Test matrix dua transport hijau; `cargo clippy --workspace --all-targets -- -D warnings` dan `cargo fmt` bersih (gate CI yang ada).
- Tidak ada duplikasi handshake/run-loop antar client; protokol tidak berubah (golden test core tetap hijau tanpa modifikasi).
