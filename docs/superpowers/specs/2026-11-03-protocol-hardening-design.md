# Design: Protocol Hardening (Fase 2a)

**Status:** Draft v1 — disetujui brainstorming 2026-11-03
**Scope:** Fase 2a dari roadmap adopsi codex-rs (`docs/BENCHMARK_CODEX.md` §Fase 2, item 1–3): capabilities negotiation, strict initialization, notification opt-out, pemisahan tipe internal/external.
**Out of scope eksplisit:** steer/follow-up queue (item 4 Fase 2) — dipisah ke Fase 2b dengan spec sendiri.
**Acuan:** `docs/BENCHMARK_CODEX.md` (aturan disiplin 1–4), `docs/superpowers/specs/2026-11-03-embedded-runtime-connection-design.md` (fondasi Fase 1, sudah landed di dev).

---

## 1. Konteks & Tujuan

Protokol runtime (Fase 1 landed): `ProtocolRequest`/`ProtocolEvent` JSONL, `PROTOCOL_VERSION = 1`, handshake `Ready → Init → InitOk`. Tiga kelemahan yang menutup jalur ke frontend non-Rust dan daemon (Fase 3+):

1. **Init tidak disiplin** — `Init` kedua kali diam-diam `InitOk`; `Run` auto-initialize dengan config ambient; `ListTools`/`SearchMemory` pre-init diam-diam return kosong; `ResetSession`/`AddSystemMessage`/`Plan` error. Bug frontend salah urutan tidak ketahuan (silent empty/wrong data) — persis jalur gagal yang disiplin codex hindari.
2. **Tidak ada capability negotiation** — API belum stabil (mis. siklus planner) tak bisa di-tag eksperimental; kontrak wire harus dikunci terlalu dini atau diubah bebas tanpa mekanisme opt-in.
3. **Tidak ada kontrol bandwidth wire** — frontend yang tidak butuh event tertentu (mis. mode exec yang hanya menunggu `Done`) tetap menerima semua delta/notification.

## 2. Keputusan Hasil Brainstorming

