# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed (BREAKING — rebrand)
- **Renamed `agentic` → `takka`.** Binary is now `takka` (was `agentic`),
  runtime daemon `takka-runtime`, crates `takka-cli` / `takka-core` /
  `takka-runtime` (library identifier `takka_core`), and the wire protocol
  identifies itself as `"takka"` with `PROTOCOL_VERSION = 2` (was 1).
  Existing user state is preserved: `~/.config/agentic` keeps being read
  (new `~/.config/takka` wins when present), legacy `AGENTIC_*` env vars
  are still honored with a deprecation warning, and
  `takka config migrate` moves config, sessions, and memory after
  confirmation. A v2 CLI rejects a v1 daemon (and vice versa) with an
  explicit `unsupported protocol version` error — CLI and daemon must be
  upgraded together.

### Added
- CLI: **embedded runtime mode** (default) — the agent engine now runs as
  a thread inside the `takka` process over in-memory channels, removing
  the need for a separate `takka-runtime` binary on PATH. The spawned
  daemon mode is still available via `takka --runtime spawn` and honors
  `TAKKA_RUNTIME_BIN` (legacy `AGENTIC_RUNTIME_BIN` still works with a
  warning). Both modes share the same protocol, handshake, and interactive
  gates; an engine panic in embedded mode surfaces as a normal terminal
  `Error` event (same "runtime died" UX as the spawned mode).
- Runtime: **protocol hardening** (Fase 2a) — `Init` wajib tepat sekali
  (`not initialized` / `already initialized` errors), capability
  negotiation (`experimental_api` opt-in, `Request::Plan` kini API
  eksperimental), dan notification opt-out (`ConnectionCapabilities.
  optOutNotify`, allowlist: thinking/assistant_delta/warning/todo_changed).
  Semua additive di dalam versi protokol saat itu.
- Runtime: **steer & queue** (Fase 2b) — the agent can now be steered
  mid-run: `Request::Steer { content }` queues user text engine-side;
  at the next tool boundary it is injected as a real user message and
  announced with `Steered` (ack via `SteerQueued`). Errors: `not
  initialized` / `no active run`. Pending gates never lose user input.
  CLI adds `--queue-mode interrupt|follow-up` and the `/queue` slash
  command: `interrupt` sends live steers, `follow-up` (default) queues
  text client-side and issues the next run itself.
- Runtime: **daemon mode (Fase 3a)** — `takka-runtime --daemon` hosts a
  single long-lived engine behind a Unix domain socket (`#[cfg(unix)]`),
  discovered via `~/.config/takka/daemon.json` (pid, socket path, random
  token, version). CLIs attach with `takka --runtime daemon`
  (auto-spawns the daemon when absent/stale) or `takka attach`; a second
  attach is refused with `daemon in use`; detach keeps the engine running
  and a re-attach adopts the live session (`InitOk` + `session_adopted`),
  with config/capability mismatches producing actionable errors. Windows
  daemon mode and named-pipe transport are planned (TODO).

