---
title: 'Story 1.4 — Finish and mid-stream cancel cleanup'
type: 'bugfix'
ticket: 'story-finish-and-mid-stream-cancel-cleanup'
created: '2026-11-06'
status: 'built'
baseline_revision: 'f6436589710dab3bc9f13817466278fa09a1b2c8'
route: 'full'
route_source: 'pinned'
risk: 'medium'
review: ''
review_source: ''
lenses_ran: []
review_loop_iteration: 0
followup_review_recommended: false
context:
  - '_bmad-output/initiative-repl-ux-hardening/tickets/epic-streaming-preview/epic-streaming-preview.md'
warnings: []
deferred: []
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** The first Ctrl+C is a ghost: `main.rs` flips a shared cancel flag that nothing reads, so a run keeps going after "Cancel requested"; only the second Ctrl+C hard-exits — killing the process mid-render and leaving the transient status line and half-drawn bands in scrollback. The Ctrl+C warnings themselves are raw `eprintln!`s that land on the status line's row. Story 1.1's review already established the enablers exist below the CLI: the protocol has `Request::Cancel`, the engine routes it into `orchestrator.cancel_handle()`, and the loop returns `AgenticError::Cancelled` at the next turn boundary — the run's Err path already erases the status line exactly once.

**Approach:** Wire the cooperative cancel at the one shared choke point — `run_turn` in `takka-cli/src/client.rs` (all runtime modes, inline REPL and TUI, fold every event through it): watch the process cancel flag between events and send `Request::Cancel` once when it flips; the run then unwinds through the existing `Err` path, whose cleanup (status line erased exactly once, preview lines left as ordinary scrollback) becomes the story's guarantee. Render the cancel the way it reads — "Cancelled by user" surfaces as a cancel notice, not the red error badge — and route the main.rs Ctrl+C warnings through the spinner-aware log-writer hook so they never stomp the status line (scope decisions recorded 2026-11-06, human answers: the cancel notice replaces the red error badge for "Cancelled by user"; the full single plan is kept over a split; the cancel wiring applies to the TUI path too, since both UIs share `run_turn`). Natural finish is already clean and stays byte-for-byte.

## Boundaries & Constraints

**Always:**
- The single-writer gate discipline: any stderr write from the signal handler takes the log-writer hook path (erase → write → repaint under `RENDER_GATE.try_lock()`; plain write as the existing fallback when a prompt holds the gate).
- The run's terminal cleanup erases the status line exactly once in BOTH paths (natural finish and cancel) and every preview line ever printed stands after the unwind as ordinary scrollback.
- Two-stage Ctrl+C semantics preserved: first Ctrl+C = the cooperative cancel request (now operative), second = force-exit 130, unchanged.
- Everything cancels through the existing engine contract: `Request::Cancel` → `cancel_handle` → `AgenticError::Cancelled` — no orchestrator/protocol/transport change.

**Never:**
- No changes to the engine wire/protocol, the daemon, providers, the TUI rendering design, the confirmation/question prompt flow, the painter, or stories 1.1–1.3's flush/paste-bookkeeping machinery.
- No new terminal writer; no raw eprintln that can race the transient line while a run is active.
- No cancel of the confirmation decision itself — pressing Ctrl+C while a prompt is up still waits for the user's answer; the cancel takes effect at the next boundary.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Cancel mid-preview | Ctrl+C flag flips while streamed paragraphs are already printed | the run returns gracefully; preview lines stand; status line erased exactly once; a cancel notice (not the red error badge); no re-render of the answer | no error expected |
| Cancel during thinking | flag flips before the first delta | clean unwind; no live lines; the notice prints once | no error expected |
| Cancel during tool execution | flag flips while a tool runs | the tool finishes or the boundary hits; notice prints; tool row + preview lines stand | engine unwinds at the next boundary |
| Second Ctrl+C | the flag is already flipped | force-exit 130 (existing handler), unverifiable in-process | unchanged |
| Natural finish | the run completes normally | byte-for-byte today's cleanup (single erase, suffix print or full render) | unchanged |
| Delayed boundary hit | the flag flips but the orchestrator is mid-LLM-call | the cancel takes effect at the next turn boundary; deltas may continue briefly; the unwind stays clean | latency is documented, bounded by the boundary |

</frozen-after-approval>

## Code Map

