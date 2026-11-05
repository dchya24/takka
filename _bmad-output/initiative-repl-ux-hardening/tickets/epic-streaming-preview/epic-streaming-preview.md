---
type: epic
title: "Live streaming reply"
parent: "initiative-repl-ux-hardening"
covers: ["R2"]
after: []
refined: true
risk: medium
---

# Live streaming reply

## Description

While an agent run streams a long answer, the inline REPL shows the accumulated
plain text progressively under the existing status line, instead of holding
everything back and revealing the answer only after the run completes. The
final answer still renders as the styled markdown pass that exists today. The
epic's And-deliverable: its closing stories set down the **live-run rendering
contract** (how streamed text, tool rows and the status painter compose inside
the single-writer gate) that `epic-mid-run-input` builds on.

## Outcome

For takka's inline-REPL users: a multi-second reply becomes readable while the
agent thinks, without breaking the cursor-safety model that made the mode
stable — signaled by scrollback staying clean on both finish and mid-stream
cancel.

## Requirements

- R1 (parent R2): Given a run is producing a long streamed answer, when tokens arrive, then the accumulated text is visible progressively in the inline REPL while the run continues, under the same cursor-safety model used today; and the completed answer still renders as its final styled pass.

## Constraints

- C1 (initiative) — All live rendering stays on the single-writer render gate + status painter; streamed text rides the proven erase → print → repaint cycle (the console-log hook's mechanism), never a second terminal writer.
- C3 (initiative) — Linux, macOS and Windows (conhost) keep working; the preview must not reintroduce the erase-then-redraw blink the in-place overwrite removed.

## Done when

- [ ] A streamed answer of tens of seconds is readable progressively in the inline REPL while the run continues.
- [ ] The answer appears in styled form exactly once at run end — rendered progressively as it streams (design decision A, 2026-11-05); no duplication and no unstyled re-render.
- [ ] A mid-stream Ctrl+C leaves clean scrollback (no stray status lines or half-drawn bands).
- [ ] The live-run rendering contract (painter + gate composition) is fixed in code and named where `epic-mid-run-input` can reference it.
- [ ] Full workspace checks green (`cargo fmt`, `clippy -D warnings`, `cargo test --workspace --all-targets` incl. e2e smoke). Production delivery: usable in a normal `takka` dev build on `dev`.

## Boundaries

- Belongs to epic-mid-run-input: anything about accepting or delivering typed
  text while the run is active.
- Belongs to epic-mid-run-input: changing how the engine surfaces steers or
  follow-ups (`Request::Steer` / queue are consumed as-is).
- Belongs to epic-tui-completions: the TUI's completion dropdowns.
- Must not change: the two-stage Ctrl+C cancel semantics; per-phase status-line
  content and phase accent colors; the tool timeline row shapes.

## References

- parent — [Requirements](../initiative-repl-ux-hardening.md#requirements) (line R2)
- source — `docs/ROADMAP.md`, section "Fase UX (open)"
- source — `CHANGELOG.md`, section [Unreleased] (status line / timeline landed this cycle)

## Notes

- Decision: 2026-11-05 — refinement picks confirmed by the user: (a) flush-granularity coalesces per renderer tick (80 ms), announcing pending `block_text` as whole lines; no flush = nothing on the terminal; (b) no reflow of already-printed preview lines on resize; only new lines measure the new width; (c) over-long streamed lines wrap once at the terminal width, single rule, same as the console-log hook; (d) preview stays plain accumulated text — final output remains the styled markdown render (the initiative's assumption, confirmed).
- Decision: 2026-11-05 — tracer bullet: story 1 prints accumulated `block_text` lines live through the painter gate (the thinnest path through every touched layer: chunk channel → renderer thread → painter → terminal).
- Decision: 2026-11-05 — breakdown approved as drafted (5 entries): 1.1 tracer first, 1.2/1.3/1.4 parallel after 1.1, 1.5 sweep closes.
- Decision: 2026-11-05 — streaming preview mechanism = design option A (progressive styled markdown via parse_partial; printed lines are the final answer); refined story 1.1 to 6 criteria, refined: true; the Done-when line above reworded accordingly (meaning unchanged).
- Decision: 2026-11-05 — declined a closing end-to-end suite story: per-entry runnable checks plus the existing e2e smoke suite cover it; the epic's Done when already includes full workspace checks.
