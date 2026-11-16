# H-01 follow-up: process-group kill for bounded shell execution

Branch: `fix/process-group-kill` (based on `origin/dev` 0c0f476)
Commit: `10b9c0e` — `fix: kill the whole process group when a bounded shell run is killed`
Status: DONE

## Problem

PR #20 added `run_bounded` (`takka-core/src/tools/process.rs`) with a wall-clock
deadline and cooperative cancellation, but the kill only reached the direct
child. `run_command` runs `sh -c 'cmd'`, where the shell tail-execs so the
killed pid *is* the work; `run_script` runs `sh /tmp/x.sh`, which does **not**
exec the script body. Killing the interpreter therefore orphaned the script's
grandchildren — a backgrounded `sleep` survived the timeout/cancel.

## What changed

### `takka-core/src/tools/process.rs`
- Unix (`#[cfg(unix)]`): the child is put into its own process group before
  spawn via `std::os::unix::process::CommandExt::process_group(0)` (stable
  since Rust 1.64). The child's pid equals the new group's pgid because it
  leads that group.
- On timeout or cancel the group is SIGKILLed before reaping:

  ```rust
  unsafe { libc::kill(-(pgid as libc::pid_t), libc::SIGKILL); }
  ```

  (equivalent to `killpg(pgid, SIGKILL)`), then the existing `child.kill()`
  and `child.wait()` run unchanged.
- Windows: **no Job Objects or equivalent** — explicitly out of scope. Only
  the direct child is killed there, so grandchildren still survive on Windows.
  This gap is documented in a code comment at the kill site.
- JSON contract / `ProcessOutput` unchanged: `success`, `exit_code`,
  `stdout`, `stderr` and the `command killed: <label>` stderr prefix are
  identical. No public API change (`run_bounded` signature untouched).

### `takka-core/Cargo.toml`
- Added a target-specific dependency, no others:

  ```toml
  [target.'cfg(unix)'.dependencies]
  libc = { version = "0.2", default-features = false }
  ```

  `Cargo.lock` only gains `libc` in the `takka-core` dependency list (`libc`
  was already in the lockfile via `takka-cli`/`tokio`, so nothing new is
  vendored). Note: `libc` 0.2 has no default features; `default-features =
  false` is kept as the brief requested and is a no-op.

## Tests (TDD — failing first, then passing)

Added to `takka-core/src/tools/run_script.rs` (Unix only), following the
existing style; a watchdog/fresh-tag pattern avoids flaky `pgrep -x sleep`:

- `script_grandchildren_die_on_timeout` — script is
  `sh -c 'exec -a <uuid-tag> sleep 4242' &\nsleep 4242` with `timeout: 1`.
  After the deadline, `pgrep -f <uuid-tag>` must find nothing within 3 s.
- `script_grandchildren_die_on_cancel` — same script, `timeout: 30`, a
  cancel flag flipped after 300 ms; the grandchild must be gone within 3 s.

The unique tag (`exec -a`) makes `pgrep -f` exact and per-test isolated, so
concurrent tests cannot cross-match.

### Failing-first evidence (before the fix)

```
test tools::run_script::tests::script_grandchildren_die_on_cancel ... FAILED
test tools::run_script::tests::script_grandchildren_die_on_timeout ... FAILED
thread '...die_on_cancel' panicked at run_script.rs:357:
  grandchild process outlived the cancel kill (tag takka-test-grandchild-cc69464e-...)
thread '...die_on_timeout' panicked at run_script.rs:331:
  grandchild process outlived the timeout kill (tag takka-test-grandchild-bdd02c12-...)
test result: FAILED. 0 passed; 2 failed
```

`pgrep -af` confirmed the two leaked `sleep 4242` processes; they were killed
manually after that run.

### After the fix

```
test tools::run_script::tests::script_grandchildren_die_on_cancel ... ok
test tools::run_script::tests::script_grandchildren_die_on_timeout ... ok
test result: ok. 2 passed; 0 failed
```

No leftover tagged processes after the suite ran.

### Full baseline

- `cargo fmt --all -- --check` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo test --workspace --all-targets` — all 11 test binaries green
  (336 + 609 + 36 + 8 + 4 + 30 + 6 + 10 + 63 + 1 + 3 = 1106 tests, 0 failed),
  run twice, no flakes.

## Files changed

- `takka-core/src/tools/process.rs` — process-group spawn + group SIGKILL.
- `takka-core/src/tools/run_script.rs` — two grandchild-kill regression tests.
- `takka-core/Cargo.toml` — unix-only `libc` dependency.
- `Cargo.lock` — one added dependency edge for `takka-core`.

No changes to `take-cli`, `takka-runtime`, `.github/`, or the JSON contract.

## Self-review

- Children still inherit stdin=null / piped stdout/stderr — `process_group(0)`
  does not alter stdio wiring.
- A child that exits on its own is untouched; the group kill only fires when
  `killed.is_some()`.
- `child.kill()` after the group SIGKILL is harmless (child is already dead;
  ESRCH/`InvalidInput` is ignored) and keeps Windows behaviour intact.
- Best effort only: a grandchild that calls `setsid()` itself escapes the
  group. That is inherent to the approach and not pursued here.
- The tests only run on Unix; the Windows path has no automated coverage,
  which matches the documented gap.
- YAGNI respected: no Job Objects, no signal-escalation ladder, no new public
  API.