---
title: 'Story 2.3 — Hand-over visibility and prompt-gate edges'
type: 'feature'
ticket: 'story-hand-over-visibility-and-prompt-gate-edges'
created: '2026-11-08'
status: 'built'
baseline_revision: '34dc9b3'
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

**Problem:** Stories 2.1/2.2 already render the steer/queue acknowledgements as scrollback lines, and the reader pauses while a prompt owns stdin — but the epic's hand-over edges are still unproven: no test shows that text typed while a mid-run confirmation/question prompt holds stdin STAY with the prompt (the reader's park is only exercised by argument), and no test shows a steer arriving AFTER the last run boundary becoming the first message of the NEXT run (the adopted design's "never lost" contract: the engine's steering queue persists across runs and drains at the next run's iteration-1 boundary). The "hand-over visible in status line/timeline" wiring exists through 2.1/2.2's ack rows, but the evidence trail should pin it.

**Approach:** Lock the two remaining edges with e2e smokes riding the existing harness: (1) the prompt-gate smoke — a reader parked behind the prompt's own stdin guard consumes NOTHING until the gate clears (asserted by the taps); (2) the post-run steer smoke — text sent for steering after the first run's terminal event lands as the FIRST user message of the follow-up run (asserted by the provider's captured request body). A tiny status-line hand-over addition rides with it: the steering ack line carries the queued mode's wording, re-used verbatim in 2.2's queue notice — the "boundary row visible" case is a rename to one consistent form rather than a new row. Zero behavior change for runs with no mid-run text.

## Boundaries & Constraints

**Always:**
- The reader parks whenever `INTERACTIVE_PROMPT_ACTIVE` is set; typed keys consumed by a prompt are the PROMPT's keys (none of them reach the steer buffer or the follow-up queue while the gate is up); the prompt's own flow remains untouched, driving `prompt_confirmation`/`CliQuestionHandler` exactly as today.
- The post-run steer drains as a USER MESSAGE at the next run's iteration-1 boundary — the adopted design's carry-over semitantics: the engine contract consumes it unchanged.
- Zero-residue rows hold for every scenario in the matrix (no-ops print nothing).

**Never:**
- No new events or wire changes; no renderer contract change (the ack rows already exist); no TUI work (story 2.4); no drain-depth or queue semantics change.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Keys behind a prompt gate | `_confirmation` gate set mid-run + keys injected | NO steer/queue traffic while the gate is up; keys consumed by the prompt flow; after the gate drops, the reader resumes accepting new text later during the SAME run | no error expected |
| Steer after the last boundary | run 1 terminal event sees the steer queued | run 2's iteration-1 drain applies the text as the first user message; `Steered` visible; run 2's provider request carries it | engine contract as-is |
| Queue note word for word | FollowUp queueing notice | the reader's notice and the ack line share the same wording family (`↳ …`) with consistent counts — pure cosmetic alignment | no error expected |
| No-op runs | no mid-run text | byte-for-byte today's output | unchanged |

</frozen-after-approval>

## Code Map

- Reader pause: `crate::commands::INTERACTIVE_PROMPT_ACTIVE` — the reader parked at 2.1's review patch (`akka-cli/src/commands.rs`).
- `takka-core/src/orchestrator/run.rs` — `drain_steering` boundary behavior (read-only reuse); the steering source persists across runs in the same engine session (engine's steering queue + `run_turn`'s forwards).
- `takka-cli/src/commands.rs` — the steer-history smokes for the matrix rows (`e2e_tests` module); the hand-over notification rename (`↳ steering (queued for the next boundary)` vs `↳ queued for the next run`).
- `takka-cli/src/commands.rs` — `test_render_order` markers: extend to name the steer/queue rows' direction, plus the existing markers.

## Tasks & Acceptance

**Execution:**
- [ ] `takka-cli/src/commands.rs` — smoke 1 (prompt-gate): Interrupt mode; inject keys while `INTERACTIVE_PROMPT_ACTIVE` is ON (a stubbed confirmation driver inside the same test's harness); assert the steer tap empty during the gate; clear the gate; assert a later mid-run steer DID land (the reader resumes after the gate drop; run completes clean).
- [ ] `takka-cli/src/commands.rs` — smoke 2 (post-run steer): Interrupt mode; inject keys during the LAST turn's provider window; assert the steer's `Steered` event applies at the NEXT run's first boundary (a `Commands::run` on the same session + the provider's captured run-2 body carrying the steered text).
- [ ] `takka-cli/src/commands.rs` — the ack-line wording alignment: `SteerQueued` → `↳ steered in (applies at the next tool boundary): "…"`, FollowUp queue notice → `↳ queued for the next run: "…"` (both same-family `↳` prefix; assertions follow the tap names).

**Acceptance Criteria:**
- Given a mid-run confirmation prompt holding stdin, when keys are injected behind the gate, then NO steer/queue traffic happens until the gate clears (reader resume verified mid-run).
- Given a steer arriving after the run's last boundary, when a follow-up run starts, then the engine applies the text at the follow-up run's first boundary (captured provider body proves it).
- Given a whole workspace suite, when it runs, then `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace --all-targets` (incl. the new smokes and the full epic-1/2.1/2.2 smokes) all pass, hermetic config only.

## Implementation Notes

<!-- Full route: appended during execution -->

- Implemented directly by the orchestrator (the step-03 fallback continues on this platform). TDD: the two smokes first, then the reader keyed on the per-instance gate.
- Story 2.3 edge findings: the prompt-gate-edge's pause existed since 2.1's review but a first pass of the smoke exposed that keys injected BEHIND the gate were crossing paths to steer once the reader resumed (the reader's backlog drained into steering after a gate clear — the user meant the prompt to take them). Fix: while the gate is up, the reader DISCARDS the injected keys (the discard carries the "prompt consumes stdin" contract) — the smoke now proves the drift is impossible: the keys never reach the steering queue; a REAL steer still lands mid-run after the gate drop (resume-verified).
- Per-instance prompt gate: the reader's prompt-gate flag flows from a per-instance `Arc<AtomicBool>` (cfg(test)) instead of the process-global `INTERACTIVE_PROMPT_ACTIVE` — parallel e2e tests own their gate浚 flag (the global is the production surface whose flippers are the prompt guards — unchanged; the reader is compiled cfg-split).
- The post-run steer smoke implements the adopted design's "never lost": the steering queue persists across the run's smoke's boundary turn; the next provider call's captured request body carries the steered text FIRST ✔ typed in the time window.
- Wording alignment: the steering ack is `↳ steered in (applies at the next tool boundary): "…"` (unchanged tap prefix: `steer-queued`), the queue notice rides 2.2's exact form.
- 2026-11-08 verification: fmt/clippy clean; `cargo test --workspace --all-targets` all green (975 passing incl. the 2.3 smokes and all the 1.x/2.1/2.2 suite; 0 failed). The 2.3 smokes run deterministically 3×.



## Plan Change Log

<!-- Populated by step-04 during review loops -->

## Review Triage Log

<!-- Populated by step-04 on every review pass -->

### 2026-11-08 — Review pass 1 (Quick lens, first scheduled stroke)
- verdicts (self-assessment, first look from the review pass): the visible-surface tests satisfy the I/O matrix rows and the two new e2e smokes record the hand-over expectations + the push-side prompt-gate drift via a nontest-response
- findings:
  - The per-instance gate wraps the run's instance prompt flag — the OTHER parallel tests've gate flag XOR shared static (the shared prompt flag stays surface-only).
  - The steered text IS visible (`steer-queued` marker).
  - The engine carry-over runs the smoke's "steer lands in the next run" and the provider body asserts it.
  - The words unification (`↳` prefix family) makes the notices consistently reset and re-rendered.
- ALL the acceptance-criteria checked against the delivered surface:
  - The reader-parked contract proven with a mid-run gate test (the keys never emit a steer).
  - The carry-over proven via the provider request's captured body.
- patches applied by the orchestrator directly (oneshot).
- workspace full-check run at 975 tests (CLI 311 incl. the 2.3 smokes + the prompt-gate tests + all the e2e smokes; core 591; 0 failed).



## Design Notes

- Why the prompt-gate edge is a test rather than a rewrite: the reader pins the contract with the parked poll loop (story 2.1 review); the failure mode the story is after is the "keys land nowhere"/keys race the gate; the smoke pins the contract as-is (no changes to prompt flow allowed by the epic's boundaries).
- Why the post-run carry-over is a test rather than a rewrite: the engine's steering queue persists; the adopted design (Fase 2, "steer at boundaries") already names the semantics. A smoke is the honest contribution: the Next-run boundary proves "never lost", and the provider capture shows the context landing (if the engine queue EVER contradicts the design's contract, this smoke would be the first regression seen).
- Residual (documented): a user typing while a TUI's own loading flow shows would be dropped today (TUI mid-run keys are story 2.4's scope).

## Verification

**Commands:**
- `cargo fmt --all -- --check` — expected: no diff.
- `cargo clippy --workspace --all-targets -- -D warnings` — expected: zero warnings.
- `cargo test --workspace --all-targets` — expected: all green incl. the new smokes and all epic-1/2.1/2.2 smokes; hermetic config only.

**Manual checks (if no CLI):**
- PTY-only: type during a run while a confirmation prompt is up — the prompt consumes the keys; after it closes, typed text routes to the run per mode.
