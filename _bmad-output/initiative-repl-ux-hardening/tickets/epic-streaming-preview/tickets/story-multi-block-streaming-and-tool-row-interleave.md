---
id: 3
type: story
title: "Multi-block streaming and tool-row interleave"
parent: epic-streaming-preview
covers: [R1]
after: [1]
hitl: false
risk: medium
---

# Multi-block streaming and tool-row interleave

## Description

Streamed text between tool calls flows in arrival order; tool rows and thinking blocks interleave through the same gate without eating preview lines or leaving the status line anywhere but last.

## Requirements

Verify: A scripted multi-turn run (text → tool → text → tool → text) interleaves blocks and tool rows in arrival order, and the final render contains all blocks exactly once.

## References

- parent — _bmad-output/initiative-repl-ux-hardening/tickets/epic-streaming-preview/epic-streaming-preview.md
