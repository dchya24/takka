# Roadmap

> Consolidated current-state + next-steps for the takka CLI + core library.
> Last updated: 2026-11-12.

This replaces the older `PLAN.md` / `IMPLEMENTATION_ROADMAP.md` /
`QUICK_START_IMPLEMENTATION.md` triad
work with the CLI/core scope. The Tauri integration is no longer tracked in
this repo.

---

## What this project is

A two-crate workspace:

- **`takka-core`** — Rust library: agent loop, tools, memory, safety, MCP
  client, multi-provider LLM support, streaming.
- **`takka-cli`** — standalone binary: shared widgets stack (`ratatui`),
  inline + full-screen TUI modes, REPL with slash commands, config wizard.

Reference docs:
- Product framing — [AGENTIC_PRD.md](AGENTIC_PRD.md)
- Architecture overview — [architecture-alignment-overview-25052026.md](architecture-alignment-overview-25052026.md)
- Config schema — [CONFIGURATION.md](CONFIGURATION.md)
- TUI/inline widget contract — [shared-widgets-architecture-26052026.md](shared-widgets-architecture-26052026.md)
- Architecture spec the codebase is aligned with —
  [`../AGENT_ARCHITECTURE.md`](../AGENT_ARCHITECTURE.md)

---

## Current state

Foundation is complete; coverage of the architecture spec is at ~95%.

### Library (`takka-core`)

| Layer | Surface |
|-------|---------|
| Loop | `Orchestrator::run` (sync) and `run_stream` (async, with concurrent read-only batches). Max-iterations cap. Cooperative cancel via shared `Arc<AtomicBool>`. Tool lifecycle events (`ToolStart`/`ToolDelta`/`ToolOutput` dengan durasi+sukses+truncated) — live output streaming untuk `run_command`/`run_script`. |
| Tools | read / write / edit / list / glob / grep / search / run_command / run_script / fetch / web_search / update_memory / spawn_subagent / apply_patch / question / todowrite (16 builtins). |
| Memory | Token-budget context window, message pinning, session isolation, disk persistence, keyword search, optional tiktoken backend. |
| Safety | Risk scoring (0.0–1.0), pattern-based detection, hard blocklist, path sandboxing, URL allowlist (+ optional IP-literal block), per-tool rate limiting, audit log, permission modes (`default` / `plan` / `yolo`). Prompt-injection scanner for content brought in by `fetch` / `web_search`. |
| Compression | Three-layer pipeline: tool-result truncation → older tool-result clearing → autocompact (heuristic or LLM-driven) at ~85% of token budget. |
| Providers | OpenAI-compatible, Anthropic-compatible. |
| Vision | Image attachments (PNG/JPEG/GIF/WebP) flow through `takka_core::Attachment`; OpenAI-compatible providers serialize them as multimodal content. Per-model `ModelCapabilities` gate the request before dispatch. |
| MCP | stdio + HTTP + SSE transports. |

### CLI (`takka-cli`)

| Layer | Surface |
|-------|---------|
| Modes | `takka run`, `takka interactive`, `takka tui`, `takka config …`. `--mode default|plan|yolo`. |
| REPL | Custom `crossterm`/ratatui input loop + slash commands (`/help`, `/new`, `/config`, `/history`, `/tools`, `/stats`, `/mcp`, `/sessions`, `/models`, `/plan`, `/search`, `/image`, `/provider`, `/quit`). `@` file completion (auto-detects images for vision channel), `/` command completion, `/models ` model completion. Sessions auto-saved to `~/.config/takka/sessions/`. Resize-safe prompt with active provider/model metadata. |
| Widgets | One ratatui-based stack used by inline + TUI: markdown, spinner, progress, panels, badges, headers, tool-call panel, unified-diff renderer. Capability detection (`NO_COLOR`, `TERM=dumb`, isatty, `--color`). Zero raw `\x1b[` escapes in the source. |
| Safety UX | Risk-coloured confirmation panel. Diff preview in the confirmation prompt for `write_file` / `edit_file` / `apply_patch`. |
| Cancel | Two-stage Ctrl+C → cooperative cancel (process-global `Arc<AtomicBool>`). |

---

## Phase 11 — Prompt Caching (landed)

Prompt caching for Anthropic providers: `cache_control` breakpoints injected
into the system prompt (SystemOnly) and conversation prefix (Prefix). Cache
metrics (`cache_read_input_tokens`, `cache_creation_input_tokens`) flow through
`ChatUsage`. CLI observability shows cache hit ratio in the status bar, stats
panel, and goodbye summary. Session persistence includes cache token counters.

**Deferred (v2):**
- ~~Wire usage events from the orchestrator to `SessionStats`~~ — caching works, just no visibility UI.
- ~~Expose cache settings in `takka config` wizard~~ — configurable via manual config.toml edit.

---

## Fase 1 — Tool lifecycle & live output (landed)

Per-tool progress live: `Event::ToolStart` / `Event::ToolDelta` (output
mentah streaming) / `Event::ToolOutput` diperkaya (`duration_ms`, `success`,
`truncated`).

