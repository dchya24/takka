# Tasks: takka-core — Core Agentic Library

**Feature**: `takka-core` — Rust library for AI agent orchestration (renamed from
`core-agentic` in the 2026-11 `takka` rebrand)
**Status**: Foundation complete; ~95% architecture coverage; Phases 9–11 landed.
ACP v1 adapter merged as PRs #16–#19; harness hardening H-01..H-03 merged as
PRs #20–#22.
**Open work**: [`14112026-harness-hardening-backlog.md`](14112026-harness-hardening-backlog.md)
(H-04–H-07) — see also `docs/ROADMAP.md` "Open work".
**Created**: 2026-04-20
**Updated**: 2026-11-14

---

## Relevant Files

- `takka-core/Cargo.toml` — Library manifest
- `takka-core/src/lib.rs` — Public re-exports
- `takka-core/src/orchestrator/` — Agent loop (mod, messages, tool_exec, compaction, run)
- `takka-core/src/agent.rs` — Higher-level Agent wrapper
- `takka-core/src/tool.rs` — Tool trait
- `takka-core/src/tool_registry.rs` — Tool registration
- `takka-core/src/memory/` — Context/memory (mod, types, store, compaction, persist)
- `takka-core/src/safety/` — Risk + permissions (mod, risk, config, audit, engine)
- `takka-core/src/events.rs` — Event emitter + types
- `takka-core/src/providers/` — `openai`, `anthropic`
- `takka-core/src/tools/` — Built-in tools (read, write, edit, list, search, glob, grep, run_command, run_script, fetch, web_search, update_memory, spawn_subagent)
- `takka-core/src/prompts.rs` — Default system prompt + AGENT.md loading
- `takka-core/src/file_tracker.rs` — Read/edit staleness detection
- `takka-core/src/memory_file.rs` — Persistent user/project memory.md
- `takka-core/src/diff_util.rs` — Unified-diff producer (consumed by widgets::diff)
- `takka-core/src/mcp/` — MCP client (transport, types, client)

---

## Instructions for Completing Tasks

**IMPORTANT:** As you complete each task, check it off by changing `- [ ]` to `- [x]`. Update after completing each sub-task.

## Tasks

### Phase 1 — Foundation (✅ done)

- [x] 0.0 Project setup
  - [x] 0.1 Create takka-core crate
  - [x] 0.2 Cargo manifest + workspace integration
  - [x] 0.3 Public exports in `lib.rs`
- [x] 1.0 Core types
  - [x] 1.1 `Message`, `MessageRole`
  - [x] 1.2 `ToolCall`, `ToolResult`, `ToolSchema`
  - [x] 1.3 `Config` (multi-provider, MCP, safety, output, agent loop)
- [x] 2.0 Tool system
  - [x] 2.1 `Tool` trait + `ToolRegistry`
  - [x] 2.2 Built-ins: read, write, edit, list, search, glob, grep
  - [x] 2.3 Built-ins: run_command, run_script
  - [x] 2.4 Built-ins: fetch, web_search (URL-aware tools)
  - [x] 2.5 Built-in: apply_patch (atomic unified-diff applier)
  - [x] 2.4 Edit-tool string replacement with uniqueness + quote normalization
  - [x] 2.5 `FileTracker` — staleness detection between read/edit
- [x] 3.0 LLM providers
  - [x] 3.1 OpenAI-compatible
  - [x] 3.2 Anthropic
  - [x] 3.3 Streaming (text deltas + buffered tool calls)
  - [x] 3.4 Failover wrapper

### Phase 2 — Orchestrator (✅ done)

- [x] 4.0 Agent loop
  - [x] 4.1 Synchronous `run()` and async `run_stream()`
  - [x] 4.2 State machine
  - [x] 4.3 Max-iterations cap
  - [x] 4.4 Cooperative cancel (`Arc<AtomicBool>`)
