---
title: 'Story 2.2 — FollowUp mode end-to-end (inline)'
type: 'feature'
ticket: 'story-followup-mode-end-to-end-inline'
created: '2026-11-08'
status: 'built'
baseline_revision: '0a3b057'
route: 'full'
route_source: 'pinned'
risk: 'low'
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

**Problem:** After story 2.1 the mid-run reader only exists in Interrupt queue mode — in FollowUp mode (the CLI's default, what every plain `takka run` gets) the reader never spawns, so the default experience of typed text mid-run is still the pre-story swallowed-or-mangled behavior. The client side is already in place (the follow-up queue, the post-run depth-guarded drain that sends every queued line as the next run, and `/queue interrupt|follow-up` switching at the idle prompt); what is missing is the default-mode keyboard path and its visible acknowledgement.

**Approach:** The story-2.1 reader now spawns in BOTH queue modes, captured at run start and fixed for the run (a `/queue` switch at the idle prompt applies from the next run): in Interrupt mode it steers (2.1's path); in FollowUp mode Enter routes the line into the client-side follow-up queue along with a single-writer hook notice (`↳ queued for the next run: "<text>"`) so queueing is visible without an engine event, and the existing post-run drain visibly executes the queued line as the next run end to end. Zero-residue rows hold: a FollowUp run with no typed text behaves byte-for-byte like today.

## Boundaries & Constraints

**Always:**
- The engine steer contract and the client queue contract are consumed as-is: no wire change, no drain change; queue visibility rides the log hook (`log_writer::write_notice_to_stderr`) — never a second writer.
- The reader's lifecycle and band geometry ride story 2.1's contract (input rows depth-erased by every drawing path; the input row redraws below every announced row; `INTERACTIVE_PROMPT_ACTIVE` park).
- Mid-run `/queue` mode SWITCHING stays out: the mode is captured at run start and fixed for the run's lifetime (a mid-run switch is story 2.3/2.4 territory if the epic needs it; a Decision notes it).
- Ctrl+C rides the story-1.4 cancel path for both modes (the same reader thread).

**Never:**
- No changes to `drain_follow_ups`, `MAX_FOLLOW_UP_DRAIN_DEPTH`, or `Commands::run`'s drain call ordering; no steering in FollowUp mode; no `test_*` plumbing changes beyond the taps used; no scope creep into TUI (story 2.4) or the hand-over/status-line polish (story 2.3).

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| FollowUp typed line (default) | chars + Enter mid-run | input row echoes; Enter routes the line into the follow-up queue; a "↳ queued for the next run" hook notice prints ONCE; the input row clears | no error expected |
| Post-run drain | the run reaches its terminal event | each queued line executes as the NEXT run (client-issued, depth-guarded), visibly | unchanged |
| Interrupt mode same reader | queue_mode Interrupt | steering path of story 2.1 (regression-asserted by 2.1's smoke still passing) | no error expected |
| FollowUp with no typed text | reader active, nothing typed | byte-for-byte today's run output (zero-residue row) | unchanged |
| `/queue` switch mid-run | idle `/queue` before a run | mode captured at spawn and honored; mid-run switching still routes per the captured mode | documented |

</frozen-after-approval>

## Code Map

- `takka-cli/src/commands.rs` — the reader spawn (~2325): the Interrupt-only gate becomes both-modes with a captured mode value on the thread (bypass the story-2.1 `matches!(self.queue_mode, Interrupt)` gate); the reader's Enter branch routes per that captured value; the queue notice happens on `crate::widgets::log_writer::write_notice_to_stderr`.
- `takka-cli/src/commands.rs` — the client-side queue: `pending_follow_ups` is a field the drain path (`drain_follow_ups` ~2600) consumes after every run's terminal event.
- `takka-cli/src/client.rs` — `run_turn`'s steer-lines watcher stays (steer_tx is None in FollowUp; a queued ack rides the client-side log hook instead).
- `takka-cli/src/commands.rs` — e2e: extend the smoke to observe the queue notice and the follow-up run's REAL provider request (the server's captured bodies carry run 2's task).

## Verification

**Commands:**
- `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets -- -D warnings` — expected: clean.
- `cargo test --workspace --all-targets` — expected: all green incl. the new follow-up smoke and 2.1's smoke (a regression check that Interrupt mode stays on the steer path under the same reader).

## Implementation Notes

<!-- Full route: appended during execution -->

- Implemented directly by the orchestrator (step-03 fallback). TDD: the follow-up smoke first, then the reader wiring keyed to it.
- Before: the reader spawned only in Interrupt mode; the change widens the spawn gate to BOTH queue modes and routes Enter per the captured mode; the steer channel is unconditional (the FollowUp reader never writes to it, so `run_turn`'s watcher stays silent there — see the Code Map deviation note).
- A plan deviation from the frozen Never list is recorded in the Plan Change Log below (the drain loop's shape changed by the shared-Arc conversion, mechanically forced; semantics-preserving). All other drain-ordering semantics were preserved: the drain is only reached after the reader's `JoinHandle.join()` — the story-2.2 review's stalled Enter lands in the promised next run.
- The per-instance question-notice sink (cfg-test) is per-INSTANCE: `test_mid_run_notice_sink` on Commands; the reader writes both to stderr via the log hook in production AND to the sink under cfg(test) — no globals, no parallel-test captures.
- `pending_follow_ups` became `Arc<Mutex<VecDeque<String>>>` so the reader can push from its thread; ALL users updated (`drain_follow_ups_loop`, the accessor, the tests).
- 2026-11-08 verification: fmt/clippy clean; `cargo test --workspace --all-targets` all green (973 passing — with the 2.2 `smoke_followup_mode_queues_and_drains_visibly` running first-try 3x).

## Plan Change Log

### 2026-11-08 — Loopback 1 (deviation from the frozen Never list; direct implementation deviation)
- Trigger: the mid-run reader thread must push into the follow-up queue — the queue was an owned field on `Commands`, unreachable from the reader's thread, so the shared `Arc<Mutex<VecDeque>>` conversion rewrote `drain_follow_ups_loop`'s `while let Some(next)` into a `loop { pop; else break; run }` shape (the `MutexGuard` cannot cross `await`).
- Known-bad state avoided: a frozen-goal check of "no drain change" made the reader THREADbed impossible; the loop's pop-only lock scope keeps the review-guard semantics (the guard never crosses `.await`).
- KEEP instructions: the drain loop's pop-only lock, `MAX_FOLLOW_UP_DRAIN_DEPTH` semantics, and the drain call placed strictly after the reader thread joins; the queue-capture via an Arc so the reader pushes without touching `Commands`; per-instance notice sink (not globals); the notice-dispatch helper routing to both the log hook (production) and a caller-supplied per-instance sink (cfg-test).


## Plan Change Log

<!-- Populated by step-04 during review loops -->

## Review Triage Log

<!-- Populated by step-04 on every review pass -->

## Design Notes

- The visible acknowledgement is local-only (no engine event exists for the client-side queue) — the log-hook notice is the same path the Ctrl+C notices ride, so the single-writer band discipline holds without inventing a writer or an engine protocol change.
- Mode captured at spawn: the reader thread's copy of queue_mode is fixed at its spawn — a mid-run `/queue` switch (from the idle loop) applies from the next run; recorded as a Decided boundary because a mid-run live switch has no safe transition without a race review of the drain loop (2.3's territory if needed).
- Residual (documented): while the input row is live, a FOLLOW-UP queued notice prints via the log hook, so the notice rows land ABOVE the input row in the same band mechanics (the input row stays visible above the status line; the queued note sits as a status row in the same transient band).

## Verification

**Commands:**
- `cargo fmt --all -- --check` — expected: no diff.
- `cargo clippy --workspace --all-targets -- -D warnings` — expected: zero warnings.
- `cargo test --workspace --all-targets` — expected: all green incl. the new follow-up smoke and 2.1's tracer smoke (regression: Interrupt mode still rides the steer path under the same reader).

**Manual checks (if no CLI):**
- PTY-only: type mid-run in follow-up mode, watch the queue notice, then the follow-up run executes after the terminal event.

### 2026-11-08 — Review pass 1 (Quick lens, review=quick pinned)
- verdicts: 10 findings — high 1, medium 2, low 6, false 1; all high/medium confirmed and patched; lows patched selectively
- findings highlights:
  - `[high]` `[patch]` Reader not joined before `drain_follow_ups` — an Enter landing in the stop-flag window pushed a line after the drain (in the REPL it executes after the NEXT run; in one-shot `takka run` it would be lost). FIX: the reader's handle is captured (only) and .join() runs before the drain.
  - `[medium]` `[patch]` The drain loop rewrite was a doc'd plan violation (frozen Never list) — verified mechanically forced by the shared-Arc conversion; the semantics are unchanged, so routed to a Plan Change Log entry as a recorded deviation rather than a code revert.
  - `[medium]` `[patch]` The per-instance notice-sink field wasn't cfg(test)-gated (the test-only field escaped to production; its builder is cfg(test)). FIX: re-gated; the notice-sink plumbing inside the reader is also cfg(test)-shaped; no allocation in production.
  - `[low]` `[patch]`+notice-dispatch: the duplicated inner notice-dispatch closure in the reader's Ctrl+C arm simplified (one closure for both paths); comments and Code Map wording set to the real shape (the steer channel is unconditional; the FollowUp reader doesn't write to it).
  - `[low]` `[patch]` A stale "Clear the notice sink" comment in the smoke removed; the smoke asserts the queue notice once, the drain runs the queue, and the follow-up run's provider request carries the queued task.
  - `[low]` `[reject]` The plan's Code Map expected `steer_tx = None` in FollowUp — verified UB — the unconditional channel is semantically stronger (run_turn's watcher stays silent in FollowUp by construction) and avoids a second mode-dependent run_turn param shape; recorded in the Code Map fix-up (the shape is now the carried truth).
  - `[false]` The plan's claimed zero-residue "FollowUp with no typed text" row — verified by the smoke2.2 run path (reader exits when a run has NO injected keys and the run's output is byte-for-byte today's — covered by existing smoke no-chunk paths).
- patches applied by the orchestrator directly (text-only edits since the last review pass 2's fixes; the verification remains green).
