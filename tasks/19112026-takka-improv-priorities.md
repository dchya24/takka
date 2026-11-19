# Prioritas improvisasi takka (siap kerjakan)

**Baseline:** `dev` @ `d69a956`.
**Rule (AGENTS.md):** work lands on `feature/*` branches merged via PR into `dev`.
Each item independently shippable with failing-first test.
**Urutan:** P-01 → P-06. Biaya sesi (P-07) terakhir.

---

## P-01 — O-01: Semantic loop detection (thrash tanpa progres)

**Scope:** `takka-core/src/orchestrator/mod.rs` (`record_tool_calls_for_loop_detection`).

**Work**
1. Detector kedua di samping exact-match: tool sama + args mirip (path sama abaikan offset baris, cwd sama untuk `ls`/`run_command`) N kali dalam window, atau K iterasi read-only beruntun tanpa `write`/`edit` padahal user minta perubahan.
2. Trip → steering nudge ("you keep re-reading X, decide or ask"), bukan hard abort. Hard abort tetap hanya exact-match.
3. Jaga de-dup semantics existing (rule 2 doc comment).

**Done when:** scripted turn selang-seling 2 reads 4x picu nudge event; read-after-edit legit tidak picu.

---

## P-02 — O-04: Satu retry otomatis untuk transient tool error

**Scope:** loop di `takka-core/src/orchestrator/run.rs` (sync + stream), antara tool registry dan memory.

**Work**
1. Retry sekali in-loop (bukan via model): hasil tool klasifikasi transient (timeout marker, allowlist per tool) → eksekusi ulang sekali args sama sebelum catat ke memory.
2. Catat kedua attempt sebagai satu history entry anotasi "auto-retry failed".
3. Persistent error (file not found, schema validation) tanpa retry.

**Done when:** stub tool gagal-transient-lalu-sukses catat satu annotated entry dan loop lanjut; persistent error catat sekali tanpa retry.

---

## P-03 — Biner read-only ke allowlist (`sed -n`, `python3 -c`, `psql -c`)

**Scope:** `takka-core/src/safety/config.rs` (`default_auto_approve_commands`), `takka-core/src/safety/engine.rs` (matcher).

**Evidence:** `d69a956` bebaskan pipe read-only, tapi biner di luar allowlist masih prompt: `sed -n` (ada `sed -i` destruktif satu biner), `python3 -c`, `psql -c` (read vs write beda query, bukan beda flag).

**Work**
1. Tambah `sed -n` (prefix exact `"sed -n"`, bukan `"sed"` — `sed -i` tetap gate).
2. `python3 -c` / `python -c`: auto-approve hanya ketika string kode tanpa pola tulis (`open(.*w`, `os.remove`, `shutil`, `subprocess`, `socket`, `__import__`). Tolak cepat pola lain → prompt seperti sekarang.
3. `psql -c`: auto-approve hanya jika query diawali `SELECT`/`EXPLAIN`/`\d`/`\dn`/`\dt` (case-insensitive, abaikan leading whitespace/`--` comment). `INSERT/UPDATE/DELETE/DROP/...` tetap gate.
4. Failing-first test tiap biner: read lolos, write gate.

**Done when:** 7 command sesi history_limit versi `psql -c SELECT` + `sed -n` + `python3 -c` print tanpa prompt; varian tulis ketiga biner tetap prompt. `cargo test -p takka-core --lib safety` hijau.

---

## P-04 — Default permission mode per-proyek

**Scope:** `takka-core/src/config.rs` (`AgentLoopConfig`), `takka-core/src/runtime/engine.rs:728`, ACP `session/new` reply.

**Evidence:** mode `default/plan/yolo` teriklan (`session_mode_state`), `session/set_mode` routing aktif (`service.rs:196`), tapi `InitOverrides.permission_mode: None` saat boot (`session.rs:254`) → tiap sesi mulai `default`, user pilih manual via UI Zed. Tidak ada jejak Zed kirim `set_mode` (Zed.log bersih).

**Work**
1. Config baru `agent.default_permission_mode: Option<PermissionMode>` (`None` = `default`, perilaku sekarang).
2. Engine pakai saat boot: `overrides.permission_mode.or(config.agent.default_permission_mode).unwrap_or_default()`.
3. `session/new` reply cantumkan mode efektif sebagai current (bukan hardcode `default`), supaya UI Zed sinkron.

