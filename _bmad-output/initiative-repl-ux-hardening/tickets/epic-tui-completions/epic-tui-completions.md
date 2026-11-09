---
type: epic
title: "TUI completion parity"
parent: "initiative-repl-ux-hardening"
covers: ["R3"]
after: ["epic-mid-run-input"]
refined: true
status: done
risk: low
---

# TUI completion parity

## Description

The TUI offers the input conveniences the inline REPL already has: typing `/`
opens a slash-command completion list, typing `@` opens file-path completion;
both are navigable (Up/Down), accept a choice to fill the input (Tab/Enter),
and dismiss (Esc). Reference implementation and behavior contract live in the
inline REPL's completion code; this epic ports the interaction to the TUI's
full-screen widget stack.

## Outcome

For takka TUI users: discoverable commands and file references without leaving
the panel — signaled by completion lists appearing and filling input exactly
as the REPL's do.

## Requirements

- R1 (parent R3): TUI completion parity — Given the user is typing in the TUI input (idle or mid-run under the epic-mid-run-input dispatcher contract), When they type `/` or `@`, Then a completion list (slash commands / file paths) appears and is navigable, And accepting a choice fills the input, as the inline REPL does today.

## Constraints

- C1 (parent C1) — All live rendering stays on the single-writer TUI frame; the dropdown is drawn inside the same ratatui frame, no second terminal writer.
- C3 (parent C3) — Linux, macOS and Windows (conhost) keep working; the TUI key-dispatch changes are verified on the modes they touch.

## Done when

- [ ] In the TUI (idle state), `/` opens the command completion list and `@` opens file completion; navigation (Up/Down), accept-fill (Tab/Enter) and dismissal (Esc) match the inline REPL's behavior — the existing code is kept and verified.
- [ ] Completion works in the mid-run input state: typed text during an active run opens and filters the dropdowns, accepting fills the mid-run input, and Enter with an open dropdown fills instead of submitting (the dispatcher contract from epic-mid-run-input holds while loading).
- [ ] Drift between the TUI and inline REPL completion behavior found by the audit is fixed or recorded as consciously dropped.
- [ ] Full workspace checks green incl. e2e smoke suite. Production delivery: usable in a normal `takka` dev build on `dev`.

## Boundaries

- Belongs to epic-mid-run-input: the mid-run key-dispatch contract this epic
  builds on (`after` names it); completion itself is idle- and run-state UI.
- Belongs to epic-streaming-preview: streaming rendering in the TUI (already
  streams today; untouched here).
- Must not change: the inline REPL's own completion behavior; the slash
  command set itself; `/models` model completion as implemented.

## References

- parent — [Requirements](../initiative-repl-ux-hardening.md#requirements) (line R3)
- source — `docs/tui-feature-gap.md`, sections G-01, G-02
- companion — [memlog](.memlog.md)

## Notes

- Waits on epic-mid-run-input because: its stories settle the TUI key dispatcher changes (mid-run input handling) this epic builds on. (Gate met 2026-11-08 — that epic closed with all five stories built.)
- Source conflict: 2026-11-09 — `docs/tui-feature-gap.md` G-01/G-02 (gap doc: the TUI lacks `/` and `@` completion) vs the code: `takka-cli/src/tui/dropdown.rs` (command/file/model/skill dropdowns, gitignore-aware file search) plus key interception in `tui/app.rs` and the rendered overlay in `tui/ui.rs` landed before this initiative (present in pre-rename state `381af4d`). The idle-state premise is already implemented; kept as verification only.
- Decision: 2026-11-09 — epic re-cut to the actual remaining gap: completions live for mid-run input (today `insert_mid_run_char` never reaches `update_dropdown`; `mid_run_input` is opaque to the dropdown engine) plus an idle parity audit against the inline REPL. G-01/G-02's needed state is treated as landed and verified, not rebuilt.
- Decision: 2026-11-09 — mid-run dropdowns follow the idle contract driven by `mid_run_input`: Up/Down navigate, Tab/Enter accept-fill into the mid-run buffer, Esc dismisses, and Enter with an open dropdown fills instead of submitting (`submit_mid_run`); typing outside a dropdown keeps the story-2.4 dispatch contract unchanged.
- Decision: 2026-11-09 — one story, no refactor sweep (three or fewer entries); tracer bullet is the whole epic: the thinnest path is wiring `mid_run_input` through the existing dropdown engine, verified with the idle-parity check in the same runnable review since all idle layers already exist and need no build step.
- Open question: (none — the epic inherits R3's wording; the one behavior choice, Enter-with-dropdown-open mid-run, was settled with the user 2026-11-09: fill, not steer.)
