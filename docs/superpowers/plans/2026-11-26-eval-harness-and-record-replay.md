# Eval harness + record/replay provider

**Status:** proposed · **Baseline:** `dev` @ `820ba71` (v0.7.0)
**Rule (AGENTS.md):** each step lands on a `feature/*` branch via PR into `dev`,
failing-first test, no `.github/` changes without asking.

## Why

v0.7.0 added a lot of loop behaviour (semantic loop detection P-01, transient
retry P-02, graduated budget shaping P-06, spend ceilings H-06/P-07). Nothing
measures whether these help. Tests exist per-feature (`ScriptedProvider` in
`takka-core/tests/support/mod.rs`) but there is no end-to-end signal for
"did a prompt/orchestrator change make the agent better or worse".

Two deliverables, in dependency order:

1. **Record/replay provider** (`CassetteProvider`) — deterministic, free,
   hermetic provider for tests and CI.
2. **Eval harness** (`takka-eval`) — runs a fixed task suite headlessly,
   verifies outcomes with commands, reports success rate / iterations / cost /
   loop-detector hits, and diffs against a stored baseline.

## What exists today (seams we build on)

| Seam | Where | Note |
|---|---|---|
| `LLMProvider` trait (`chat`, `chat_stream`) | `takka-core/src/providers/mod.rs:308` | Wrapper-friendly: `Arc<dyn LLMProvider>` is injected into `Orchestrator::new` and `Engine::with_provider` (`runtime/engine.rs:180`). |
| `ScriptedProvider` | `takka-core/tests/support/mod.rs` | `chat()` only; `chat_stream()` returns an error, so it cannot exercise `run_stream`. |
| `ChatUsage` incl. cache tokens | `providers/mod.rs:259` | Already carries what the harness needs for cost/cache metrics. |
| `Event` stream | `takka-core/src/events.rs` | `Usage`, `ToolFinished`, `TurnLimitExceeded`, `CostLimitExceeded`, `System` (nudges) — enough to derive metrics without touching the orchestrator. |
| Pricing | `takka-core/src/pricing.rs` | Reuse for USD per task. |
| Hermetic config helper | `hermetic_runtime_config` in `takka-cli/src/commands.rs` | Mandatory pattern: never read `~/.config/takka/config.json`. |

## Part A — Record/replay provider

### Design
- New `takka-core/src/providers/cassette.rs`, exported from `providers`.
- `CassetteProvider { inner: Option<Arc<dyn LLMProvider>>, mode, path }`.
  - `Mode::Record` — delegate to `inner`, append each exchange to the cassette.
  - `Mode::Replay` — serve exchanges in order; error loudly when exhausted or
    when the request does not match.
  - `Mode::Auto` — replay if the cassette exists, else record (local dev only).
- Cassette = JSONL, one exchange per line:
  `{ seq, request_fingerprint, kind: "chat"|"stream", response | chunks[] }`.
