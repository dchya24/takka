---
title: 'Story 1.2 — Preview line rules: wrap and resize'
type: 'feature'
ticket: 'story-preview-line-rules-wrap-and-resize'
created: '2026-11-06'
status: 'in-review'
baseline_revision: '5a2b35d60c062af23339adaf665736aebbdebd67'
route: 'full'
route_source: 'pinned'
risk: 'low'
review: 'quick'
review_source: 'pinned'
lenses_ran: ['quick']
review_loop_iteration: 1
followup_review_recommended: false
context:
  - '_bmad-output/initiative-repl-ux-hardening/tickets/epic-streaming-preview/epic-streaming-preview.md'
warnings: []
deferred: []
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** The story-1.1 preview prints whole logical lines and relies on the terminal's natural wrap ("same rule as the console-log hook", decision 2026-11-05b/c) — but that rule is undocumented and untested, and one line of the live-run rendering contract still breaks under a mid-stream resize: `StatusPainter` pads frames to the widest frame seen and snapshots `show_cancel_hint` at construction, so after the terminal narrows mid-run the padded status line exceeds the new width, wraps, and the single-line cursor accounting (`MoveUp(1)` redraw and end-of-run cleanup) leaves ghost lines.

**Approach:** Lock the wrap/resize doctrine in code comments and tests (streamed preview lines are announced once each as whole logical lines; the terminal does all wrapping; nothing in the preview path caches a width), and make the status painter width-aware across a mid-run resize: each frame re-measures the ambient width and resets the pad baseline when it changed, so every outgoing line fits the terminal as it is now. (Scope decision, human answer 2026-11-06: the `StatusPainter` resize fix is included in this story.)

## Boundaries & Constraints

**Always:**
- Preview filed text stays stable across any resize: `preview_committed` grows by whole logical lines only, the strict-prefix check and end-of-run suffix logic never consult a width, and nothing ever re-renders, truncates, or re-wraps an already-printed preview line.
- Wrapping of an over-long streamed line happens once, at the terminal's own ambient width (the console-log hook rule: write the whole line, let the terminal wrap it) — both in preview flush and end-of-run suffix printing.
- The status painter keeps its single-writer gate discipline and pad-to-widest-frame rule *within* one ambient width; re-measuring only resets the pad baseline, it does not introduce a second writer.
- Existing tests (`status_painter_tests`, preview flush tests, PTY smokes) keep passing unchanged unless they encoded the old resize-blind behavior.

