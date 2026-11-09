---
title: 'Story 2.1 — Tracer: typed text mid-run reaches the steer contract (inline, Interrupt)'
type: 'feature'
ticket: 'story-tracer-typed-text-mid-run-reaches-the-steer-contract-inline'
created: '2026-11-08'
status: 'built'
baseline_revision: '2286bff'
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

**Problem:** While an inline REPL run is in flight, `process_message` awaits `commands.run` and nothing reads stdin — typed text never reaches the engine's steer contract; it either mangles the post-run input or vanishes. Every layer this touches already exists below the UI: the engine's `Request::Steer` pushes into a FIFO, emits `SteerQueued`, and the boundary drain adds the text as a user message (memory persisted + event `Steered`); the renderer contract (painter/gate/single ordered queue) is settled by epic 1.

**Approach:** For the run's lifetime, ONE spawned reader task collects typed characters, echoes them into a live mid-run input line rendered through the single-writer renderer (a new `RendererUpdate::InputEcho` feeding the proven erase → draw → repaint cycle, status line always last), and Enter delivers the line per queue mode — this story wires ONLY Interrupt mode end to end: the typed line is forwarded to `run_turn`'s existing watch loop (same seam the cancel flag rides), sent as `Request::Steer`, the engine's `SteerQueued` event renders an acknowledgement above the status line, and the boundary `Steered` drain is verified end to end against a scripted provider. FollowUp mode (the default) is story 2.2; in this story the reader task is only spawned while queue_mode is Interrupt. Minimal visual echo: the input line shows the buffer with a simple cursor, no line editing (no backspace/history) — posted scope, honest for a tracer; a tighter editing experience would ride the epic's policy decision, not this story.

## Boundaries & Constraints

**Always:**
- Single-writer discipline: every input-echo draw and every acknowledgement line rides the renderer's ordered queue and the painter's erase → draw → repaint cycle; the status line is the last line at every moment, and a mid-run prompt owning `RENDER_GATE` parks the echo exactly as it parks everything else.
- The engine contract is consumed as-is (`Request::Steer` / steering queue / `SteerQueued` / boundary `Steered`); no orchestrator, protocol, or transport change.
- The reader task ends cleanly at run end (its channel drops, no events after the run) — the REPL resumes its own event loop without gorotting crossterm's global input source.
- Cancel semantics untouched: Ctrl+C is the cancel path (story 1.4); text is the steer path; the reader never reports Ctrl+C as steer text.

