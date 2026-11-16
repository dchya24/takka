# Tasks: takka-cli — Agentic CLI

**Feature**: `takka-cli` — the `takka` binary, built on `takka-core` and
`takka-runtime` (renamed from `agentic-cli` in the 2026-11 `takka` rebrand)
**Status**: Foundation done; shared widgets stack landed; alignment refactor
merged; Phases 9–11 landed. ACP v1 adapter merged as PRs #16–#19; harness
hardening H-01..H-03 merged as PRs #20–#22.
**Open work**: [`14112026-harness-hardening-backlog.md`](14112026-harness-hardening-backlog.md)
(H-04–H-07) — see also `docs/ROADMAP.md` "Open work".
**Created**: 2026-04-20
**Updated**: 2026-11-14
**Depends on**: `tasks-core-agentic.md`

---

## Relevant Files

- `takka-cli/Cargo.toml` — Binary manifest
- `takka-cli/src/main.rs` — Entry point + Ctrl+C handler + `--color` flag
- `takka-cli/src/cli.rs` — clap argument tree
- `takka-cli/src/commands.rs` — Command handlers (run, interactive, config, models, mcp, tools, …)
- `takka-cli/src/interactive.rs` — Reedline REPL + slash-command parser
- `takka-cli/src/confirmation.rs` — Risk-coloured confirmation panel
- `takka-cli/src/file_ref.rs` — `@`-completion for file references
- `takka-cli/src/widgets/` — Shared ratatui widgets (markdown, spinner, progress, components, tool_call, diff, inline, capabilities)
- `takka-cli/src/tui/` — Full-screen TUI mode (app, ui, dropdown, input)

---

## Instructions for Completing Tasks

**IMPORTANT:** As you complete each task, check it off by changing `- [ ]` to `- [x]`. Update after completing each sub-task.

## Tasks

### Phase 1 — Foundation (✅ done)

- [x] 0.0 Project setup
  - [x] 0.1 Crate skeleton with `takka-core`, `clap`, `tokio`
  - [x] 0.2 Cargo manifest + workspace integration
- [x] 1.0 CLI argument parsing
  - [x] 1.1 Subcommands (`run`, `interactive`, `tui`, `config`, `models`, …)
  - [x] 1.2 Options (`--provider`, `--model`, `--config`, `--mode`, `--color`, …)
- [x] 2.0 Configuration
  - [x] 2.1 Reads `takka_core::Config` (multi-provider). The legacy single-provider shape was deleted.
  - [x] 2.2 Env-var substitution
  - [x] 2.3 Config wizard / templates / setup commands

### Phase 2 — Run loop & output (✅ done)

- [x] 3.0 Single-task run
  - [x] 3.1 `takka run <task>` integrates with `Orchestrator`
  - [x] 3.2 Streaming output through `widgets::inline`
  - [x] 3.3 Confirmation handler wired into `Safety`
- [x] 4.0 Interactive REPL
  - [x] 4.1 Reedline-based input
  - [x] 4.2 Slash commands: `/help`, `/clear`, `/config`, `/history`, `/tools`, `/stats`, `/mcp`, `/save`, `/load`, `/provider`, `/models`, `/plan`, `/search`, `/quit`
  - [x] 4.3 `@`-completion with recursive `.gitignore`-aware listing
  - [x] 4.4 `/`-completion popup with command descriptions
  - [x] 4.5 Tab navigation, inline hints, Ctrl+R history search
  - [x] 4.6 Status bar (model, provider, tokens, elapsed)
  - [x] 4.7 Resize-safe full-width prompt
- [x] 5.0 Full-screen TUI mode
  - [x] 5.1 `takka tui` launches alternate-screen ratatui app
  - [x] 5.2 Subscribes to orchestrator events; renders tool calls + diffs through shared widgets
  - [x] 5.3 Dropdown widget for `@` and `/`
  - [x] 5.4 Live transient spinner shared with inline mode

### Phase 3 — Shared widgets architecture (✅ done)

- [x] 6.0 Widget extraction
  - [x] 6.1 Single ratatui-based stack used by inline + TUI
  - [x] 6.2 Capability detection (`NO_COLOR`, `TERM=dumb`, isatty, `--color` override)
  - [x] 6.3 Markdown renderer
  - [x] 6.4 Tool-call panel + tool-result notification
  - [x] 6.5 Unified-diff renderer + summary line
  - [x] 6.6 Spinner + progress widgets
  - [x] 6.7 Panels, badges, headers, gradients, tables, sparklines
  - [x] 6.8 Zero raw `\x1b[` escapes remain in `takka-cli/src/`

### Phase 4 — Safety / permissions (✅ done)

- [x] 7.0 `--mode default|plan|yolo` flag wired through to `Safety`
- [x] 7.1 Two-stage Ctrl+C → cooperative cancel (process-global `Arc<AtomicBool>`)
- [x] 7.2 Risk-coloured confirmation panel (border colour drives by risk level)

### Phase 5 — Memory & search (✅ done)

- [x] 8.0 `/search <query>` slash command surfaces `Memory::search`
- [x] 8.1 Snippet renderer with role badges + UTF-8-safe windowing

### Phase 6 — Confirmation UX (✅ done)

- [x] 9.0 Diff preview in the confirmation prompt for `write_file` /
      `edit_file` / `apply_patch` (rendered through `widgets::diff`)
