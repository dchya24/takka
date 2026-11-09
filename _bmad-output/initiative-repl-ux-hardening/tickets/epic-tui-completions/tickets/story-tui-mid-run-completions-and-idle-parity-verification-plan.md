---
title: 'Story 3.1 — TUI mid-run completions and idle parity verification'
type: 'feature'
ticket: 'story-tui-mid-run-completions-and-idle-parity-verification'
created: '2026-11-09'
status: 'built'
baseline_revision: 'cca0badc102942760476aabe034b8bbcd04f328c'
route: 'full'
route_source: 'auto'
risk: 'low'
review: 'quick'
review_source: 'pinned'
lenses_ran: ['quick']
lenses_ran: []
review_loop_iteration: 0
followup_review_recommended: false
context:
  - '_bmad-output/initiative-repl-ux-hardening/tickets/epic-tui-completions/epic-tui-completions.md'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** In the TUI, typed text during an active run lands in `mid_run_input` (story 2.4) but the completion dropdowns never engage there — `insert_mid_run_char` never reaches the dropdown engine, `accept_dropdown` writes the idle buffer, and the mid-run buffer isn't even echoed in the input pane. Idle-state completions already exist (landed pre-initiative); the epic's remaining gap is mid-run parity plus a verified idle-parity audit against the inline REPL.

**Approach:** Make the existing dropdown engine buffer-agnostic: the same update/accept/navigation paths read from and write back to either the idle buffer or the mid-run buffer; echo the mid-run buffer in the input pane; fix the one idle drift found (skill follow-up re-trigger missing in `update_dropdown`); verify both states.

## Boundaries & Constraints

**Always:**
- The story-2.4 mid-run dispatch contract is untouched: Enter with no dropdown steers, typing-filled text is delivered as plain text per `/queue` mode (completions assist typing only; mid-run slash commands are already decided out — they steer as text).
- Idle-state dropdown behavior is behavior-identical except the recorded skill-follow-up drift fix; hover/audit diffs must be justified or reverted.
- All rendering stays in the single ratatui frame; no second terminal writer (C1).

