---
id: 1
type: story
title: "Tracer: typed text mid-run reaches the steer contract (inline, Interrupt)"
parent: epic-mid-run-input
covers: [R1]
after: [epic-streaming-preview]
hitl: false
risk: medium
---

# Tracer: typed text mid-run reaches the steer contract (inline, Interrupt)

## Description

A spawned reader task inside `Commands::run` collects typed text while a run is active in the inline REPL; the first Enter delivers it per the client contract — in Interrupt mode it goes to the engine as `Request::Steer`, `SteerQueued` shows on screen, and the boundary drain puts the text into the LLM context + memory (persist) at the next tool/run boundary; `takka run` rides the same path.

## Requirements

Verify: Against the scripted multi-turn provider, typing text during turn 2 (before its tool call) surfaces the steer acknowledgement on screen and the completed turn states the steered text; `takka run` accepts the same mid-run text.

## References

- parent — _bmad-output/initiative-repl-ux-hardening/tickets/epic-mid-run-input/epic-mid-run-input.md
