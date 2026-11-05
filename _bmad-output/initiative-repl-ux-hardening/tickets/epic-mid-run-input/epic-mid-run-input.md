---
type: epic
title: "Talk to the agent mid-run"
parent: "initiative-repl-ux-hardening"
covers: ["R1"]
after: [{ epic = 1, needs = "the live-run rendering contract settled in the single-writer status painter and gate" }]
refined: false
risk: medium
---

# Talk to the agent mid-run

## Description

Text typed while an agent run is active is no longer swallowed: in the inline
REPL and in the TUI it is delivered to the engine's existing steer/follow-up
contract — steered into the running task (`Interrupt`) or queued as the next
run (`FollowUp`) — and visibly takes effect at the next tool/run boundary. The
engine contract (`submit_user_text`, `Request::Steer`, `--queue-mode`) is
consumed as-is; the work is the UI plumbing that routes typed keys to it during
a run, plus the visible confirmation of what happened to the text.

## Outcome

For takka users: a two-way conversation is possible while the agent works —
context or corrections typed mid-run arrive instead of vanishing — signaled by
typed text visibly steering or queueing per mode in both UIs.

## Requirements

<!-- Refinement fills these from parent R1 (initiative Requirements line 1: R1). -->

## Constraints

<!-- Refinement fills from the initiative's C2 (reuse the steer/queue contract; no new wire protocol), C1 (no second terminal writer), C3 (platforms); story-level ones beyond that go per ticket. -->

## Done when

- [ ] In the inline REPL, text typed mid-run steers (Interrupt) or queues (FollowUp) per `/queue` mode and takes effect at the next boundary, visibly.
- [ ] The same holds in the TUI (its mid-run key handling routes through the same contract).
- [ ] The existing client/engine steer + follow-up tests stay green; new UI-path tests cover delivery on both queue modes.
- [ ] Boundary events land as existing `SteerQueued`/run events so the status line/timeline shows the hand-over.
- [ ] Full workspace checks green incl. e2e smoke suite. Production delivery: usable in a normal `takka` dev build on `dev`.

## Boundaries

- Belongs to epic-streaming-preview: how streamed text renders while a run is
  active (this epic consumes that contract, `after` names it).
- Belongs to epic-tui-completions: the TUI's completion dropdowns.
- Must not change: the engine wire protocol; `/queue` default; two-stage
  Ctrl+C cancel; the confirmation/`question` prompt flow (their stdin-ownership
  gate is what mid-run input must obey).

## References

- parent — [Requirements](../initiative-repl-ux-hardening.md#requirements) (line R1)
- source — `docs/ROADMAP.md`, section "Fase UX (open)"
- source — `docs/tui-feature-gap.md`, section "Real-time streaming" row (TUI mid-run input currently gated by `is_loading`)
- adopted design — `docs/superpowers/specs/2026-08-06-interactive-live-progress-and-steering-design.md` (Fase 2b steer/queue contract this epic wires into)
- companion — [memlog](.memlog.md)