**Never:**
- No new dropdown types, no slash-command-set changes, no `/models` completion semantics change.
- No engine/wire protocol changes; no changes to the inline REPL's own completion code unless the audit finds the drift is the inline side's bug (record, don't fix inline — REPL is the reference contract).

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Mid-run command dropdown | `is_loading`, typed `/mod` | command dropdown filters like idle; Tab/Enter fills `/models ` etc. into `mid_run_input`; no steering | none |
| Mid-run file dropdown | `is_loading`, typed `check @tik` mid-sentence | file dropdown opens on the `@` token; accept replaces from `@` to end within `mid_run_input` | none |
| Mid-run model/skill follow-up | typed `/models ` or `/skill `/`/sk ` then chars | model dropdown filters; skill list opens from `/sk` accept and filters follow-up chars | skill discovery unavailable/empty → dropdown does not open (stays closed, no empty trap) |
| Dropdown navigation mid-run | dropdown open, Up/Down/Tab/Enter/Esc | same key contract as idle; Enter-with-dropdown fills, never submits/steers | none |
| Mid-run no dropdown | typed text, Enter | exact story-2.4 path: steer_text per queue mode, `↳` hand-over line, buffer cleared | none |
| Mid-run echo | `is_loading`, non-empty `mid_run_input` | input pane echoes the mid-run buffer with the dark loading title | none |
| Mid-run correction | typed chars then Backspace | last char removed from `mid_run_input`; dropdown re-evaluated (restores parity for accepted fills) | none |

</frozen-after-approval>

## Code Map

Project root: `takka-cli/src/`.

- `tui/app.rs` -- the whole story's core. `App::input` + `cursor_pos` (idle buffer), `App::mid_run_input: String` (mid-run buffer, cursor always at end, story 2.4, ~line 83). Refactor targets: `update_dropdown()` (~1128) and `find_at_trigger()` (~1150) read `self.input`/`self.cursor_pos` directly; `accept_dropdown()` (~1202) writes the idle buffer; `open_skill_dropdown()` (~1311) uses `try_lock` on the shared `Commands` (mid-run the lock may be held by the run → returns empty; add the empty guard). `insert_mid_run_char` (~1407) and `submit_mid_run` (~1378) are the mid-run handlers; the idle insert/delete/move methods each call `update_dropdown()`.
- `tui/app.rs` `handle_key_event()` -- key dispatcher (~2062): the dropdown interception block runs BEFORE the idle/loading split (Up/Down/Tab/Enter/Esc already route right once a dropdown exists); `KeyCode::Char` and `Backspace` split on `is_loading` (~2121-2131); Backspace/mid-run is the missing arm.
- `tui/ui.rs` -- `draw_input()` (~334): renders `app.input` even while loading; the loading branch needs to render `mid_run_input` (title already flips to dark " Input (waiting...) "). `draw_dropdown()` (~396) is buffer-agnostic — no changes.
- `tui/dropdown.rs` -- `Dropdown::new()` filters at construction (pure per type+query); `DropdownType::{Command,File,Model,Skill}`; skill chaining contract (`/sk` accept → `new_skill`, then follow-up typing filters) lives in the REPL loop, not here.
- `interactive.rs` (~478) -- inline REPL's skill follow-up re-evaluation ("if the user selected `/skill` the input is now `/skill ` which should trigger the skill dropdown") — the reference behavior `update_dropdown` lacks; do NOT change this file.
- `tui/app.rs` `mod tests` (~2170) -- existing smokes: `tui_submit_mid_run_queues_in_followup_mode` pattern to reuse for the new tests.

Skill drift detail: after accepting `/sk` in the TUI, the opened Skill dropdown dies on the next typed char (update_dropdown re-evaluates only Command/Model/File triggers). Fix: `/skill `/`/sk ` argument becomes a Skill follow-up trigger; empty/discovery-unavailable sources keep it closed.

## Tasks & Acceptance

**Execution:**
- [ ] `takka-cli/src/tui/app.rs` -- Make the dropdown engine buffer-agnostic: a small internal view (text ref + cursor) feeding `update_dropdown`/`find_at_trigger`/`accept_dropdown`, with write-back choosing the idle buffer (text+cursor_pos) or the mid-run buffer (text, cursor=end). Byte-identical idle behavior. -- The epic's core gap.
- [ ] `takka-cli/src/tui/app.rs` -- Mid-run wiring: `insert_mid_run_char` → `update_dropdown`; add the `Backspace` mid-run arm (trim last char, re-evaluate); clear the dropdown when the buffer is cleared/submitted. -- Parity behavior.
- [ ] `takka-cli/src/tui/ui.rs` -- `draw_input`: while loading render `mid_run_input` in the input pane; else unchanged. -- Completions must be visible; story 2.4 left the buffer un-echoed.
- [ ] `takka-cli/src/tui/app.rs` -- Skill follow-up parity: `update_dropdown` re-triggers the Skill dropdown for `/skill `/`/sk ` arguments (rebuild via discovery, filter by query); `open_skill_dropdown` gains the empty/discovery-unavailable guard (stay closed). -- The single idle drift; also removes the mid-run empty-dropdown Enter trap.
- [ ] `takka-cli/src/tui/app.rs` tests -- New unit smokes: mid-run `/` command dropdown opens+filters+accept-fills `mid_run_input` without steering; `@` file trigger mid-run; Up/Down/Tab/Esc navigation lands on the mid-run buffer; Enter with open dropdown fills (buffer keeps text, no steer, dropdown closed); Backspace correction; skill guard; idle skill re-trigger. Reuse the 2.4 smoke harness (`Commands::new(Config::fallback())` + queue modes). -- The runnable check.

**Acceptance Criteria:**
- Given a `Commands` in FollowUp mode with `is_loading` true, when the app receives chars `/mod` then Tab then Enter, then the buffer holds the filled model form, the queue-first submit steers it as text, and the message log gains the `↳` hand-over.
- Given the mid-run file trigger, when the user types an `@` token and accepts, then `mid_run_input` contains the completed path inline and the dropdown closes (or re-triggers for directories like idle).
- Given the same text typed idle and mid-run, then the dropdown opens on the same triggers with the same items; the only idle delta is the skill follow-up fix.
- Given the full workspace checks, then fmt/clippy green and `cargo test --workspace --all-targets` all green, e2e smoke suite included.

## Implementation Notes

- Story implemented by the `implementer` subagent (commit `919b4ca` on `feature/tui-mid-run-completions`, base `cca0badc…`); orchestrator verified fmt/clippy/tests itself (986→987 green on full workspace reruns) and audited the I/O matrix row by row — the mid-run-echo row initially had no covering test; a TestBackend draw test (`draw_input_echoes_mid_run_buffer_while_loading` in `ui.rs`) was added directly (commit `d407c8d`).
- 2026-11-09 orchestrator tests runs: `commands::e2e_tests` smokes are timing-sensitively flaky under parallel load (`smoke_cancel_mid_preview_unwinds_with_notice`, `smoke_follow_up_queue_spawned` — each failed once, passed solo and in serial reruns; both live in `commands.rs` paths this story never touches). Pre-existing, same class AGENTS.md notes for daemon smokes.
- Review patch: mid-run plain Up/Down/Left/Right/Home/End are idle-only (history/cursor keys would edit the idle buffer the loading pane no longer renders); new mid-run/no-op test added. Full workspace rerun after patch: 987 passed / 0 failed.

## Plan Change Log

## Review Triage Log

- First pass — lens `quick` (2026-11-09): verdicts 0 high / 1 medium / 4 low / 2 false of 7 findings; routes: 2 patch (one group), 5 rejected on their evidence.
- F1 (app.rs `evaluate_dropdown` close-empty guard — "second idle delta") — verdict low → record: verified real vs the old TUI (`/zzz` previously swallowed keys) but it is exactly the REPL reference contract (`interactive.rs` `*dropdown = next.filter(|dd| !dd.is_empty())`); already laid in the report's Judgment calls; recorded explicitly in the audit records (ratio patch below).
- F2 (app.rs `accept_dropdown` Command branch fills `/skill ` — "second idle delta") — verdict low → record: verified; mechanically required for the follow-up trigger and REPL-identical (`buffer.set_text("/skill ")`); recorded in the audit records.
- F3 (REPL exact-match direct-open not ported) — verdict false: the drift IS recorded (report "Left incomplete / risky" item 1 returns the epic's "fixed or recorded" done-when); the claim contradicted only a plan-wording nuance, and its fix would edit the plan → rejected.
- F4 (audit record incomplete: skill-accept auto-submit and provider-qualified model-accept drifts unrecorded) — verdict low → patch (documentation): both pre-existing accept-path drifts verified true in `interactive.rs:799-825`; now recorded in the report's audit records as consciously kept (the model one with its candidate-story note); fixes are documentation, no code surface.
- F5 (mid-run plain Up/Down edit the hidden idle buffer via history; cursor keys too) — verdict medium → patch: verified in `handle_key_event` (unconditional history/cursor arms) × new mid-run echo rendering; users pressing Up mid-run would silently mutate a buffer that is not even the pane's content. Patch: the arms gain `!app.is_loading` guards (SHIFT scrolling untouched); covered by the new `tui_mid_run_history_keys_leave_buffers_alone`. Re-verification after patch: fmt/clippy clean, 987/0 full workspace.
- F6 (new tests read ambient config/skills dirs) — verdict low → rejected: assertions tolerate both outcomes (nothing machine-dependent can fail them), the touched classes make no runtime/provider call (AGENTS.md's hermetic requirement targets runtime-touching tests), and the smallest honest fix is a `filter_models` injection API — beyond a direct correction. Recorded as a note instead (report audit record).
- F7 (AC verification is report-claimed, not inspection-verifiable) — verdict false: the ACs' verification is the orchestrator's own under the build workflow; fmt/clippy clean and the full workspace suite ran 987 passed / 0 failed after the patch (earlier failed names solo-rerun green).

## Design Notes

- Buffer-agnostic engine, NOT a duplicated mid-run engine: one code path, two views (idle: text + `cursor_pos`; mid-run: text, cursor always at end — push-only plus the new Backspace).
- Delivery as text: mid-run acceptance may fill strings like `/models gpt-4o ` or `/skill x`; Enter later delivers them via `steer_text` per queue mode — this is the settled initiative decision (mid-run slash commands route as text, epic-mid-run-input 2026-11-06), completions are assistive.
- Model dropdown construction is pure (static model registry), so it works mid-run; only the Skill sources touch app state (guarded).

## Verification

**Commands:**
- `cargo fmt --all -- --check` -- expected: no diff.
- `cargo clippy --workspace --all-targets -- -D warnings` -- expected: zero warnings.
- `cargo test --workspace --all-targets` -- expected: all green incl. the new TUI completion smokes, prior 1.x/2.x smokes, and the e2e smoke suite.

**Manual checks (if no CLI):**
- PTY-only (idle + mid-run): type `/`, `@`, `/sk`, `/models ` in both states; dropdown opens/fills matches the inline REPL; mid-run Enter with closed dropdown still steers with the visible `↳` line.