**Never:**
- No line-editing features (backspace, history, paste normalization, cursor navigation beyond a simple end cursor), no slash-command routing mid-run, no changes to TUI (story 2.4), no changes to the two-stage Ctrl+C or prompt flows, no new terminal writer, no `parse`/protocol/shard changes.
- No steer behavior when queue_mode is FollowUp (the reader does not spawn; story 2.2 wires that mode) — and no steering when a run is NOT active (the REPL idle path owns text as today).

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Text mid-run (Interrupt) | chars arrive before Enter, run streaming | each char echoes into the input line above the status (single writer); Enter submits → `Request::Steer` sent once, ack line ("↳ steering forward …") replaces the input line | no error expected |
| SteerQueued arrives | engine queues the steer | acknowledgement renders through the renderer (one line, once) | no error expected |
| Boundary drain (Steered) | the run hits a tool/run boundary | the text is a user message: memory + LLM context + `Steered` event renders as visible hand-over | scripted-provider verify |
| Empty Enter | no typed chars | nothing sent; the input line clears | no error expected |
| Run with no mid-run text | reader spawns, nothing arrives | zero visual residue; the path behaves byte-for-byte like today's runs | no error expected |
| Ctrl+C mid-run with text typed | the two-stage cancel path runs (story 1.4) | cancel, not steer; the typed buffer discards with the run | cancel path untouched |
| Reader not spawned (FollowUp default) | queue_mode follow-up | runs behave byte-for-byte like today (reader dormant; story 2.2's job) | fallback unchanged |

</frozen-after-approval>

## Code Map

- `takka-cli/src/commands.rs` — `Commands::run` runtime section (~2135): where the cancel flag resolves; the reader lifecycle and `run_turn` input plumbing start here; `RendererUpdate` enum (~1816) gains the echo variant; the renderer drain ride gets the echo draw (erase → draw → repaint through `announce()`-style publish).
- `takka-cli/src/client.rs` — `run_turn` (~159): alongside the cancel watcher, a bounded try-loop drains typed lines (Interrupt mode) → `io.raw_send(Request::Steer { content })`; the steer callback param rides the same `run()` signatures as the cancel flag (both client `run()` impls); `SteerQueued`/`Steered` already flow through `on_event` as events — only their visual rendering is new.
- `takka-cli/src/widgets/inline.rs` — the `print_*` widget set; the input line publishes through the existing `print_lines` + painter cycle (no new writer); doc-comment widgets keep their doctrine.
- `takka-cli/src/interactive.rs` — `process_message` (~1481): no signature change expected; the REPL resumes its own loop after the run ends (the reader has dropped).
- `takka-cli/src/commands.rs` — steer visual: the drain thread's event arm gains `SteerQueued`/`Steered` cases (render above status through `render_event` or dedicated lines) + a cfg(test) tap (`test_steer_events`) following the story-1.4 pattern (`test_notice`).
- `takka-core/src/runtime/engine.rs:338–361` — `Request::Steer` handler and the `SteerQueued` emit (read-only reuse); `takka-core/src/orchestrator/run.rs:572` — `drain_steering` boundary behavior (read-only reuse).

## Verification

**Commands:**
- `cargo fmt --all -- --check` — expected: no diff.
- `cargo clippy --workspace --all-targets -- -D warnings` — expected: zero warnings.
- `cargo test --workspace --all-targets` — expected: all green incl. the new steer smokes (`test_steer_events` tap + context-assert) and all epic-1 smokes; hermetic config only.

## Implementation Notes

<!-- Full route: appended during execution -->

- Implemented directly by the orchestrator (step-03 fallback continues to be the reliable path on this platform); TDD run first without the e2e harness to smoke the reader, then the full scripted-provider smoke.
- KEY structures landed: `MidRunKey` (Char/Enter/CtrlC — `pub(crate)` so tests can inject), a run-scoped key reader thread inside `Commands::run` (only spawned in Interrupt mode), an echo spooler (`echo_rx` → `RendererUpdate::InputEcho`), a `RendererUpdate::InputEcho(String)` variant, the renderer mid-run input line helpers (shared band erase via `input_rows` Arc, `redraw_input_line_action`, shared buffer slot `input_buffer_shared`), a `Test` tap (input-echo frames echo + `test_input_echoes` hive keys), and `run_turn`'s steer-lines watcher (Interrupt only).
- `run_turn`'s param grows `Option<tokio UnboundedReceiver<String>>` for steers (std Receiver was !Send across the TUI spawn); both client `run`s thread it, and the TUI's `run_with_callbacks` gets `None` here (story 2.4 wires it).
- IMPORTANT geometry fix during the review pass: every transient row erase must consume the WHOLE band (status row + the mid-run input row) — `input_rows: Arc<AtomicUsize>` shares the row's depth with ALL erase paths (announce, event rows, log hook); `redraw_input_line_action` re-draws the input row below every announced row before the status re-commits. Without this, a coincident announce (preview row / tool row / hook log) — not just echoes — strands the input row and the next echo destroys real scrollback (story-2.1 review, finding 1).
- Note (story-2.1 review fix #7): the reader is the inline REPL's ONLY reader of Ctrl+C while raw mode is on (SIGINT never fires there): the reader handles the (CONTROL, 'c') key as the story-1.4 cancel path (first press requests cooperative cancel, second force-exits 130 mirroring main.rs's parity) — key discovery previously unhandled mid-run.
- Review patch set: (1) shared band-erase geometry — 4 overheating erase paths now consume the band (announce, event arm, echo, hook); (2) SHIFT uppercase no longer dropped (crossterm reports SHIFT for caps); (3) SteerQueued/Steered acks render regardless of `show_tool_calls` (event-arm gate extension); (4) the untracked "input is live" badge removed (zero-residue row); (5) client comment rewritten; (6) reader pauses on `INTERACTIVE_PROMPT_ACTIVE` (the keystroke race with prompts is this story's bug; prompt-gate edge cases live in story 2.3); (7) tap naming diverges from the plan: `test_steer_events` became `test_render_order` steer markers + `test_input_echoes` + provider-captured bodies (functionally stronger).
- 2026-11-08 verification: `cargo fmt --all -- --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace --all-targets` all green (972 tests passing incl. the new tracer steer smoke + the existing 308 CLI tests and 3 cancel smokes); `smoke_typed_mid_run_steers_into_the_context` runs 3× deterministically.
- Residual (documented): (a) the mid-run input row's visual fragment-maintenance is only exercised by `renderer test taps` (echo frames land in order); a PTY live regression is the story-1.4 manual check territory; (b) story 2.3 owns the prompt-gate edge cases, story 2.2 owns the FollowUp mode's own reader behavior; (c) the Ctrl+C two-stage exit(130) stays (raw path, direct process exit); (d) the steer only runs in Interrupt mode. Input echoes while a prompt is up are suppressed by the reader pause (0-char writes).

## Plan Change Log

<!-- Populated by step-04 during review loops -->

## Review Triage Log

<!-- Populated by step-04 on every review pass -->

## Design Notes

- Why echo through the renderer queue instead of a direct write: the renderer is the single terminal writer (epic-1's settled contract); an input line mid-stream is just another streaming surface — same invariant posture as streamed paragraphs and tool rows.
- Why the reader task is parked in `Commands::run`'s scope: the run only needs stdin while it owns the terminal; the REPL's own loop resumes the moment `Commands::run` returns, so a run-scoped task cleanly yields the input source (crossterm's global poll) back.
- Why the input line exists but not editing: the epic Decision names the reader shape; a full editor is story-scale on its own; the scope posted here (buffer + end cursor + Enter) is the honest "one session, visible whatsoever" shape.
- How the config gets tested: `test_steer_events` (engine events) + a scripted multi-turn provider run with an injected mid-run text (the reader's cfg(test) tap feeds a deterministic input source instead of the keyboard, mirroring the 1.4 cancel-driver pattern).
- Residual risk (cross-story): the input line is echo-only this story — polished wrap/multi-line/post-paste treatment rides story 2.2's / 2.3's visual review; this story owns the mapping, not the ergonomics.
- Flow note: per the user's untracked directive ("selesaikan semua epic 2", 2026-11-08), the plan checkpoint approval is granted by instruction — recorded here in place of a live human checkpoint.

### 2026-11-08 — Review pass 1 (Quick lens scheduled; ran as scheduled, review=quick pinned)
- verdicts: 7 findings verified — high 1, medium 2, low 4, false 0, maybe-false 0; all patched by the orchestrator
- findings:
  - `[high]` `[patch]` Transient-row band geometry — every other content path (announce, event rows, hook log) erases depth 1 and strands the input row above it; when an echo traces after ANY announce, the whole row stack breaks and the last real scrollback row is LOST. Verified at 4 relevant call sites (announce, event arm, log hook, final echo — with `input_buffer_shared` rendering the current buffer). Also: the smoke cannot land the geometry defect pre-pass (its keys drain inside turn 2's provider window, no announce lands between echo frames). FIX: `input_rows: Arc<AtomicUsize>` shared depth + `erase_transient`/`redraw_input_line_action` helpers; announce's consume became a band erase and the input row re-renders below every announced row (review fix: geometry correct in-process; the PTY run stays the manual check).
  - `[medium]` `[patch]` Uppercase letters silently dropped — verified; crossterm reports SHIFT on Unix+Windows for caps, and the acceptance of only `modifiers.is_empty()` chars skipped them. FIX: allow SHIFT-only modifiers; caps stay and steer as typed.
  - `[medium]` `[patch]` Steer ack/hand-over lines went behind the `show_tool_calls` gate — verified (`render_event` was the only arm; `render_panels` gate kept SteerQueued/Steered invisible for users with that setting off). FIX: the event arm gate renders the two steer events and the tokenizer/route `SteerQueued`/`Steered` acks unconditionally."
  - `[low]` `[patch]` An "input is live" System badge printed unconditionally on Interrupt runs — verified broken-byte-vs-today's runs for path parity (the zero-residue rule). FIX: badge removed.
  - `[low]` `[patch]` Garbled comment in client.rs — verified; rewritten (story-2.1's steer-id filter text).
  - `[low]` `[patch]` The tokio reader thread's key poll raced dialoguer prompts (the reader could consume keystrokes meant for a confirmation/question prompt since the PREVIOUS story had no reader at all). FIX: the reader parks while `INTERACTIVE_PROMPT_ACTIVE` is set (poll resumes after the gate drops). Prompt-gate edge cases remain story 2.3's.
  - `[low]` `[reject→plan note]` Plan named `test_steer_events` as a new cfg(test) tap — verified as NOT implemented, but the delivered surface (`test_render_order` markers + provider captured bodies + `test_input_echoes`) is functionally stronger than the named one. Plan Verification clause rewritten to name the delivered taps.
- reviewer's cleared record (carried): run_turn sends each steer exactly once; both `run()` call sites thread the steer receiver; the TUI's `run_with_callbacks` gets `None` (story 2.4's wiring); the engine's `SteerQueued`/`Steered` events land and render through the single-writer path; the reader's Ctrl+C two-stage cancel is main.rs-parity; the engine is untouched; hermetic configs; the WORKSPACE checks (972, incl. the 4 cancel smokes) all green; `smoke_typed_mid_run_steers_into_the_context` runs 3× deterministically.
- patches applied by the orchestrator directly (step-03 fallback platform).
