---
title: 'TUI mid-run input smokes — story 2.4 verification plan'
type: 'feature'
created: '2026-11-08'
---

# Additions to the story plan (tui smokes)

**Story 2.4 plan's Verification section (additions):**

- TUI smoke 1 (Interrupt): a `Commands` with a queued runtime mode via `with_queue_mode(Interrupt)`, a `test_events` tap; `App::submit_mid_run()` invoked with the mid-run buffer primed ("tui steering text"); assert the steer's request reaches the engine (via the captured events list) and the message log gains the `↳ steered in…` System line.
- TUI smoke 2 (FollowUp): the same with FollowUp mode; assert `pending_follow_ups` gains the queued line and the message log surfaces `↳ queued for the next run`.
