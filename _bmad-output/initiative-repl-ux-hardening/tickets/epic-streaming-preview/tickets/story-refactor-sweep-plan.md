---
title: 'Story 1.5 — Refactor sweep'
type: 'chore'
ticket: 'story-refactor-sweep'
created: '2026-11-06'
status: 'built'
baseline_revision: '20b0b39'
route: 'oneshot'
route_source: 'pinned'
risk: 'low'
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

**Problem:** The epic's first four stories left exactly three audit-trail debts on the table: (1) story 1.1's deferred hygiene finding — three `U+FFFD` mojibake bytes in the `Transient status line` section-comment separator of `takka-cli/src/commands.rs`; (2) story 1.1's deferred cancel-coverage — resolved ALREADY by story 1.4's cancel wiring + three phase smokes, but never recorded where the deferral was filed; (3) the initiative's `deferred-work.md` entry — story 1.2's frozen-Intent/Matrix paragraph still says the superseded "reset to the new frame's width" pad rule; the code implements the min-cover rule, so the plan text reads wrong from a cold start. This sweep is the epic's designed cleanup story: "cleanup only, scoped at its start from the plans and review findings the other entries deferred."

**Approach:** Fix the mojibake bytes (pure comment-text repair, zero behavior change); record the now-covered deferrals in this plan (the built stories' plans stay append-only); and settle the deferred-work entry per the human's checkpoint answer — either by a human-approved, dated amendment of the story-1.2 frozen paragraph (restating the min-cover rule wording) or by recording that it stays with the human. No code behavior changes — the deliverable is a comment fix and settled records (and workspace checks green).

</frozen-after-approval>

## Implementation Notes

<!-- Oneshot route: filled during execution -->

- Mojibake fix executed first (the sweep's only code change): the `// ── Transient status line ───…` section separator in `takka-cli/src/commands.rs` carried three `U+FFFD` characters (9 bytes of `EF BF BD`, story 1.1 review defer #1); they were replaced with the proper `─` box-drawing character matching the file's other separators — a pure comment repair, no code change to any line of logic.
- Deferral ledger settled here (the built stories' plans are the historical record and stay untouched, with one sanctioned exception the frozen Approach pre-authorized: the story-1.2 frozen wording, amended under the human's deferral answer):
  - Story 1.1 defer #1 (comment mojibake) — SETTLED by this fix.
  - Story 1.1 defer #2 (no test can drive a mid-run cancel; cancel wiring deferred) — SETTLED by story 1.4 (the `run_turn` cancel watcher + `Request::Cancel` wiring + three per-phase cancel smokes; the 1.4 plan and triage log carry the full record).
  - `deferred-work.md` (frozen-Intent wording of the story-1.2 plan) — SETTLED per the human's checkpoint answer (2026-11-06, answer 1): the story-1.2 plan's frozen Approach sentence and Matrix row 2 now carry the min-cover rule wording with a dated human-approved amendment note. The `deferred-work.md` original entry stays untouched (append-only file); this note is its resolution record.
- 2026-11-06 verification: `cargo fmt --all -- --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace --all-targets` all green (971 passed, CLI 308 + core 577 + integrations, 0 failed). The mojibake byte count in `commands.rs` verifies as 0 (byte-level grep).
- Review routed quick (the sweep's risk is confined to a comment); the lens verdict lands in the Review Triage Log below.

## Review Triage Log

<!-- Populated by step-04 on every review pass -->

### 2026-11-06 — Review pass 1 (Quick lens, review=quick pinned)
- verdicts: 4 findings verified — medium 1, low 3, false 0, maybe-false 0; all patched
- findings:
  - `[medium]` `[patch]` The amended story-1.2 frozen clause still carried "Every outgoing line fits the terminal as it is now" — the same overclaim verified false by the story-1.2 re-review pass 2 (presence-of-content-wider-than-terminal) and contradicted by the plan's own residual note; Matrix row 2's parenthetical also dropped the outer `max(line_width, …)` term. PATCH APPLIED: the clause now states the baseline-fits-≤-width rule plus the wrap-on-wider-content residual (pointer to the code-doc patch); the Matrix parenthetical states the full pad.
  - `[low]` `[patch]` The settled deferred-work entry carried no resolution trace. PATCH APPLIED: one-line resolution append (append-only file preserved).
  - `[low]` `[patch]` The 1.5 ledger header said the built plans "stay untouched" while a sub-bullet described the sanctioned 1.2 frozen edit. PATCH APPLIED: header now names the one sanctioned exception.
  - `[low]` `[patch]` "three `U+FFFD` bytes" was byte/char imprecise. PATCH APPLIED: "three `U+FFFD` characters (9 bytes of `EF BF BD`)".
- reviewer's cleared record (carried): mojibake fix byte-verified (0 `EF BF BD`; separator now U+2500 matching siblings); min-cover formula wording matches `frame_pad_on_resize` argument-for-argument; every cross-reference (Loopback 1, story-1.4 records, defers) resolves; diff scope = the three intended files; AGENTS.md rules hold; "cleanup only" verified — zero behavior change; workspace checks green (fmt, clippy, 971 tests).
- patches applied by the orchestrator directly (oneshot sweep; text-only).
- Controller note on the frozen-block edit: the amendment followed this thread's checkpoint (deferral #3 → answer 1, 2026-11-06) — the human-approved path the frozen Approach pre-authorized; no external checkpoint record exists beyond the thread.
