# takka evals

Task suite and baselines for `takka-eval` (crate `takka-eval`). Design and
rationale: `docs/superpowers/plans/2026-11-26-eval-harness-and-record-replay.md`.

## Run

```bash
cargo run -p takka-eval -- run                      # replay lane (default, hermetic)
cargo run -p takka-eval -- run --filter bugfix      # by id substring or tag
cargo run -p takka-eval -- run --baseline evals/baselines/replay.jsonl
cargo run -p takka-eval -- list
```

Exit status is non-zero when a task fails or `--baseline` finds a regression.

## Live lane (spends money)

```bash
cargo run -p takka-eval -- run --lane live --config ~/.config/takka/config.json \
    --model <id> --n 3 --budget-usd 2 --out /tmp/live.jsonl
```

The live lane runs the real provider from the config file you pass explicitly
(it never reads ambient config). Guards: a per-run cap (`--max-cost-usd`, used
when a task sets none), a total budget (`--budget-usd`; the run stops and exits
non-zero when it is spent), and a refusal for models missing from the price
table (`--allow-unpriced` overrides, with no cost enforcement). `--n` repeats
each task; `passed` is the majority outcome and the table shows `pass/runs`.
The task fixtures and prompts are sent to the provider.

## Task layout

```
evals/tasks/<id>/
  task.json        id (= directory name), prompt, tags, limits, verify, expect
  repo/            fixture copied to a temp workdir for each run
  cassette.jsonl   recorded provider exchanges (replay lane)
```

`verify.command` runs with `sh -c` in the workdir after the agent finishes;
exit 0 means success. `verify.forbid_paths` lists files the agent must not
touch. `expect` asserts behaviour besides passing (`min_loop_nudges`,
`min_auto_retries`, `max_iterations_used`, `limit_hit`).

## Cassettes

The shipped cassettes are **hand-authored**: they prove the harness and the
orchestrator wiring, not model quality. To record one from a real model, wrap a
provider with `CassetteProvider::record` (see `takka-core/src/providers/cassette.rs`);
requests carry a strict fingerprint, so changing the prompt, tools or model makes
replay fail until the cassette is re-recorded.

## Baselines

`evals/baselines/replay.jsonl` is the replay lane's last accepted result. Replay
is deterministic, so it guards harness and orchestrator plumbing; compare live
runs against a live baseline of your own, never against this one.
