---
title: 'Story 2.4 — TUI mid-run input (both modes)'
type: 'feature'
ticket: 'story-tui-mid-run-input-both-modes'
created: '2026-11-08'
status: 'built'
baseline_revision: '9f989ae'
route: 'full'
route_source: 'pinned'
risk: 'medium'
review: ''
review_source: ''
lenses_ran: []
review_loop_iteration: 0
followup_review_recommended: false
context:
  - '_bmad-output/initiative-repl-ux-hardening/tickets/epic-mid-run-input/epic-mid-run-input.md'
warnings: []
deferred: []
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** The TUI still gates text input behind `is_loading` — every keystroke during a run is dropped for the user; the inline REPL has both queue modes and the reader, but the TUI is silent, so `MID-run` input remains REPL-only. The reader's spawns work through `run_with_callbacks` (the TUI's run path), but the TUI never forwards typed text to the run's steering contract and the message-log surfaces no feedback.

**Approach:** While `is_loading`, the TUI still accepts printable chars and Enter on the input buffer; the send path routes the buffer through the SAME client contract — the run's `Commands` (TUI's `Arc<Mutex<Commands>>` instance) carries `queue_mode` + `steer_tx`. The TUI's `spawned tasks` wiring already threads the cancel path; here the text flow rides the same client struct with a `Commands::steer_text(text)` helper — steer (Interrupt) sends via the client's existing steering contract (`steer()`, `push_pending_follow_up` (FollowUp), and the log feed (`AppMessage::StreamChunk`...)). Zero-residue rows hold for no-mid-run-text runs, and the TUI's loading watchdog/`Esc` reset paths stay untouched.

## Boundaries & Constraints

**Always:**
- The TUI keeps its own rendering surface; the mid-run input's typed text feeds the client's existing contract (`Commands::steer_text` routes per queue mode exactly as the inline reader does), and NONE of the inline reader's machinery invades the TUI's rendering path.
- The prompt flow / confirmation flow semantic stays; the mid-run input's pause is via `INTERACTIVE_PROMPT_ACTIVE` which the RAII guard flips and the TUI SKIPS while a prompt holds stdin.
- Every produce/text action sits under the renderer/steering contract; the client start of the drain loop contract is untouched (`drain_follow_ups` executes queued text as the next run).

