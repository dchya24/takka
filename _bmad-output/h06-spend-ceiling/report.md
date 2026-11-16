# H-06 — Spend ceiling (`agent.max_cost_usd`) — report

**Status:** DONE
**Branch:** `feature/h06-spend-ceiling` (base `origin/dev` @ `6ac3330`)
**Commits:** 7 (see below). Not pushed, not merged.

## What was implemented

### Config key

`takka-core/src/config.rs` → `AgentLoopConfig::max_cost_usd: Option<f64>`
(`#[serde(default)]`). Unset = `None` = unlimited = unchanged behaviour.

Verified against current `config.rs` (spec line references had drifted as
warned). The surrounding mechanism is `max_iterations`, applied at
`RuntimeEngine::initialize` — the ceiling follows it exactly:

```rust
orchestrator.set_max_cost_usd(config.agent.max_cost_usd);
```

Re: "configOptions" — checked `takka-runtime/src/acp/session_config.rs`: the
ACP `configOptions` surface only exposes the `model` and `mode` selects, no
`agent.*` settings. Nothing to keep in sync. I also added the key (`null`) to
the `agent` block of `takka-cli/config/default.json` so the template documents
it, same as `max_iterations`.

### Event

`Event::CostLimitExceeded { max_cost_usd: f64, spent_usd: f64 }`, wire tag
`cost_limit_exceeded`, camelCase payload (`maxCostUsd` / `spentUsd`), in
`EventType` and `event_type()`, plus a wire-tag round-trip test mirroring
`turn_limit_exceeded_serializes_with_wire_tag`.

### Where accumulation happens

`takka-core/src/orchestrator/run.rs`, in **both** loops (sync `run*_inner` and
`run_stream*_inner`), immediately after the `Event::Usage` emission:

```rust
let cost_limit_reached = Self::accumulate_usage_cost(self, response.usage.as_ref());
```

- `reset_run_cost()` at the top of each run → the ceiling bounds **one user
  turn**, like `max_iterations`, not a session.
- `accumulate_usage_cost` prices the report with the new
  `takka_core::pricing::lookup(model)` table and calls `record_run_cost`.
- The accumulator lives behind a `Mutex` in `Orchestrator` (`run_cost_usd:
  Mutex<f64>`) because runs take `&self` — `AgentRuntime`/`RuntimeEngine` clone
  the orchestrator's `Arc` into their run thread, so a `&mut` loop signature
  would have forced a structural change outside this task. `set_max_cost_usd`
  stays `&mut` (host-side, between runs), matching `set_max_iterations`.

**Stop point — not mid-stream:** `cost_limit_stop(crossed)` is only called at
turn *completion* boundaries:
1. after `handle_tool_calls` / `handle_tool_calls_parallel` — the crossing
   turn's tool batch still executes;
2. on a final text answer, after the assistant message lands in memory.

Both return `Ok(...)`: a budget stop is a policy outcome, not a failure, so
`SessionCompleted` (not `SessionFailed`) is emitted by the lifecycle envelope
and the paid-for answer is delivered.

Documented decisions (and why):
- **`>=` comparison**: a ceiling of exactly the reported spend counts as
  reached.
- **Non-positive ceiling = unlimited** (`set_max_cost_usd` filters `<= 0`):
  `max_cost_usd: 0` cannot mean "stop before the first token", so it is
  treated as unset rather than making the agent unusable.
- **Fail-open on unpriced/absent usage**: an unknown model or a provider that
  reports no `Usage` accumulates nothing and never trips the ceiling. Inventing
  a price could stop a run the user did not overspend. Pinned by
  `missing_usage_never_trips_the_ceiling`.
- **New `takka-core/src/pricing.rs`**: the spec says cost is "recorded only",
  and indeed nothing in the tree priced usage (`capabilities.rs` only
  *mentions* a `pricing.rs`). A small mirror of the capability registry was
  the minimal way to get a dollar figure: list prices in USD per million
  tokens, exact-then-longest-`contains` fallback, cache read/write prices where
  vendors publish them.

### CLI / TUI

New shared module `takka-cli/src/widgets/budget.rs` (chip + stop notice, so
REPL/TUI cannot drift):

- **Status bar (REPL, both bars)** — `chip` reads `agent.max_cost_usd` via new
  `Commands::max_cost_usd()` and the run's spend from a new
  `SessionStats::run_cost_usd` field (f64 bits in an `AtomicU64`). The run's
  cost is priced in `commands/repl.rs` from the same `pricing` table and stored
  on `RunStats::cost_usd`, then pushed into the stats after each run.
  Renders `💵 $0.38 of $0.50 left`; **hidden entirely when unlimited**.
- **TUI header** (`tui/ui.rs` → `draw_header`): `App::budget_chip()` reads the
  same config + `SessionStats::run_cost_usd` (refreshed on `TaskComplete`).
- **Event rendering**: `commands/event.rs` `render_event` maps the stop event
  to a warning badge with the actionable hint (`raise or remove
  agent.max_cost_usd`). The TUI gets a dedicated `AppMessage::CostLimit`
  variant forwarded from both TUI event closures and lands in the log as a
  `MessageRole::System` notice (not a red error).
- **Runtime/ACP** (`takka-runtime/src/acp/translate.rs`): ACP v1 has no stop
  reason for a budget cut, so the event becomes an `agent_message_chunk`
  naming ceiling, spend and the key — not `Error`, which would turn a policy
  stop into a failed prompt.

