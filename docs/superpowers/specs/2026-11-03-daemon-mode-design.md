# Design: Daemon Mode Exclusive (Fase 3a)

**Status:** Draft v1 — disetujui brainstorming 2026-11-03
**Scope:** Fase 3a dari roadmap adopsi codex-rs (`docs/BENCHMARK_CODEX.md` §Fase 3): daemon long-lived di Unix socket, sandar ke pola `LocalDaemon` codex — **satu engine, satu sesi aktif, satu attach**; multiplexing multi-sesi ditunda ke Fase 3b (protokol v2).
**Acuan:** `docs/BENCHMARK_CODEX.md` (aturan disiplin 1–2: satu kontrak di semua jalur; handshake di satu tempat), `docs/superpowers/specs/2026-11-03-protocol-hardening-design.md` (2a: strict init + capabilities) dan `2026-11-03-steer-queue-design.md` (2b) — keduanya landed di dev.

---

## 1. Konteks & Tujuan

Fase 1–2b menghasilkan: CLI adalah pure client di atas protokol versioned (`RuntimeMode::{Embedded, Spawn}` via `AgentConnection`), engine generik atas `Transport` (blocking, single-reader/single-writer), handshake + capabilities di satu tempat (`perform_handshake`), dan strict init (`Init` tepat sekali per proses engine). Yang hilang untuk menyusul pola codex: daemon **proses long-lived** yang bertahan antar CLI/REPLInstance — sehingga setiap `agentic` baru harus mengulang seluruh state agent dari nol.

Fase 3a menyelesaikannya: jalankan engine sebagai daemon di Unix socket; CLI bisa attach, lepas, dan attach-ulang dengan state orchestrator tetap — proses daemon berlanjut antar CLI dan sesi percakapan melintasi re-attach.

## 2. Keputusan Hasil Brainstorming

| Keputusan | Pilihan | Alasan |
|---|---|---|
| Model sesi | **Daemon eksklusif** (3a): satu engine, satu sesi aktif, satu attach; attach kedua ditolak `"daemon in use"` | multi-sesi berarti protokol v2 + routing engine + korelasi SQ/EQ — scope penuh Fase 3b; infrastruktur daemon (transport, lifecycle, discovery, attach UX) bernilai sendiri dan menjadi fondasinya |
| Transport | **Unix domain socket** sekarang; **named pipe Windows = TODO** sampai workstation Windows tersedia | keputusan user; Windows menerima hard error actionable (`daemon mode not supported on Windows yet — use --runtime embedded\|spawn`), tercatat README + known limitation, bukan silent fallback |
| Lifecycle | **Auto-spawn oleh CLI** (discovery via `daemon.json`; stale → CLI spawn daemon sendiri); **tanpa idle auto-shutdown** | sama pola `RuntimeClient::spawn` Fase 1; daemon justru dibuat long-lived; matikan via attach `--shutdown` atau kill |
| Re-attach Init | **Adopt-on-re-attach** via flag `adopt_on_reinit` di engine (default `false` — embedded/spawned UNCHANGED, test 2a tetap hijau): caps+overrides kompatibel → `InitOk` + `Event::SessionAdopted`; caps minta fitur baru → `Error "capability '<x>' required"`; overrides beda → `Error "daemon already running with different config; restart daemon or match its config"` | keeps 2a strictness per-mode; kontinuitas sesi = inti nilai daemon; korelasi request-id tak tersentuh |
| Auth | Token acak (32 hex dari `/dev/urandom`, tanpa dep baru) per start daemon; disimpan di `daemon.json` (~600 perms); client kirim via `Init.daemon_token`; mismatch → `"unauthorized"` (dicek sebelum logic adopt) | Unix socket = lokal by construction; token double-gate terhadap proses lokal lain scanning; zero dependency |
| Broker | **Engine tidak berubah**: daemon adalah broker yang mem-pump frame socket ⇄ `MemoryTransport` milik satu `RuntimeEngine`; detach → event di-drain ke discard (buffer engine tak pernah macet) | disiplin codex: engine tetap transport-agnostic satu kontrak; lifecycle koneksi = urusan daemon |
| Versi protokol | **Tetap 1** — `Init.daemon_token` + `SessionAdopted` semuanya additive | kebijakan 2a |

## 3. Arsitektur Target

```text
agentic-runtime --daemon                    agentic-cli --runtime daemon | attach
┌─────────────────────────────────┐         ┌──────────────────────────────────┐
│  daemon broker (baru, cfg(unix))│         │  AgentConnection::Daemon         │
│  ┌───────────────────────────┐  │  unix   │   discovery: daemon.json         │
│  │ accept → SATU koneksi     │◄─┼─socket──┼─►│  connect + token di Init       │
│  │ pump frames ⇄ engine      │  │  JSONL  │   handshake standar              │
│  │ ┌───────────────────────┐ │  │         └──────────────────────────────────┘
│  │ │ RuntimeEngine         │ │  │
│  │ │ (MemoryTransport)     │ │  │   attach kedua → Error "daemon in use"
│  │ │ TIDAK BERUBAH         │ │  │   detach → engine hidup, event → discard
│  │ └───────────────────────┘ │  │   re-attach → InitOk + SessionAdopted
│  │ daemon.json{pid,path,     │  │   Request::Shutdown → daemon exit bersih
│  │ token,version}, perms 600 │  │
│  │ + UnixListener            │  │
│  └───────────────────────────┘  │
└─────────────────────────────────┘
```

Komponen baru:

