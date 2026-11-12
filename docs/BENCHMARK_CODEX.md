# Benchmark: codex-rs vs agentic

Dokumen ini menetapkan **codex-rs (OpenAI Codex CLI, Rust)** sebagai benchmark arsitektur untuk pengembangan `agentic`, khususnya pada dimensi **pemisahan engine ↔ frontend ↔ protokol**.

Sumber: repositori `openai/codex` (folder `codex-rs`), diverifikasi via docs terindeks (README crate, `docs/protocol_v1.md`, `AGENTS.md`). Setiap klaim codex di dokumen ini berasal dari file tersebut.

---

## 1. Peta Arsitektur codex-rs

codex-rs adalah Cargo workspace dengan crate-crate berikut (semua prefixed `codex-`):

```
codex-rs/
├── core/                    # codex-core      — engine: agent loop, sessions (padanan takka-core)
├── tui/                     # codex-tui       — frontend Ratatui (padanan takka-cli)
├── exec/                    # codex-exec      — headless non-interaktif
├── protocol/                # codex-protocol  — tipe "internal" (core↔tui) + "external" (app-server)
├── app-server/              # codex-app-server        — server JSON-RPC untuk client eksternal (IDE)
├── app-server-protocol/     # codex-app-server-protocol — tipe protokol app-server
├── app-server-client/       # codex-app-server-client   — client + lifecycle (bootstrap, handshake, shutdown)
├── exec-server/             # codex-exec-server — JSON-RPC server untuk kontrol subprocess/pty
├── linux-sandbox/ utils-pty/ # infra sandbox & terminal
└── sdk/typescript/          # SDK resmi bahasa lain (thread.runStreamed(), dll.)
```

### 1.1 Pola inti: Submission Queue / Event Queue (SQ/EQ)

Dari `codex-rs/docs/protocol_v1.md`:

> Core engine berjalan sebagai **background thread atau proses terpisah**. Berkomunikasi dengan UI melalui **Submission Queue** (UI→core) dan **Event Queue** (core→UI). Submission punya `sub_id`; event membawa id yang menaut ke operasi user-turn pemicunya.

Ini konsepnya sama dengan `ProtocolRequest`/`ProtocolEvent` + `request_id` di `takka-core::runtime::protocol` milik kita.

### 1.2 Tiga target koneksi (dual/tri-mode)

Dari `codex-rs/tui/src/lib.rs` — TUI memilih target saat startup:

```rust
match target {
    AppServerTarget::Embedded =>                          // in-process (thread + channel)
        start_embedded_app_server(...).map(AppServerClient::InProcess),
    AppServerTarget::LocalDaemon { endpoint } =>          // daemon di Unix socket
        connect_remote_app_server(endpoint).await,
    AppServerTarget::Remote { endpoint } =>               // remote via WebSocket
        connect_remote_app_server(endpoint).await,
}
```

- `codex exec` (headless) langsung pakai `InProcessAppServerClient::start(...)` — **tidak spawn proses sama sekali**.
- IDE extension (VS Code) spawn `codex app-server` sebagai subprocess JSON-RPC stdio.
- `codex-app-server-client` ada sebagai crate tersendiri yang "memusatkan startup & lifecycle in-process app-server runtime ... bootstrap, handshake, wiring transport, graceful shutdown" — agar CLI client tidak menduplikasi logika itu.

**Insight kunci:** *embedded* dan *spawned* bukan dua produk — keduanya hanyalah **dua transport di belakang satu abstraction client**.

### 1.3 Handshake & capability negotiation

`initialize` wajib **tepat sekali per koneksi**, sebelum method lain:

```jsonc
// request
{ "method": "initialize", "id": 0, "params": {
    "clientInfo":  { "name": "codex_vscode", "title": "...", "version": "0.1.0" },
    "capabilities": {
      "experimentalApi": true,                      // opt-in API eksperimental
      "optOutNotificationMethods": ["thread/started"] // matikan notifikasi tertentu
    }
}}
// response → userAgent, codexHome, platformOs, ...
// lalu client kirim notifikasi ack: { "method": "initialized" }
```

Error semantik: `"Not initialized"` (request sebelum init), `"Already initialized"` (init ulang). Capability **tidak bisa diubah** selama proses hidup; method eksperimental tanpa opt-in ditolak dengan JSON-RPC error.

### 1.4 Model resource `thread` + streaming

- Method: `thread/start`, `turn/*`, `thread/queue/add` (antri maks 100 pesan saat turn berjalan), notifikasi `item/agentMessage/delta`, `thread/queue/changed`, dsb.
- Event loop TUI = `tokio::select!` multiplex **4 sumber**: channel internal widget, event thread aktif, event terminal (keyboard/paste/resize), dan push dari app-server (`ServerNotification`/`ServerRequest`).

### 1.5 Pemisahan tipe protokol

`codex-protocol` README:

> Crate ini mendefinisikan tipe protokol Codex: **"internal types"** untuk komunikasi `codex-core` ↔ `codex-tui`, dan **"external types"** untuk `codex app-server`.

