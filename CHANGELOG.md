# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added (REPL/TUI UX hardening)
- **REPL: live streamed-answer preview** — while a run streams, the
  accumulated answer text is now visible progressively in the inline REPL
  (plain text; the completed answer still renders as the final styled
  markdown pass). Multi-block answers interleave in arrival order, the
  transient status line keeps its spot above the preview via the
  single-writer render gate, and a mid-stream Ctrl+C leaves the scrollback
  clean (the two-stage cancel semantics don't change).
- **Mid-run input takes effect** — text typed while a run is active is no
  longer silently dropped in the inline REPL or the TUI. Per the
  `--queue-mode` selection it steers the live run (`interrupt` — engine
  `Request::Steer`, announced as `Steered`) or queues it as the next run
  (`follow-up`, default), takes effect at the next tool/run boundary and is
  visibly announced. A queued `follow-up` starts as the next run after
  the current one ends. Prompt-gate steers wait for visible confirmation
  prompts to settle instead of stealing lines from the gate. The wire
  contract is unchanged — no protocol bump.
- **TUI completion parity** — the full-screen TUI now has the inline
  REPL's completion conveniences: `/` opens the slash-command list, `@`
  opens file completion, choices are navigable and accepting a choice
  fills the input. Works idle and mid-run; completion rendering applies an
  idle-only rule for plain arrow keys and Home/End so the mid-run typed-
  text echo stays stable.

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
- Runtime/core: providers' **real token usage** now reaches frontends as an
  `Event::Usage` notification, emitted after every model call from both the
  streaming (final chunk) and non-streaming paths. Additive to protocol v2;
  frontends that never receive it keep their estimate.
- CLI: the REPL's end-of-run summary uses those measured numbers
  (`✓ done · 2.6s · 2 tools · ↑3.7k ↓963`) and only marks them with `~` when the
  provider reported nothing and the ~4-chars/token estimate is in play.
  Prompt-cache reads/writes are folded into the session counters too.
- CLI/TUI: the TUI status bar's token counters were never incremented (always
  `0 ↑ / 0 ↓`); they now accumulate the provider-reported usage.
- CLI: the inline status line shows the **running tool's own clock**
  (`⠴ run_command · 1.2s`) instead of the whole run's, so a stuck tool is
  obvious; re-marking the same tool does not restart its clock.
- CLI: `/timeline` sets the tool timeline detail — `detailed` (default: rows
  plus nested output, errors, diffs) or `compact` (one row per tool). Bare
  `/timeline` prints the current mode.
- CLI: the model's pre-tool reasoning is now rendered inline as a DIM
  `thinking...` block, wired to the long-documented `output.show_thoughts`
  config key (previously read by nothing). Blocks are capped at 12 lines with
  an `… N line(s) hidden` notice. `--no-thoughts` disables it for a session
  and `/thoughts` (alias `/thinking`) toggles it live in the REPL.
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
- CLI: tool activity renders as a **timeline** instead of three loose lines per
  call — one branch row per finished tool with its most telling argument,
  result note and duration (`├ ✓ read_file  src/main.rs   12 lines · 120ms`),
  with streamed output, errors and diffs nested under a `│` guide. Tools that
  stream output (`run_command`, `run_script`) get a live `⟳ … running…` row
  first and close with a nested outcome line. Durations the runtime did not
  measure are omitted instead of printing `0ms`.
- CLI: the inline REPL's transient `Thinking…` line is now a live status
  line — phase (`thinking` / `streaming` / `<tool name>` / `waiting for
  approval`), elapsed time, a live output-token estimate, and a
  `ctrl+c to cancel` hint on wide terminals. The old indeterminate
  bouncing bar carried no information and is gone from inline mode.
- CLI: the end-of-run line now summarizes the run in one place
  (`✓ done · 9.1s · 3 tools · ↑1.2k ↓342`) instead of `✓ Done (9.1s)`
  followed by a separate session badge; the session line below it is
  labeled `📊 session` and no longer repeats the "done" outcome.