- [x] 5.0 System prompt
  - [x] 5.1 Default prompt with the three rules
  - [x] 5.2 `AGENT.md` walk-up auto-load
  - [x] 5.3 Persistent memory section in prompt

### Phase 3 — Memory (✅ done)

- [x] 6.0 Memory management
  - [x] 6.1 `Memory` struct + message tracking
  - [x] 6.2 Token-budget context window (replaces message-count slicing)
  - [x] 6.3 Pinned messages
  - [x] 6.4 Session isolation
  - [x] 6.5 Disk persistence (`persist`, `load`, `list_sessions`, `delete_session`)
  - [x] 6.6 Keyword search (`Memory::search`)
  - [x] 6.7 Optional tiktoken backend behind a feature flag
  - [x] 6.8 Configurable `context_budget_ratio`

### Phase 4 — Safety (✅ done)

- [x] 7.0 Safety system
  - [x] 7.1 Risk scoring (0.0–1.0)
  - [x] 7.2 Configurable confirmation thresholds
  - [x] 7.3 Hard blocklist (`rm -rf /`, `mkfs`, `dd if=`, fork bombs, …)
  - [x] 7.4 Pattern-based risk detection (25+ regex patterns)
  - [x] 7.5 Path sandboxing
  - [x] 7.6 Per-tool rate limiting
  - [x] 7.7 Audit log (ring buffer)
  - [x] 7.8 Permission modes: `default` / `plan` / `yolo`
  - [x] 7.9 URL allowlist for `fetch` / `web_search` (`safety.allowed_domains`, `safety.block_ip_urls`)
  - [x] 7.10 Prompt-injection scanner for content from `fetch` / `web_search` (annotates results, does not block)

### Phase 5 — Compression (✅ done)

- [x] 8.0 Three-layer compression pipeline
  - [x] 8.1 Layer 1 — UTF-8 safe truncate of large tool results
  - [x] 8.2 Layer 2 — replace older tool results with `[Cleared]` placeholder
  - [x] 8.3 Layer 3 — heuristic autocompact at ~85% budget
  - [x] 8.4 Layer 3 — LLM-based summarization (opt-in, falls back on error)
  - [x] 8.5 Wired through Config (`agent.auto_compact_with_llm`, `agent.summarizer_model`)

### Phase 6 — Advanced (✅ done)

- [x] 9.0 Subagents
  - [x] 9.1 `SpawnSubagentTool` with fresh context
  - [x] 9.2 Shared `ToolRegistry` + `Provider` via `Arc`
  - [x] 9.3 Linked cancel flag with parent
- [x] 10.0 Web access
  - [x] 10.1 `fetch` tool — HTML→text + per-session cache
  - [x] 10.2 `web_search` tool — Tavily / Brave / DuckDuckGo backends
- [x] 11.0 Persistent memory
  - [x] 11.1 User-global `~/.config/takka/memory.md`
  - [x] 11.2 Project-local `<cwd>/memory.md` walk-up
  - [x] 11.3 `update_memory` tool

### Phase 7 — Events / Concurrency / MCP (✅ done)

- [x] 12.0 Events system
  - [x] 12.1 Event types (`ToolCall`, `ToolOutput`, …)
  - [x] 12.2 `EventEmitter` (Mutex<Vec<…>>) for `&self.on()`
  - [x] 12.3 Public `on_event` / `clear_event_handlers`
- [x] 13.0 Concurrency
  - [x] 13.1 Concurrent read-only tool batches in `run_stream`
  - [x] 13.2 Concurrent read-only tool batches in sync `run` (via
        `std::thread::scope`)
- [x] 14.0 MCP integration
  - [x] 14.1 stdio transport
  - [x] 14.2 HTTP transport
  - [x] 14.3 SSE transport
  - [x] 14.4 Tool schema conversion

### Phase 8 — Testing & docs