Artinya frontend boleh import tipe engine langsung (seperti `takka-cli` meng-import `takka-core`), selama **kontrak lintas-proses** punya ruang tipe tersendiri.

---

## 2. Perbandingan Side-by-Side

| Dimensi | codex-rs | agentic (kondisi sekarang) | Status |
|---|---|---|---|
| Library engine | `codex-core` | `takka-core` | ✅ setara |
| Frontend TUI (Ratatui) | `codex-tui` | `takka-cli` (TUI full-screen) | ✅ setara |
| Headless runner | `codex-exec` via `InProcessAppServerClient` | embedded (default) + `takka-runtime` spawn | ✅ dual-mode |
| Protokol lintas-proses | SQ/EQ + JSON-RPC 2.0, tipe di crate terpisah | `ProtocolRequest`/`ProtocolEvent` JSONL, module `takka_core::runtime::protocol` | ✅ konsep setara; masih module (ok di monorepo) |
| Client abstraction | `AppServerClient { InProcess, Remote }` | `AgentConnection { InProcess, Spawned }` | ✅ setara |
| Transport | stdio / Unix socket / WebSocket / in-memory channel | `StdioTransport` + `MemoryTransport` (kini dipakai CLI lewat `InProcessClient` sebagai mode embedded default) | ⚠️ infra 80% ada |
| Handshake | `initialize` → `initialized`, capabilities negotiation | `Ready` → `Init` → `InitOk` (+ capabilities) | ✅ setara |
| Versioning protokol | versi + gating capability per-method | `PROTOCOL_VERSION = 1` + capabilities + opt-out | ✅ setara |
| Korelasi request↔event | `sub_id` + event id per turn | `request_id` di envelope | ✅ setara |
| Antrian saat streaming | `thread/queue/add` (steer/follow-up) | `Request::Steer` antrian engine + follow-up client-side | ✅ ala pi (split shell/core) |
| Daemon long-lived | `LocalDaemon` (Unix socket) | `takka-runtime --daemon` (Unix socket, satu attach) | ✅ (3a: unix socket, satu attach) |
| Kontrol subprocess terisolasi | `exec-server` + pty + sandbox | `run_command` di core | 🔜 beda scope |
| Editor protocol | ACP (JSON-RPC/stdio, Zed) | `takka-runtime --acp` | ✅ (Fase 4: landed 2026-11-11; sisa bukti sesi Zed nyata) |

**Ringkasan gap:** yang memisahkan kita dari pola codex bukan fondasi — fondasinya (protokol versioned, handshake, engine generik atas `Transport`) sudah ada. Gap Fase 1 — **abstraction koneksi di sisi CLI** (`AgentConnection { InProcess, Spawned }`) dan **pemakaian `MemoryTransport`** lewat `InProcessClient` sebagai mode embedded default — **sudah tertutup**. Fase 3a landed (daemon eksklusif, unix socket), dan **Fase 4 landed sebagai ACP** (keputusan 2026-11-10: ACP menggantikan rencana "SDK bahasa lain" — adapter `takka-runtime --acp`, T-03–T-08 + conformance CI). Jarak yang tersisa ke pola codex adalah **multiplexing multi-sesi** (Fase 3b: session id routing, bump protokol).

---

## 3. Roadmap Adopsi (prioritas)

### Fase 1 — Dual-mode connection di CLI ✅ (landed 2026-11)

1. Tambah `InProcessClient` di `takka-cli`:
   - Buat 2 pasang channel `std::sync::mpsc`, bangun `RuntimeEngine::new(MemoryTransport::new(req_rx, evt_tx))`.
   - Jalankan `engine.run()` di `std::thread` native (jangan di runtime tokio TUI — `MemoryTransport` blocking).
   - Bridge ke async lewat `tokio::task::spawn_blocking` / adapter channel.
   - Handshake **identik** dengan `RuntimeClient::spawn`: tunggu `Ready` → `Init` → tunggu `InitOk`.
2. Satu abstraction koneksi, ala `AppServerClient`:

   ```rust
   pub enum AgentConnection {
       Spawned(RuntimeClient),     // jalur lama, tetap ada
       InProcess(InProcessClient), // baru
   }
   // send / next_event / send_and_wait / run / shutdown — delegasi
   ```

3. Flag mode: `--runtime embedded|spawn` + env `AGENTIC_RUNTIME_BIN` tetap dihormati. Rekomendasi default akhir: **embedded** (satu binary langsung jalan, hilang masalah binary discovery); selama transisi boleh spawn-first lalu flip setelah test stabil.
4. Panic isolation: tangkap panic di thread engine → terjemahkan jadi event `Error` → TUI tawarkan respawn (UX sama seperti "runtime died" sekarang).
5. **Test matrix**: skenario identik dijalankan terhadap `StdioTransport` dan `MemoryTransport` (regresi serialisasi hanya muncul di jalur spawn).

### Fase 2 — Protocol hardening gaya codex ✅ (landed 2026-11)

