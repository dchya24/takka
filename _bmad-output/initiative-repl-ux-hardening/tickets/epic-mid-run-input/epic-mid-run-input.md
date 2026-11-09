---
type: epic
title: "Talk to the agent mid-run"
parent: "initiative-repl-ux-hardening"
covers: ["R1"]
after: ["epic-streaming-preview"]
refined: true
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

- R1 (parent R1): Mid-run input takes effect — Given an agent run is active (streaming or executing tools), When the user types a message in the inline REPL or the TUI, Then the text is delivered per `/queue` mode — steered into the running task (Interrupt) or queued as the next run (FollowUp) — And it takes effect at the next tool/run boundary, visibly to the user.

## Constraints

- C1 (parent C1) — All live rendering stays on the single-writer render gate + status painter; no second terminal writer; this epic only consumes the streaming contract (epic-streaming-preview settled it).
- C2 (parent C2) — Mid-run text reuses the engine steer/follow-up contract (`Request::Steer`, steering queue, follow-up queue, `--queue-mode`); no new wire protocol.
- C3 (parent C3) — Linux, macOS and Windows (conhost) keep working; the inline REPL and TUI changes are verified on the modes they touch.

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

## Notes

- Waits on epic-streaming-preview because: its closing stories fix the live-run rendering contract (painter + gate) this epic builds on. (Gate met 2026-11-06 — that epic closed with all five stories built.)
- Decision: 2026-11-06 — mid-run key reading for the inline path runs as ONE spawned crossterm-event reader task inside `Commands::run` for the run's lifetime; the TUI keeps its own loop and un-gates its loading-gated input instead (two surfaces, one client contract).
- Decision: 2026-11-06 — slash commands typed mid-run are OUT of this epic: the engine's steering source only takes text (`Request::Steer`), the REPL idle loop owns slash commands, and the adopted design's `pending_commands` idea is not wired here. Anything that needs it later becomes a new story.
- Decision: 2026-11-06 — queued (FollowUp) text is acknowledged locally through the log-writer hook notice (no engine event exists), while steered (Interrupt) text rides the engine's own `SteerQueued` event.
- Decision: 2026-11-06 — the breakdown closes with the default "Refactor sweep" story (more than three entries); no separate end-to-end suite story (the e2e smoke suite already covers the assembled flow; per-entry runnable checks cover the rest).
- Decision: 2026-11-06 — tracer bullet: story 2.1 is the thinnest path through every layer (reader task → client contract → engine steer → visible acknowledgement → boundary drain), verified end-to-end against the scripted provider; follow-up mode and TUI land after.
- Open question: (none — the epic inherits R1's wording and the adopted design's semantics; the one ambiguity, "visibly effected" for a steered message, is covered by the engine's Steered/boundary events and pinned by story 2.3).
