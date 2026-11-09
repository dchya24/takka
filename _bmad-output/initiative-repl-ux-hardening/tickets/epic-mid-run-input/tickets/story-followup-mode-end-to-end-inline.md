---
id: 2
type: story
title: "FollowUp mode end-to-end (inline)"
parent: epic-mid-run-input
covers: [R1]
after: [1]
hitl: false
risk: low
---

# FollowUp mode end-to-end (inline)

## Description

Typed mid-run text in follow-up mode (the default) routes into the client-side follow-up queue instead of the engine, a log-hook notice acknowledges the queueing (single-writer discipline), and the existing post-run drain delivers it as the next run — verified through the drain depth guard and `/queue` mode switching.

## Requirements

Verify: A scripted run with mid-run typed text in follow-up mode shows the queue notice, finishes, and the queued text runs visibly as the next run (client-issued, depth-guarded); `/queue interrupt` then steers instead.

## References

- parent — _bmad-output/initiative-repl-ux-hardening/tickets/epic-mid-run-input/epic-mid-run-input.md