| Keputusan | Pilihan | Alasan |
|---|---|---|
| Semantik init | **Strict**: `Init` wajib tepat sekali, sebelum request resource lain; error semantik `"not initialized"` / `"already initialized"` (A) | konsisten vs kondisi campuran sekarang; CLI selalu Init lebih dulu lewat handshake, tak ada jalur CLI yang berubah; bug frontend ketahuan segera (disiplin #4: gating, bukan fork) |
| API eksperimental pertama | **`Request::Plan`** + capability `experimental_api` (A) | planner = permukaan request terbaru/tersulit; ideal di-experimental-kan sebelum dikunci; memberi jalur gating nyata yang teruji tanpa mengubah perilaku CLI |
| Notification opt-out | **Allowlist tertutup** via enum `NotificationKind`, tiga kategori tak-ter-opt-out-kan (A) | frontend tak bisa kebetulan mematikan event yang menghidupkan run; aturan mudah diverifikasi |
| Per-request opt-out | Tidak — opt-out dinegosiasikan **sekali di `Init`**, imutableselama koneksi | pola codex; capabilities tidak berubah di tengah koneksi |
| Pemisahan tipe internal/external | **Grouping eksplisit + doc contract di `protocol.rs`**; ekstrak crate `agentic-protocol` ditunda sampai frontend non-Rust nyata (Fase 5) | YAGNI; rekomendasi tertulis di benchmark doc itu sendiri |
| Kebijakan versi | Field additive dengan `#[serde(default)]` tetap **v1**; bump ke v2 hanya untuk perubahan breaking (rename/remove/semantik) | kompatibilitas frame lama (demo.js, test lama) terjaga |
| Tipe opt-out di wire | **Enum `NotificationKind`** (`#[serde(tag = "type")]`-style snake_case) — nama tak dikenal gagal deserialisasi frame → `protocol_error` | ketat pada jalur spawn (typo ketahuan); type-safe tak mungkin salah di embedded (channel memori) |

## 3. Perubahan Protokol (semua additive, tetap v1)

```rust
/// Capabilities yang dideklarasikan frontend saat Init (sekali per koneksi,
/// imutable setelahnya).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionCapabilities {
    /// Opt-in API eksperimental (saat ini: Request::Plan).
    #[serde(default)]
    pub experimental_api: bool,
    /// Notification yang TIDAK dikirim engine ke frontend ini.
    #[serde(default)]
    pub opt_out_notify: Vec<NotificationKind>,
}

/// Allowlist tertutup notification yang boleh di-opt-out. Tipe enum —
/// satu-satunya mekanisme enforcement: nilai di luar daftar tidak bisa
/// direpresentasikan (embedded) / gagal deserialisasi (spawn).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    Thinking,
    AssistantDelta,
    Warning,
    TodoChanged,
}
```

- `Request::Init` bertambah field `capabilities: ConnectionCapabilities` dengan `#[serde(default)]` — frame lama tanpa field tetap valid.
- Mapping ke `Event` (cek di sink): `Thinking → NotificationKind::Thinking`, `AssistantDelta → AssistantDelta`, `Warning → Warning`, `TodoChanged → TodoChanged`; selain itu `None` (tidak pernah difilter).
- **Tiga kategori yang tak-ter-opt-out-kan** (by construction, bukan oleh runtime check): per-request responses (`Done`/`Error`/`InitOk`/`ToolListResult`/`SkillActivatedResult`/`MemorySearchResult`/`SessionReset`…) dan run-holding gates (`ConfirmationRequest`/`QuestionRequest`/`PlanApprovalRequest`) — karena semuanya di luar empat varian enum.
- **Doc contract di `protocol.rs`**: section komentar yang memisahkan tipe *wire* (kontrak versioned: struct/enum di atas) dari tipe *internal* (helper #[cfg(test)] dsb.), plus komentar kebijakan versi.

## 4. Perubahan Perilaku Engine (`core-agentic/src/runtime/engine.rs`)

### 4.1 Strict initialization

- Field baru `initialized: bool` menggantikan kegantungan "orchestrator ada = sudah init" (presence-as-state); constructor `with_session`/`with_provider` (yang sudah membangun orchestrator) men-set `initialized = true`.
- **Wajib terpasang lalu melanggar** (bagian(s) yang mengirim `Error { "not initialized" }` saat `initialized == false`):
  - `Request::Init` di-`Run` dihapus (auto-init path dihapus)
  - untuk pertama `Run`, `ResetSession`, `SearchMemory`, `ListTools`, `SkillActivate`, `AddSystemMessage`, `Plan`
- **`Init` kedua kali** → `Error { "already initialized" }`.
- **Tetap permisif** (kontrol koneksi, bukan resource): `Cancel` (sudah `as_ref()`-guard), `Shutdown` (wajib tetap bekerja), `ConfirmResponse`/`QuestionResponse` (sudah Warning "no … pending" — tetap seperti itu).
- Error hanya untuk metodikal resource sebelum init; setelah init semua berjalan seperti hari ini.

### 4.2 Capability gate

- Field `capabilities: ConnectionCapabilities` (default) di isi saat `Init` sukses; **tidak bisa diubah** setelahnya (init kedua = error).
- Gate pada `Request::Plan` (urutan cek: `initialized` dulu, lalu capability): tanpa `experimental_api: true` → `Error { "capability 'experimental_api' required" }`.
- Bagian lain request tak berubah.

### 4.3 Opt-out via satu choke point (`EventSink`)

Panggilan `transport.write_event` yang tersebar di 7 tempat (emit() helper, run `on_event` closure, delta closure, todo handler, question handler, 2× confirmation handler (with_provider + install_confirmation_handler), planner forwarder + approval callback) **dipusatkan** ke satu struct:

```rust
struct EventSink<T: Transport> {
    transport: Arc<T>,
    opt_outs: Arc<Mutex<HashSet<NotificationKind>>>, // di-isi saat Init
}
impl<T: Transport> EventSink<T> {
    fn send(&self, request_id: Option<String>, event: Event); // returns bool delivered? void cukup
        // 1. map event → Option<NotificationKind>; None → kirim
        // 2. ada dalam opt_outs → skip
        // 3. selain itu transport.write_event
    fn set_opt_outs(&self, kinds: Vec<NotificationKind>);
}
```

- `RuntimeEngine` menyimpan `sink: EventSink<T>` (atau clone-share Arc bagian-bagiannya) — semua write site lewat sink supaya **satu** tempat keputusan filter.
- Gate & responses tidak pernah difilter (tipe enum menjamin) — bahkan frontend yang opt-out semua 4 kind tetap menerima gates dan responsnya.

### 4.4 Mapping Tipe

Perubahan fungsional terbatas pada dua file: `core-agentic/src/runtime/{protocol,engine}.rs`; test core (`runtime_engine.rs` + `protocol.rs`) di-update untuk semantik baru — itu bagian dari subyek kerja. Modul core lain (orchestrator/tools/safety/memory) **tidak tersentuh**.

## 5. Perubahan CLI

- `AgentConnection::connect(mode, overrides, caps: ConnectionCapabilities)`, dengan `InProcessClient::start(overrides, caps)` dan `RuntimeClient::spawn → perform_handshake(..., caps)` — handshake tetap satu tempat (`perform_handshake`, tidak ada pasangan baru yang bebas duplikasi).
- `Commands::ensure_connection` mengirim konstanta `CLIENT_CAPABILITIES: ConnectionCapabilities = ConnectionCapabilities { experimental_api: true, opt_out_notify: vec![] }` (CLI memakai `/plan`/`--plan`, jadi opt-in experimental itu jujur).
- Renderer, event order, slash command, TUI: **tak ada yang berubah**.
- `scripts/protocol-demo.js`: cek apakah memakai `Plan`; jika ya, tambahkan `capabilities: { experimentalApi: true }` ke Init-nya.

## 6. Data Flow

1. Connect / spawn → Ready → client mengirim `Init { overrides, capabilities }` → engine validasi (initialized flag, isi caps + opt_outs sink) → InitOk.
2. Selama koneksi: setiap write event melewati sink → event allowlist yang diopt-out tidak pernah memasuki wire/channel; sisanya terkirim.
3. `Plan` tanpa opt-in → error capability; dengan opt-in → siklus planner normal.
4. Request salah urutan → error semantik tunggal, bukan data kosong/diam-diam.

## 7. Testing Strategy

Semua test hermetic (endpoint `http://127.0.0.1:9`), matriks embedded/spawned tetap jalan.

| Skenario | Hasil diharapkan |
|---|---|
| Frame `Init` lama tanpa `capabilities` (serde default) | tetap valid; caps default (semua false) |
| Serialisasi `opt_out_notify: ["done"]` | gagal deserialisasi frame → `protocol_error` event (wire) |
| Run/ListTools/SearchMemory/AddSystemMessage/Plan/ResetSession/SkillActivate **sebelum** Init | `Error "not initialized"` |
| Init kedua kali | `Error "already initialized"` |
| Plan setelah init tanpa `experimental_api` | `Error "capability 'experimental_api' required"` |
| Plan dengan `experimental_api: true` | siklus planner normal (mock provider) |
| Init dengan opt-out keempat kind + run/task | event yang diopt-out tidak muncul di stream; `Done`/`Error`/gates tetap diterima |
| CLI embedded + spawned | seluruh suite lama tetap hijau (client kini mengirim konstanta caps) |

- Test engine baru di `core-agentic/tests/runtime_engine.rs` mengikuti pola harness yang ada (mock provider scripted).
- Test CLI: unit `client.rs` (caps di-serialize di Init frame untuk jalur spawn) + regression lama.

## 8. Out of Scope (Fase 2b+)

- Steer/follow-up queue (item 4 Fase 2) — Fase 2b, spec sendiri; opt-out `AssistantDelta` sudah defence-in-depth untuk itu.
- Daemon long-lived, Unix socket, `agentic attach` — Fase 3.
- Ekstrak crate `agentic-protocol` — Fase 5 (saat frontend non-Rust nyata).
- Perubahan planner/orchestrator/safety — tidak ada.

## 9. Success Criteria

- Request salah urutan menghasilkan error semantik tunggal `"not initialized"` / `"already initialized"` / `"capability 'experimental_api' required"` — tidak ada lagi silent-empty/auto-wrong-config.
- Frame protokol lama (tanpa `capabilities`) tetap diterima engine — additive kompatibel, `PROTOCOL_VERSION` tetap 1.
- `Plan` kini gated oleh opt-in eksperimental; CLI tetap berjalan identik.
- Frontend bisa memangkas wire 4 notification via `opt_out_notify`; gates/respons imun.
- Semua test hijau, `cargo clippy -D warnings` dan `cargo fmt` bersih, tanpa dependensi baru.
