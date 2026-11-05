---
type: initiative
title: "REPL/TUI UX hardening: live streaming, mid-run input, TUI completion parity"
sources:
  - "docs/ROADMAP.md"
  - "docs/tui-feature-gap.md"
  - "CHANGELOG.md"
companions: []
risk: medium
---

# REPL/TUI UX hardening: live streaming, mid-run input, TUI completion parity

## Description

takka's agent engine already speaks steer and follow-up (`Request::Steer`, `--queue-mode`), and the inline REPL gained a live status line, tool timeline and measured usage this cycle — but the daily-use surface still has three gaps the sources name. While a run streams, the answer text only appears after it completes; text typed during a run is silently swallowed by both UIs; and the TUI lacks the slash-command and `@`-file completion the inline REPL has. When this initiative is done, a long streamed answer becomes readable while it streams, a user can talk to the agent mid-run in either UI, and the TUI accepts completions like the REPL does — without undoing the cursor-safety work that made the interactive mode stable.

## Outcome

For takka's daily users and its own developers: the interactive surface (inline REPL and TUI) behaves like a live assistant rather than a batch tool — output streams as produced, input during a run is steered or queued instead of dropped, and both UIs share the same input conveniences. The sources' success signal here is the ROADMAP's own "Fase UX (open)" line closing and the TUI gap table (tui-feature-gap.md) turning to parity on completion and mid-run input.

## Requirements

(SPEC-derived from sources + session 2026-11-05; ids never renumbered — authoritative line log: `.memlog.md`.)

1. **Mid-run input takes effect** (R1)
   **Given** an agent run is active (streaming or executing tools)
   **When** the user types a message in the inline REPL or the TUI
   **Then** the text is delivered per `/queue` mode — steered into the running task (Interrupt) or queued as the next run (FollowUp)
   **And** it takes effect at the next tool/run boundary, visibly to the user

2. **Streaming reply is readable while it streams** (R2)
   **Given** a run is producing a long streamed answer
   **When** tokens arrive
   **Then** the accumulated text is visible progressively in the inline REPL while the run continues, under the same cursor-safety model used today
   **And** the completed answer still renders as its final styled pass

3. **TUI completion parity** (R3)
   **Given** the user is typing in the TUI input
   **When** they type `/` or `@`
   **Then** a completion list (slash commands / file paths) appears and is navigable
   **And** accepting a choice fills the input, as the inline REPL does today

## Constraints

- **C1** — All live rendering stays on the single-writer render gate + status painter; no second terminal writer is introduced. (commands.rs renderer; the corruption bugs it fixed)
- **C2** — Mid-run text reuses the engine steer/follow-up contract (`Request::Steer`, follow-up queue, `--queue-mode`); no new wire protocol. (ROADMAP Fase 2b)
- **C3** — Linux, macOS and Windows (conhost) keep working; console-mode and raw-mode changes are verified on the modes they touch. (CI matrix)

## Done when

- [ ] A streamed answer of tens of seconds is readable while it streams in the inline REPL, and the scrollback ends clean (no stray status lines) after the run or a mid-stream Ctrl+C. (R2)
- [ ] Text typed while a run is active, in the inline REPL and in the TUI, steers the run or queues as the next run per `/queue`, demonstrated end-to-end against a scripted provider. (R1)
- [ ] In the TUI, typing `/` opens command completion and `@` opens file completion; accepting a choice fills the input. (R3)
- [ ] Full workspace checks green: `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test --workspace --all-targets`, including the e2e smoke suite.

## Boundaries

- Out: Fase 3b multi-session daemon multiplexing and any protocol bump. (ROADMAP open work)
- Out: Windows named-pipe daemon transport. (ROADMAP open work)
- Out: SDK surface for other languages; package-manager distribution; code signing. (ROADMAP open work)
- Out: replacing the ratatui/shared-widget stack or the single-writer renderer design.
- Touch point: engine steer/follow-up contract (`Request::Steer`, follow-up queue, `--queue-mode`) — consumed as-is; owner: epic-mid-run-input
- Must not change: confirmation/`question` prompting behavior, the two-stage Ctrl+C cancel semantics, and the REPL/TUI session persistence contract.

## References

- source — `docs/ROADMAP.md`, sections "Fase UX (open)" and "Open work"
- source — `docs/tui-feature-gap.md`, sections G-01, G-02
- source — `CHANGELOG.md`, section [Unreleased] (this cycle's UX landed work)
- decision — session 2026-11-05: user picked this initiative over a release-prep initiative (memlog)

## Notes

- Assumption: while streaming, the reply is shown as accumulated plain text; the final render remains the existing styled markdown pass. (design pick at epic refinement)
- Decision: 2026-11-05 — user chose this initiative over "v0.6 release prep"; release-initiative remains the candidate next.
- Decision: 2026-11-05 — branch rule for this initiative's work: direct commits on `dev` allowed for this session; a `feature/*` + PR flow applies from the next session onward (recorded in `AGENTS.md` Policy).
- Decision: 2026-11-05 — no platform-baseline epic: the workspace, CI and e2e smoke suite already exist; Epic 1 opens with its tracer story (Tree check: 'or a Decision line saying why not').
