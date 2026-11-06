---
title: 'Story 1.3 — Multi-block streaming and tool-row interleave'
type: 'feature'
ticket: 'story-multi-block-streaming-and-tool-row-interleave'
created: '2026-11-06'
status: 'in-review'
baseline_revision: 'df729f878ad6e8c4349f2b1e276bced231321ce6'
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

**Problem:** The inline REPL renders streamed text and tool/thinking events from two separate channels drained chunks-first-every-tick, so when streamed answer deltas and a tool event arrive together, the tool row lands below the text that arrived after it — the same-tick arrival-order inversion story 1.1 flagged as out of scope. Beyond rendering order, a multi-turn run (text → tool → text → tool → text) also double-prints its last streamed block: the preview announces every block (`block_text` = T1+T2+T3) while the end-of-run path renders only `final_result` (which carries just the final turn's text, T3), re-printing it after it was already previewed. And with `show_thoughts` on (the default), each tool-call turn's `Thinking` event re-prints text the preview has just announced.

**Approach:** Unify the renderer's two channels into one ordered queue (`RendererUpdate::Chunk | Event`), driven in exact arrival order by the single-writer loop: chunks accumulate in `block_text`, the preview flushes its completed text BEFORE the next tool/thinking row renders (preserving arrival order through the same atom), and a status line is never anything but last. Make the end-of-run answer decision multi-turn aware: when the committed preview strictly precedes the parse of the full streamed text and `final_result` is a pure suffix of it, the suffix beyond the committed lines is the end-of-run print (from the streamed text's own parse); today's single-turn decision and the documented fallbacks stay for divergence and for a final result that escaped the stream. Thinking: each tool-call turn's Thinking event is SUPPRESSED when its content is already fully previewed (the previewed lines are the answer — design decision A); it renders as today whenever it is not fully previewed or the preview has diverged (decisions recorded 2026-11-06, human answers: suppress only when fully previewed; keep the full single plan despite its length).

## Boundaries & Constraints

**Always:**
- The single-writer render gate and status-painter protocol are untouched: every preview flush is consume → print → commit, every tool/thinking row is consume → render → commit, and the status line stays the last line at every instant, including between an item tail and an event row in the same tick.
- Arrival order governs: the renderer processes its one queue strictly in arrival order; text announced live sits above the row that arrived after it, and tool rows never "eat" preview lines or arrive below later text.
- The churn boundary from stories 1.1/1.2 holds: no engine wire/protocol change, no second terminal writer, no rework of `completed_text` / `preview_flush` / `preview_prefix_matches` logic, no painter change, `show_thoughts`/`show_tool_calls` gating semantics unchanged.
- The end-of-run single-render guarantee extends to multi-turn runs under design decision A: every streamed block is displayed exactly once (styled) — whatever the turn count — with the run-1 fallbacks preserved (divergence → today's full render of `final_result`; final text that escaped the stream → today's full render) and documented as such.

**Never:**
- No changes to the TUI path (`run_with_callbacks`), the daemon protocol, providers, or the orchestrator; to two-stage Ctrl+C semantics; to confirmation/question prompting; to the tool timeline row shapes; to thinking-header style.
- No code-side wrapping/truncation (story 1.2 doctrine holds), no reflow of printed lines, no width state anywhere.
- No reordering that puts any text output before an event row it did not arrive before — no "paint everything, then reorder" tricks; ordering must fall out of the queue itself.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| text → tool → text across ticks | a scripted multi-turn provider: deltas for T1, then tool events, then deltas for T2 (another turn) | T1 preview lines, then the tool row, then T2 preview lines — arrival order; status line last between every step | No error expected |
| Same-tick coalescing | tool event and later-turn deltas already sit in the queue at one tick | the queued items process in arrival order: the preview flushes the text completed so far, the tool row prints below it, later text flushes after — no reordering by tick boundaries | No error expected |
| Thinking between blocks (show_thoughts on) | a tool-call turn streamed content before the call | per the recorded decision on Thinking (open question) — suppressed when fully previewed, or rendered as today after the previewed lines | No error expected |
| Multi-turn end of run | `final_result` (T3) is a suffix of `block_text` (T1+T2+T3), preview strictly covers the stream | the end-of-run print is only the suffix of the streamed text's parse beyond the committed lines — every block on screen exactly once | No error expected |
| multi-turn divergence | the preview diverged mid-run | today's behavior: full render of `final_result` at end (documented duplication fallback) | fallback, not error |
| Single-turn run | one streamed block, `final_result` == `block_text` | byte-for-byte today's story-1.1 behavior (preview suffix decision, full-render fallback) | fallback paths unchanged |