### Changed
- Tests: the two CLI e2e smoke tests now spawn the real `takka-runtime`
  daemon against a pinned unreachable provider endpoint, making them
  hermetic — they no longer read the ambient user config (`~/.config/takka/config.json`)
  or call a real LLM provider (which broke CI and risked using the
  developer's credentials). Tool-call / response semantics stay covered by
  the in-process `takka-core/tests/runtime_engine.rs` suite, and the dead
  mock-provider plumbing (`Commands::with_mock_provider`, `ScriptedProvider`)
  was removed.

## [0.5.1] — 2026-09-03

Patch release: inline REPL completion-dropdown fixes and a daemon-binary
lookup fix.

### Fixed
- CLI: the skill / model completion dropdown now disappears immediately when
  an item is accepted, and the render cycle that follows no longer erases
  the command output printed below it (e.g. the skill "📖 Preview" section
  after activation)
- CLI: `runtime_binary()` no longer mistakes the `takka-runtime/` source
  directory at the workspace root for the daemon binary, which made the
  spawn fail with a cryptic "Permission denied"

## [0.5.0] — 2026-09-01

Major release: core-agentic hardening (P0–P2 + Fase D), runtime-CLI decoupling
(Phase 1–4), and the first `agentic-runtime` daemon release.

### Breaking Changes (core-agentic 0.2.1 → 0.3.0)

- **Wire event renames**: `thought` → `thinking`, `tool_start` → `tool_started`
- **Removed process-global handlers**: `QUESTION_HANDLER`, `SKILL_LOADER` statics
  and their setters (`set_question_handler`, `set_skill_loader`, etc.) — handlers
  are now per-instance (`QuestionTool::with_handler`, `SkillTool::with_skill_loader`,
  `ToolDeps::with_question_handler`)
- **Removed `Tool::is_read_only()`** — `metadata()` is the single capability
  source; default is conservative `Mutating + Exclusive`
- **9-state `OrchestratorState` machine** with legal-transition guard:
  `Created, Idle, WaitingForModel, ExecutingTools, WaitingForUser, Compacting,
  Completed, Failed, Cancelled`
- **New `Request` variants**: `ListTools`, `SkillActivate`, `Plan` (full planner
  cycle), `ConfirmResponse`
- **New events**: `tool_list`, `skill_activated_result`, `plan_approval_request`,
  `tool_finished`, `assistant_delta`, `planning`, `warning`, `question_request`,
  `todo_changed`, `plan_approval_request`

### Added (core-agentic 0.3.0)

- **Context Engine** (`src/context/`) — first-class subsystem for request
  assembly: `ContextEngine::build()` with token budget, turn-aware window,
  and provider sanitization
- **Tool Capability Model** (`ToolMetadata`) — mutability, concurrency,
  idempotency, risk floor, side effects; consumed by `ToolRegistry::plan_batches`
  / `execute_batch` (replaces hardcoded read-only/mutating scheduler branches)
- **Session lifecycle events** — `SessionStarted`, `ModelRequest`, `ModelChunk`,
  `SkillActivated`, `PlanCreated`, `StepStarted/StepCompleted`,
  `CompactionStarted`, `WaitingForUser`, `SessionCompleted`, `SessionFailed`,
  `StateChanged`
- **AgentRuntime / AgentLoop split** (`src/runtime/mod.rs`) — lifecycle owner
  with pluggable decision loop (`StandardLoop`); `AgentRuntime::spawn()` for
  subagents; pause/resume via iteration-boundary Condvar
- **Session checkpoint & resume** (`src/session.rs` + `orchestrator/checkpoint.rs`)
  — `AgentSession` (versioned JSON), `SessionStore` (atomic save via tmp+rename,
  list newest-first, delete, id validation), auto-checkpoint per tool boundary,
  `resume_session()` (attach store + restore history + adopt session id)
- **Policy engine** (`safety/engine.rs`) — `Safety::evaluate_tool(PolicyRequest)`
  unified pipeline (per-arg scoring + tool risk floor); floor never grants
  permission; println! leaks in core removed
- **SkillRegistry** — query-aware candidate scoring (exact/partial name, tags,
  word hits); `SkillMetadata` gains `tags: Vec<String>` + `version: Option<String>`
- **SubagentPolicy** — `max_depth`, `max_iterations`, `max_tokens`, `max_children`,
  `timeout` enforced in `SpawnSubagentTool`; depth compounds via patched child
  registry; timeout flips child cancel flag
- **Headless runtime engine** (`src/runtime/engine.rs`) — transport-neutral
  agent daemon with interactive protocol (question/confirmation gates routed
  through the transport)
- **Versioned runtime protocol** (`src/runtime/protocol.rs`) — `ProtocolEvent`
  / `ProtocolRequest` with version + request-id tagging
- **`Orchestrator::provider()`** accessor — runtime daemon builds its planner
  on the same provider

### Added (agentic-runtime 0.1.0 — new crate)

- **`agentic-runtime`** — headless stdio JSONL daemon binary
- Protocol smoke tests (init → init_ok handshake)
- Node/Bun protocol demo (`scripts/protocol-demo.js`)

### Added (agentic-cli 0.4.2 → 0.5.0)

- **`RuntimeClient`** (`src/client.rs`) — spawn/connect to `agentic-runtime`
  daemon, send requests, stream events
- **Daemon-driven slash commands**: `/tools`, `/skill <name>`, `/plan`,
  `/search`, `/restart` — all route through the runtime daemon
- **Planner lifecycle** via daemon: `plan_approval_request` event + dialoguer
  approval gate + live `plan_progress` execution events
- **Golden event-stream test** — verifies the daemon produces the expected
  event sequence (SessionStarted → StateChanged → AssistantDelta →
  SessionCompleted → Done)

### Changed (agentic-cli 0.5.0)

- **CLI is a pure renderer** (Phase 4 complete) — no `Orchestrator::new` in the
  CLI crate; all agent logic runs in the `agentic-runtime` daemon
- `Commands::new()` eagerly reads session metadata (agent.md, memory.md) from
  the filesystem via `init_session_metadata()`
- REPL slash commands route through `RuntimeClient` instead of direct
  orchestrator access

### Removed (agentic-cli 0.5.0)

- `ensure_orchestrator()` and `orchestrator` field from `Commands`
- `Orchestrator` import from non-test CLI code
- Mock provider smoke test (covered by `runtime_engine` golden tests)

## [0.4.2] — 2026-08-26

### Fixed
- CLI: gate renderer during interactive prompts (flicker/race fix)

## [0.4.1] — 2026-08-26

No user-facing changes (internal release).

## [0.4.0] — 2026-08-25

### Added
- Prompt caching support with cache observability in UI
- `/search <query>` slash command with snippet renderer
- Diff preview in confirmation prompt for write/edit/patch
- 60-line cap on diff preview
- `/restart` and `/reset` slash commands
- Status-line indicators for AGENT.md and memory.md
- Skills system: `/skills` command, `agentic skill create` wizard, status-bar indicators
- Planner agent: `/plan` command with progress bar, replan notification
- Plan mode integration with `agentic run --plan`

## [0.3.0] — 2026-06

### Added
- Full-screen TUI mode with ratatui
- Shared widgets architecture (markdown, spinner, progress, diff, capabilities)
- Confirmation UX with risk-coloured panels
- Session control (`/restart`, `/reset`)
- Memory search (`/search`)

## [0.2.0] — 2026-05

### Added
- Initial `core-agentic` library release
- Multi-provider support (OpenAI-compatible, Anthropic)
- Built-in tools (read, write, edit, list, search, glob, grep, run_command, run_script, fetch, web_search)
- Safety system (risk scoring, confirmation, sandboxing, rate limiting)
- Memory management with context window
- Streaming support

[Unreleased]: https://github.com/dchya24/agentic/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/dchya24/agentic/compare/v0.4.2...v0.5.0
[0.4.2]: https://github.com/dchya24/agentic/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/dchya24/agentic/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/dchya24/agentic/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/dchya24/agentic/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/dchya24/agentic/releases/tag/v0.2.0
