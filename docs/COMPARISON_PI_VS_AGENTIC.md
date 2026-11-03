# Perbandingan: Flow `pi agent` vs `agentic-cli`

Dokumen ini membandingkan aliran kerja dua stack agent:

- **`pi` (dianalisis di `AGENT_FLOW_ANALYSIS.md`)** — TypeScript/Node, monorepo dua package:
  - `packages/agent` (`@earendil-works/pi-agent-core`) — *stateful agent core* generik, provider-agnostic.
  - `packages/coding-agent` (`@earendil-works/pi-coding-agent`) — CLI `pi` di atas core (interactive/print/rpc, session, tools, extensions).
- **`agentic` (yang kita bangun)** — Rust, juga monorepo dua crate:
  - `core-agentic` — engine: `Orchestrator` (agent loop), providers, tools, memory/compaction, safety, planner, MCP, skills, subagent.
  - `agentic-cli` — CLI `agentic`: `run`/`interactive`/`tui`, widgets, session JSON, file `@`-ref, confirmation.

Kesimpulan singkat: **arsitekturnya identik secara konsep (engine terpisah dari shell), tapi implementasinya berbeda dalam derajat kompleksitas.** pi unggul di lifecycle-event granular, queueing (steer/follow-up), session tree/branch, dan extension system. agentic unggul di safety/permission engine, planner, subagent, MCP, dan loop-guard (loop detection, graceful finalization). Beberapa hal di pi belum kita punya (fork/branch session, extensions), dan beberapa hal di kita tidak ada di pi (safety engine, planner, subagent).

---

## 1. Peta Arsitektur (side-by-side)

```
pi (TypeScript)                          agentic (Rust)
─────────────────────                    ─────────────────────
packages/coding-agent                    agentic-cli
  main.ts ─parse args─►                   src/main.rs ─clap parse─►
    AgentSessionRuntime                    Commands (src/commands.rs)
    AgentSessionServices                    ├─ run / run_stream
    InteractiveMode/runPrintMode/           ├─ interactive REPL
    runRpcMode                              └─ tui (ratatui)
         │ session.prompt(text)                     │ commands.run(task)
         ▼                                         ▼
  AgentSession (coding-agent)             Commands::run (CLI layer)
    expansion ext/skill/template            ├─ setup provider+tools
    steering/followUp queue                 ├─ assemble system prompt
    convertToLlm (messages.ts)              │   (AGENT.md + skills + memory)
    auto-retry/compaction                   ├─ confirmation handler
         │ agent.prompt(messages)           ├─ session push/save
         ▼                                  ▼
packages/agent (core)                    core-agentic (engine)
  Agent ─► runAgentLoop                    Orchestrator ─► run / run_stream
    ├─ transformContext                     ├─ build_messages (+compaction)
    ├─ convertToLlm (inject)                ├─ provider.chat / chat_stream
    ├─ streamFn (inject)                    ├─ handle_tool_calls(_parallel)
    ├─ beforeToolCall/afterToolCall         ├─ loop detection + iteration cap
    └─ steering/followUp polling            └─ safety gate per tool call
  event lifecycle:                           events (enum):
    agent_start → turn_start →               Thought / ToolCall / ToolOutput
    message_* → tool_execution_* →           / ConfirmationRequest / Error /
    turn_end → agent_end                     Completed / System / PlanProgress
```

**Kunci pembagian tanggung jawab — sama persis di kedua stack:**
- Engine (core) hanya tahu loop + tools + memory; tidak tahu soal UI/CLI.
- Shell (coding-agent / agentic-cli) menyediakan model+auth, tools, session di disk, system prompt, dan UI.
- Bedanya: pi memutus dependency provider lewat **injection (`StreamFn`)**, sedangkan agentic memakai **trait `LLMProvider` (`Arc<dyn LLMProvider>`)** — keduanya provider-agnostic, hanya mekanismenya beda.

---

## 2. Perbandingan Konsep Inti (Tabel)