### Docs

`docs/CONFIGURATION.md`: the file had no `agent` section at all, so I added one
documenting `max_iterations` and `max_cost_usd` (per-turn accounting, stop
after the crossing turn, unlimited default, unknown-model fail-open, status-bar
chip, how to lift the ceiling), plus the key in the top-level JSON sketch.
English. No API keys mentioned.

## Tests

Spec-mandated cases (all in `takka-core/tests/orchestrator_loop.rs`, real
orchestrator + scripted provider, `claude-3-5-sonnet` for a known price):

| Spec case | Test |
| --- | --- |
| under budget runs to completion | `under_budget_run_completes_without_stopping` |
| over budget stops after the crossing turn, not mid-stream | `over_budget_stops_after_the_crossing_turn_completes` (asserts the tool result is in memory **and** the provider got exactly 1 request) |
| unset = unlimited | `unset_ceiling_is_unlimited` |
| stream path | `streaming_over_budget_stops_at_the_turn_boundary` (asserts the full stream arrived) |
| extra | `missing_usage_never_trips_the_ceiling`, `run_cost_resets_between_runs` |

Supporting coverage: `pricing.rs` (6 unit tests), `config.rs` (default +
round-trip), `events.rs` (wire tag), orchestrator accumulator unit tests (5),
`widgets/budget.rs` (4), TUI header chip (`ui.rs`), TUI notice
(`tui/app/mod.rs`), ACP mapping.

**Test strength verified by mutation**: disabling `cost_limit_stop` makes the
two over-budget tests fail; charging a dummy cost on missing usage makes
`missing_usage_never_trips_the_ceiling` fail.

## Verification

```
cargo fmt --all -- --check                        → OK
cargo clippy --workspace --all-targets -D warnings → OK (no warnings)
cargo test --workspace --all-targets              → 1101 tests, 0 failed
bash scripts/acp-conformance.sh                   → 24/24 checks passed
```

**Baseline note:** before any change the full suite had
`commands::daemon::daemon_smoke_tests::in_use_refusal_smoke` fail once (the
known busy-socket flake); rerun green as `AGENTS.md` allows. Final runs were
green without a rerun.

## Files changed

```
 docs/CONFIGURATION.md                 |  42 +
 takka-cli/config/default.json         |   1 +
 takka-cli/src/commands/event.rs       |  11 +
 takka-cli/src/commands/mod.rs         |   6 +
 takka-cli/src/commands/render.rs      |   3 +
 takka-cli/src/commands/repl.rs        |  14 +
 takka-cli/src/interactive/mod.rs      |  19 +-
 takka-cli/src/interactive/render.rs   |  29 +-
 takka-cli/src/interactive/repl.rs     |   4 +-
 takka-cli/src/tui/app/{input,messages,mod,sessions,slash}.rs
 takka-cli/src/tui/ui.rs               |  43 +
 takka-cli/src/widgets/mod.rs          |   1 +
 takka-cli/src/widgets/budget.rs       | new, 96
 takka-core/src/config.rs              |  28 +
 takka-core/src/events.rs              |  28 +
 takka-core/src/lib.rs                 |   2 +
 takka-core/src/orchestrator/mod.rs    | 130 +
 takka-core/src/orchestrator/run.rs    |  87 +-
 takka-core/src/pricing.rs             | new, 230
 takka-core/src/runtime/engine.rs      |   3 +
 takka-core/tests/orchestrator_loop.rs | 248 +
 takka-core/tests/support/mod.rs       |  19 +
 takka-runtime/src/acp/translate.rs    |  39 +
```

## Self-review notes / concerns

1. **Per-run, not per-session, ceiling.** The spec says "accumulate cost; when
   the ceiling is crossed", without fixing the scope. I chose per user turn,
   for consistency with `max_iterations` (same config block, same
   "one user turn is bounded" semantics in `TurnLimitKind::MaxTurnRequests`)
   and because it is the only scope that can be reset deterministically from
   `&self` runs. Per-session budgets were explicitly out of scope. Documented in
   `CONFIGURATION.md` and the code. **Flagging in case you meant per session.**
2. **User-facing string on a path the spec does not name.** The budget chip in
   the REPL's prompt status bar is only repainted between turns, so it
   reflects the *last completed run's* spend; the TUI header behaves the same
   way. A live mid-run counter would need per-usage-event plumbing into the
   renderer, which the spec did not ask for.
3. **`pricing.rs` table** is a small, curated list of list prices (OpenAI,
   Anthropic, DeepSeek, GLM). Prices drift; an unmaintained table under-prices
   and therefore stops late, never early. No persistence, no overrides —
   consistent with YAGNI.
4. **Fail-open choice** (unpriced model / no usage ⇒ no stop) is a safety
   trade-off, called out above and covered by a test.
5. Nothing outside the task was touched; no `.github/`, no keys, no
   pre-existing working-tree changes existed in this worktree at start (the
   tree was clean at `6ac3330`), and it is clean now.

## Commits

```
90b7b1b feat(core): add CostLimitExceeded stop event
e42a94f feat(core): add a model list-price table
d8e7e57 feat(core): add the agent.max_cost_usd config key
ea32fa9 feat(core): stop the agent loop at agent.max_cost_usd
350479a feat(cli): show the remaining spend budget and the stop notice
b30652b feat(runtime): surface a spend-ceiling stop to ACP clients
5895314 docs: document agent.max_cost_usd
```