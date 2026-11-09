---
id: 4
type: story
title: "Finish and mid-stream cancel cleanup"
parent: epic-streaming-preview
covers: [R1]
after: [1]
hitl: false
risk: medium
---

# Finish and mid-stream cancel cleanup

## Description

The run's terminal paths (natural finish and Ctrl+C) erase the status line exactly once and leave the printed preview lines as ordinary scrollback — no stray bands, no lost last line.

## Requirements

Verify: Ctrl+C at each stream phase (thinking, preview, tool execution) leaves clean scrollback with already printed text intact.

## References

- parent — _bmad-output/initiative-repl-ux-hardening/tickets/epic-streaming-preview/epic-streaming-preview.md
