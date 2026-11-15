# H-04 — Split the god files: implementation report

**Branch:** `feature/h04-split-god-files` (base `origin/dev` @ `129a07c`)
**Status:** DONE (with one note on `client.rs`, see Concerns)
**Date:** 2026-11-18

## What was implemented

Mechanical, move-only split of the four god files named in
`tasks/14112026-harness-hardening-backlog.md` § H-04, plus the
`tui/app.rs` / `interactive.rs` pair required by the acceptance
criterion and the test modules of `client.rs` (the only remaining file
above the ~1,500-line limit in the touched crates).

Discipline honoured: no logic edits. Every production line was verified
byte-identical (content multiset comparison, whitespace-normalised,
`pub(crate) ` prefixes stripped) between the original file and the new
module set; the only textual differences are rustfmt re-wraps of a few
>100-char signatures, the added `pub(crate)` visibility qualifiers, the
added imports / module declarations / re-exports, and — for split
inherent impls — the per-file `impl Type { ... }` wrappers.

### `takka-runtime/src/acp.rs` (4,444 lines) → `takka-runtime/src/acp/`

| File | Lines | Contents |
| --- | --- | --- |
| `mod.rs` | 247 | module docs, config (`AcpConfig`, timeouts), `initialize` responder + capability set, param/error helpers, re-exports |
| `framing.rs` | 250 | method-name consts, `AcpSink`/`Writer`, `serve`/`serve_with_config`/`serve_over`, `framing_loop`, test-only `FakeEngine` seam |
| `service.rs` | 463 | `RouteOutcome`, `AcpService` struct, routing (`handle_frame`/`route_frame`), shutdown/teardown, engine-thread spawn/join |
| `session.rs` | 733 | `Session` state, `session/new`/`boot_engine`/`session/prompt`/`session/cancel`, session modes, session errors |
| `session_config.rs` | 1,127 | `session/set_mode`/`set_config_option`/`session/load`, config-option build helpers |
| `prompt.rs` | 494 | prompt turn loop, prompt content mapping, resource-link decoding |
| `translate.rs` | 467 | `TranslatorState` engine-event → `SessionUpdate` translator, `tool_kind` |
| `permissions.rs` | 236 | `PermissionWaiter(s)` bridge, `request_permission` |
| `test_support.rs` | 570 | shared test harness (`VecSink`, `ChanReader`, `TestAcp`, fake engines) — test builds only |

`crate::acp::*` surface is unchanged: `mod.rs` re-exports every
submodule with `pub(crate) use`.

### `takka-cli/src/commands.rs` (8,491 lines) → `takka-cli/src/commands/`

| File | Lines | Contents |
| --- | --- | --- |
| `mod.rs` | 899 | module entry; render/interaction statics, `InteractivePromptGuard`, `QueueMode`, `MidRunKey`, `Commands` struct + constructors/builders/getters |
| `repl.rs` | 1,139 | `Commands::run`, `run_with_callbacks` |
| `render.rs` | 1,039 | `RunStats`/`TimelineStyle`, status painter, streamed-line preview, `print_*`, snippet/boundary helpers |
| `event.rs` | 425 | `render_event`, `ToolTimeline`, thinking block, tool-delta rendering |
| `slash.rs` | 803 | tool/model/mcp/plan/skill slash commands, `config` + `config show` dispatch |
| `confirm.rs` | 420 | question handler, todo renderer, provider presets, mid-run key polling |
| `config.rs` | 1,008 | config wizard (`init`/`edit`/`validate`/`reset`/`migrate`/`backup`/`restore`/`export`/`import`) + `config_migrate` core module |
| `model.rs` | 434 | model switching, config persistence, model listing/picking |
| `daemon.rs` | 675 | daemon attach/steer paths + daemon smoke tests |
| `e2e/mod.rs` | 309 | e2e suite entry, `hermetic_runtime_config(_at)` fixtures, lifecycle/capacity smoke tests |
| `e2e/support.rs` | 319 | scripted SSE provider server + tap-event helpers |
| `e2e/preview.rs` | 384 | live-preview smoke tests |
| `e2e/cancel.rs` | 377 | cancel/unwind smoke tests |
| `e2e/steer.rs` | 391 | mid-run steer / follow-up smoke tests |

`crate::commands` surface is unchanged (`Commands`, `QueueMode`,
`TimelineStyle`, `RunStats`, `SPINNER_ACTIVE`, `INTERACTIVE_PROMPT_ACTIVE`,
`TUI_ACTIVE`, `TUI_NEEDS_REDRAW`, `RENDER_GATE`,
`InteractivePromptGuard`, `consume_spinner_line`,
`commit_spinner_placeholder` — all re-exported from `mod.rs`).
`hermetic_runtime_config` lives in `commands/e2e/mod.rs` and stays
`pub(crate)`.

### `takka-cli/src/tui/app.rs` (2,687 lines) → `takka-cli/src/tui/app/`