</frozen-after-approval>

## Code Map

- `takka-cli/src/commands.rs` — `Commands::run` (channel pair at ~1783/1817; renderer thread spawn ~1835; drain loop ~1944–2040; unite the chunk/event channels into ONE `RendererUpdate` channel — the send-order guarantee comes free, because every runtime mode folds deltas+events through the single async `run_turn` loop in `takka-cli/src/client.rs:156–196`, so a unified queue's arrival order is already total).
- `takka-cli/src/commands.rs` — `RenderFinal` (~1832) and the end-of-run decision block (~2236–2268): extend with the multi-turn source-selection rule (suffix match committed → parse of the streamed text); `test_preview` / `test_final_print` / `test_preview_diverged` taps stay, plus one new `test_render_order` tap (cfg(test) markers for printed items: text lines vs tool/thinking rows) to assert interleaving in smokes.
- `takka-cli/src/commands.rs` — e2e script harness (`spawn_scripted_sse_server_mode` ~5145): extend to a scripted sequence of POST responses so the OpenAI-compatible provider performs tool-call turns (`delta.tool_calls` on POST 1 with `finish_reason: tool_calls`, text deltas on POST 2+...) — `list_files` is the verified-safe auto-approved tool for the scripted call; the runtime path (`run_turn`) is what the smokes drive.
- `takka-cli/src/commands.rs` — event path internals used as-is: `render_event` (~3870: ToolCall/ToolStarted → `timeline.on_call/on_started` (no print), Thinking → think block, ToolOutput/ToolFinished → timeline row), plus the `show_thoughts`/`render_panels` gates in the event-closure and the renderer drain (checkout: duplicates Thinking is a decision made at the checkout).
- Untouched reference surfaces: `completed_text` / `preview_flush` (story 1.1 helpers), `StatusPainter` + `frame_pad_on_resize` (story 1.2), `takka-core/src/orchestrator/run.rs` (`ModelChunk` emission) and `takka-core/src/orchestrator/tool_exec.rs` (ToolCall → ToolStarted → ToolDelta → ToolOutput order) — the event sequence arrives pre-ordered; do not touch.

## Tasks & Acceptance

**Execution:**
- [ ] `takka-cli/src/commands.rs` — replace the `(chunk_tx, chunk_rx) + (event_tx, event_rx)` pair with ONE `RendererUpdate { Chunk(String), Event(Event) }` channel: both runtime callbacks enqueue into it (same shared-borrow pattern as today's `Sender::send`), disconnect timing preserved (`drop` after the run joins); the renderer drains it as one queue. Delete the per-tick chunks-first rule — drain, then act.
- [ ] `takka-cli/src/commands.rs` — drain-loop: on `Chunk` append to `block_text`; on `Event` first flush any not-yet-flushed completed text (the same `preview_flush` machinery), then the event's consume → render → commit; trailing chunks flush at tick end exactly as today. Filter `ModelChunk` out at the enqueue boundary — it has no renderer effect today (the Live status phase rides `ProgressState`).
- [ ] `takka-cli/src/commands.rs` — multi-turn end-of-run decision: when `!preview_diverged` and the committed preview is a strict line-prefix of the FULL streamed text's parse and `final_result` is a string suffix of `block_text`, print only the suffix of that parse beyond the committed lines; otherwise keep today's decision (single-turn parse of `final_result`, or the documented full-render fallback).
- [ ] `takka-cli/src/commands.rs` — the Thinking event decision (open question, answered): apply it in the renderer's event arm, gated on the run's preview state only (never a provider/state change).
- [ ] `takka-cli/src/commands.rs` — e2e: a scripted multi-turn provider with the verify's shape text → tool → text → tool → text; assert via the taps (preview lines, final print, and the new render-order marker list) that blocks and tool rows interleave in arrival order and the final render contains every block exactly once; existing smokes stay green.
- [ ] `takka-cli/src/commands.rs` — unit tests for the new end-of-run selection (three branches: multi-turn suffix, single-turn today-behavior, fallback) at the seam where the decision is a pure function.

**Acceptance Criteria:**
- Given a scripted multi-turn run text → tool → text → tool → text, when the run completes, then the streamed blocks interleave the tool rows in arrival order (asserted through the render-order tap) and every streamed block appears in the final presentation exactly once.
- Given a single-turn streamed run, when it completes, then the story-1.1 taps show byte-for-byte today's behavior (preview + suffix-only final print).
- Given the preview diverging mid-run, then the end-of-run path takes today's documented full-render fallback, no new behavior.
- Given the whole workspace suite, `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace --all-targets` (incl. the e2e smoke suite, hermetic config only) all pass.

## Implementation Notes

<!-- Full route: appended during execution -->

- Implemented directly by the orchestrator (step-03 fallback): the three implementer-subagent attempts failed on the platform (one truncated before any edit, two dispatch timeouts); no code existed in the tree after any of them, so this is a from-plan implementation, not a patch of a subagent's work.
- The unified queue is `enum RendererUpdate { Chunk, Event }`; the runtime callbacks borrow the single `Sender` (`&self` sends keep the original binding alive for the post-run `drop` — the event closure shadows it with a clone because it is a `move` closure for the test-events tap, mirroring the old pattern). Disconnect timing: after the run joins, exactly as before.
- `PreviewTracker` regroups the story-1.1 flush state with an `announce()` method so the drain loop can announce text at arrival-order boundaries (before an event row) and at tick end (pure-text ticks — unchanged rule).
- Thinking suppression: `!diverged && !content.trim().is_empty() && flushed.trim_end().ends_with(content.trim_end())`; in the I/O matrix row 3 both halves are pinned by smokes (`smoke_multi_turn_*` suppressed, `smoke_unpreviewed_thinking_still_renders` not-fully-previewed → renders today).
- End-of-run multi-turn rule is a GATE (`end_of_run_uses_full_stream`) plus caller-verified strict prefix match against the full stream's parse; mismatch falls back to today's source/decision (review patch, plan task 3 semantics).
- RunStats `output_chars` keeps today's meaning (the orchestrator's final answer or the stream when it returned nothing) — not the suffix-decision source (review patch).
- The e2e harness gained `ScriptedTurn` + `spawn_scripted_sse_server_turns` (per-connection scripted responses: text deltas, then one OpenAI tool-call delta with `finish_reason: tool_calls`); the OpenAI provider accumulates tool_call deltas and the loop drives list_files (safe builtin, Yolo). Review patched the marker tap to record what actually prints (ToolFinished is a synthesized echo with no render arm — no marker; thinking excluded for whitespace-only content), and deduplicated the SSE accept/drain code into `accept_scripted_connection` + `write_sse_frame` shared by both harnesses.
- 2026-11-06 verification (post-patches): `cargo fmt --all -- --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace --all-targets` all green (CLI 300 incl. the two interleave/thinking smokes + 2 helper gate tests; core 577; integration suites green; 0 failed). Two earlier mid-patch flaky suites (daemon smoke "busy slot" class, centrally noted in AGENTS.md) went green on rerun.
- The story-1.3 verify is asserted end to end: `smoke_multi_turn_interleaves_tool_rows_in_arrival_order` pins text → tool → text → tool → text in arrival order via the `test_render_order` tap, the preview committing every block exactly once, and an EMPTY final print (nothing re-printed).

## Plan Change Log

<!-- Populated by step-04 review loops -->

## Review Triage Log

<!-- Populated by step-04 on every review pass -->

## Design Notes

- Why one unified queue instead of chunk/event drain reordering in the same tick: the runtime side (`run_turn`) emits deltas and events sequentially from one loop, so a single queue inherits the total order for free; per-tick dual-channel drain would have to approximate arrival order with bookkeeping. One channel also deletes the `chunk_rx`/`event_rx` mutex-labeling noise and yields the "text never prints above a row that arrived before it" rule by construction.
- Thinking duplication context: each tool-call turn emits `Event::Thinking { content }` (the whole turn content) AFTER the same text was already streamed and previewed — with `show_thoughts` default true the screen shows the streamed text twice. Decision A says the printed lines ARE the answer, so the question is whether the Thinking recap stays a feature (a mask recap of the turn above the tool row) or is suppressed when its content already sits in preview lines; the record will land in Implementation Notes after your call.
- End-of-run multi-turn mechanics: `block_text` accumulates deltas of ALL turns; the orchestrator's return carries ONLY the last turn's answer (`final_result` = T3). Story 1.1's suffix rule compared against parse(`final_result`) — correct for single-turn runs only. The rule above: `final_result.is_a_suffix_of(block_text)` && committed strict-prefix match against parse(`block_text`) → print the parse-beyond-committed suffix. The blank lines between turns keep the parse's line counts stable (the streamed shape carries paragraph boundaries).
- Scripted tool turn for the smokes: `spawn_scripted_sse_server_mode` returns per-connection identical scripts — the cleanest extension is a scripted vector of responses consumed in connection order: response 1 emits `delta.tool_calls` for `list_files` and `finish_reason: tool_calls`, later responses text+`stop`. `list_files` is a read-only builtin auto-approved under the e2e's `Yolo` permission mode; a failed call would still exercise the full event shape, but success keeps the flow readable.
- Residual, documented: if the final turn's answer escaped the streamed text (final_result not a suffix), the fallback prints it fully — duplication acceptable and documented (story 1.1 matrix carried over).

## Verification

**Commands:**
- `cargo fmt --all -- --check` — expected: no diff.
- `cargo clippy --workspace --all-targets -- -D warnings` — expected: zero warnings.
- `cargo test --workspace --all-targets` — expected: all green including the new multi-turn interleave smoke and all story-1.1/1.2 smokes (hermetic config only, no ambient `~/.config/takka/config.json`).

**Manual checks (if no CLI):**
- PTY-only: run a real multi-turn agentic task in the inline REPL and confirm visually: preview text above its following tool row, status line last at every step, and the run's final scrollback showing each streamed block exactly once.

### 2026-11-06 — Review pass 1 (Quick lens, review=quick pinned)
- verdicts: 4 findings verified — medium 0, low 4, false 0, maybe-false 0; 1 non-finding (verification evidence was the orchestrator's own run, not the implementation report the reviewer lacked)
- findings:
  - `[low]` `[patch]` Tap markers violated the "printed items" contract — verified: `ToolFinished` has no `render_event` arm (falls to `_ => {}`) and the runtime engine synthesizes it as an echo right after `ToolOutput`, so each tool logged two markers for one printed row; the smoke's "live-delta row + completion outcome" explanation was factually wrong (no ToolDelta in the script; compact mode returns before printing anyway); `thinking` was marked for whitespace-only content while `render_event` prints nothing. PATCH APPLIED: markers now record what actually printed (`ToolOutput` row under `show_tool_calls`, thinking for non-empty content only), ToolFinished/ToolDelta excluded with the reason in the comment; both smoke expectations and comments corrected.
  - `[low]` `[patch]` `end_of_run_source` deviated from the plan's stated rule — verified: it selected the full stream on the suffix condition alone, so "suffix holds, prefix mismatches" printed the FULL multi-turn stream where "today's decision" would full-render only `final_result`; reachability is narrow (parse vs parse_partial discrepancies on a held-back tail) but the branch contradicted plan task 3. PATCH APPLIED: gate split into `end_of_run_uses_full_stream` (suffix + non-diverged + non-empty commit only) with the strict prefix match verified against the full stream's parse in the caller; mismatch falls back to today's source and decision; unit tests rewritten for the gate semantics.
  - `[low]` `[patch]` `self.last_run.output_chars` silently changed meaning for multi-turn runs — verified: the suffix-decision source counts every turn's deltas, changing the `/stats` semantics without a documented decision. PATCH APPLIED: `RunStats` measures today's source (the orchestrator's final answer, or the stream when it returned nothing), decoupled from the print source.
  - `[low]` `[patch]` Test-harness duplication — verified: the multi-turn SSE server copy-pasted the request-drain loop, the response head string, and the chunked frame writer. PATCH APPLIED: `accept_scripted_connection` + `SCRIPTED_SSE_HEADER` + `write_sse_frame` extracted, both harnesses rewired.
- reviewer's non-finding: AC 3 (fmt/clippy/tests green) could not be verified by inspection — the orchestrator ran the full workspace suite itself: all green post-patches (CLI 300, core 577, 0 failed).
- checked and cleared by the reviewer (carried as-is): unified-queue order totality, Thinking-suppression condition matches decision A, ModelChunk filter loses nothing, gated announce ≡ unconditional announce, suppressed-Thinking `continue` is commit-safe, sender clone/drop disconnect protocol, 1.1/1.2 machinery untouched, TUI/daemon/provider/orchestrator untouched, single-turn byte-identical.
- patches applied by the orchestrator (the step-03 subagent could not be re-engaged — the platform failed all launches; the oneshot fallback applies per step-04).