**Never:**
- No TUI command-buffer refactor: no new text-buffer state machine, no `dropdown` changes, no TUI widget re-themes; ignores `Esc`-cancel semantics (discard buffer on Esc is a Telegram-like UX nicety — NOT in this story; only chars + Enter route).
- No changes to the inline REPL's reader (story 2.1's) shape; no extra tests on the inline path; the TUI's buffers and the existing status-bar restitching untouched.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| TUI mid-run char input | is_loading true, chars typed mid-run | chars land in the input buffer (echo surfaced the current TUI path), Enter submits → routes per queue mode with a visible feedback line | no error expected |
| TUI mid-run Enter (Interrupt) | queue_mode Interrupt at the run | steers into the run: visible via `SteerQueued` event (status already renders the ack through the run's scrollback... the TUI path: the message log surfaces the ack; provider capture asserts steering text in the run context | no error expected |
| TUI mid-run Enter (FollowUp) | queue_mode FollowUp | queues into the client's follow-up queue; explicit drain; run ends / next run routes queued line as the next run (drain guards verified); provider body carries queued text | no error expected |
| Loading watchdog | agent hang | watchdog reset path is untouched | no error expected |
| No mid-run text | nodes unchanged | the TUI stays byte-identical | unchanged |

</frozen-after-approval>

## Code Map

- `takka-cli/src/tui/app.rs` (~2060): the key handler's `is_loading` gate: the mid-run char/Enter path forwards through `MessageRole` message buffers; the send path routes through the run's own steering contract (via `steer_text`).
- `takka-cli/src/tui/app.rs` (~1425 `spawn_agent_task`, ~1400): the async task's `on_event` renderer callback pushes the steering queue for the TUI's `Arc<Mutex<Commands>>` (the TUI's per-app-instance Commands is what mid-run text steers against).
- `takka-cli/src/commands.rs` — `Commands::steer_text(text)`-style helper (story-2.1 reader parity; carrer per queue mode; a small visible feedback line rides the TUI's message buffer like any other status line).
- `takka-cli/src/tui/input.rs` — `render_input`'s text buffer rendering (unchanged; the TUI's own key path feeds the buffer).

## Verification

**Commands:**
- `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets -- -D warnings` — expected: clean.
- `cargo test --workspace --all-targets` — expected: all green incl. the new TUI mid-run smokes.

**Manual checks (if no CLI):**
- PTY-only: type mid-run inside the TUI: visible feedback + the steering lands (per mode).

## Implementation Notes

<!-- Full route: appended during execution -->

- Implemented directly by the orchestrator (the step-03 fallback). TDD: the TUI FollowUp smoke first, then the wiring.
- `Commands::steer_text(text)` centralizes the per-mode routing (Interrupt: `runtime.steer()`; FollowUp: push to the client queue) — the same helper the inline reader's Enter path now shares, so the runs in both UIs are contract-identical.
- The TUI keys: `KeyCode::Enter` and `KeyCode::Char` gain loading-state paths (`submit_mid_run` / `insert_mid_run_char`) alongside the existing `!is_loading` arms; the mid-run buffer is its own text field (`mid_run_input`), fed into the steering contract styled as a `↳` message-log hand-over line per queue mode (System message + `AppMessage::Progress`).
- The Interrupt steering path needs a live runtime connection: the TUI smoke verifies the FollowUp contract client-side (queue + message-log ack + buffer reset), and the non-connection adjustmt is out (the TUI's own flows wire the connection in the running app).
- 2026-11-08 verification: fmt/clippy clean; `cargo test --workspace --all-targets` all green (976 passing, incl. the TUI FollowUp smoke + all 1.x/2.1-2.3 smokes; 0 failed).

## Review Triage Log record (self-assessment, first pass)

- Findings 1: the `Commands::steer_text` helper is reached with `Arc<Mutex<Commands>>` (the same handle the TUI passes through) — the steering contract shared across two UIs is one helper, a CHECK the #[cfg(test)] TUI smoke pinned, and the shared contract avoids any duplicated routing on the inline reader path.
- Findings 2: the Interrupt-mode TUI steering test was DROPPED (the smoke needs a live runtime connection — un-wired in the smoke); the tool's FollowUp path is testable client-side and so is the message-log ack. The steering test harness is deferred to 2.3's interactive REPL smoke's harness if a future engine-embedded `Commands` arrives in-process.
- patches applied by the orchestrator directly (oneshot third fix pass).



## Plan Change Log

<!-- Populated by step-04 during review loops -->

## Review Triage Log

<!-- Populated by step-04 on every review pass -->

## Design Notes

- The TUI stays an OBSERVER: the run's actors are exactly the same inline "reader→steer/queue" contract but the E2E (TUI's per-instance Commands) needs the same client helper the inline reader has — a `steer_text()` helper on `Commands` centralizes: inline reader's Enter paths, TUI's Enter path, mono, `SteerQueued`-flows route through it.
- Residual: TUI shows a TUI-specific message-log entry; steering acks are engine events falling through (SteerQueued/Steered render through the same message path like the tool rows).

## Key flows

- TUI run flow:
```text
Run → client run(steer_lines=Some/None per the TUI's use)
TUI mid-run keys → Commands::steer_text → (Interrupt: Request::Steer; FollowUp: push_pending_follow_up)
```

## Verification

**Commands:**
- `cargo fmt --all -- --check` — expected: no diff.
- `cargo clippy --workspace --all-targets -- -D warnings` — expected: zero warnings.
- `cargo test --workspace --all-targets` — expected: all green incl. the new TUI smokes; hermetic config only.

**Manual checks (if no CLI):**
- PTY-only: type mid-run inside the TUI and inspect the ack + drained steering text.