| Komponen | Lokasi | Tanggung jawab |
|---|---|---|
| `UnixSocketTransport` (client) | `core-agentic/src/runtime/transport.rs` (`#[cfg(unix)]`) | connect path; JSONL ⇄ UnixStream |
| `SocketConnectionTransport` (server-side) | sama file | satu koneksi accepted → `Transport` letak balik (BufReader/Writer) |
| broker + `UnixListener` + daemon.json + token | `agentic-runtime/src/daemon.rs` | accept satu koneksi, pump frames, detach-drain, refusal koneksi kedua, Shutdown, stale-unlink |
| flag `adopt_on_reinit` + adopt logic + `SessionAdopted` | `core-agentic/src/runtime/engine.rs` + `events.rs` | re-init behavior daemon-only; caps/overrides/token validation |
| `Init.daemon_token` | `core-agentic/src/runtime/protocol.rs` | wire field additive |
| `RuntimeMode::Daemon` + discovery + auto-spawn | `agentic-cli/src/client.rs` + `commands.rs` | connect dengan token; spawn daemon bila kosong/stale; wait-for-ready |
| `agentic attach` + `--shutdown` | `agentic-cli/src/cli.rs` + `commands.rs` | REPL ke daemon yang sudah hidup (tanpa spawn); shutdown remote; `--runtime daemon` global value |
| `--runtime daemon` value baru di `RuntimeMode` | di belakang `AgentConnection` yang sama | default tetap **embedded** — perilaku mode existing tidak berubah |

## 4. Data Flow

1. **Spawn (mode daemon oleh CLI):** `--runtime daemon` → baca `~/.config/agentic/daemon.json` → tidak ada/stale → CLI spawn `agentic-runtime --daemon` → poll daemon.json (timeout konfigurasi kecil) → connect → `Ready` → `Init { overrides, capabilities, daemon_token }` → `InitOk`.
2. **Run & event:** identik embedded/spawned — client-side `run_turn` tidak berubah; stream dari socket.
3. **Attach kedua saat aktif:** daemon melihat koneksi lain → kirim `Error "daemon in use"` → tutup. Koneksi aktif tidak tersentuh.
4. **Detach:** client disconnect (quit/REPL exit) → broker menutup socket, **terus men-drain event engine ke discard**, run yang sedang berjalan selesai normal di engine.
5. **Re-attach:** koneksi baru → Init (token valid) → engine `adopt_on_reinit=true`: caps/overrides kompatibel → `InitOk` + `	SessionAdopted { session_id }` → sesi berlanjut (memory/orchestrator tidak di-rebuild).
6. **Shutdown:** `Request::Shutdown` dari attach → engine break biji → broker: join, unlink socket, hapus daemon.json → exit 0.

## 5. Testing Strategy

Semuanya hermetic (`support::tempdir` untuk socket path + info path), dan kompilasi gate pada Windows (semua modul daemon `#[cfg(unix)]`) — unit test Windows: pesan error actionable dari mode daemon flag.

| Skenario | Diharapkan |
|---|---|
| Full run via daemon (start → Run → Done) | urutan event setara spawned; handshake standar |
| Attach kedua saat aktif | `Error "daemon in use"`; koneksi pertama tak tersentuh |
| Detach mid-run → re-attach | run selesai di engine; `SessionAdopted`; konteks kontinu (capturing provider assert) |
| Adopt dgn caps beda / token salah / overrides beda | 3 error actionable persis |
| Re-Init di embedded & spawned | tetap `"already initialized"` (2a test tak tersentuh) |
| Shutdown via attach | daemon exit 0; socket file + daemon.json di-unlink |
| Stale daemon.json | auto-spawn meng-unlink dan spawn baru |
| Attach saat tidak ada daemon | error actionable "no daemon running" |
| Platform tanpa Unix socket (Windows/CI) | modul `#[cfg(unix)]` di-skip; unit test Windows memverifikasi pesan error actionable |

E2E matriks CLI (embedded/spawned/daemon × lifecycle × deadlock) diperluas; smoke daemon memakai socket di tempdir.

## 6. Security

- Token acak 32 hex per daemon start (`/dev/urandom`, read 16 byte); tidak pernah lewat wire kecuali Init (koneksi socket sendiri).
- `daemon.json` perms 600 di config dir (konten internal); Unix socket dir perms mengikuti default umask.
- Tanpa TLS/auth lebih advance — daemon lokal-only; model serupa `LocalDaemon` codex.
- Liveness daemon.json: PID hidup + probe connect; daemon baru TIDAK meng-adopt daemon lama (daemon = proses; lama harus mati → stale cleanup unlink).

## 7. Out of Scope (3b+)

- Multiplexing multi-sesi, protokol v2, session routing di engine, session store shared — Fase 3b.
- Windows named pipe transport — TODO (menunggu workstation Windows).
- Idle auto-shutdown / supervision / auto-restart.
- TLS / remote daemon.
- SDK (Fase 4).

## 8. Success Criteria

- `agentic --runtime daemon run "task"` bekerja end-to-end; `agentic attach` berjalan REPL pada daemon yang hidup; attach kedua ditolak dengan pesan benar; ...lifecycle detach→re-attach mempertahankan konteks dan meng-emmit `SessionAdopted`.
- 2a strictness embedded/spawned **tidak berubah** (semua test lama hijau tanpa dimodifikasi).
- `PROTOCOL_VERSION` tetap 1; additive; **tanpa dependensi baru**; Unix-only module `cfg(unix)`; Windows punya error actionable + test.
- Suite + fmt + clippy -D warnings hijau.
