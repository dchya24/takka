# Story 3.1 — TUI mid-run completions and idle parity verification — Implementation Report

- **Status:** DONE
- **Commit:** `919b4ca` — `feat(tui): mid-run completions via buffer-agnostic dropdown engine (story 3.1)` on branch `feature/tui-mid-run-completions` (base: `cca0bad`, the plan's baseline revision)
- **Test summary:** `cargo test --workspace --all-targets` → **985 passed / 0 failed** (incl. 9 new TUI smokes, prior 1.x/2.x smokes, daemon/e2e smoke suite, `protocol_smoke`); `cargo fmt --all -- --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` zero warnings.

## What changed

### `takka-cli/src/tui/app.rs`

1. **Buffer-agnostic dropdown engine** (the core gap):
   - New private `InputTarget { Idle, MidRun }` plus `App::active_target()` / `buffer_view()` / `buffer_write_back()`. The target derives from `is_loading` at each call: idle → `input` + `cursor_pos`; loading → `mid_run_input` with the cursor always at the end (story-2.4 contract).
   - `find_at_trigger` moved from `&self` method to a free `fn find_at_trigger(text: &str, cursor: usize)` (body byte-identical to before).
   - `update_dropdown` now snapshots the active `(text, cursor)` view and delegates to a new pure associated fn `evaluate_dropdown(text, cursor, skill_pairs)`, where `skill_pairs` is an `impl FnOnce()` invoked only when a skill trigger needs discovery (ordinary typing never pays for a discovery walk — same laziness as the inline REPL).
   - `accept_dropdown` reworked: takes the dropdown up front, reads the active buffer view, computes the fill locally, writes back through `buffer_write_back` (mid-run write-back ignores the cursor — it is always the end). All five fill branches (Command / File / Skill / Model / skill-chain) keep the previous idle arithmetic byte-for-byte; the only behavior delta is the skill chain (below).
2. **Mid-run wiring** (parity behavior):
   - `insert_mid_run_char` now calls `update_dropdown()` — mid-run typing reaches the dropdown engine.
   - New `delete_mid_run_char()` (pop last char + re-evaluate) wired to a new `KeyCode::Backspace if app.is_loading` dispatcher arm.
   - `submit_mid_run` clears the dropdown when the buffer clears.
   - Loading→idle flips (watchdog timeout, `TaskComplete`, `Error`, Ctrl+C cancel) now clear the dropdown so a stale mid-run dropdown cannot leak into the idle state (no-op for all pre-existing idle flows, which always had `dropdown == None` at those points).
3. **Skill follow-up parity** (the recorded idle drift):
   - `evaluate_dropdown` gains the inline REPL's trigger 1c: `/skill <partial>` / `/sk <partial>` re-opens the discovered skill list filtered by the argument, so an accepted `/skill `/`/sk ` survives narrowing instead of dying on the next char.
   - `open_skill_dropdown` rebuilt on an extracted `skill_pairs()` helper and gains the empty/unavailable guard via `open_skill_dropdown_with(pairs)` (testable injection): nothing discoverable → dropdown stays closed (no empty trap).
   - Accepting `skill`/`sk` from the command dropdown now fills `/skill ` into the active buffer before re-opening the skill list — previously the TUI left the buffer as `/sk` and only opened the list, which made the follow-up trigger unreachable (see Justifications).

### `takka-cli/src/tui/ui.rs`

4. **Mid-run echo**: `draw_input` renders `mid_run_input` (cursor at end) in the input pane while `is_loading`; idle rendering and the dark " Input (waiting...) " title/border are unchanged. `draw_dropdown` was already buffer-agnostic — untouched.

### Tests (in `takka-cli/src/tui/app.rs` `mod tests`, reusing the story-2.4 harness: `Commands::new(Config::fallback())` + `QueueMode::FollowUp`)

9 new smokes covering the story's I/O matrix:

| Test | Covers |
|---|---|
| `tui_mid_run_command_dropdown_fills_buffer_without_steering` | `/mod` mid-run opens + filters like idle; accept fills `mid_run_input` = `/models `; queue untouched |
| `tui_mid_run_file_trigger_accept_replaces_from_at` | `@` trigger mid-sentence mid-run; accept replaces from `@` to end |
| `tui_mid_run_enter_with_dropdown_fills_then_next_enter_steers` | Enter-with-dropdown fills (no steer, queue empty); Enter-without-dropdown follows the exact 2.4 path (`↳` hand-over line, queue gains text) |
| `tui_mid_run_navigation_contract_esc_tab_up` | Esc keeps the buffer; typing re-opens; Tab accepts into mid-run buffer; Up wraps |
| `tui_mid_run_backspace_corrects_buffer_and_dropdown` | Backspace trims + re-evaluates; empty-buffer backspace safe; idle buffer untouched |
| `tui_mid_run_model_trigger_parity` | `/models gpt` mid-run constructs the same pure model dropdown; no-match query stays closed (guarded, environment-tolerant) |
| `tui_open_skill_dropdown_guard` | empty discovery → stays closed; skills present → opens (injected pairs, hermetic) |
| `tui_skill_followup_retrigger_matches_repl` | `/skill `/`/sk ` follow-up filtering, empty-query full list, empty-discovery and no-match stay closed (pure evaluator, hermetic) |
| `tui_idle_parity_and_skill_chain_fill` | idle `/mod` identical items; idle no-match stays closed; accepting `skill` fills `/skill ` with cursor at end |

Hermeticity: command-table assertions are const-based; skill tests inject pairs through `evaluate_dropdown` / `open_skill_dropdown_with`; file/model tests are environment-tolerant (assert type/query when items exist, else assert the guard kept it closed) matching the existing `dropdown.rs` test conventions.

## Verification

- `cargo fmt --all -- --check` — no diff.
- `cargo clippy --workspace --all-targets -- -D warnings` — zero warnings.
- `cargo test --workspace --all-targets` — all green: 985 passed / 0 failed across all crates, including `commands::e2e_tests` (lifecycle/embedded/spawned/daemon smokes), `daemon_smoke_tests`, `takka-runtime` `protocol_smoke`, and all prior TUI smokes (no daemon flake observed; no rerun needed).
- TDD note: the brief did not mandate strict test-first; implementation was built first, compiled clean, then the 9 smokes were written and passed on the first run except formatting/clippy nits (fixed: line wraps, `single_match`).

## Judgment calls (justified diffs from strict "byte-identical idle")

1. **Empty dropdowns now commit closed** (`next.filter(|dd| !dd.is_empty())`), mirroring the inline REPL's documented guard. Required by the acceptance criteria: an invisible empty dropdown (e.g. `/models <id> x` matching nothing, or `/zzz`) previously intercepted Enter/arrows it could not serve — under the mid-run contract that would swallow the very Enter that must steer. Renders identically (`draw_dropdown` already ignored empty dropdowns); the only changed states are ones where a dropdown with no items ate keys.
2. **Accepting `skill`/`sk` now fills `/skill `** into the buffer before opening the skill list (inline REPL reference behavior). Without the fill, the new follow-up trigger could never engage after accept (typed chars would concatenate into the command query `/skx…`), so this is part of the recorded skill-follow-up drift fix, not a separate behavior change.
3. **Loading→idle flips clear the dropdown** (4 sites, one line each). Behavior-neutral for all pre-existing flows (dropdown always `None` there) and prevents a stale mid-run dropdown re-targeting the idle buffer after a run ends mid-completion.

## Self-review findings

- Verified equivalence of `accept_dropdown` arithmetic per branch against the old code (File `@`-replacement, Model prefix/after-cursor splice, Skill name extraction) — identical for idle.
- Confirmed no engine/wire/protocol changes; inline `interactive.rs` untouched; slash-command set and `/models` semantics untouched; single-frame rendering untouched (C1); story-2.4 dispatch contract intact (Enter with no dropdown still steers per queue mode — covered by test).
- Duplicate section-header comment left by a failed edit was caught in self-review and removed before commit.

## Left incomplete / risky

- **REPL exact-match direct-open not ported:** the inline REPL also opens the skill list directly when the typed query is exactly `skill`/`sk` (before any space); the plan's fix scope names only the follow-up trigger, so the TUI keeps its command-dropdown view for `/sk` (accepting `skill` still chains into the skill list). Recorded here as a consciously kept difference; trivially portable later if the audit wants full equality.
- **REPL skill accept auto-submits, TUI fills only:** in the inline REPL, accepting a skill from the Skill dropdown builds `/skill <name>` AND submits it (`return Some(cmd)`, interactive.rs ~800); the TUI only fills the buffer — the user presses Enter themselves. Consciously kept: TUI submit is the Enter key's job (mid-run delivery must stay steerable); recorded as a kept difference.
- **REPL model accept is provider-qualified AND auto-submits; TUI fills unqualified only:** the REPL emits `/models <provider>/<model-id>` from the dropdown's `model_meta` to avoid a same-name collision across providers, and submits immediately. The TUI extracts the plain id via `get_model_id` and only fills — an ambiguous display name could resolve to a different provider's model on switch. Consciously kept under the story's "no `/models` semantics change" constraint; a full port is a candidate story if parity is pursued further (it needs the TUI to resolve a command run on accept, which touches the submit contract — not a fill-engine detail).
- **Idle key-handling deltas fixed en route, recorded explicitly:** (a) empty dropdowns now commit closed in the TUI exactly like the REPL's `next.filter(|dd| !dd.is_empty())` — previously an idle `/zzz` or a post-model-accept `/models <id> ` left an open-but-empty dropdown that silently swallowed Enter/Up/Down/Tab/Esc; (b) accepting `skill`/`sk` from the command list now fills `/skill ` like the REPL (mechanically required so follow-up typing goes into the argument query instead of concatenating into the command part). Both are idle-visible and REPL-parity-restoring; they exit the strict "byte-identical idle" reading of the plan and were re-verified by the quick review lens.
- **Mid-run plain Up/Down/Left/Right/Home/End are now idle-only (review patch):** history browsing edits the idle buffer, which the loading pane no longer renders after the mid-run echo — pressing Up mid-run would have silently rewritten the hidden idle buffer (surfaced only after the run). The guard makes mid-run no-ops; SHIFT+Up/Down scrolling is untouched. Covered by `tui_mid_run_history_keys_leave_buffers_alone`.
- **Test-ambient note for `tui_mid_run_model_trigger_parity` / ambient config reads:** `Dropdown::filter_models` and skill discovery read ambient config/`~` skill dirs; the assertions tolerate both outcomes (matching items or the closed guard), so they cannot fail on a developer machine's state, but the exercised branches are not machine-independent. A hermetic port would need injected model/skill sources (a `filter_models` API change) — out of this story's reach; the injected-pairs tests (`tui_open_skill_dropdown_guard`, `tui_skill_followup_retrigger_matches_repl`) cover the logic hermetically.
- **Skill discovery mid-run uses default `DiscoveryConfig` when the Commands lock is contended** (pre-existing `try_lock` fallback). The empty guard now ensures that can only end in "closed" or "a working list", never an empty trap.
- **Manual PTY checks** (idle + mid-run typing of `/`, `@`, `/sk`, `/models ` in a real terminal) were not performed in this session — no interactive TTY available to the agent; all behavior is covered by the unit smokes at the App level, but a human PTY pass is recommended before release per the plan's manual-checks section.
- Daemon smoke flake risk (per AGENTS.md) did not materialize; rerun before diagnosing if CI shows one.
