# Backlog: orchestrator loop improvisation (5 items)

**Scope:** improve the agent loop in `takka-core/src/orchestrator/` — loop
detection, wind-down, compaction cadence, tool-error retry, plan-aware
execution. Each item independently shippable on its own `feature/*` branch
with a failing-first test.
**Baseline:** `dev` @ `232ff97`.
**Rule (AGENTS.md):** work lands on `feature/*` branches merged via PR into `dev`.
**Out of scope:** protocol changes, provider changes, session/checkpoint format
changes.

---

## O-01 — Semantic loop detection (thrash without progress)

**Evidence**

- `takka-core/src/orchestrator/mod.rs:368-424` — `record_tool_calls_for_loop_detection`
  only trips on byte-identical `(tool, args)` repeated `LOOP_DETECTION_THRESHOLD`
  (3) times consecutively. A thrash pattern (`read A → read B → read A → read B`,
  same file at shifting offsets, `ls` of the same dir) never trips it.
- Consequence seen in the wild: agent re-reads the same files across many
  iterations while making no edits; the only backstop is `max_iterations`
  forced finalization.

**Work**

1. Add a second detector alongside the exact-match one: same tool with
   *similar* args (same path ignoring line-offset params, same cwd for
   `ls`/`run_command` families) N times within a window, or K consecutive
   read-only iterations with zero `write`/`edit` while the user asked for a
   change.
2. On trip: emit a steering nudge ("you keep re-reading X, decide or ask"),
   not a hard abort — keep the hard abort only for the exact-match case.
3. Keep per-turn de-dup semantics (rule 2 in the doc comment).

**Done when:** a scripted turn that alternates two reads 4x triggers the nudge
event; a legit read-after-edit sequence does not.

---

## O-02 — Graduated tool-budget shaping above 80% of max_iterations

**Evidence**

- `takka-core/src/orchestrator/run.rs` (`approaching` branch, both sync and
  stream loops) — above 80% the loop only appends a transient `wind_down_message`
  and emits a UI notice. The model routinely ignores it; the 80%→100% cliff
  then strips all tools at once (forced finalization).
- `approaching_limit` (`mod.rs:474`) is a pure threshold with no staged action.

**Work**

1. Stage the wind-down: e.g. ≥80% strip high-cost read-only tools (full-file
   reads) from `tool_defs`, keep edits; ≥90% keep only `write`/`edit`;
   100% unchanged (strip all, force text answer).
2. Announce each stage via the existing `Event::System` channel so the UI can
   display it.

**Done when:** a run with a stub provider records shrinking `tool_defs` at
each stage; finalization behavior unchanged.

---

## O-03 — Iteration-cadenced compaction

**Evidence**

- `maybe_autocompact` (`compaction.rs:25`) triggers only on token budget.
  A 40-iteration loop of small tool results never compacts, but the context
  fills with stale patterns the model keeps re-deriving from.
- `DEFAULT_KEEP_RECENT_TOOL_RESULTS` (6) keeps recent results verbatim with no
  notion of loop age.

**Work**

1. Add a light pass every N iterations (e.g. 10, configurable): collapse tool
   results older than N iterations to one-line summaries (tool name + outcome),
   keeping the existing verbatim window for the newest K.
2. Coordinate with `maybe_autocompact` so the two never fight (cadenced pass
   is a no-op right after a full compact).

**Done when:** a 30-iteration stub run shows old tool results collapsed while
the newest K stay verbatim; a post-compact iteration skips the pass.

---

## O-04 — One automatic retry for transient tool errors

**Evidence**

- Tool error → recorded in memory → model often retries the identical call →
  exact-match loop detector trips → abort. No retry layer exists between the
  tool registry and the loop.
- Example transient shapes: file-read timeouts, flaky command exits
  (non-deterministic stderr/exit code on retry).

**Work**

1. Retry once, inside the loop (not the model): on a tool result classified
   transient (timeout marker, or allowlist per tool), re-execute once with the
   same args before recording to memory.
2. Record both attempts as one history entry annotated "auto-retry failed" so
   the model sees failure, not an invitation to repeat it.
3. Persistent errors (file not found, schema validation) never retry.

**Done when:** a stub tool failing transiently-then-succeeding records one
annotated entry and the loop continues; a persistent error records once with
no retry.

---

## O-05 — Optional plan-then-execute for multi-step tasks

**Evidence**

- The loop is purely reactive: every iteration re-asks the model "what next?".
  Multi-step tasks (explore → edit → verify) re-derive the plan each turn.
- Checkpoint already reserves `plan` and `task_state` slots (`session.rs`,
  `checkpoint.rs`), but the loop never reads or writes them.

**Work**

1. Opt-in first-iteration step: ask the model for a short step list, store in
   the existing `plan`/`task_state` slots, surface as a checklist event.
2. Following iterations: prepend compact plan state (done/current) to the
   request instead of re-deriving; update on tool completion.
3. Off by default (single-shot questions must not pay the extra call);
   enable via config flag and/or a model-declared need ("this needs N steps").

**Done when:** a 3-step stub task shows one plan call + checklist updates, and
resumes from checkpoint with plan intact; single-step runs make zero extra
calls with the flag off.