- `takka-cli/src/client.rs` — `run_turn` (~159): the single loop every mode and UI shares; add the cancel watcher here (check `crate::cancel_flag()` between events, `io.raw_send(Request::Cancel)` exactly once when it flips). `raw_send` is the existing one-line send used by `Request::Run`.
- `takka-cli/src/main.rs` — the Ctrl+C task (~163): its two `eprintln!` warnings reroute through the log-writer hook; the flag, the two-stage structure, and `process::exit(130)` stay.
- `takka-cli/src/widgets/log_writer.rs` — global hook slot + `write_line_to_stderr`: add a small spinner-aware helper that mirrors the console-log hook's protocol (try-lock the gate, erase, write, re-commit) for the signal task; the guard is already installed during runs.
- `takka-cli/src/commands.rs` — `Commands::run` Err branch (~2395 area, "Cancelled by user" match) → the cancel notice; add a cfg(test) tap (`test_notice`) capturing which terminal badge printed, beside the existing `test_preview`/`test_final_print`/`test_render_order`.
- `takka-core/src/lib.rs:151` — `AgenticError::Cancelled` displays "Cancelled by user" (the string the CLI matches); `takka-core/src/runtime/engine.rs:338` — `Request::Cancel` → `cancel_handle().store(true)` (read-only reuse); `takka-core/src/orchestrator/run.rs:124,377` — the boundary checks (read-only reuse).
- Untouched reference surfaces: `command s.rs` renderer drain + `PreviewTracker` + `end_of_run_*` (stories 1.1–1.3), the StatusPainter, the confirmation/question handler (boundary), TUI panel code.

## Tasks & Acceptance

**Execution:**
- [ ] `takka-cli/src/client.rs` — `run_turn` gains the cancel watcher: on each loop pass (cheap flag load) when the process flag is set it sends `Request::Cancel` once (a local `sent` bool); both `RuntimeClient` and `InProcessClient` call sites pass the shared flag (or `run_turn` reads `crate::cancel_flag()` directly — choose the form that keeps the file dependency-clean).
- [ ] `takka-cli/src/main.rs` — the Ctrl+C task routes its two warnings through the new spinner-aware log-writer helper; flag/two-stage/exit code unchanged.
- [ ] `takka-cli/src/widgets/log_writer.rs` — spinner-aware notice writer mirroring the console-log hook's protocol (gate try_lock + erase → write → repaint; plain write on contention).
- [ ] `takka-cli/src/commands.rs` — the Err branch distinguishes "Cancelled by user" (value of `AgenticError::Cancelled`'s display string) as the cancel notice badge; cfg(test) `test_notice` tap captures the printed notice kind.
- [ ] `takka-cli/src/commands.rs` — e2e smokes for the matrix rows: (1) cancel mid-preview (drive `crate::cancel_flag()` from the test after the first preview line lands via the `test_preview` tap; expect Err, preview intact, empty final print, notice — not the error badge), (2) cancel during thinking (flag set before the first delta arrives; the notice, no live lines), (3) cancel during tool execution (flag set after the first `tool` marker in `test_render_order`); all on the scripted SSE harness, hermetic config.
- [ ] `takka-cli/src/commands.rs` — unit test for the notice decision (cancelled → notice; other errors → red badge).

**Acceptance Criteria:**
- Given a scripted streaming run, when the process cancel flag flips mid-run at each of the three phases, then the run returns gracefully with the status line erased exactly once and already-printed text standing in scrollback (asserted through the taps).
- Given the natural finish, then today's behavior is unchanged (single erase, existing taps).
- Given the second Ctrl+C, then the force-exit path is untouched (main.rs diff only reroutes the warnings).
- Given the whole workspace suite, `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace --all-targets` (incl. the e2e smoke suite, hermetic config only) all pass.

## Implementation Notes

<!-- Full route: appended during execution -->

- Implemented by the step-03 subagent (the dispatch survived this time; it died after the code was written but before its report — the orchestrator finished the verification + several fix rounds directly).
- `run_turn` takes the cancel flag as a parameter (`Arc<AtomicBool>`); `Commands::run` resolves it via `run_cancel_flag()` — the process-global under normal operation, a test-injected instance under cfg(test) (`with_cancel_flag`) so concurrent tests don't cancel each other.
- The watcher sends `Request::Cancel` exactly once; engine's existing handler stores the orchestrator's cancel handle; core honors it at the iteration boundary. Boundary-latency contract: a single-turn text-only run has no further iteration boundary, so a mid-stream cancel there completes normally (the answer is already arriving) — documented, asserted by the tool-phase smoke's deterministic landing (the flag flips INSIDE turn 2's provider call via the 2nd `ModelRequest`, and turn 2 carries a tool call so the iteration-3 boundary exists).
- `"Cancelled by user"` (the `AgenticError::Cancelled` display string) maps through `run_outcome_badge` to the info badge; any other error keeps `error_badge`. cfg(test) `test_notice` tap records the badge kind per run.
- The signal task's warnings now go through `log_writer::write_notice_to_stderr` — routed THROUGH the installed spinner hook (the erase → write → repaint protocol stays owned by the hook; plain write without a live spinner or when a prompt holds the gate).
- Test-tooling hardening along the way: `wait_for_tap_event_count` (await the Nth tap occurrence; deterministic cancel landing), the cancel smokes pin the Err-free surface via the notice (Commands::run always returns Ok — unwinds surface as badges; review-fixed), and three smokes cover the 3-phase matrix rows plus stable reruns (3× green).
- 2026-11-06 verification (after the fix rounds): `cargo fmt --all -- --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace --all-targets` all green (CLI 306 incl. the three cancel smokes + hook unit test; core 577; no FAILED).

## Plan Change Log

