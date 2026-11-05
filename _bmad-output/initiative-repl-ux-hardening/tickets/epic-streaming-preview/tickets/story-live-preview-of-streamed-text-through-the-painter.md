---
id: 1
type: story
title: "Live preview of streamed text through the painter"
parent: epic-streaming-preview
covers: [R1]
after: []
hitl: false
risk: medium
refined: true
---

# Live preview of streamed text through the painter

## Description

During a run, the renderer prints each streamed markdown chunk as styled lines
above the status line (progressive preview), and the printed lines stand as the
final answer — no separate markdown re-render at finish.

## Requirements

Verify: Once per run, run the REPL against a scripted streaming provider and
capture the terminal.

1. **Progressive styled output** — Given a scripted provider streaming
   markdown chunks with delays, when the run is active, then each chunk
   renders (via `markdown::parse_partial`) and prints above the status line
   within two renderer ticks of arrival.
2. **Open-construct safety** — If a chunk ends inside an unfenced code block,
   the preview closes it temporarily (`close_unclosed_fence`) so no later
   non-code text can render with code styling; the fence closes properly once
   the remainder arrives.
3. **The answer appears exactly once** — at run end, nothing re-prints or
   re-styles the already printed answer; the streamed styling is the final
   state.
4. **Status line stays last** — while the preview prints, the status line
   remains the bottom-most line and keeps updating (phase, clock, tokens) the
   way it does today.
5. **No-chunk fallback** — for a run producing no streamed chunks
   (non-streaming provider), nothing prints live; the single styled markdown
   render at the end matches today's behavior exactly.
6. **Mid-stream cancel leaves the preview** — Ctrl+C during streaming keeps
   the already printed styled lines as ordinary scrollback and erases only the
   status line (full cleanup coverage is story 1.4's).

## Boundaries

- Must not change: the status painter's content and phase accent colors; the
  tool timeline row shapes; the thinking header's DIM style (a sibling block
  that interleaves with the preview — story 1.3 covers their order).
- Must not change: the two-stage Ctrl+C cancel semantics; the
  confirmation/`question` prompt flow and its stdin-ownership gate; the log
  line hook (it shares the erase→print→repaint cycle this story reuses).

## References

- parent —
  `_bmad-output/initiative-repl-ux-hardening/tickets/epic-streaming-preview/epic-streaming-preview.md` (Requirements R1, Constraints C1/C3)
- decision — initiative `.memlog.md`, 2026-11-05: streaming preview design pick
  A (progressive styled render, no final re-render); the initiative's assumption
  (d) is superseded by this dated decision
- design — this story replaces the duplicate-render risk by rendering the
  answer once, in flight: the epic's Done when ("no duplication, no raw-text
  remnant") is satisfied by not re-rendering, not by erasing scrollback

## Notes

- Decision: 2026-11-05 — user confirmed design option A: the preview renders
  styled markdown progressively and the printed lines are the final answer
  ("the answer appears exactly once"); the final styled markdown pass exists
  only on the no-chunk fallback path.
- Decision: 2026-11-05 — `(refine = true)` set while the criteria were drafted;
  cleared against the user's approval of the criteria above.