- [x] 9.1 60-line cap with `… N more diff line(s) hidden` notice

### Phase 7 — Session control + context indicators (✅ done)

- [x] 10.0 `/restart` (alias `/reset`) slash command
  - [x] 10.1 Drops conversation memory in-place
  - [x] 10.2 Clears pending cancel flag + accumulated event handlers
  - [x] 10.3 Keeps provider, tools, system prompt, AGENT.md, persistent memory loaded
  - [x] 10.4 SessionStats `reset()` zeroes status-bar counters
- [x] 11.0 Status-line indicators for AGENT.md + persistent memory
  - [x] 11.1 `Commands` records `agent_md_path` + `memory_md_loaded` during init
  - [x] 11.2 Banner panel shows a `🔗 ctx  📄 AGENT.md  ·  🧠 memory.md` line when present
  - [x] 11.3 Status bar shows the same chips on a second row when present

### Phase 8 — Testing & docs

- [x] 12.0 Unit tests for argument parsing + widget helpers
- [x] 12.1 README in `takka-cli/`
- [x] 12.2 End-to-end smoke test for `takka run` against a mock provider
  - 3 tests in `commands.rs`: basic tool→text flow, event emission, ScriptedProvider

### Phase 9 — Planner Agent (CLI integration)

- [x] 13.0 `/plan` slash command exists
  - [x] 13.1 Numbered list rendering of plan steps in inline output
  - [x] 13.2 `plan_inline()` method: create plan → render → dialoguer Confirm → execute
  - [x] 13.3 Conversation tracking: plan entries pushed to conversation history with timing

- [x] 13.0b TUI & inline enhancements
  - [x] 13.4 TUI panel widget created (`takka-cli/src/tui/plan_panel.rs`) — integration blocked by pre-existing TUI build errors (crossterm in dev-dependencies)
  - [x] 13.5 Plan progress bar in inline mode: live `labeled_bar` + step description rendering during execution
  - [x] 13.6 Replan notification: surface when planner revises remaining steps (requires event extension)
  - [x] 13.7 Wire planner events (`PlanProgress`) to inline widget renderer via `planner.on()` callback

- [x] 14.0 Plan mode integration
  - [x] 14.1 `takka run --mode plan` works with the new planner (not just deny writes)
  - [x] 14.2 `takka run --plan "<goal>"` shorthand for plan-then-execute without entering interactive mode

### Phase 10 — Skill System (CLI integration) (✅ done)

- [x] 15.0 `/skills` REPL command
  - [x] 15.1 List available skills with name, description, source directory
  - [x] 15.2 `/skills <name>` — show skill details (instructions preview)
  - [x] 15.3 Auto-complete skill names in `/skills <name>`
- [x] 16.0 `takka skill create <name>` wizard
  - [x] 16.1 Scaffold `SKILL.md` in `~/.config/takka/skills/<name>/` with frontmatter template (`--global` flag)
  - [x] 16.2 Name validation + directory creation + template file
- [x] 17.0 Status bar indicators
  - [x] 17.1 Show `⚡ <name>` chip when a skill is active in session
  - [x] 17.2 Banner panel line: `📄 AGENT.md  ·  🧠 memory.md  ·  ⚡ skill:<name>`
- [x] 18.0 `SkillResolver` trait (CLI-side, following `QuestionHandler` callback pattern)
  - [x] 18.1 Trait: `fn resolve(&self, skill_name: &str) -> Option<String>`
  - [x] 18.2 Global handler slot with `set_skill_loader` / `resolve_skill` / `activate_skill` (same pattern as `set_question_handler`)
  - [x] 18.3 CLI uses `SkillIndex` to resolve, skill tool auto-registers

### Phase 11 — Prompt Caching (CLI integration)

- [x] 19.0 Cache observability in UI
  - [x] 19.1 Add cache hit ratio to status bar: `📦 cache 68%` (when provider supports it)
  - [x] 19.2 Add cached token counts to `/stats` output
  - [x] 19.3 Show cache savings in response summary + goodbye panel
- [ ] ~~20.0 Config integration~~ (deferred — caching works via manual config.toml edit)
  - [ ] ~~20.1 Expose `provider.cache.*` settings in `takka config` wizard~~ (deferred)

> **Note:** The cache metrics (cache_read_tokens, cache_creation_tokens) flow through `ChatUsage` but are not yet wired from provider responses to `SessionStats`. This requires emitting usage events from the orchestrator in a follow-up task.

> **Architecture reference:** [docs/shared-widgets-architecture-26052026.md](../docs/shared-widgets-architecture-26052026.md)
> **Roadmap:** [docs/ROADMAP.md](../docs/ROADMAP.md)

### Fase 1 — Tool live output rendering (landed)

- [x] 21.0 Inline `render_event`: header `⟳ tool`, delta DIM, durasi + nota truncated di `ToolOutput`
- [x] 21.1 Pure helper `render_tool_delta` (unit-testable)
- [x] 21.2 TUI: role `MessageRole::ToolActivity` + `AppMessage::ToolDelta` + rendering indent DIM (kedua match site + `/search` label)

> Spec: `docs/superpowers/specs/2026-08-06-interactive-live-progress-and-steering-design.md`
> Next: hardening items H-04–H-07 in
> [`14112026-harness-hardening-backlog.md`](14112026-harness-hardening-backlog.md)
> — live tool output, steering and the non-blocking REPL are landed.
