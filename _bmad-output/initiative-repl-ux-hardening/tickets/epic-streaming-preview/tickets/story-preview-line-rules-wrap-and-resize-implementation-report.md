---
title: 'Story 1.2 — Preview line rules: wrap and resize — implementation report'
story: 'story-preview-line-rules-wrap-and-resize'
plan: 'story-preview-line-rules-wrap-and-resize-plan.md'
implemented: '2026-11-06'
baseline_revision: '5a2b35d60c062af23339adaf665736aebbdebd67'
commit: 'c1a65f6'
status: 'DONE'
---

# Implementation report — story 1.2 (wrap and resize)

## Summary

Implemented the plan **as amended by Plan Change Log Loopback 1** on top of
baseline `5a2b35d` (the working tree was clean at that revision; the run-1
implementation referenced in the plan's Implementation Notes was not present in
this history, so this is a fresh, Loopback-conformant implementation, not a
patch of run 1). Commit: `c1a65f6`.

The run-1 Implementation Notes judgment call ("directional reset") was **not**
applied, per the Loopback 1 SUPERSEDED instruction. The helper is exactly the
amended min-cover rule, and there is no `ambient_width` field.

## What changed

### `takka-cli/src/commands.rs`
1. **New pure helper** `frame_pad_on_resize(frame_width, ambient_now, line_width) -> usize`
   (free function, outside `impl StatusPainter`, unit-testable without a TTY):
   `pad = line_width.max(frame_width.min(ambient_now))` — the Loopback-1
   min-cover contract verbatim. Doc comment states the rule, the
   within-one-width equivalence to today's pad-to-widest, the partial-narrow
   retention, the deep-narrow cap, and the documented transition-frame residual.
2. **`StatusPainter::next_line`** re-measures the ambient width
   (`inline::terminal_width()`) every frame, delegates to the helper, pads when
   `pad > width`, and stores the returned pad as the new `frame_width` baseline.
   Caller state remains exactly one `usize` — no `ambient_width` field (Code Map
   bullet as amended). Painting order unchanged; `show_cancel_hint` untouched
   (run-start snapshot in `new()`).
3. **Struct doc** updated to describe the min-cover rule and the per-frame
   re-measure.
4. **Doctrine comment** on the live-preview block (story 1.1 section): whole
   logical lines, terminal does all wrapping once at its ambient width
   (console-log hook precedent `log_writer::write_line_to_stderr`), nothing in
   the preview path measures or caches a width, printed lines never
   re-rendered/re-wrapped/truncated on resize, painter's re-measure resets only
   its pad baseline.
5. **New tests** (5) in `status_painter_tests`, per the amended Tasks list:
   - `pad_unchanged_width_is_todays_pad_to_widest_bit_for_bit`
   - `pad_on_widening_keeps_the_widest_baseline`
   - `pad_on_partial_narrowing_keeps_the_retained_baseline` (the Loopback-1 fix:
     pad stays at `min(baseline, new width)`, covering the predecessor)
   - `pad_on_deep_narrowing_caps_at_the_new_ambient_width`
   - `pad_widen_then_narrow_needs_only_the_frame_width_state`
6. **New tests** (2) in `preview_flush_tests`:
   - `overlong_paragraph_flushes_as_one_whole_line` — 300-char paragraph
     flushes as exactly ONE logical line whose plain text is all 300 chars.
   - `flush_is_width_blind_and_prints_only_genuinely_new_lines` — Loopback-1
     finding 4 patch applied: honest name/assertion. The test documents that
     `preview_flush` has no width input and its output is a pure function of
     (committed, completed); re-flushes print only genuinely new lines and
     `committed` entries stay untouched opaque strings. (The plan task's
     "after a simulated width change" wording is honored in the comment —
     a width change is invisible to the flush by construction, which the test
     asserts honestly rather than simulating a width it cannot feed in.)

### `takka-cli/src/widgets/inline.rs`
7. **Wrap-doctrine doc paragraph on `print_line`**: line written whole, terminal
   wraps it once at its own ambient width, same rule as the console-log hook;
   no code-side wrap/truncate; printed lines are final on resize.

### Untouched (boundaries verified)
`log_writer.rs` (reference only), `spinner.rs`, story 1.1 flush machinery
(`completed_text` / `preview_flush` / `preview_prefix_matches` bodies and the
renderer-thread state machine), per-phase status content/colors/timeline,
cancel semantics, engine contract.

## TDD evidence

- RED first: tests written before the helper; compile failed with exactly
  11 × `E0425 cannot find function frame_pad_on_resize` (plus one test-authoring
  lifetime typo E0716 and later one clippy `needless_borrow`, both fixed in the
  tests before/with the implementation — the implementation itself was written
  only after the RED state was clean).
- GREEN: after implementing the helper + rewiring `next_line`, all new and all
  pre-existing painter/flush tests pass.

## Verification (all green)

- `cargo fmt --all -- --check` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo test --workspace --all-targets` — 0 failures:
  - CLI bin: 296 passed (289 baseline + 7 new; includes daemon/e2e smokes; the
    known-flaky `shutdown_busy_daemon_errors_after_deadline` passed first try;
    hermetic config only).
  - takka-core: 577 passed; integration suites (orchestrator 26, planner 8,
    protocol 4, runtime_engine 24, runtime_loop 6, skills 10, runtime main 6,
    protocol_smoke 3): 87 passed. Total 960.
- Pre-existing tests pass unchanged: 2 `status_painter_tests` and
  9 `preview_flush_tests` (AC: they encoded only within-one-width behavior,
  which the helper preserves bit-for-bit).

## Self-review findings

- One authoring slip during RED (temporary borrowed in a test) and one clippy
  needless borrow — both fixed in test code; no production rework needed.
- First helper placement accidentally landed inside `impl StatusPainter`
  (caught: tests call it as a free fn → E0425 persisted); moved outside the
  impl before GREEN. Final placement: free fn between the impl and the preview
  section.
- Edge traced, consistent with the doctrine: a frame line wider than the
  ambient terminal is padded to its own width (terminal wraps it, as with any
  over-long line); a later narrow then caps the pad at the new ambient width.
- Manual PTY check from the plan's Verification section not run (no CLI
  session available here); the helper unit tests pin every branch of the rule,
  and the e2e smokes cover the preview path hermetically.

## Files changed

- `takka-cli/src/commands.rs` (helper, `next_line`, docs, doctrine, 7 tests)
- `takka-cli/src/widgets/inline.rs` (`print_line` doctrine doc)

## Left open / risky

- The documented residual stands (per plan): the ONE deep-narrow transition
  frame painted before the new width is first observed is irrecoverable without
  visual-row accounting; every subsequent frame fits. Status-line content can
  still exceed very narrow terminals (per-phase content is a boundary — out of
  scope).
- The plan file itself and this report remain untracked (ticket artifacts for
  the orchestrator to commit/shelve per the bmad flow).