| File | Lines | Contents |
| --- | --- | --- |
| `mod.rs` | 878 | docs/const, `AppMessage`, `InputTarget`, `App` struct + `new`, `run_tui`, `handle_key_event`, tests |
| `sessions.rs` | 261 | session save/new/resume, `/sessions` view, `switch_model`, `model_info`, `context_indicators` |
| `input.rs` | 594 | cursor editing, buffers, dropdowns, history, submit / mid-run submit |
| `slash.rs` | 806 | `/` command handlers (search, image, providers, mcp, skills, plan) |
| `messages.rs` | 174 | async message pump / tick |

`tui::app::{App, MessageRole, run_tui}` keep their paths.

### `takka-cli/src/interactive.rs` (2,461 lines) → `takka-cli/src/interactive/`

| File | Lines | Contents |
| --- | --- | --- |
| `mod.rs` | 428 | docs, `SessionStats`, `SLASH_COMMANDS`, `RenderState`, `run` entry, tests |
| `repl.rs` | 1,013 | `repl_loop`, `handle_input`, slash dispatch (`ReplAction`), `handle_repl_action`, `process_message` |
| `dropdown.rs` | 166 | `/` + `@` + skill dropdown plumbing |
| `render.rs` | 874 | banner/status bars/help/stats/history/goodbye/sessions output |

`crate::interactive::run` keeps its path.

### `takka-cli/src/client.rs` (1,525 lines) → `takka-cli/src/client.rs` + `client/`

Test-only move (production code untouched): the three inline test
modules moved to `client/in_process_tests.rs` (252), `client/daemon_info_tests.rs`
(114), `client/handshake_tests.rs` (152); `client.rs` is now 1,004 lines
and declares them with the same module paths (`daemon_info_tests` stays
`pub(crate)` because `commands::daemon` uses its `ENV_LOCK`).

## Verification

Baseline (recorded before any change, HEAD `129a07c`):

- `cargo test --workspace --all-targets`: **1,075 tests, all green**
  (takka-cli unittests 328, takka-core lib 594 + 88 integration,
  takka-runtime unittests 61 + 4 smoke).
- `cargo fmt --all -- --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.

After all commits:

- `cargo test --workspace --all-targets`: **1,075 tests, all green —
  same count and same per-target breakdown** (`h04-before-tests.txt` /
  `h04-final-tests.txt` captured during the run).
- `cargo fmt --all -- --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- No file above 1,500 lines remains in `takka-cli` / `takka-runtime`
  (largest: `commands/repl.rs` 1,139; `acp/session_config.rs` 1,127).
- Move-only evidence: whitespace-normalised line-multiset diffs of the
  old file vs. the new module set show zero added/removed production
  lines apart from visibility keywords, imports, module declarations,
  re-export lines and impl wrappers.

During development one intermediate `cargo test -p takka-cli` run
reported 6 failures in the daemon/`--runtime` smoke tests; an immediate
rerun of the same tree passed 328/328, and every later full run was
green (matches the known busy-socket flake noted in `AGENTS.md`).

## Commits

| Commit | Scope |
| --- | --- |
| `f3d98b9` | `refactor(runtime)`: split the acp god module |
| `9ce50cb` | `refactor(cli)`: split the tui app module |
| `dea6192` | `refactor(cli)`: split the interactive repl module |
| `367844e` | `refactor(cli)`: split the commands god module |
| `d9e0817` | `refactor(cli)`: move the client test modules into their own files |

## Self-review notes / concerns

- The enumerated `commands/` set from the spec is `mod.rs`, `repl.rs`,
  `render.rs`, `slash.rs`, `confirm.rs`. Three more topical submodules
  were required to satisfy the line limit and to keep tests next to the
  code they cover: `event.rs` (stream renderer, 218-line `render_event`
  alone), `config.rs` (the config wizard was ~1,100 lines by itself),
  `model.rs` (model switching + its tests), `daemon.rs` (daemon attach
  + its 550-line smoke suite) and `e2e/` (the 1,774-line e2e test module
  had to be split; grouped by fixture/harness, preview, cancel, steer).
  Same for `acp/`: `service.rs`, `session_config.rs`, `prompt.rs` and
  the shared `test_support.rs` harness were added because the enumerated
  five files could not hold the 2,100-line test module under 1,500 lines.
- `client.rs` was not in the spec's file list but was the last file over
  the limit in `takka-cli`; only its test modules moved (zero production
  diff), which is why one extra commit exists.
- Inherent impls (`impl Commands`, `impl App`, `impl AcpService`) are now
  written in several files; Rust allows this within a crate, but it does
  mean "find the method" needs `rg "fn <name>"` rather than scrolling.
- Visibility widened to `pub(crate)` for items that crossed module
  boundaries. Nothing became public outside the crate; no behavioural
  change.
- Test module paths changed (e.g. `commands::e2e_tests::smoke_*` →
  `commands::e2e::preview::smoke_*`). Test names/counts are otherwise
  unchanged; no test was rewritten.
