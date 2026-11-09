---
id: 2
type: story
title: "Preview line rules: wrap and resize"
parent: epic-streaming-preview
covers: [R1]
after: [1]
hitl: false
risk: low
---

# Preview line rules: wrap and resize

## Description

Over-long streamed lines wrap once at the terminal width (same rule as the console-log hook); lines already printed are never reflowed after a resize, only new lines measure the new width.

## Requirements

Verify: A 300-char line prints wrapped with no ghost columns; resizing mid-stream leaves printed lines untouched and new lines measure the new width.

## References

- parent — _bmad-output/initiative-repl-ux-hardening/tickets/epic-streaming-preview/epic-streaming-preview.md