**Never:**
- No custom code-side wrapping or truncation of preview/status lines (no textwrap, no manual line-breaking), no reflow of printed lines, no new terminal writer.
- No changes to per-phase status-line content, phase accent colors, tool timeline row shapes, cancel semantics, or the engine contract; `show_cancel_hint` stays evaluated at painter construction (run-start width).
- No rework of story 1.1's flush machinery (`completed_text` / `preview_flush` / divergence guard) beyond adding tests that pin the wrap rules.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| 300-char streamed paragraph completes | `block_text` gains one 300-char line followed by a blank-line boundary | `completed_text` announces it whole; `preview_flush` returns exactly ONE logical line of exactly 300 chars; the terminal wraps it visually | No error expected |
| Terminal narrowed mid-stream | ambient width shrinks after preview lines are printed | already-printed lines untouched; each new painter frame re-measures and pads so it fits the new width (`frame_width` reset to the new frame's width) | No error expected |
| Terminal widened mid-stream | ambient width grows | painter pads from the widest retained baseline; next line wider re-baselines `frame_width` naturally | No error expected |
| Over-long paragraph printed, then end of run | committed preview is a strict line-prefix of the final parse | suffix-only print as today; committed accounting never counted visual (wrapped) rows, so it stays exact | prefix mismatch keeps the documented full-render fallback |

</frozen-after-approval>

## Code Map

- `takka-cli/src/commands.rs` — `StatusPainter` (~line 3689): `frame_width` pad-to-widest rule, `show_cancel_hint` snapshot in `new()`; `next_line`/`commit`/`redraw`/`erase` is the entire surface the resize fix touches. Extract the width-reset decision into a pure helper so it is unit-testable without a real TTY.
- `takka-cli/src/commands.rs` — live-preview state machine inside the renderer thread (~line 1880–1990) and helpers `completed_text` / `line_plain_text` / `preview_prefix_matches` / `preview_flush` (~line 3764–3850): already width-blind by construction; only get tests + the doctrine comment. `status_painter_tests` (~line 4350) and `preview_flush_tests` (~line 4391) are the test homes.
- `takka-cli/src/widgets/inline.rs` — `print_line/print_lines` (no wrap logic — the terminal's own wrap is the rule) and `terminal_width()` (fresh `crossterm::terminal::size()` per call, 40-col floor): read-only reuse; document the doctrine on `print_line`.
- `takka-cli/src/widgets/log_writer.rs` — `write_line_to_stderr`: the console-log hook precedent for the whole-line rule (writes raw bytes, zero custom wrapping); reference only.
- `takka-cli/src/widgets/spinner.rs` — `status_line` content: untouched.

## Tasks & Acceptance

**Execution:**
- [x] `takka-cli/src/commands.rs` — make `StatusPainter::next_line` re-measure the ambient width (`inline::terminal_width()`) each frame through a pure `frame_pad_on_resize(frame_width, ambient_now, line_width) -> usize` helper implementing the min-cover rule: `pad = max(line_width, min(frame_width, ambient_now))`. Rationale: every column the predecessor could have painted without wrapping at the current width is covered again, so the no-`Clear` in-place overwrite strands nothing; deep-narrow transitions (baseline itself no longer fits) best-effort cover the maximal one row, and the frame never exceeds the new width. Painting order and the pad-to-widest rule within one ambient width unchanged. (Loopback 1: supersedes the run-1 rule "narrowing resets the baseline to the new frame's width" — a partial narrowing must keep the retained baseline or stale columns resurface.)
- [x] `takka-cli/src/commands.rs` — unit tests for the resize helper: unchanged width (today's pad-to-widest behavior bit-for-bit), widening (widest-so-far still padded, a wider frame re-baselines), partial narrowing with retained baseline (pad stays at `min(baseline, new width)`, covering the predecessor), deep narrowing below the baseline (pad capped at the new ambient width), and widen-then-narrow (the stored baseline state is only `frame_width` — no stale ambient state involved).
- [x] `takka-cli/commands.rs` — doctrine comments at the preview block + `inline::print_line`: over-long lines wrap once at the terminal's ambient width by the terminal itself; nothing caches width; printed lines are never re-rendered on resize. Rationale: the story's deliverable under design decision A is a locked contract `epic-mid-run-input` can lean on.
- [x] `takka-cli/src/commands.rs` — wrap-rule tests in `preview_flush_tests`: a 300-char paragraph flushes as exactly one logical line whose plain text is the full 300 chars (no truncation/splitting); running another flush after a simulated width change prints only genuinely new lines and leaves `committed` untouched as opaque strings.

**Acceptance Criteria:**
- Given the pure preview helpers with a 300-char completed paragraph, when `preview_flush` runs, then the result is a single logical line containing every character (the terminal's own wrap owns the visual result).
- Given the resize helper, when the ambient width shrinks or grows mid-run, then the outgoing status frame fits the current width with a reset or re-baselined pad and no frame is padded from a stale wider width.
- Given the full workspace suite, when it runs, then `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace --all-targets` (incl. the e2e smoke suite) all pass.

## Implementation Notes

- Implemented on top of baseline 5a2b35d. TDD: the five new tests landed first (red — `frame_pad_on_resize` not found), then the implementation (green). Implementation commit: 3c8ea7e.
- `StatusPainter` gained an `ambient_width` field (the width the current `frame_width` baseline was measured at; 0 until the first frame re-measures). `next_line` now calls `inline::terminal_width()` every frame and delegates to the pure helper; the returned pad width doubles as the new baseline, so caller state stays two `usize` stores. `show_cancel_hint` untouched (run-start width, per boundary).
- Judgment call (documented refinement of the golden rule): the reset is directional — on a *narrowing* the baseline resets to the new frame's own width; on a *widening* the widest retained baseline still pads and a wider frame re-baselines naturally. The plan's widening matrix row ("painter pads from the widest retained baseline") and its narrow→wide test bullet pin this; a literal "any ambient change resets" reading would contradict that row and re-introduce stale columns, because the in-place `MoveUp` + repaint cycle writes no `Clear` — a narrower frame after a wider on-screen one must still pad to the widest seen.
- Helper signature is the plan's `(frame_width, ambient, line_width) -> usize` style widened to `(frame_width, ambient, ambient_now, line_width)`: the width-reset decision must live inside the pure function to be unit-testable without a TTY, and detecting "the ambient changed" requires both the stored and the fresh measurement.
- Within one width the behavior is bit-for-bit today's pad-to-widest (helper's `max(line_width, frame_width)` branch); the two pre-existing `status_painter_tests` pass unchanged, as do all nine pre-existing `preview_flush_tests`. First frame and non-TTY runs (`cargo test`, piped output — constant `terminal_width()`) take exactly the old path.
- Doctrine comments added: wrap/resize paragraph on the live-preview block in `commands.rs` and a wrap-doctrine paragraph on `inline::print_line` (both citing the console-log hook precedent `log_writer::write_line_to_stderr`); `log_writer.rs` itself untouched (reference only). No code-side wrap/truncate added anywhere; story 1.1 flush machinery untouched beyond tests.
- 2026-11-06 verification: `cargo fmt --all -- --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace --all-targets` all green (CLI 294 incl. 5 painter + 11 flush tests and the daemon/e2e smokes — the known-flaky `shutdown_busy_daemon_errors_after_deadline` passed first try; core 577; all other suites green, total 952). Hermetic config only.

## Plan Change Log

<!-- Append-only. Populated by step-04 during review loops. Do not modify or delete existing entries.
     Each entry records: what finding triggered the change, what was amended, what known-bad state
     the amendment avoids, and any KEEP instructions (what worked well and must survive re-derivation).
     Empty until the first bad_plan loopback. -->

### 2026-11-06 — Loopback 1 (bad_plan from Quick lens, run 1)
- Trigger: review findings — (medium) partial narrowing reset the pad baseline to the new frame's width, stranding the predecessor's stale columns under the no-`Clear` in-place overwrite; (medium) a stored `ambient_width` no longer described the width the baseline was measured at after a widening, re-triggering the same defect on a later narrow; (low) the manual PTY check promised a ghost-free transition frame that the no-visual-row-accounting doctrine cannot deliver.
- Root cause (outside the frozen block): the run-1 Design Notes golden rule ("on ambient change `frame_width := line_width`") and the run-1 Implementation Notes judgment call mandated a reset condition (`ambient_now < ambient` → pad to `line_width`) that only covers deep narrows; a partial narrowing must KEEP the retained baseline.
- Amended (non-frozen): Tasks (helper contract → min-cover rule `max(line_width, min(frame_width, ambient_now))`, no stored ambient state; test list gains partial-narrow + widen-then-narrow cases), Design Notes (golden rule rewritten; transition-frame residual documented), Verification manual check (reworded: every frame after the first observation of the new width fits it), Code Map painter bullet (no `ambient_width` field).
- Known-bad state avoided: baseline reset on partial narrowing reintroducing stale-column residue; a second stored width (`ambient_width`) whose semantics diverge from the helper's inputs; a manual-check criterion promising an impossible transition frame.
- KEEP instructions (must survive re-derivation): the pure helper remains the shape (unit-testable without a TTY); the preview flush stays entirely width-blind (no width input, no cached width); the doctrine comments on the preview block and `inline::print_line` stand as written in run 1; TDD order (tests first, then helper); existing test homes (`status_painter_tests`, `preview_flush_tests`); story 1.1's flush machinery untouched; `show_cancel_hint` stays the run-start snapshot. Run-1 Implementation Notes (the "directional reset" judgment call) are SUPERSEDED by this amendment — do not re-apply.

## Review Triage Log

<!-- Populated by step-04 on every review pass -->

### 2026-11-06 — Review pass 1 (Quick lens, review=quick pinned)
- verdicts: 4 findings verified — medium 2, low 2, false 0, maybe-false 0
- findings:
  - `[medium]` `[patch]` Painter pad reset on partial narrowing leaves stale columns — verified at the helper (`ambient_now < ambient` → pad = `line_width`): baseline 60 built at 100 cols, narrow to 80, next frame 40 wide paints 40 cols while the predecessor occupied 60 (60 ≤ 80, no wrap) and `redraw`'s `MoveUp(1)` + `MoveToColumn(0)` writes no `Clear` — columns 40–59 keep predecessor glyphs. Fails the story's own AC ("no frame is padded from a stale wider width"). Routed into the bad_plan entry (shared root cause with the next finding).
  - `[medium]` `[patch]` `ambient_width` stops meaning "width the baseline was measured at" after a widening (stored unconditionally = ambient_now while the baseline may predate it) — verified by tracing the widening branch; a later narrow then wrongly takes the reset branch even though the on-screen frame still fits. Same root cause as above; grouped.
  - `[low]` `[patch]` The deep-narrow TRANSITION frame (painted before the new width was observed) is irrecoverable without visual-row accounting, so the manual check's "re-fits without ghost residuals" cannot hold for that frame — verified inherent under the no-row-accounting doctrine. Plan-level wording fix; routed into the same bad_plan entry (the Verification section).
  - `[low]` `[patch]` `flush_after_a_simulated_width_change_prints_only_new_lines` simulates no width change and its own comment concedes it — verified: `preview_flush` has no width input; the test duplicates story 1.1's suffix-only coverage under a misleading name/comment. Code-level fix (honest name/assertion); survives loopbacks as a patch.
- routes: bad_plan ×1 (findings 1–3 grouped), patch ×1 (finding 4). Loopback 1 initiated.

## Design Notes

- Why no code-side wrapping: decision 2026-11-05c names the console-log hook as the precedent, and that hook (`log_writer::write_line_to_stderr`) writes whole lines with zero custom wrapping — the terminal wraps at its own ambient width. Any code-side wrap would create width state to cache ("measure on each print" was explicitly rejected in epic refinement) — violating this story's "only new lines measure the new width" under the natural reading, and re-creating the wrap/line-count bugs the renderer was built to eliminate.
- Why the painter fix belongs here: the story's verify line ("no ghost columns; resizing mid-stream leaves printed lines untouched") is only honestly satisfiable when the whole live-run surface obeys the same width doctrine; the painter is the one surface currently breaking it. It stays inside the C1 single-writer gate.
- Golden rule for the helper: `pad = max(line_width, min(frame_width, ambient_now))` — cover everything the predecessor could paint unwrapped at the current width; within one width (baseline measured at the same ambient) this is exactly `max(line_width, frame_width)`, bit-for-bit today's pad-to-widest. A stored `ambient` value is not needed by the rule; the only state is `frame_width` itself.
- Residual (documented, inherent): the ONE transition frame on a deep narrow (terminal shrinks below an already-painted wider frame between ticks) is past repair without visual-row accounting — the predecessor has already wrapped/clipped when the new width is first observed; the helper's cap at the new ambient width makes every SUBSEQUENT frame fit exactly. A one-frame ghost from the transition frame itself is accepted, documented, and out of this story's fix scope.
- Known residual (documented, out of scope): status-line *content* itself can exceed the width on very narrow terminals (long tool name + hint) — per-phase content is a boundary, so it is filed as a documented edge, not fixed here.

## Verification

**Commands:**
- `cargo fmt --all -- --check` — expected: no diff.
- `cargo clippy --workspace --all-targets -- -D warnings` — expected: zero warnings.
- `cargo test --workspace --all-targets` — expected: all green, incl. the new wrap-rule and resize-helper tests and the existing preview smokes; hermetic config only (no ambient `~/.config/takka/config.json`).

**Manual checks (if no CLI):**
- PTY-only: run a scripted streaming answer, narrow the window mid-stream — printed preview lines stay untouched, from the first frame after the new width is observed onward the status line re-fits (frames padded at most to the new width, never past it); the terminal-shrink transition frame itself may leave one stale visual row (documented residual above); a mid-stream Ctrl+C leaves clean scrollback.

### 2026-11-06 — Re-review pass 2 (Quick lens, after Loopback 1 re-derivation)
- verdicts: 4 findings verified — medium 0, low 4, false 0, maybe-false 0; no loopback routes
- findings:
  - `[low]` `[defer]` The frozen Intent/Matrix paragraph still carries the run-1 wording ("resets the pad baseline when it changed", "frame_width reset to the new frame's width"), contradicting the Loopback-1 min-cover rule the code implements — verified by comparing the frozen block against `frame_pad_on_resize` and `pad_on_partial_narrowing_keeps_the_retained_baseline`. Fix edits the human-owned <frozen-after-approval> block → filed to initiative `deferred-work.md` for a human amendment; the plan body + Change Log carry the authoritative rule meanwhile.
  - `[low]` `[reject]` Task 4's letter ("after a simulated width change") is not literally met by `flush_is_width_blind_and_prints_only_genuinely_new_lines` — verified unavoidable: `preview_flush` takes no width parameter and consults none, and `crossterm::terminal::size()` is a constant ioctl under `cargo test`, so no hermetic width simulation exists. The invariant (re-flush prints only genuinely new lines, `committed` untouched) IS asserted; the honest name/comment was the run-1 patch (triage pass 1, finding 4). Remaining gap is task wording whose only fix edits this build's plan → rejected per the fix-edits-the-plan rule; the Implementation Notes + report disclose the substitution.
  - `[low]` `[patch]` `frame_pad_on_resize` doc overclaims "the frame never exceeds the terminal" — verified false when `line_width > ambient_now` (the `max` returns the content width; proof-of-existence in the struct's own run-start hint logic and the plan's residual bullet). PATCH APPLIED: doc sentence scoped to the baseline contribution ("wraps only when its own content is wider..."). 
  - `[low]` `[patch]` Undocumented residual: a deep narrow clamps the baseline (frame_width := min(baseline, ambient)) so the clamp persists after a re-widen, and stale columns beyond the clamp can resurface until a naturally wider frame paints — verified by tracing `self.frame_width = pad` + `redraw`'s no-Clear overwrite; plan-mandated behavior (one-usize-state constraint) that was merely undocumented. PATCH APPLIED: same doc comment now names the re-widen resurfacing as the same family of cosmetic, bounded residuals.
- patches: helper doc comment rewritten (one contiguous comment block, no logic change); verification re-run green (fmt/clippy clean; workspace tests CLI 296 + core 577, 0 failed).
