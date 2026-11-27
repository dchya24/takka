# Loop-behaviour ablation: first results

Status: **preliminary**. One live task per feature, one model, small samples.
Treat the direction of each effect as a lead to confirm, not as a verdict.

Setup: live lane, model `glm-5.3-flash` (priced at $0.15 in / $0.50 out / $0.03
cache-read per 1M tokens; the provider reports no cache hits), hermetic
agent prompt, `PermissionMode::Yolo`. Each feature is switched off with
`takka-eval run --disable <loop-nudge|auto-retry|budget-shaping>`; the
defaults (all on) are what ships. Per-run records come from `run_details` in
the result files.

## Results

| Feature | Task | ON | OFF |
|---|---|---|---|
| P-02 transient auto-retry | `flaky-api-token` (n=5 each) | 5/5 pass, 3 iterations, retry fired 5/5 | 4/5 pass; the failing run stopped at iteration 2 |
| P-06 tool-budget shaping | `tight-three-modules`, cap 7 (n=5 each) | 5/5 pass, mean 5.8 iterations, shaping fired 3/5, forced finalization 1/5 | 5/5 pass, mean 6.0 iterations, forced finalization 2/5 |
| P-01 semantic loop nudge | `chain-hop` (n=10 each, interleaved ON/OFF) | 10/10 pass, median 14 iterations, nudge fired 9/10 | 10/10 pass, median 17 iterations |

`chain-hop` cost and speed (interleaved, so provider latency drift hits both
sides equally):

| | Nudge ON | Nudge OFF |
|---|---|---|
| Total cost | $0.36 | $0.22 (one $0.12 outlier) |
| Typical run | $0.03 - 0.04 | about $0.009 |
| Mean wall time per run | 84 s | 47 s |
| Prompt tokens per iteration | about 14,400 | about 7,000 |
| Completion tokens per iteration | about 380 | about 150 |

An earlier non-interleaved batch of `chain-hop` had 3 of 5 ON runs hit the
300 s task timeout (OFF: 0 of 5). The interleaved rerun saw no timeouts, but
ON runs were again the slow ones (longest 212 s).

## Reading

- **P-02 retry: keep.** It fires every time the scripted 503 appears and the
  OFF run that gave up is what the feature exists to prevent. n=5 is too small
  to claim the pass-rate difference on its own.
- **P-06 shaping: keep, low confidence.** The effect is within noise on this
  task; the only visible difference is fewer forced finalizations (1/5 vs
  2/5). Needs tasks where the cap actually binds to say more.
- **P-01 nudge: tune.** On `chain-hop` it did not improve pass rate and made
  runs about 4x more expensive and 1.8x slower per run. That task legitimately
  reads the same file many times at different offsets, so it is closer to a
  false-positive case than to a real loop. The detector ignores
  `offset`/`line`/`limit`-style arguments, so legitimate paging looks like
  re-reading. Why the nudge makes the model use more tokens per step is not
  established (no transcript was inspected).

## Caveats

- One model, one task per feature, small n; no significance testing.
- `log-first-error` never fires the nudge (the model greps instead of paging)
  and carries no information about P-01. It is a candidate to drop.
- Runs killed by the timeout used to report zero metrics; that is fixed (the
  child streams its events to a log), but no live run has exercised it yet.
- Cache read is 0% for this provider, so cost comparisons here ignore caching.

## Next

1. Inspect a `chain-hop` ON transcript after the first nudge to see what
   changes in the model's behaviour.
2. Add P-01 tasks with a genuine thrash (nudge should help) next to
   legitimate-paging cases (nudge should stay quiet), then re-run interleaved.
3. Grow the P-06 and P-02 task sets before drawing keep/remove conclusions.

Raw result files are not committed; regenerate with `takka-eval run --lane
live --config <config> --model glm-5.3-flash --n 5 [--disable <feature>]`.
