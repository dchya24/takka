---
id: 3
type: story
title: "Hand-over visibility and prompt-gate edges"
parent: epic-mid-run-input
covers: [R1]
after: [1, 2]
hitl: false
risk: medium
---

# Hand-over visibility and prompt-gate edges

## Description

The hand-over is visible in the status line/timeline per mode (SteerQueued → status; steered text applied → boundary row; follow-up → the queue notice), text typed while a mid-run confirmation/question prompt owns stdin stays with the prompt, and text arriving after the last turn of a run becomes the first message of the next run (adopted design §steer semantics).

## Requirements

Verify: A scripted run with a confirmation gate shows the prompt owns the typed text; a steer after the run's terminal event lands as the first user message of the next run; status line/timeline show the hand-over markers.

## References

- parent — _bmad-output/initiative-repl-ux-hardening/tickets/epic-mid-run-input/epic-mid-run-input.md