- `Tool::execute_streaming` (default = `execute`, non-breaking) —
  `run_command`/`run_script` override untuk streaming per-baris.
- Orchestrator: throttled delta emission (`DeltaThrottler`, ~80ms + budget
  8KB per tool) di path sync (direct emit) dan async (channel + forwarder
  thread).
- Renderer inline: header `⟳ tool` → delta DIM → `✓ tool — 0.4s` (+ nota
  truncated). TUI: role `ToolActivity` untuk delta live.
- Session/context contract TIDAK berubah: hasil final tetap di-truncate
  oleh `tool_result_max_chars`; delta hanya observability.

Spec: `docs/superpowers/specs/2026-08-06-interactive-live-progress-and-steering-design.md` (Fase 1).
Next: sisa Fase UX dari design yang sama sudah landed — streaming preview, mid-run input (REPL + TUI) dan TUI completion parity (initiative `initiative-repl-ux-hardening` ditutup 2026-11-09; 987 tests hijau).

## Open work

**Protocol:** Fase 2a (capabilities + strict init + notification opt-out), **Fase 2b: steer/follow-up queue** (`Request::Steer` + `--queue-mode` + `/queue`), dan **Fase 3a: daemon mode** (Unix socket broker + token + `takka attach`; satu sesi aktif) sudah landed. **Fase 4: ACP juga landed (2026-11-11, PR #10–#15)** — keputusan 2026-11-10: ACP menggantikan rencana lama "SDK bahasa lain" (dibatalkan). `takka-runtime --acp` melayani ACP v1 (JSON-RPC over stdio, permukaan integrasi Zed): sesi multi (bounded), prompt streaming, mode, cancel, turn-limit; **stored-session lifecycle (`session/load` + `session/resume` di atas checkpoint store) landed 2026-11-14** — thread lama bisa dibuka lagi setelah restart, error "loading or resuming sessions is not supported" hilang; conformance hermetik kini 18-check (`scripts/acp-conformance.sh`), gate CI ubuntu+macOS (T-08). Sisa ACP: **bukti sesi Zed nyata** (manual). **Fase 3b: multiplexing multi-sesi (bump protokol berikutnya)** menyusul belakangan. Revisi ACP: [agentclientprotocol.com](https://agentclientprotocol.com).

**Packaging (C):** ditunda (keputusan 2026-11-10 — jangan dibuka tanpa persetujuan eksplisit).

**Daemon (open):** daemon mode masih **Unix-only** — transport named pipe untuk Windows belum diimplementasikan (`takka --runtime daemon` gagal dengan pesan aksi di Windows).

**Fase UX (landed 2026-11-09):** inisiatif `initiative-repl-ux-hardening` selesai — (R2) streaming preview live di inline REPL, (R1) input-during-run steer/queue di REPL + TUI, (R3) TUI slash-command + `@`-file completion. Rincian di `CHANGELOG.md` bagian [0.6.0] "REPL/TUI UX hardening"; arsip di `_bmad-output/initiative-repl-ux-hardening/`.

**CI (closed):** daemon smoke test macOS (#2) — dua bug macOS/BSD nyata (accept mewarisi `O_NONBLOCK`; path socket melebihi `SUN_LEN`), diperbaiki dan ditutup via PR #11 (2026-11-11).

**Test flaky (closed):** env race `ENV_LOCK` + poisoning cascade dan deadline dinding tetap pada event-wait helpers (#4) — diperbaiki (ENV_LOCK `into_inner()` recovery, drop-order guard, quiet-window deadlines) dan ditutup via PR #13 + #14 (2026-11-12). Sisa opsional: dua cancel-smoke Windows tetap keluarga timing-flake yang dikenal (sudah jarang muncul); hardening menyusul kalau muncul lagi.

Sized by effort. Each item has its own home in
[`../tasks/tasks-core-agentic.md`](../tasks/tasks-core-agentic.md) or
[`../tasks/tasks-agentic-cli.md`](../tasks/tasks-agentic-cli.md).

Prioritas saat ini (keputusan 2026-11-10, diperbarui 2026-11-12): housekeeping docs, ACP (Fase 4), dan CI debt (#2, #4) tuntas. Tersisa: bukti sesi Zed nyata (manual), Fase 3b multiplexing, packaging (ditunda).

### Quick wins (under a day each)

- [x] **`/new` REPL command.** Mid-session reset: saves current session,
      creates fresh one, clears conversation + memory + cost.
      Aliases `/n`, `n`. Replaces `/clear` and `/restart`.
- [x] **Status-line indicators for `AGENT.md` + persistent memory.**
      Banner + status bar surface `📄 AGENT.md` / `🧠 memory.md`
      chips when those sources are folded into the system prompt.
- [x] **Session management.** Auto-save conversations to
      `~/.config/takka/sessions/` as JSON. `/sessions` to list,
      `/sessions <id>` to resume. No more `history.txt`.
- [x] **`question` and `todowrite` tools.** Interactive question tool
      + session-scoped task list. Documented in
      [milestone-4-interactive-tools-02062026.md](milestone-4-interactive-tools-02062026.md).
      CLI handler wiring pending.

### Medium (2–4 days)

- [x] **Integration tests for the orchestrator agent loop.** 7 tests in
      `takka-core/tests/orchestrator_loop.rs` covering happy-path tool
      execution, max-iterations cap, plan-mode denial, cooperative
      cancel, event emission, memory recording, and the
      `/restart` workflow.
- [x] **Streaming markdown polish.** `MarkdownContent::parse_partial`
      auto-closes unclosed code fences for in-flight delta streams.
- [x] **End-to-end smoke test** for `takka run` against a mock provider,
       so refactors flag regressions before they hit a release.

### Medium (2–4 days)

- [x] **Wire `question` + `todowrite` CLI handlers.** Register
       `QuestionHandler` in interactive/TUI modes, `TodoChangeHandler`
       for progress rendering. See milestone-4 doc.
- [x] **End-to-end smoke test** for `takka run` against a mock provider,
       so refactors flag regressions before they hit a release.

### Larger (1–2 weeks)

- [x] **Planner agent.** Subagent infra (`SpawnSubagentTool`) in place.
      Goal → plan → step execution with replanning on failure, approval
      flow, dependency ordering, Subagent delegation, `PlanProgress` +
      `PlanReplanned` events. 8 E2E integration tests in
      `tests/planner_loop.rs`. `--mode plan` and `--plan` flag route
      through the planner agent. Implemented in a parallel branch and
      merged alongside milestone-3/4 work.
- [x] **Skill system.** Skill format (`SKILL.md`), discovery
      (6 locations: global `~/.agents/skills/`, `~/.config/takka/skills/`,
      legacy `~/.config/agentic/skills/` fallback,
      project `.agents/skills/`, `.agentic/skills/` walk-up, + compat dirs),
      `skill` tool to load domain-specific instructions, and
      `skill create` / `skill list` / `skill info` CLI commands.
      Implemented in Milestone 5.

### Distribution and platform support

- [x] **Checksum-verified user-local installers.** POSIX installer for Linux/macOS
      and PowerShell installer for Windows, with explicit opt-in config setup.
- [ ] **Linux ARM64 and Windows ARM64 release artifacts.** Installers currently
      reject these targets because the release workflow publishes Linux and
      Windows x86_64 only.
- [ ] **Package-manager distribution.** Homebrew, Scoop/Winget, and community
      Linux package repositories.
- [ ] **Code signing.** Authenticode for Windows and signing/notarization for
      macOS release artifacts.
- [ ] **Shell completion packages.** Generate and distribute completions for
      common shells.

### Out of scope (intentional)

- LSP integration, codesearch, prompt caching, file watching — out of scope
  per the architecture spec ("Production Extensions Beyond This Tutorial").

---

## How to choose what's next

Post-v0.6.0 state: REPL/TUI hardening, rebrand, protocol hardening,
steer/queue, daemon mode, embedded runtime, and the ACP adapter are all
landed. The remaining known work, in the order the working agreement
(2026-11-10) implies:

1. **Zed proof (manual).** Run takka as a real ACP agent inside Zed
   (open project → chat → approve a command), record the evidence —
   the strongest check the conformance script cannot give.
2. **Fase 3b: multi-session multiplexing.** Session-id routing + protocol
   envelope bump + per-session capability gating. Deliberately postponed.
3. **Packaging/distribution (C).** Deliberately postponed — do not open
   without an explicit go-ahead.

---
## Recent history

Key changes that landed recently (v0.6.0 shipped 2026-11-10; after that):

```
(latest)     feat(acp): ACP v1 adapter sessions T-03–T-07, D9/D12 + conformance gate (PR #10, #15)
(latest)     fix(runtime): plan-approval lost wakeup + ENV_LOCK poisoning + activity deadlines (PR #12, #13, #14)
(latest)     fix(daemon): macOS/BSD unix-socket bugs, issue #2 closed (PR #11)
3b1f740  feat(safety+cli): interactive diff preview in confirmation prompt
72a7edf  feat(safety): prompt-injection detector for fetch / web_search
3685965  feat(tools): add apply_patch tool for unified-diff application
bcb0ced  feat(orchestrator): token cost tracking + soft USD budget cap
7ebce34  feat(safety): URL allowlist for fetch / web_search
d91d94a  docs(tasks): refresh task files to reflect post-M3 state
18b6e94  feat(repl): /search slash command for conversation memory
4888cca  feat(tools): add web_search with Tavily/Brave/DuckDuckGo backends
e8d5dbb  feat(config): wire summarizer_model + auto_compact_with_llm from Config
dd7ceed  refactor(cli): drop dead config/markdown modules and trim unused helpers
ff9aef1  refactor(core): split memory/orchestrator/safety into submodules
```

Earlier milestone work is documented in
[`milestone-1-foundational-alignment-25052026.md`](milestone-1-foundational-alignment-25052026.md),
[`milestone-2-quality-of-life-25052026.md`](milestone-2-quality-of-life-25052026.md),
[`milestone-3-architectural-additions-25052026.md`](milestone-3-architectural-additions-25052026.md).
