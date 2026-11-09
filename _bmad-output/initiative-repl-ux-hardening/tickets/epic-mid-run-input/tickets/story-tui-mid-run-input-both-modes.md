---
id: 4
type: story
title: "TUI mid-run input (both modes)"
parent: epic-mid-run-input
covers: [R1]
after: [1]
hitl: false
risk: medium
---

# TUI mid-run input (both modes)

## Description

The TUI un-gates text input while `is_loading`: typed text routes through the same client contract (steer or local queue per `/queue`), lands in the message log, and the shared client-side drain depth guard takes effect here too.

## Requirements

Verify: A scripted run in the TUI accepts typed text in both queue modes; the message log shows the queueing/steering and the hand-over lands in the context like the inline path.

## References

- parent — _bmad-output/initiative-repl-ux-hardening/tickets/epic-mid-run-input/epic-mid-run-input.md