<!-- Populated by step-04 review loops -->

## Review Triage Log

<!-- Populated by step-04 on every review pass -->

## Design Notes

- Why wire through `run_turn` and not `main.rs`: the engine-side handlers (`Request::Cancel`, `cancel_handle`, `AgenticError::Cancelled` at boundaries) all exist — the missing piece is exactly one CLI consumer of the flag. `run_turn` is the busiest reader (one pass per event) so the watcher adds no thread and no poller; latency on thinking/tool phases is bounded by the next event arrival + core boundary, which is the honest contract the matrix documents.
- Why a notice instead of the red error badge: "Cancelled by user" is a desired outcome, not a failure; the match keys on `AgenticError::Cancelled`'s display string — fragile in theory (core could reword it) but the string is a `thiserror` literal and the CLI side documents the coupling. The engine/protocol stays untouched, which is the harder boundary.
- The signal-task warnings must not free-write stdout: they land while the renderer owns the last line; the log-writer hook path keeps the erase → write → repaint cycle and its existing try-lock fallback for prompt-held gates.
- Reuse, do not re-decide: the run's terminal cleanup already erases exactly once (story 1.1's end-of-run block, shared by Ok and Err); the cancel path only needs to REACH it, not re-render it.

## Verification

**Commands:**
- `cargo fmt --all -- --check` — expected: no diff.
- `cargo clippy --workspace --all-targets -- -D warnings` — expected: zero warnings.
- `cargo test --workspace --all-targets` — expected: all green incl. the three new cancel smokes and all story-1.1/1.2/1.3 smokes; hermetic config only (no ambient `~/.config/takka/config.json`).

**Manual checks (if no CLI):**
- PTY-only: `takka run` a scripted task, Ctrl+C during thinking/preview/tool — clean scrollback, notice, exit dominated by the next boundary; second Ctrl+C force-exits.

### 2026-11-06 — Review pass 1 (Quick lens, review=quick pinned)
- verdicts: 7 findings verified — high 1, medium 1, low 5, false 0, maybe-false 0; all patched by the orchestrator
- findings:
  - `[high]` `[patch]` The process-global cancel flag is never reset — verified: only the signal task writes `swap(true)`; after any first Ctrl+C, every later run in the same process (the long-lived inline REPL) insta-cancels on its first event pass, and the next press force-exits 130 mid-render — reintroducing exactly the mid-render kill the story set out to fix, and violating the two-stage Always constraint across runs. PATCH APPLIED: `Commands::run` and `run_with_callbacks` reset the shared flag at run start; the per-run two-stage semantics restored and pinned by a new e2e (`smoke_cancel_then_next_run_starts_fresh`: cancel run 1, run 2 on the same Commands completes normally with an empty badge list).
  - `[medium]` `[patch]` The TUI still rendered a cancelled run as `Error: Cancelled by user` — verified: `run_with_callbacks` threads the flag through `run_turn` (shared wiring per scope decision) but the notice-vs-error distinction only lived in the inline Err branch; tui/app.rs maps run errors to an Error message unconditionally. PATCH APPLIED: tui/app.rs maps "Cancelled by user" to a System "Run cancelled" message (same decision as the inline notice).
  - `[low]` `[patch]` `wait_for_tap_event_count` could fail spuriously after the condition was met — verified: the deadline assert preceded the satisfied-condition check, so a starved poll that woke late with `saw >= count` failed. PATCH APPLIED: satisfied-condition check now precedes the deadline assert.
  - `[low]` `[patch]` Stale comments describing the never-operative pre-story mechanism — verified in interactive.rs (~1502) and main.rs's CANCEL_FLAG doc. PATCH APPLIED: both comments now describe the `run_turn` → `Request::Cancel` → cancel-handle forwarding.
  - `[low]` `[patch]` `run_outcome_badge(&e)` computed twice in the Err branch — verified. PATCH APPLIED: bound once; the rendered badge and the cfg(test) tap can no longer diverge.
  - `[low]` `[patch]` AC wording "status line erased exactly once … asserted through the taps" had no counting test — verified: the shared cleanup block guarantees it structurally but nothing counted. PATCH APPLIED: cfg(test) `test_terminal_cleanup_count` incremented at the cleanup block; the three cancel smokes assert exactly 1.
  - `[low]` `[patch]` (found in self-verification after the patches) the log-writer hook tests race on the process-global hook slot once the notice test joined — PATCH APPLIED: a test-local mutex serializes the two hook-driven tests.
- reviewer's record of cleared items (carried as-is): watcher sends Cancel exactly once; both run() call sites thread the flag; main.rs warnings reroute with the hook fallback; the "Cancelled by user" string match is sound across all transports (thiserror literal passes through `Event::Error`); prompt-held gates don't cancel decisions; hermetic configs; natural finish byte-identical; no engine/protocol/daemon file touched.
- patches applied by the orchestrator (the step-03 implementer died after writing the code but before its report; the orchestrator finished verification and the fix rounds directly, per the oneshot fallback in step-04).