1. ✅ Tambah `capabilities` ke `Init` (mis. `experimental_api: bool`) + gating per-request di `RuntimeEngine`; error semantik `"not initialized"` / `"already initialized"`.
2. ✅ Notification opt-out list (hemat bandwidth delta untuk frontend yang tak butuh) — allowlist `NotificationKind` via `ConnectionCapabilities.optOutNotify`.
3. ✅ Pisahkan tipe "internal" (UI↔core, boleh berubah bebas) dari "external" (wire, harus versioned ketat) — di codex ini dua ruang tipe di `codex-protocol`. Di kita: kelompokkan eksplisit di `protocol.rs`, atau ekstrak crate `agentic-protocol` saat frontend non-Rust nyata dibutuhkan.
4. ✅ Antrian pesan saat streaming (steer/follow-up) — landed sebagai **Fase 2b** (lihat sub-catatan di bawah).

#### Fase 2b — Antrian steer/follow-up ✅ (landed 2026-11)

Split ala pi: **steer di engine, follow-up di client**. `Request::Steer { content }`
menaruh teks user di antrian FIFO engine; di batas iterasi (tool boundary) berikutnya
teks disuntik sebagai user message nyata dan diumumkan `Steered` (ack `SteerQueued`).
Antrian aman terhadap gate: steer yang masuk saat gate pending tidak mengganggu alur
gate dan dikonsumsi di batas berikutnya setelah gate selesai — input user tidak hilang.
Gating ketat: `Steer` sebelum `Init` → `not initialized`, tanpa run aktif →
`no active run`.

CLI: flag global `--queue-mode interrupt|follow-up` + slash `/queue`. `interrupt`
mengirim steer live ke engine; `follow-up` (default) mengantri teks client-side dan
memicu run berikutnya sendiri setelah `Done`. Additive — `PROTOCOL_VERSION` tetap 1.

### Fase 3 — Daemon mode ✅ (landed 2026-11, sebagai 3a: satu sesi aktif; 3b multi-session open)

1. `UnixSocketTransport` di `takka-core::runtime::transport` (trait `Transport` sudah siap; `#[cfg(unix)]`).
2. `takka-runtime --daemon` long-lived, satu sesi aktif; CLI attach lewat discovery `daemon.json`.
3. `agentic attach` di CLI (+ `--shutdown`).

#### Fase 3b — Multiplexing (open)

3a sengaja membatasi diri ke **satu sesi aktif** (attach kedua ditolak dengan
`daemon in use`). Sisa gap menuju daemon multi-sesi ala codex:

- **Session id routing** — `Request`/`Event` belum membawa `session_id`; engine
  broker hanya melayani satu sesi. Perlu routing per-sesi di broker + session store shared.
- **Protokol v2** — `PROTOCOL_VERSION` naik ketika envelope berubah (session id,
  multiplexing), plus capability gating per-method.
- **SQ/EQ** — pemisahan submission queue / event queue ala codex bila beberapa
  client menulis ke satu engine secara konkuren.

### Fase 4 — ACP (landed 2026-11-11, menggantikan "SDK bahasa lain")

Keputusan 2026-11-10: permukaan integrasi editor yang benar adalah **ACP v1**
(JSON-RPC over stdio, protokol resmi Zed), bukan SDK bahasa lain — SDK apa pun
tinggal client JSONL di atas protokol yang sama, dan ACP memberi integrasi Zed
gratis. Landed sebagai PR #10–#15: adapter `takka-runtime --acp` (sesi multi
bounded, prompt streaming, mode, cancel, turn-limit), conformance hermetik
14-check (`scripts/acp-conformance.sh`, T-03–T-07) kini gate CI ubuntu+macOS
(T-08, PR #15). Sisa: **bukti sesi Zed nyata** (manual).

---

## 4. Aturan Disiplin (dari pelajaran codex)

1. **Satu kontrak**: semua jalur (embedded/spawn/daemon) lewat `ProtocolRequest`/`ProtocolEvent` yang sama. In-process = skip serialisasi, **bukan** skip protokol. Begitu ada kode yang memanggil `Orchestrator` langsung dari UI, kita punya dua produk berbeda perilaku.
2. **Client abstraction dulu, transport belakangan**: codex menaruh lifecycle (bootstrap, handshake, shutdown) di satu crate client agar tidak terduplikasi. Duplikasi handshake di dua client = bug menunggu waktu.
3. **Tipe internal vs external dipisah**: frontend boleh impor tipe engine, tapi tipe wire itu *contract publik* — perubahanannya dikontrol versi, bukan sekadar refactor.
4. **Capability gating, bukan hard fork**: fitur eksperimental dibuka lewat handshake, tidak boleh pecah protokol jadi dua.

---

## 5. Kesimpulan

codex-rs membuktikan jalur yang kita pilih (spawn-process runtime + protokol versioned + engine generik atas transport) adalah jalur yang benar, dan bahwa **in-process bukan pengganti melainkan tambahan**: `Embedded`, `LocalDaemon`, dan `Remote` hidup damai di balik satu `AppServerClient`. Untuk agentic, jarak ke pola itu pendek: `MemoryTransport` sudah ada di core, tinggal `InProcessClient` + `AgentConnection` di CLI (Fase 1), lalu hardening protokol (Fase 2) sebelum membuka daemon/SDK (Fase 3–4).
