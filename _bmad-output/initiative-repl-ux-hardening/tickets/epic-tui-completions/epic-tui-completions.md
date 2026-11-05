---
type: epic
title: "TUI completion parity"
parent: "initiative-repl-ux-hardening"
covers: ["R3"]
after: ["epic-mid-run-input"]
refined: false
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

<!-- Refinement fills these from parent R3 (initiative Requirements line 1: R3). -->

## Constraints

<!-- Refinement fills from the initiative's C1/C3; story-level ones beyond that go per ticket. -->

## Done when

- [ ] In the TUI, `/` opens the command completion list; `/@` opens file completion; navigation, accept-fill and dismissal match the inline REPL's behavior.
- [ ] Completion works in both idle and mid-run input states (the dispatcher contract from epic-mid-run-input holds while loading).
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

- Waits on epic-mid-run-input because: its stories settle the TUI key dispatcher changes (mid-run input handling) this epic builds on.