**Done when:** config `default_permission_mode: plan` → sesi baru langsung plan mode tanpa klik; tanpa config → perilaku sekarang. Round-trip serde test seperti `max_cost_usd_round_trips`.

---

## P-05 — Resume sertakan ringkasan tool turns

**Scope:** `takka-core/src/orchestrator/checkpoint.rs` (`resume_session`).

**Evidence:** `resume_session` replay hanya `User`/`Assistant` text sebagai `HistoryReplay`; tool turns dilewati ("tool turns carry no standalone text and are skipped"). Sesi tool-heavy (44 tool calls sesi history_limit) resume buta terhadap kerja eksplorasi sebelumnya → agent ulangi baca file.

**Work**
1. Saat resume, kompresi tiap run tool beruntun jadi satu `HistoryReplay` ringkas: `tool_name + target/path + outcome 1 baris` (maks N baris per run, misal 20, sisanya "… +M more").
2. Jangan masukkan ke memory sebagai pesan model — replay event saja (UI context), tanpa ubah urutan pesan checkpoint.
3. Cap panjang agar resume sesi 80-message tidak banjiri konteks.

**Done when:** resume checkpoint 44-tool-call tampilkan ringkasan ≤20 baris per run; pesan memory checkpoint byte-identik sebelum/sesudah.

---

## P-06 — O-02 + O-03 + O-05 (loop shaping lanjutan)

**Scope:** `takka-core/src/orchestrator/run.rs`, `compaction.rs`, slot `plan`/`task_state` checkpoint.

- **O-02 graduated tool-budget shaping:** ≥80% strip read-only high-cost tools dari `tool_defs`; ≥90% hanya `write`/`edit`; 100% tetap strip semua. Announce tiap stage via `Event::System`.
- **O-03 iteration-cadenced compaction:** tiap N iterasi (10, configurable) collapse tool result lama jadi 1 baris, jaga verbatim window K terbaru; no-op setelah full compact.
- **O-05 plan-then-execute opt-in:** iterasi 1 minta step list → slot `plan`/`task_state` existing → prepend compact plan state tiap iterasi. Off default, via config flag.

**Done when:** kriteria done tiap O-item di `tasks/17112026-orchestrator-loop-backlog.md` terpenuhi.

---

## P-07 — TERAKHIR: Pagu biaya level sesi

**Scope:** `takka-core/src/config.rs`, `orchestrator/mod.rs` (`run_cost_usd`), `runtime/engine.rs:746`.

**Evidence:** `max_cost_usd` reset tiap `run()` (`reset_run_cost`) → pagu jaga satu user turn, bukan sesi. Sesi panjang (80 messages, puluhan provider call) tanpa pagu total.

**Work**
1. Config baru `agent.max_cost_usd_per_session: Option<f64>` (`None` = unlimited, perilaku sekarang).
2. Akumulator sesi terpisah dari `run_cost_usd` (jangan sentuh reset per-run): tambah `session_cost_usd`, akumulasi di `accumulate_usage_cost` yang sama.
3. Cross → `Event::CostLimitExceeded` + stop di iterasi boundary berikutnya (pola sama `cost_limit_stop`), state checkpoint `cancelled`-vs-`failed`: pakai `failed` dengan pesan pagu agar beda dari user-cancel.
4. Reset saat `reset_session()` (sesi baru), bukan tiap run. Resume (`resume_session`) pulihkan dari checkpoint agar pagu tidak reset diam-diam — perlu field baru di `AgentSession` (bump `SESSION_FORMAT_VERSION`, migrasi load lama → 0.0).
5. Urutan cek: per-run dulu (sekarang), lalu per-sesi; pesan error bedakan keduanya.

**Done when:** stub run 3 turn × $0.01 dengan pagu sesi $0.02 stop di turn 3 pesan sesi-limit (bukan run-limit); resume lanjutkan akumulator, bukan dari nol; tanpa config → perilaku sekarang. Format lama (v1) load tanpa error.

---

## Mapping ke backlog lama

| Prioritas baru | Asal |
|---|---|
| P-01 | O-01 verbatim |
| P-02 | O-04 verbatim |
| P-03 | Baru (sisa celah `d69a956`) |
| P-04 | Baru (default mode per-proyek) |
| P-05 | Baru (resume tool summary) |
| P-06 | O-02 + O-03 + O-05 |
| P-07 | Baru (biaya sesi, terakhir) |