- **Fingerprint**: hash of (model, last N messages' role+content, tool names).
  Strict by default (mismatch = error with a diff of what changed); `--loose`
  replays by sequence only. Strict mode is what makes prompt changes visible.
- **Streaming**: record the `ChatChunk` sequence and replay it as a stream, so
  `run_stream` and the daemon path are covered (the gap in `ScriptedProvider`).
- **Secrets**: only request/response bodies are stored, never headers or config.
  Add a scrub pass (API-key-shaped strings, `$ENV` values) before writing.

### Steps
- **A1** `CassetteProvider` replay-only + unit tests (sequence, mismatch error,
  exhaustion error). *Done when:* an existing orchestrator integration test is
  ported from `ScriptedProvider` to a cassette fixture and still passes.
- **A2** Streaming replay. *Done when:* a golden-event test over `run_stream`
  passes from a cassette (compare `runtime_engine.rs` golden test style).
- **A3** Record mode + scrubber + fingerprint strictness. *Done when:* record →
  replay round-trip test; scrubber test with a fake key.
- **A4** `takka` flag/env for dev use (`TAKKA_CASSETTE=path`, mode via env), kept
  out of the user-facing docs unless we decide to expose it.

## Part B — Eval harness

### Design
- New workspace crate `takka-eval` (binary + library). Not part of the shipped
  `takka` binary; keeps `takka-core` free of eval-only dependencies.
- **Task spec** (`evals/tasks/<id>/task.json` + `repo/` fixture):
  ```json
  {
    "id": "fix-off-by-one",
    "prompt": "The test in tests/pagination.rs fails. Fix the bug.",
    "tags": ["bugfix", "rust"],
    "max_iterations": 20,
    "max_cost_usd": 0.50,
    "verify": {
      "command": "cargo test --quiet",
      "forbid_paths": ["tests/"]
    }
  }
  ```
- **Runner**: copy fixture to a temp dir, build a hermetic engine config
  (cwd = temp dir, `permission_mode = yolo`, safety sandbox paths = temp dir),
  inject provider (cassette in replay, real provider in live), run the prompt,
  collect events, then run `verify.command` independently of the agent.
- **Metrics per task** (derived from events, no orchestrator changes):
  pass/fail, iterations, tool calls by name, tokens in/out, cache read ratio,
  USD, wall time, loop-nudge count, auto-retry count, limit hit
  (`TurnLimitExceeded` / `CostLimitExceeded`), forbidden-path edits.
- **Output**: `results.jsonl` (machine) + table summary; `--baseline file` diffs
  pass-rate and median cost/iterations and exits non-zero on regression
  thresholds.
- **Two lanes**:
  - *replay* — cassettes checked in, deterministic, runs in CI/`cargo test`.
    Verifies harness plumbing and catches fingerprint drift (prompt changed →
    re-record required, visible in the PR).
  - *live* — real provider, run manually or nightly; produces the numbers that
    actually compare model/prompt/orchestrator changes. Never in default CI.

### Steps
- **B0 (done)** Spike (½ day): confirm the cleanest headless entry point —
  `Engine::with_provider` over an in-memory `Transport` vs `Orchestrator::run`
  directly. Write findings to this doc. *Done when:* a 20-line example runs one
  scripted task in a temp dir and prints its events.
- **B1** `takka-eval` crate skeleton: task loader, temp-dir fixture copy,
  runner, verify step, `results.jsonl`. One trivial task + cassette. *Done
  when:* `cargo run -p takka-eval -- --lane replay` passes on that task.
- **B2** Metrics extraction from the event stream + summary table + unit tests
  over canned event sequences.
- **B3 (done, with caveats below)** Seed suite of 8–12 tasks covering: bugfix, small feature, refactor,
  multi-file edit, "needs exploration", a task that tempts loops (should trigger
  P-01 nudge), a transient-failure task (P-02), and a budget-tight task (P-06).
  Record cassettes with one real model.
- **B4** Baseline compare (`--baseline`, regression thresholds) + first
  committed baseline.
- **B5** Live lane (`--lane live --model provider/id`), cost guard using
  `max_cost_usd`, repeat count `--n` for variance.
- **B6** CI wiring for the replay lane — **touches `.github/`; ask first.**
  Until approved, document the command in `CONTRIBUTING.md`.

## B0 findings (spike result)

- `Engine::initialize` hard-wires `OpenAIProvider` from the ambient config and
  reads project instructions, skills and persistent memory from the developer's
  machine. It is the wrong entry point for a hermetic eval.
- Drive `Orchestrator` directly: inject any `Arc<dyn LLMProvider>`, register
  `builtin_tools_with_deps(ToolDeps::new())`, `PermissionMode::Yolo`, a prompt
  from `assemble_system_prompt(None, None, None, None)`, and collect `Event`s
  via `on_event` (events are `Serialize`).
- Builtin tools resolve relative paths against the **process cwd**, which is not
  safe to change in a shared process. Each task therefore runs in a **child
  process** (`takka-eval child`, re-exec of the same binary) with the temp
  workdir as cwd. The parent verifies independently and derives metrics from the
  child's serialized events.
- Task specs are `task.json`, not TOML: no new dependency, and `serde_json` is
  already in the workspace.
- Seed tasks use POSIX `sh` fixtures so verification is fast and needs no
  per-task toolchain build.

## B3 outcome and caveats

- Nine `sh`-fixture tasks ship in `evals/tasks/`: bugfix, new-file feature,
  flag feature, multi-file edit, rename refactor, exploration, error recovery,
  a loop-thrash task (`expect.min_loop_nudges`), and a tight-budget task
  (`expect.max_iterations_used`).
- **Cassettes are hand-authored, not recorded.** Recording needs a real provider
  and credentials, which must not be used from an unattended session. They
  exercise the harness and orchestrator wiring; they say nothing about model
  quality. Run the live lane (B5) to get real numbers, and re-record with
  `CassetteProvider::record` when a cassette should reflect a real model.
- **No seed task for transient auto-retry (P-02).** It cannot be triggered
  deterministically offline: `run_command` timeouts come back as an `Ok` JSON
  result (not `Tool error:`), and `fetch` connection failures surface reqwest's
  top-level message only ("error sending request for url ..."), which the
  transient classifier's markers do not match. P-02 stays covered by the core
  stub-tool test. The `fetch` wording gap is a likely real bug worth its own
  fix (`providers::reqwest_chain` already shows the pattern).

## Order and size

A1 → A2 → B0 → B1 → B2 → A3 → B3 → B4 → B5 → A4 → B6.
Roughly: Part A ≈ 3–4 small PRs, Part B ≈ 5–6 small PRs.

## Risks / open questions
- Strict fingerprints make cassettes brittle: every prompt tweak forces
  re-recording. Mitigation: `takka-eval --rerecord <task>` and a loose mode.
- LLM nondeterminism in live lane: use `--n` and report variance rather than a
  single pass/fail.
- Fixture tasks that need network (`web_search`, `fetch`) are excluded from the
  seed suite.
- Windows: verify commands are shell-specific; seed suite targets Unix first.
