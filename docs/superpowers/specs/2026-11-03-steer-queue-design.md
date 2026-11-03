# Design: Steer & Queue (Fase 2b)

**Status:** Draft v1 — disetujui brainstorming 2026-11-03
**Scope:** Fase 2b dari roadmap adopsi codex-rs (`docs/BENCHMARK_CODEX.md` §Fase 2 item 4): antrian steer/follow-up — "memotong" atau mengantri pesan saat agent sedang berjalan, parity model queueing pi (`COMPARISON_PI_VS_AGENTIC.md` §Queueing — "gap besar").
**Acuan:** `docs/BENCHMARK_CODEX.md` (aturan disiplin), `docs/superpowers/specs/2026-11-03-protocol-hardening-design.md` (Fase 2a sudah landed: strict init, capabilities, EventSink).

---

## 1. Konteks & Tujuan

Saat ini satu-satunya cara "berbicara" ke agent saat run aktif adalah cancel (hard-cut). pi menyelesaikan ini dengan dua mekanisme dengan tanggung jawab berbeda (penting — pi sendiri tidak menaruh semuanya di core):

- **`agent.steer()`** — dipoll di **core** antar turn: pesan user masuk ke percakapan dan mempengaruhi agent yang sedang berjalan; lifecycle turn yang sama berlanjut.
- **follow-up + continue (`_handlePostAgentRun`)** — diproses di **shell**: pesan diantrikan; setelah run selesai shell mengirim prompt berikutnya sendiri.

Goal Fase 2b: membuka gap ini di agentic dengan pembagian tanggung jawab yang sama, di atas kontrak yang sudah hardened.

## 2. Keputusan Hasil Brainstorming

| Keputusan | Pilihan | Alasan |
|---|---|---|
| Cakupan | **Full `QueueMode`** (pilihan user) — kedua mode tersedia | parity pi |
| Pembagian lapisan | **Split ala pi**: steer = engine/core + wire; follow-up = frontend-pure; QueueMode = konfigurasi frontend | mempertahankan sulit: setiap `Run` tetap frontend-issued (tidak ada run yang digenerate engine), korelasi request-id alur `run_turn` tidak tersentuh; follow-up nyaris nol diubah ke protokol |
| Konten steer masuk sebagai | **User message asli** di iteration boundary (bukan system note) | jujur ke LLM (instruksi baru dari user) dan ke memory/session |
| Steer saat gate interaktif pending | **Diantrikan** — diterapkan di boundary setelah gate menyelesaikan; tidak pernah hilang | prinsip pi; tanpa kode khusus (run thread block di handler, queue menumpuk) |
| Wire events | `SteerQueued` (reply segera atas `Request::Steer`) + `Steered` (notification di injeksi, di-tag run id) | frontend butuh ack segera (fire-and-forget) DAN tanda konsumsi untuk UI; keduanya di luar allowlist opt-out Fase 2a (bandwidth kecil, penting UX) |
| Steer saat idle | `Error "no active run"`; pre-init → `"not initialized"` | engine tidak pernah auto-start run; idle-tangani mode follow-up di client |
| Steer dipakai | Fire-and-forget (`send`, bukan `send_and_wait`) | non-blocking start untuk REPL |
| Default QueueMode CLI | **`follow-up`** (flag `--queue-mode interrupt \| follow-up`, slash `/queue`) | least-surprise: tidak mengubah jalur run yang sedang berjalan tanpa disuruh |
| Injeksi per iterasi | Drain FIFO seluruh antrian di boundary | multi-steer order dipertahankan |
| CLI input-during-run plumbing | **Di luar scope** (Fase UX sendiri) | inline REPL masih spinner-blocked; refactor input loop sensitif (flicker/race sudah pernah berdarah); API + queue + drain tetap lengkap & teruji |
| Versi protokol | Tetap **1** — `Request::Steer` + 2 event, semuanya additive | kebijakan Fase 2a |

## 3. Arsitektur Target

```text
FRONTEND (agentic-cli)                           ENGINE (core-agentic + runtime)
──────────────────────                           ────────────────────────────────
QueueMode = Interrupt | FollowUp                 steer_queue: Arc<Mutex<VecDeque<String>>>
run aktif + user input:                          Request::Steer arm:
  ├─ Interrupt → connection.steer(text)            ├─ pre-init → Error "not initialized"
  │               fire-and-forget                  ├─ idle     → Error "no active run"
  │                                                └─ running  → push FIFO + SteerQueued(reply)
  └─ FollowUp  → pending_follow_ups.push(text)   orchestrator hook (set_steering_source):
                  (client-side, tanpa wire)        di tiap iteration boundary drain FIFO:
                                                    memory.add_message(User, text)
                                                    + emit Event::Steered{content}
                  setelah Done:                     (lewat on_event bridge yang ada —
                  drain queue → Run(berikutnya)      auto ter-tag run request-id)
                   (client issue Run, loop drain)

gate pending → steer menumpuk di belakang gate → boundary setelahnya
```