- [x] 15.0 Unit tests (290+ across the crate; see milestone docs)
- [x] 15.1 Integration tests for the orchestrator agent loop
      (`tests/orchestrator_loop.rs`: 7 end-to-end tests covering happy
      path, max-iterations, plan-mode denial, cancel, event emission,
      memory recording, restart workflow)
- [x] 16.0 Architecture docs
  - [x] 16.1 `AGENT_ARCHITECTURE.md` reference
  - [x] 16.2 Three milestone docs (foundational / quality-of-life / additions)
  - [x] 16.3 Architecture alignment overview

### Phase 9 — Planner Agent

- [x] 17.0 Planner core (implemented in `takka-core/src/planner.rs`)
  - [x] 17.1 `PlannerAgent` struct: `create_plan()` (LLM-driven), `execute_plan()`, `replan()`
  - [x] 17.2 Plan representation: `Plan`, `Step`, `PlanStatus`, `StepStatus`, `PlanResult` with deps
  - [x] 17.3 ~~`plan` tool~~ (not needed — planner is invoked by user via `/plan`, not by agent tool call)
  - [x] 17.4 Step execution loop with status tracking (`Pending`→`InProgress`→`Completed`/`Failed`)
  - [x] 17.5 Replanning on failure with `max_replan_attempts` config
  - [x] 17.6 Event emission: emits `ToolCall`, `ToolOutput`, `System`, `ConfirmationRequest`, `Completed` events
  - [x] 17.9 Unit tests: **47 tests** covering plan creation, serialization, approval, execution, replanning, subagent delegation

- [x] 17.10 Integration tests: 7 E2E tests in `tests/planner_loop.rs` (manual execution, dependencies, failure, approval flow, LLM-driven, event emission, result summary)

### Phase 9b — Planner Agent (completed items from remaining list)

- [x] 17.7 Subagent delegation: steps with `tool: "spawn_subagent"` delegate to `SpawnSubagentTool` instead of direct execution
- [x] 17.8 Config wiring: `planner.max_steps`, `planner.max_replan_attempts`, `planner.require_approval`, `planner.model`, `planner.provider` in `takka_core::Config::agent.planner`
- [x] 17.11 `PlanProgress` event type: specific event with plan_id, step_id, step_status, steps_total/completed/failed/pending
- [x] 17.12 Wire planner events to CLI/TUI renderer: `planner.on()` handler in `plan_inline()` with labeled_bar + step description

### Phase 10 — Skill System (✅ done)

- [x] 18.0 Skill format & discovery
  - [x] 18.1 Define `SKILL.md` schema (frontmatter: name, description; body: instructions) per Agent Skills standard
  - [x] 18.2 Discovery paths:
    - [x] 18.2a Global: `~/.agents/skills/` (cross-agent: pi, opencode, codex)
    - [x] 18.2b Global: `~/.config/takka/skills/` (takka-specific)
    - [x] 18.2c Project: `.agents/skills/` walk-up from cwd (cross-agent)
    - [x] 18.2d Project: `.agentic/skills/` walk-up from cwd (takka-specific)
    - [x] 18.2e Extra: `skills.compat_dirs` from config (e.g. `~/.claude/skills/`)
    - [x] 18.2f Scan order + name collision: project overrides global, first-found wins
  - [x] 18.3 `SkillIndex` — in-memory index of discovered skills (name, description, path, files)
  - [x] 18.4 Config: `SkillsConfig` with `blocklist: Vec<String>` and `compat_dirs: Vec<String>`
      in `takka_core::Config`
  - [x] 18.5 System prompt integration: inject `📦 Skills: <name> (<description>)` for indexed skills
  - [x] 18.6 Blocklist filter: excluded skills not added to index
  - [ ] 18.7 File watching / refresh for skill directories (optional, v2)