| Konsep | pi (`packages/agent`) | agentic (`core-agentic`) | Catatan |
|---|---|---|---|
| Unit utama | Kelas `Agent` + `runAgentLoop` | `Orchestrator` + `run`/`run_stream` | Sama: satu objek menyimpan state loop |
| Message internal | `AgentMessage` (user/assistant/toolResult + custom via declaration merging) | `Message` di `memory/` (role + content + attachments) | pi punya ekstensibilitas tipe; kita lebih sederhana |
| Event stream | Lifecycle penuh: `agent_start → turn_start → message_start/update/end → tool_execution_start/update/end → turn_end → agent_end` | Enum datar: `Thought/ToolCall/ToolOutput/ConfirmationRequest/Error/Completed/System/PlanProgress` | **pi jauh lebih granular**; kita tidak punya konsep "turn" maupun "message lifecycle" |
| Streaming | `StreamFn → AssistantMessageEventStream` (objek ter-streaming, update berulang) | `run_stream(on_chunk: FnMut(String))` + akumulasi tool_calls via HashMap index | Mirip, tapi pi punya event stream untuk UI; kita pakai callback |
| Tool preflight | `beforeToolCall` (bisa ubah/block), lalu eksekusi | Safety gate `PermissionDecision` (allow/ask/deny) per tool sebelum eksekusi | pi: hook teknis; kita: **safety engine utuh** (mode plan/yolo, blocked commands, risk) |
| Eksekusi paralel | Mode `"parallel"`/`"sequential"` per batch; `terminate` dihitung semua tool | Batching: run panjang *consecutive read-only* secara concurrent (`spawn_blocking`), state-changing solo | Kita pakai heuristic read-only; pi pakai mode eksplisit |
| Loop guard | Auto-retry + auto-compaction (di AgentSession) | `max_iterations` (default 50) + **loop detection** (signature tool+args berulang) + **graceful finalization** (tool dicopot di iterasi terakhir) | **agentic jauh lebih kuat** di sini |
| Compaction | `shouldCompact` (token estimate), `findCutPoint`, `generateSummary` (LLM) | 3 lapis: truncate (`tool_result_max_chars`) → clear old (`keep_recent_tool_results`) → autocompact (heuristic atau LLM via `summarizer_model`) | Konsep identik (3 lapis) |
| Queueing | `steer()`/`followUp()`/`nextRun()` + `QueueMode` | `Request::Steer` (queue → injeksi FIFO user-message di batas iterasi, gate-safe) + `QueueMode` (`interrupt` steer live; `follow-up` antre lokal lalu jalankan run berikutnya) | Sekarang **split ala pi**: event `SteerQueued`/`Steered` di wire; interrupt cancel flag lama tetap ada |
| Persistence | `SessionManager` JSONL: tree/branch, `leafId`, fork, navigateTree, versioned | `session.rs` JSON: satu file per sesi, flat array `messages`, auto-title, cost/token | **pi jauh lebih canggih** (branch/fork); kita flat tapi cukup |
| System prompt | Di-assemble di coding-agent (system-prompt.ts, templates) | `assemble_system_prompt`: DEFAULT + AGENT.md (project) + skills section + config override + persistent memory | Mirip; kita tambah **persistent memory** dan AGENT.md auto-load |
| Extension | `ExtensionRunner` + hooks (`input`, `context`, `before_agent_start`, `message_end`, `tool_call`...) | **Tidak ada** | **Gap besar**: pi punya plugin system |
| Slash command | Extension `registerCommand` + builtin | Builtin di `interactive.rs` (hardcoded match) | Kita tidak extensible |
| Subagent | Tidak dibahas di doc (harness `AgentLane` masih `unavailable()`) | `SpawnSubagentTool` — benar-benar jalan (share tool registry + cancel flag) | **agentic unggul** |
| Planner | Tidak ada di core | `planner.rs` (1650 baris): dekomposisi LLM → plan ber-dependency → eksekusi + replan | **agentic unggul** |
| MCP | Via extension | Client MCP stdio/HTTP/SSE + `tool_adapter` | agentic punya built-in |
| Safety/permission | Tidak dibahas di doc | Modul `safety/` utuh: `PermissionMode` (default/plan/yolo), `ConfirmationRequest`, blocked commands, URL allowlist, injection guard | **agentic unggul** |
| Web | — | `fetch` + `web_search` (dengan permission domain) | agentic punya |

---

## 3. Perbandingan Alur End-to-End (satu prompt)

### 3.1 pi — Interactive Mode

```
User Enter → onSubmit(text) → session.prompt(text, {streamingBehavior:"steer"})
  → AgentSession.prompt
     ├─ extension event `input` (transform/handled)
     ├─ ekspansi skill/template (/skill:)
     ├─ (sedang streaming) → _queueSteer → agent.steer()   ← memotong turn berjalan
     └─ (idle) → validasi model/auth → cek compaction
          → agent.prompt(messages)
             → runAgentLoop
                ├─ agent_start, turn_start, message_start
                ├─ transformContext → convertToLlm → streamFn
                │    └─ message_update* → message_end (assistant)
                ├─ tool call? → preflight → execute (parallel/seq)
                │    └─ tool_execution_start/update/end + message_end (toolResult)
                ├─ poll steering/followUp queue → turn berikutnya
                └─ turn_end → agent_end
          → _handlePostAgentRun → continue() (auto-retry/compaction/queued msg)
```

### 3.2 agentic — `agentic run "task"` / interactive

