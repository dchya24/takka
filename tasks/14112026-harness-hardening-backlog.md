# Backlog: harness hardening (audit 2026-11-14)

**Scope:** defects and maintainability debt found by auditing `takka-core` /
`takka-cli` / `takka-runtime` source (not the roadmap wishlist).
**Baseline:** `dev` @ `9b3442a` (PR #18 pending merge).
**Rule:** each item is independently shippable on its own `feature/*` branch and
needs a test that fails before the fix.

Explicitly **out of scope** here (decided elsewhere, do not reopen without user
sign-off): packaging (C), Fase 3b multiplexing, LSP integration, file watching.

---

## H-01 — Shell tools can hang the agent forever (P0)

**Evidence**

- `takka-core/src/tools/run_command.rs:61-83` — `execute()` uses
  `Command::output()` with no deadline; `run_command` schema (lines 29-55)
  exposes only `command` and `cwd`, so the model cannot even ask for a timeout.
- `takka-core/src/tools/run_command.rs:88-155` and
  `takka-core/src/tools/run_script.rs:209-217` — streaming path reads stdout to
  EOF, *then* stderr. A child that writes more than one pipe buffer (~64 KiB) to
  stderr while stdout stays open blocks forever: classic pipe deadlock.
- `takka-core/src/tools/run_script.rs:171` — `run_interpreter` takes
  `_timeout_secs` and never uses it, while the schema advertises
  `"timeout": { default: 30 }` (`run_script.rs:56-63`) and the tool description
  claims "Supports timeout" (`run_script.rs:35`).
- No cancellation: `run_command`/`run_script` never observe the orchestrator's
  `Arc<AtomicBool>` cancel flag (`grep -n cancel` on both files: no hits), so
  Ctrl+C does not stop a running child.
- Unbounded memory: `run_command` accumulates full stdout+stderr with no cap;
  `run_script` caps only *after* buffering everything (`run_script.rs:222-241`).

**Work**

1. One shared helper (e.g. `takka-core/src/tools/process.rs`):
   `run_bounded(cmd, deadline, cap_bytes, on_progress) -> Output` that
   - spawns the child, drains stdout **and** stderr on two reader threads
     (or `tokio` equivalent already used by the crate) to avoid the deadlock;
   - enforces a deadline: on expiry `child.kill()` + `wait()`, return a
     `ToolError`-shaped result telling the model the command was killed;
   - caps each stream at 64 KiB *while reading*.
2. `run_script`: honour the documented `timeout` (clamp, e.g. 1..=600 s).
3. `run_command`: add optional `timeout` param (same default/clamp) and document
   it in the schema.
4. Propagate cancellation: check the cancel flag in the drain loop, kill, and
   return a "cancelled" result instead of waiting out the deadline.
5. Keep the existing JSON contract (`success`, `exit_code`, `stdout`, `stderr`,
   plus `interpreter` for scripts) — the CLI and orchestrator depend on it.

**Acceptance**

- `cargo test -p takka-core` green, with new tests:
  `timeout_kills_child_and_reports`, `large_stderr_does_not_deadlock`
  (child writes ~1 MiB to stderr then sleeps), `output_capped_while_streaming`,
  `cancel_kills_running_child`.
- Manual: `takka run` on a prompt that runs `sleep 300` returns within the
  configured timeout and the process is gone (`pgrep sleep` empty).

**Effort:** ~0.5–1 day.

---

## H-02 — Mutex poisoning can cascade into a crash (P1)

**Evidence**

- 212 `.lock().unwrap()` sites in `src`; **93** of them outside `#[cfg(test)]`
  blocks. Hot spots: `takka-core/src/runtime/engine.rs` (20),
  `takka-core/src/orchestrator/mod.rs` (13),
  `takka-core/src/safety/engine.rs` (9),
  `takka-core/src/orchestrator/checkpoint.rs` (9),
  `takka-cli/src/widgets/log_writer.rs` (3),
  `takka-runtime/src/daemon.rs` (5).
- Poisons are self-inflicted: one panic while holding a lock makes every later
  `unwrap()` panic too, taking down unrelated sessions. The project already hit
  this class of bug in the `ENV_LOCK` incident (PR #13/#14), where the fix was
  `into_inner()` recovery.

**Work**

1. Add one helper (e.g. `takka-core/src/sync.rs`):
   `pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T>` using
   `unwrap_or_else(|e| e.into_inner())`.
2. Convert the 93 non-test sites, ordered by blast radius: `runtime/engine.rs`
   and `daemon.rs` first, then orchestrator/safety/file_tracker.
3. Leave sites that genuinely assert an invariant as `expect("...")` with a
   message, and leave test code alone.
4. Do **not** convert `RwLock` blindly — decide per lock and note it in the PR.

**Acceptance**

- `grep -rn '\.lock().unwrap()' takka-*/src` reports only `#[cfg(test)]` code
  (add a tiny CI guard script if cheap).
- New test: a thread panics while holding one of the converted locks, and a
  second thread still reads/writes without panicking.

**Effort:** ~0.5 day.

---

## H-03 — Token estimation: three implementations, one dead API (P1)

**Evidence**

- Three different estimates: `takka-core/src/memory/types.rs:403`
  (`text.len() as u32 / 4`, plus optional tiktoken), and identical byte-based
  copies at `takka-core/src/providers/mod.rs:317` and
  `takka-core/src/providers/openai.rs:589`.
- `Provider::count_tokens` is **dead code** — no call site in the workspace
  (`grep -rn 'count_tokens('` shows only the three definitions).
- `len()` is UTF-8 **bytes**, not chars: Indonesian text under-counts slightly,
  CJK under-counts ~25% (3 bytes/char) against a ~1 token/char reality. The
  heuristic feeds `needs_compaction`, the context window, and the 85%
  autocompact threshold, so under-counting risks a provider-side context error.
- `tiktoken` is off by default (`takka-core/Cargo.toml [features] default = []`).

**Work**

1. Make `estimate_tokens` the single source of truth; delete the two provider
   copies and either route `count_tokens` through it or remove the trait method.
2. Replace `len() / 4` with a cheap char-class heuristic: ASCII ≈ bytes/4,
   non-ASCII ≈ 1 token per char (documented, still allocation-free).
3. Add a unit test with an Indonesian paragraph and a CJK string asserting the
   estimate is no longer below the tiktoken reference by more than ~20%.
4. Re-check the autocompact threshold with the new heuristic (it tightens, so
   sessions compact earlier — expected, note it in CHANGELOG).

**Acceptance**

- Only one `estimate_tokens` definition remains; `cargo clippy` clean.
- New tests: `ascii_estimate_matches_bpe_within_tolerance`,
  `non_ascii_is_not_underestimated`.

**Effort:** ~0.5 day.

---

## H-04 — Split the god files (P2, no behaviour change)

**Evidence:** `takka-cli/src/commands.rs` 8,494 lines,
`takka-runtime/src/acp.rs` 4,247, `takka-cli/src/tui/app.rs` 2,687,
`takka-cli/src/interactive.rs` 2,461. `commands.rs` mixes the REPL loop,
streaming/timeline rendering, slash commands, and config wizard; `acp.rs` mixes
JSON-RPC framing, session state, translation, and permission waiters.

**Work**

1. Mechanical split only, same public surface, tests move with their code:
   - `takka-cli/src/commands/` → `mod.rs` (entry), `repl.rs`, `render.rs`,
     `slash.rs`, `confirm.rs`.
   - `takka-runtime/src/acp/` → `mod.rs`, `framing.rs`, `session.rs`,
     `translate.rs`, `permissions.rs`.
2. No logic edits inside the same commit; if a fix is needed, separate PR.

**Acceptance**

- `cargo test --workspace --all-targets` green before and after with the same
  test count; no file above ~1,500 lines in the touched crates.
- `git diff --stat` for the split shows moves only.

**Effort:** ~1 day, mechanical.

---

## H-05 — Documentation contradicts itself on ACP conformance (P2)

**Evidence**

- `.github/workflows/ci.yml:50` says "hermetic 14-check conformance run";
  `docs/ROADMAP.md` ("Open work") says "18-check" (`scripts/acp-conformance.sh`);
  `docs/BENCHMARK_CODEX.md:181` still says "14-check";
  `scripts/acp-conformance.sh` has 9 `ok` call sites (one takes a label
  argument), and the shared memory for PR #18 records 23 checks.
- Task trackers are stale: `tasks/tasks-core-agentic.md:4` and
  `tasks/tasks-agentic-cli.md:4` still reference `core-agentic`/`agentic-cli`
  paths and "Phase 9–11 planned"; project was renamed to `takka-*` in 2026-11.
- `docs/superpowers/plans/2026-11-11-acp-conformance-report.md` still lacks the
  four-point real-Zed verification recorded in the cycle memory.

**Work**

1. Pick the script as the source of truth: have it print the check count in its
   summary, then quote that number everywhere (CI comment, ROADMAP, BENCHMARK).
2. Refresh the two task trackers: crate names, current status line, pointer to
   this backlog; keep history, don't rewrite completed items.
3. Once the user verifies the four Zed points (model list, usage, allow_always,
   prompt after load), append the evidence to the conformance report.

**Acceptance:** `grep -rn "14-check\|18-check\|23 check" .github docs README.md`
returns only the current number; task trackers mention `takka-*`.

**Effort:** ~2 hours + user verification.

---

## H-06 — No spend ceiling (P2, feature)

**Evidence:** cost is *recorded* only — `takka-cli/src/session.rs:39` (`cost`),
`session.rs:216-219` (`update_stats`). No `max_cost`/budget key exists in
`takka-core/src/config.rs`, and nothing aborts a run when spend grows.

**Work**

1. Config: `agent.max_cost_usd: Option<f64>` (unset = unlimited, current
   behaviour).
2. Orchestrator: after each usage event, accumulate cost; when the ceiling is
   crossed, stop the loop with a dedicated event (`CostLimitExceeded`) instead
   of a generic error, mirroring `turn_limit_exceeded` in `events.rs:284`.
3. CLI/TUI: surface the remaining budget in the status bar and render the stop
   event with an actionable hint.

**Acceptance:** tests for "under budget runs to completion", "over budget stops
after the crossing turn, not mid-stream", "unset = unlimited"; docs in
`docs/CONFIGURATION.md`.

**Effort:** ~1 day.

---

## H-07 — Daemon transport is Unix-only (P3, known gap)

**Evidence:** `takka-runtime/src/daemon.rs:32` and `takka-cli/src/client.rs:626`
document Windows named pipes as a TODO; `not_supported_error` is the current
behaviour on non-unix hosts.

**Work:** implement the named-pipe counterpart behind the existing transport
seam, plus the same auth-token handshake. Do not fake parity with a TCP socket —
that widens the local attack surface.

**Acceptance:** daemon smoke test matrix gains a Windows leg; the existing
"unsupported platform" guard test is replaced by a real transport test.

**Effort:** ~1–2 days, needs a Windows runner.

---

## Suggested order

1. **H-01** (user-visible hang / real deadlock) — ship first.
2. **H-02** then **H-03** (small, independent, both reduce crash/overflow risk).
3. **H-05** (cheap, unblocks review of the ACP docs).
4. **H-04** (mechanical, do it before the next large `commands.rs`/`acp.rs`
   change, not in parallel with a feature PR).
5. **H-06**, **H-07** last; both need a product decision.

## Verification baseline

Before each item: `cargo fmt --all -- --check`, `cargo clippy --workspace
--all-targets -- -D warnings`, `cargo test --workspace --all-targets`,
`bash scripts/acp-conformance.sh` (POSIX). The daemon smoke suite may flake on a
busy socket slot — rerun once before diagnosing (see `AGENTS.md`).