- [x] 19.0 `skill` tool
  - [x] 19.1 `SkillLoader` trait — reads SKILL.md + referenced resources (relative paths from skill dir)
  - [x] 19.2 Tool schema: `{ "name": "skill", "arguments": { "name": "...", "activate": true } }`
  - [x] 19.3 Execution: look up skill in index → read SKILL.md + files → return as tool output
  - [x] 19.4 Option (`activate: bool`): keep skill instructions active for session duration
  - [x] 19.5 Graceful degradation: unknown skill → return error, model recovers
  - [x] 19.6 Unit tests: 22 tests (17 discovery + 5 tool) covering parsing, discovery, blocklist, collisions, tool execution
  - [x] 19.7 Integration tests: 10 E2E tests in `tests/skills_loop.rs` (discovery, tool execution, referenced files, blocklist, prompt section)

### Phase 11 — Prompt Caching

- [x] 20.0 Provider-level cache support
  - [x] 20.1 Anthropic: inject `cache_control` breakpoints in request payload (system prompt, conversation prefix)
  - [x] 20.2 OpenAI: no-op (automatic server-side caching, nothing to implement)
  - [x] 20.3 Config: `provider.cache.enabled`, `provider.cache.breakpoint_strategy` (system_only | prefix | full)
- [x] 21.0 Cache invalidation strategy
  - [x] 21.1 System prompt change → new cache epoch (handled implicitly by Anthropic API — content-based cache keys)
  - [x] 21.2 Tool list change → new cache epoch (same as above)
  - [x] 21.3 Per-turn: only cache prefix up to last pinned/memory message (Prefix strategy implementation)
- [x] 22.0 Observability
  - [x] 22.1 Track cache hits/misses in `ChatUsage`/`AnthropicUsage`: `cache_read_input_tokens`, `cache_creation_input_tokens` passed through from API response
  - [ ] ~~22.2 Expose cache metrics via CLI status bar + `/stats` command~~ (deferred — caching works, just no visibility UI)
- [x] 23.0 Tests
  - [x] 23.1 Unit tests for `cache_control` injection in Anthropic provider (5 tests: absent-by-default, system-only, prefix, single-turn, wire format)
  - [ ] ~~23.2 Integration test: verify cached vs non-cached request shapes~~ (deferred)
  - [ ] ~~23.3 Cost tracking tests: correct token accounting with cache discounts~~ (deferred)

### Out of scope (intentional)

- LSP integration, file watching — out of scope per architecture doc

> **Architecture reference:** [AGENT_ARCHITECTURE.md](../AGENT_ARCHITECTURE.md)
> **Coverage detail:** [docs/architecture-alignment-overview-25052026.md](../docs/architecture-alignment-overview-25052026.md)
> **Roadmap:** [docs/ROADMAP.md](../docs/ROADMAP.md)

### Fase 1 — Tool lifecycle & live output (landed)

- [x] 24.0 Event enum: `ToolStart`, `ToolDelta`, `ToolOutput` diperkaya (`tool_call_id`, `duration_ms`, `success`, `truncated`)
- [x] 24.1 `Tool::execute_streaming` (default = `execute`, non-breaking) + `ToolRegistry::execute_streaming_by_name`
- [x] 24.2 `run_command` streaming (baca per-baris stdout/stderr, kontrak JSON sama)
- [x] 24.3 `run_script` streaming (pertahankan truncation 64KB)
- [x] 24.4 Orchestrator: `ToolStart`/`ToolDelta`/enriched `ToolOutput` di path sync + async (channel + forwarder thread)
- [x] 24.5 `DeltaThrottler` (~80ms + budget 8KB/tool)
- [x] 24.6 Test: lifecycle events sync + stream path (dengan `StreamingScriptedProvider` di test support)

> Spec: `docs/superpowers/specs/2026-08-06-interactive-live-progress-and-steering-design.md`
> Next: hardening items H-04–H-07 in
> [`14112026-harness-hardening-backlog.md`](14112026-harness-hardening-backlog.md)
> — the live-output/steering work described by this spec is landed.