- CLI: interactive sessions (`takka`, `interactive`, `attach`, `tui`) keep
  console logging at `warn` even when `RUST_LOG=info` is exported, so log
  lines can't interleave with the REPL UI. `-v`/`--debug` opt back in; the
  file log always keeps full detail.
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
- CLI: the tool confirmation prompt no longer turns a typed `y`/`a` into
  "Invalid input". In raw mode `read_line` cannot work (Enter arrives as `\r`)
  *and* the REPL's key loop was consuming the keystrokes first. The prompt now
  reads a single key via crossterm, echoes it, and publishes an
  `INTERACTIVE_PROMPT_ACTIVE` flag that the REPL honours (its read loop polls
  instead of blocking, so keys stay in the terminal for the prompt). Enter takes
  the safe default (`No`), Esc aborts. Cooked mode (`takka run`, piped input)
  still accepts typed `y`/`n`/`a`/`q` lines.
- Safety: auto-approval now understands quoting, so a `git commit` whose message
  contains `;`, `&&`, `>` or a flag-like word is no longer mistaken for shell
  chaining (`git commit -m "feat: a; b && c > d"` runs without a prompt). Real
  syntax still disqualifies: pipelines, `>`, `>>`, `` ` ``, `$(` and `<(` are
  rejected anywhere, and risk patterns still scan quoted text (`bash -c "rm -rf
  /"` executes its quotes). Input redirection (`<`, `<<`) is allowed, which
  makes heredoc commit messages (`git commit -F - <<EOF`) auto-approved.
- CLI: the confirmation panel shows the **whole** command (wrapped) instead of
  truncating it at 120 characters.
- Safety: the confirmation panel reported `Risk Level: LOW` for everything
  because the request was scored without a target. Requests are now scored
  against the actual command/path, and `git push` (+force-push), `git rebase`
  and `git checkout -- .` have patterns (MEDIUM/HIGH) so the panel says what it
  means.
- Safety: `run_command` no longer asks for permission on every single call.
  `run_command` carries a risk floor of 40/100, so the confirmation gate fired
  even for `git log`; a new `safety.auto_approve_commands` list (read-only
  inspection + local, reflog-undoable git edits such as `git add`/`git commit`)
  now bypasses the floor. Matching is strict — the whole command must be simple
  (no `|`, `` ` ``, `$(`, `>`, `<`) or a chain (`&&`, `;`) whose **every**
  segment is on the list, with no destructive flag (`--force`, `--hard`, `-D`,
  `--delete`, `-exec`, …). `git push`, `git reset`, `git rebase`, `git clean`
  and unknown commands still prompt; yolo mode still blocks the blocklist.
- Safety: plan mode (`--mode plan`) now permits read-only shell commands
  (`git log`, `ls`, `grep`, …) instead of denying every `run_command` — the
  same list gates it, and mutating subcommands (`git add`, `git commit`, …)
  stay denied.
- CLI: tool confirmations in the inline REPL now use the shared confirmation
  panel (risk level, action, diff preview, `[y]es/[n]o/[a]lways/[q]uit`)
  instead of a bare `dialoguer` prompt that the status line redraw used to
  overwrite; the panel also parks the renderer and restores the status line
  afterwards.
- Core: the confirmation prompt describes the pending action in a readable
  form (`run_command(command="cat nope.txt")`) instead of Rust's `Debug`
  output (`Object {"command": String("cat nope.txt")}`).
- CLI: console log lines (config load, transport warnings) no longer land on
  the transient status line and strand it on screen after the response — the
  renderer now installs a line-buffered writer hook that erases the status
  line, prints the log line, and repaints.
- CLI: output tokens were never accumulated, so the session counter always
  showed `↓0` (e.g. `📊 ↑1.1k / 3↓`); the REPL now adds this run's estimated
  input/output tokens and tool-call count to the session stats.
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