```
commands.run(task)
  → Orchestrator::run / run_stream(task)
     ├─ state = Planning
     ├─ memory.add_message(user)
     ├─ loop (iteration ≤ max_iterations)
     │   ├─ [guard] cancel? → Err(Cancelled)
     │   ├─ [guard] approaching 80%? → nudge "start wrapping up"
     │   ├─ maybe_autocompact()
     │   ├─ build_messages() (truncate + clear-old)
     │   ├─ ChatRequest(system_prompt, tools) → provider.chat / chat_stream
     │   │    └─ (stream) on_chunk(text delta) + accumulate tool_calls
     │   ├─ loop detection? (tool+args sama ×3 beruntun) → abort
     │   ├─ tool_calls ada?
     │   │    └─ handle_tool_calls(_parallel):
     │   │         ├─ Safety gate (allow/ask/deny) per tool → ConfirmationRequest
     │   │         ├─ batch: consecutive read-only → concurrent; lain → solo
     │   │         └─ hasil push ke memory → lanjut loop
     │   └─ tidak ada tool_calls → finalisasi, return content
     └─ [finalizing] iterasi terakhir: tool dicopot, model dipaksa jawab teks
  → session push_message + save (cost/token di-update)
```

**Perbedaan struktural utama alur:**

1. **`run` vs `prompt`-loop.** pi memisah "user prompt" dari "agent loop" sebagai dua fase (`AgentSession.prompt` lalu internal `runAgentLoop` dengan banyak turn). agentic menggabung keduanya: `Orchestrator.run` langsung memasukkan user message dan menjalankan loop internal sampai selesai. Konsekuensi: di pi, shell (AgentSession) bisa ikut campur *antar* turn (steer, compaction, retry); di agentic, loop adalah black-box — shell hanya bisa cancel dan menerima event.

2. **Turn vs iterasi.** pi punya konsep eksplisit `turn_start/turn_end` (satu turn = satu LLM call + satu batch tool). agentic menyebutnya `iteration` dengan counter + cap. Fungsinya sama, tetapi pi meng-expose ke event stream.

3. **Urutan persistensi.** pi persist per `message_end` (append-only ke JSONL) — crash di tengah tidak kehilangan pesan. agentic persist sesi *setelah* run selesai (push_message) — intermediate tool result tidak tersimpan sampai akhir (kecuali via log).

4. **Interruption.** pi punya dua mekanisme: steer (motong turn berjalan) dan followUp (antri setelah selesai). agentic hanya punya cancel (cooperative di loop boundary, hard-exit di Ctrl+C kedua). Interactive mode kita: user mengetik prompt baru → menunggu run selesai → baru diproses (tidak ada antrian steer).

---

## 4. Kesamaan yang Harus Dipertahankan

Ada banyak pola yang ternyata kita **sudah sejalan** dengan pi — ini konfirmasi desain:

1. **Engine terpisah dari shell** — kedua stack memisahkan core loop dari CLI. Ini fondasi yang benar.
2. **Provider-agnostic** — pi lewat `StreamFn` injected, kita lewat `LLMProvider` trait + `ChatRequest`. Keduanya memungkinkan OpenAI/Anthropic/Z.ai tanpa mengubah loop.
3. **Compaction 3 lapis** — pi dan kita sama-sama truncate → clear-old → summarize (LLM). Bahkan detail batas compact (jangan re-summarize summary) sama.
4. **Tool result sebagai context** — hasil tool call push ke memory dan dikirim kembali ke LLM sebagai bagian dari history (model's output becomes its own input).
5. **Loop guard** — keduanya punya pembatas iterasi + mekanisme anti runaway (pi: max turn di setting; kita: max_iterations + loop detection signature).
6. **System prompt assembly berlapis** — pi dan kita sama-sama membangun dari beberapa sumber (default + project + skills + override).
7. **Session dir per-user** — `~/.config/...` dengan resume across runs.

---

## 5. Gap pi yang Belum Kita Punya (kandidat roadmap)

Urut berdasarkan prioritas/impact:

| # | Gap | Apa yang dilakukan pi | Effort di agentic |
|---|---|---|---|
| 1 | **Steering/follow-up queue** | `agent.steer()` memotong turn berjalan; `followUp()` antri; `QueueMode` | Sedang — perlu loop kita expose "turn boundary" dan shell perlu channel ke orchestrator. Tanpa ini, interactive mode tidak bisa menyela agent yang ngobrol sendiri |
| 2 | **Session branch/fork** | JSONL + `parentId` + `leafId` + `navigateTree` | Sedang — format JSON kita flat; butuh migrasi format + konsep branch |
| 3 | **Extension system** | `ExtensionRunner`, hooks lifecycle, custom tools/commands/UI | Besar — ini perubahan arsitektural (event hooks sudah ada, tinggal di-generalize) |
| 4 | **Event granularity** | `message_start/update/end`, `tool_execution_*` | Kecil — `Event` enum kita bisa diperluas tanpa breaking. ✅ **Tertutup sebagian (Fase 1)**: `ToolStart`/`ToolDelta`/`ToolOutput` diperkaya sudah live; `turn_start/end` masih belum (Fase 2+) |
| 5 | **Persist per message** | Append-only per `message_end` | Kecil-sedang — kita sudah push per turn di interactive? perlu dicek konsistensi |
| 6 | **RPC mode** | JSON-RPC over stdio | Sedang — berguna untuk integrasi IDE |
| 7 | **Custom message types** (bashExecution, compactionSummary) | declaration merging + `convertToLlm` | Kecil — kita sudah punya `Message` dengan role; tinggal tambah varian |

## 6. Gap agentic yang Tidak Dimiliki pi (keunggulan kita)

| Fitur | Kenapa berharga |
|---|---|
| **Safety engine** (`safety/`) | Permission mode default/plan/yolo, blocked commands, URL allowlist, injection guard — pi tidak punya layer ini di core |
| **Loop detection** (tool+args signature) | Menghentikan model yang mengulang tool call identik — pi hanya andalkan max-iterations |
| **Graceful finalization** | Di iterasi terakhir tool dicopot + nudge "wrap up", jadi user tetap dapat jawaban alih-alih hard abort |
| **Planner** (`planner.rs`) | `agentic run --plan` — dekomposisi tugas jadi plan ber-dependency, eksekusi bertahap + replan saat gagal |
| **Subagent** (`SpawnSubagentTool`) | Isolasi context untuk subtask, share tool+cancel |
| **MCP client built-in** | stdio/HTTP/SSE tanpa extension |
| **`@` file reference + image attach** | UX di `file_ref.rs` — auto-detect gambar, respect `.gitignore` |
| **TUI full-screen** (ratatui) | Progress, diff preview, plan panel, dropdown — pi interactive mode-nya line-based |
| **Structured logging** | `tracing` 2-layer (console + file TRACE) memudahkan debugging loop |

---

## 7. Rekomendasi

1. **Pertahankan** pemisahan engine/shell dan trait `LLMProvider` — jangan pindah ke desain pi yang `StreamFn`-injected kecuali kita butuh extension yang memanggil agent core langsung.
2. **Prioritaskan steering/follow-up** kalau interactive mode ingin terasa "hidup" seperti pi (Enter saat streaming = menyela, Alt+Enter = antri). Ini butuh orchestrator expose *turn boundary* (event channel atau callback hook antar-iterasi) — sekarang loop kita hanya membuka satu jalan keluar: selesai atau cancel.
3. **Perluas event enum** ke arah granularity pi (`turn_start/turn_end`, `tool_execution_*`) — murah, dan jadi fondasi untuk TUI yang lebih informatif serta extension system di masa depan.
4. **Jangan tiru extension system** dulu — kita sudah punya pengganti pragmatis: `question` tool (interaksi), skills, MCP, dan `SkillTool`. Tambahkan extension hanya jika ada kebutuhan nyata (mis. custom slash command dari user).
5. **Upgrade session ke format branch/fork** hanya jika kebutuhan resume/fork muncul — format JSON flat saat ini cukup dan lebih portabel.
6. **Jaga keunggulan safety + planner** — itu diferensiasi agentic dibanding pi.

---

## 8. Peta File untuk Navigasi Cepat

**pi (dari `AGENT_FLOW_ANALYSIS.md`):**
- `packages/agent/src/agent.ts`, `agent-loop.ts`, `types.ts`, `stream-fn.ts`
- `packages/coding-agent/src/core/sdk.ts`, `agent-session.ts`, `session-manager.ts`, `model-runtime.ts`, `messages.ts`
- `packages/coding-agent/src/modes/` (interactive / print / rpc), `core/extensions/runner.ts`

**agentic:**
- `core-agentic/src/orchestrator/mod.rs` (konfigurasi loop: max_iterations, loop detection, compaction)
- `core-agentic/src/orchestrator/run.rs` (`run` + `run_stream`)
- `core-agentic/src/orchestrator/tool_exec.rs` (batching parallel/sequential + safety gate)
- `core-agentic/src/orchestrator/messages.rs` (`build_messages`, truncate, clear-old)
- `core-agentic/src/orchestrator/compaction.rs` (autocompact)
- `core-agentic/src/safety/` (engine, risk, config, injection)
- `core-agentic/src/planner.rs`, `core-agentic/src/tools/spawn_subagent.rs`
- `agentic-cli/src/commands.rs` (shell layer: provider, tools, system prompt, run)
- `agentic-cli/src/interactive.rs` (REPL), `src/tui/` (full-screen), `src/session.rs` (persistence)
- `agentic-cli/src/confirmation.rs` (handler yang dipasang ke orchestrator)