Komponen baru:

| Komponen | Lokasi | Tanggung jawab |
|---|---|---|
| `Event::SteerQueued { content }` | `core-agentic/src/events.rs` | reply ack — dikirim engine saat menerima Steer |
| `Event::Steered { content }` | `core-agentic/src/events.rs` | notification konsumsi — di-emmit orchestrator saat drain |
| `Request::Steer { content }` | `core-agentic/src/runtime/protocol.rs` | wire request (`"type":"steer"`, snake_case) |
| `steering_source` hook | `core-agentic/src/orchestrator/**` | `set_steering_source(Box<dyn Fn() -> Vec<String> + Send + Sync>)`; poll di guard cluster (cancel/pause) tiap iterasi; default `None` → no-op total |
| `steer_queue` + arm `Steer` | `core-agentic/src/runtime/engine.rs` | antrian engine + wiring source ke orchestrator + reply/gating |
| `AgentConnection::steer(content)` | `agentic-cli/src/client.rs` | fire-and-forget send |
| `QueueMode` + `--queue-mode` + `/queue` | `agentic-cli` (cli.rs, commands.rs, interactive.rs-config minimal) | konfigurasi frontend + visualisasi antrian |
| `pending_follow_ups` + drain-after-Done | `agentic-cli/src/commands.rs` | antrian lokal client-pure; setelah run selesai kirim Run berikutnya (drain loop, guard depth) |

## 4. Data Flow (per mode)

### Interrupt (steer)
1. Frontend mengirim `Request::Steer { content }` saat run aktif → engine push FIFO → dikirim `Event::SteerQueued` (reply, request id steer).
2. Orchestrator sampai boundary (tool batch selesai) → drain antrian → tiap teks: masuk memory sebagai user message → `Event::Steered` di-emmit (terkait run id via on_event).
3. LLM call berikutnya memakai konteks yang sudah memuat pesan steer — agent menyesuaikan jalannya di tengah task; run selesai biasa dengan `Done`.

### Follow-up (frontend-pure)
1. Frontend yang melihat run aktif menyimpan pesan ke `pending_follow_ups` (tanpa wire).
2. Setelah Engine mengirim `Done` → frontend mengeluarkan pesan antrian (FIFO) dan mengirim `Run` baru sendiri — alur ini sama seperti User menjalankan prompt berikutnya di antara dua done.
3. Gate interaktif run baru berjalan normal. Catatan jujur tentang sisa steer yang belum terkonsumsi: antrian milik engine per session (bukan per run) — jika run dituntaskan saat masih ada steer mengantre pada boundary terakhir yang terlewat, sisa itu tetap tersimpan dan akan dikonsumsi pada run berikutnya (session-scope behavior, deterministik dan terdokumentasi; frontend yang ingin perilaku strict per-run cukup memantau `Steered` vs `SteerQueued`).

## 5. Testing Strategy

Semua hermetic (mock provider yang merekam `ChatRequest`, matrix embedded + spawned tetap):

| Skenario | Hasil diharapkan |
|---|---|
| Steer tiba saat run jalan → boundary berikutnya | `Steered` di-tag run id; `ChatRequest` LLM berikutnya memuat steered text sebagai role user (mock provider assert) |
| SteerQueued reply | segera dan correlated request id steer |
| Steer berturut-turut ×3 | dikonsumsi di boundary berikutnya secara FIFO, urutan dipertahankan |
| Steer saat gate pending | diterapkan pada boundary setelah gate selesai — tidak hilang |
| Steer pre-init / idle | `"not initialized"` / `"no active run"` |
| Orchestrator tanpa source | no-op total (hook Option) |
| Follow-up: antre kedua pesan → 2 run | `Done` pertama → client `Run` kedua → `Steered`-style antrian bergeser; pesan tidak hilang |
| `/queue` + flag `--queue-mode` | mode tampilan menjadi terlalu sedikit; diganti runtime |
| Smoke matriks lama (embedded+spawned × 2 perilaku) | tetap hijau |

## 6. Out of Scope

- Input-during-run plumbing untuk inline REPL (spinner-blocked) — Fase UX terpisah; wiring ke UI TUI tidak disentuh di plan ini.
- Engine-generated runs (follow-up engine-driven) — dihindari; run selalu frontend-issued.
- Daemon long-lived (Fase 3), SDK (Fase 4).
- Perubahan allowlist opt-out Fase 2a.

## 7. Success Criteria

- Adjust agent berjalan di tengah run melalui `Request::Steer` di kedua transport; riwayat memory jujur (role user).
- Follow-up queue bekerja sepenuhnya di sisi client tanpa protokol tambahan; tidak ada run yang digenerate engine.
- Gate pending tidak pernah memanggakan input user.
- `PROTOCOL_VERSION` tetap 1; additive; tanpa dependensi baru; seluruh test + fmt + clippy hijau.
