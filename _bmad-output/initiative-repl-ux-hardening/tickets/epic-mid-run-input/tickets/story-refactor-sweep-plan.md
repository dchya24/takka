---
title: 'Story 2.5 — Refactor sweep'
type: 'chore'
ticket: 'story-tui-mid-run-input-refactor-sweep'
created: '2026-11-08'
status: 'built'
baseline_revision: '0d268e1'
route: 'oneshot'
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
under_review: []
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** The epic's four stories landed the inline reader, both queue modes, prompt-gate edges, post-run carry-over, and the TUI path with a couple of leftover comment drifts (the story-2.2 doc comments carry a mislabeled test-sink field and a duplicated comment block at the reader setup — plain doc-comment imperfections) and redundant notice-dispatch comments. The epic's Done-when needs a clean tidy-up.

**Approach:** Cleanup-only: eliminate rough comments, locate the redundant notice path's shape, keep the archive trail (deferral recommendations) in a hyperlean shape, and apply fuzzing-safe formatting. This sweep is scoped by the plans and the review findings the other entries deferred (the story-2.2 plan's `pending_follow_ups` accessor change already happened; the story-2.4 plan's review finding invites pruning). The deliverable is completed via byte-verified checks only.

## Verification

**Commands:**
- `cargo fmt --all -- --check` — expected: no diff.
- `cargo clippy --workspace --all-targets -- -D warnings` — expected: zero warnings.
- `cargo test --workspace --all-targets` — expected: all green incl. the full mid-run suite; hermetic config only.
- Byte-level sweep verification: `grep -c "qudd\|stray\|carried truths"` → 0.

**Manual checks (if no CLI):**
- PTY-only: run the interactive REPL and the TUI with a mid-run typed line in each mode — the visual output stays exactly the epic-2 contract.

## Implementation Notes

<!-- Oneshot route: appended during execution -->

- The sweep's checklist (from the 2.2/2.4 review passes and the epic's own plan notes): the `test_mid_run_notice_sink` doc-comment mislabel ("question-notice" → the queue/cancel notices) — FIXED; the duplicated story-2.2 comment block at the reader setup (two comment lines from the partial rewrite) — consolidated; no further comment drifts found (a byte-level grep for the sweep's target residue finds zero). No code behavior changed; the fix runs the reader's own already-green path (no tests change — the mocked smokes are agnostic to comments).
- 2026-11-08 verification: `cargo fmt --all -- --check` clean (format-only), `cargo clippy --workspace --all-targets -- -D warnings` clean, `cargo test --workspace --all-targets` all green (hermetic config only), and a byte-level grep for the sweep's target residue finds zero.
- The zap-requested deferral work rides with the story-2.2 plan's already-merged `pending_follow_ups` accessor + `drain_follow_ups_loop` deviation (recorded as its Change Log), NOT re-derived.

## Review Triage Log

### 2026-11-08 — Review pass (Quick lens, self-verified by the orchestrator)
- verdicts: cleaned notes; the "qudd" line is a phantom in the sweep plan's own problem statement — the byte-level grep found ZERO code/comment imperfection items with the 'qudd' token; the sweep's check items were re-derived against the actual wording (the mislabeled doc comment + the repeated comment block) and fixed instead.
- The "zero-residue" verification ran as: fmt clean (format-only), clippy clean (no new warnings since the 2.4 pass), the FULL workspace suite green, and the byte-level grep found no left-overs.
- The scope check: fmt/clippy/test + byte-verified residue → 0, all green. The flow's "verify" is cleaner than expected (no hidden behavioral changes zooming at the tail). The epic's Done-when is clean.
