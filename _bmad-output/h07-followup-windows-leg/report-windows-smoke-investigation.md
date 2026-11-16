# H-07 follow-up — Windows daemon smoke failures: investigation

Branch: `chore/ci-windows-leg-and-cleanup`
Status: BLOCKED (needs a decision from the orchestrator; nothing pushed)
HEAD: `0d83d1a` (unchanged — no new commits)

## The failure as recorded (CI run 43794716164, PR #29)

Windows `Test (windows-latest)`: `cargo test --workspace --all-targets` runs
**exactly one** test binary before aborting: `takka-cli`'s
`unittests src\main.rs`. It aborts on the first failing binary, which is why
the output ends at `test result: FAILED. 329 passed; 7 failed`.

Consequence for the brief's suspect #4: **the `takka-runtime` lib test binary
never ran on Windows in that CI run** (`takka-runtime` is compiled after
`takka-cli` in cargo's order and the run aborts before it). So server-side
transport coverage on Windows is *not* proven by that run — `--no-fail-fast`
(or reordering) is needed before any "the server is fine, it's the client"
claim. This matters for the listening/stderr facts below.

The seven failures split into two *distinct* signatures:

| tests | panic site | signature |
|---|---|---|
| `adopt_smoke`, `in_use_refusal_smoke`, `shutdown_busy_daemon_errors_after_deadline`, `shutdown_retries_through_busy_slot` | `daemon.rs:342` (`connect`) | `daemon attach failed: no daemon running …; daemon still running after 2s; stderr: … INFO takka_runtime::daemon: daemon: listening socket=C:\…\takka-da-NNN\test.sock` |
| `commands_attach_shutdown_stops_daemon`, `daemon_lifecycle_smoke` | `daemon.rs:263` (`spawn`) | `daemon info file did not appear within 60s; … exited with exit code: 1; stderr: daemon error: Access is denied. (os error 5)` |
| `autospawn_smoke` | `daemon.rs:540` | `spawned daemon did not publish …\daemon.json within 60s` |

The first group's panic message *cannot be produced by the code it names* —
see next section. That is the core finding.

## Why this contradicts the code (four independent impossibilities)

1. **`connect_daemon_at(info_path, …, spawn_if_missing=false)` can never yield
   `NO_DAEMON_MESSAGE` when the info file parses.** The only path to that
   message with `spawn_if_missing=false` is
   `parse_daemon_info(info_path)` failing (`client.rs:717/729`). But the
   failing tests' fixture waits for the info file to exist *and* the
   quoted daemon log says "listening", which is logged **after**
   `write_info_file` (`daemon.rs:218-219`). The panic at `:342` is the
   `Err` arm of `fx.connect()` → `connect_daemon_at(…, false)`. A parse
   failure would require a truncated/corrupt file that no code path writes.

2. **The client side never logs to a file.** The quoted `stderr:` line was
   written by the *runtime child* into `<fixture dir>/daemon-stderr.log`
   (`daemon.rs:239-243` passes `RUST_LOG=takka_runtime=debug` for exactly
   that file). The content is the ANSI-coloured `INFO … daemon: listening`
   line, i.e. the runtime's own tracing subscriber. Nothing on the CLI side
   writes `daemon-stderr.log`, so the file's post-startup content (e.g. a
   daemon that exits later) is only observable indirectly.

3. **The 2 s grace does not elapse on a failed attach.** `evidence()`
   loops on `child.try_wait()` until it returns `Some` (→ `exited with …`)
   **or** the 2 s deadline (→ `still running after 2s`), then reads the log
   files. `still running after 2s` therefore means `try_wait()` polled
   `None` for the whole window — the child was genuinely alive at the
   moment of the panic.

4. **A silent death is impossible in this broker.** After the
   `daemon: listening` log the only exits are:
   - `serve` → `Ok(())` (engine thread finished → `tracing::info!(
     "daemon: engine thread finished")`, `daemon.rs:171`, at
     `RUST_LOG=takka_runtime=debug`; plus `Ok(()) => exit(0)` in
     `main.rs:92`), or
   - `serve` → `Err(e)` → `eprintln!("daemon error: {e}")` + `exit(1)`
     (`main.rs:94-95`).
   A panic in a spawned child thread (accept loop / reader / drain) does
   **not** unwind `serve`, so it cannot end the process. A panic on
   `main`'s thread only ends in exit code 101 *after* the payload text is
   printed (`RUST_BACKTRACE` unset → no extra output). The abort-on-panic
   path would need `panic = "abort"`, which the workspace does not set.
   So the log file must have contained *either* a second line with
   `\n`, or **no trailing newline at all** (a partial last line is exactly
   what the log shows: there is no `\n` before the harness's ` ; stdout:`).

   On Windows the runtime's tracing writer is `std::io::stderr`
   (`main.rs:9-13` + `main.rs:24`) → `WindowsStdout/Stderr` → an anonymous
   pipe with a **DWORD-length byte counter**, i.e. unbounded. So the
   usual UNIX "exit with unflushed libc buffer" truncation cannot explain a
   partial line either. (The only observable, an empty file, would be 0
   bytes, not one complete line — the `\n` cannot be lost in a pipe.)

5. **The "pipe gone from under the broker" shape is ruled out locally.**
   I reproduced the suspect hypothesis on Linux (a second process
   *unlinks and re-binds the same socket path* while a broker is serving
   on it, which is what `serve`'s own
   `let _ = std::fs::remove_file(&config.socket_path)` stale-cleanup would
   do if two daemons ever shared one path): the victim broker kept
   **serving**, accepted the next client on its still-open socket, and
   printed nothing (`LOG LINES: ['… daemon: listening …']`). So even that
   scenario does not produce a silent listener death.

**Conclusion:** the quoted evidence is not self-consistent with the current
code as committed at `0d83d1a`. The four notes above are individually
verifiable; together they say the run's evidence either mixes states
(child alive per `try_wait`, but its pipe unreachable and its log frozen at
the "listening" line) or the child observed is not the process serving the
pipe. That is not enough to author a *fix* — any patch would be a guess,
and the brief forbids guessing.

## What the eight code hypotheses in the brief actually reduce to

I diffed the server seam (`takka-runtime/src/daemon.rs mod platform`,
`pipe_name`, `create_instance`, `LocalListener`, `connect_instance`,
`write_info_file`) against the client seam (`takka-cli/src/client.rs mod
transport`, `pipe_path`, `connect`, plus `spawn_daemon_process`,
`wait_for_daemon_info`, `connect_daemon_at`) character by character. No
derivation or parse mismatch exists:

- `pipe_path` server-side: `\\.\pipe\takka-{file_stem}` (verbatim if the
  path already starts with `\\.\pipe\`). Client-side: verbatim if it starts
  with `\\.\pipe\`, else prefix. Both sides agree for any path the CLI
  itself generates (it always passes a filesystem path, never a bare pipe
  name); the fixture's `test.sock` → `\\.\pipe\takka-test` on **both**
  sides.
- Cross **process** collisions cannot occur: `FILE_FLAG_FIRST_PIPE_INSTANCE`
  is set on the first instance, so a second broker on the same name fails
  `CreateNamedPipeW` with `ERROR_ACCESS_DENIED` (= os error 5, exit 1,
  `daemon error: …`). The two `os error 5` tests show exactly *that*, i.e.
  two concurrently running brokers chose the same pipe name.
- The info file is written by the platform `write_info_file`
  (`OpenOptions::create(true).truncate(true).write(true)`, no mode bits on
  Windows) and read by `parse_daemon_info`; path case / `\\?\` / short
  `RUNNER~1` prefixes cannot diverge, because *both* sides use the same
  path string from the same tempdir join (the daemon does not canonicalise
  it, and `info_json` writes `config.socket_path.to_string_lossy()`).
- The client's attach path uses the PR-#26 loop (only `ERROR_PIPE_BUSY`
  is retried; everything else fails immediately) — it does not silently
  retry a dead endpoint, and `connect_instance` treats
  `ERROR_PIPE_CONNECTED` as success.

So the remaining prime suspect is the *fixture/concurrency* domain, not the
seam:

- `pipe_name` reduces the endpoint to its **file stem** (`test.sock` →
  `takka-test`), and the unique `takka-da-{pid}-{nanos%91e9}` component is
  a *directory* name that the pipe name never sees. Every smoke that runs
  concurrently in the same test binary uses the same stem, i.e. the same
  pipe name `\\.\pipe\takka-test`; one of them wins the first instance and
  the rest get `ERROR_ACCESS_DENIED` (`os error 5`), which explains the two
  spawn panics. The `autospawn_smoke` variant uses the *same* stem through
  `spawn_daemon_process` (`takka-daemon-{pid}.sock` → `\\.\pipe\takka-takka-daemon-{pid}`),
  so it is the same collision with a different name.
- Making the stem unique per fixture (or passing a fully-qualified, unique
  `\\.\pipe\…` value, which both seams already accept verbatim) removes
  that collision **without changing the transport**. It does not, by
  itself, explain the first group's frozen "listening"-only log with
  `try_wait() == None` — and both CI jobs run the identical test binary
  (`takka-cli`'s `src/main.rs`), so this needs CI evidence, not another
  local guess.

## Systemic issue: the CLI test binary is not hermetic against `HOME`

Independent of the pipe question, `takka-cli`'s `unittests src\main.rs`
inherits the real environment. `Commands::ensure_connection` /
`connect` read `HOME`/`USERPROFILE` (`daemon_info_path`, skill discovery
via `discover_skills`, `load_project_instructions`…) and the e2e smokes
spawn the *real* runtime:

- `commands::e2e::smoke_lifecycle_spawned` starts `takka-runtime` with
  inherited stdio; the runtime then loads `~/.config/takka/config.json`
  even though the Init frame carries a hermetic override.
- `takka-core`'s `skills::discovery::tests::*` and `config.rs` tests set
  `HOME`/`USERPROFILE` for parallel threads of that binary only. Inside
  their windows any other thread calling `Config::load()` with `HOME`
  unset falls back to `Config::fallback()`, which has **no providers** →
  `initialize()` returns `"no provider configured"`. The spawned runtime
  is subject to the same window.

I am **not** claiming this is what produced this run's seven failures (the
evidence above does not support any single story, and the failure set on
Windows is a strict subset of the macOS/Linux set, both of which share the
same code — so a deterministic `HOME` bug would already fail there too).
But it is a real, reproducible-in-principle hole in the "hermetic smoke"
claim, and fixing it (fixture passes an explicit emptiness: e.g. `HOME` +
`USERPROFILE` pointing at the fixture dir, or a `.config/takka/skills`
blocklist) would also make any future Windows failure attributable instead
of guessable.

## Local Linux baseline (unchanged code)

- `cargo fmt --all --check` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo test --workspace --all-targets` — 11 binaries, all green
  (336 / 607 / 36 / 8 / 4 / 30 / 6 / 10 / 63 / 1 / 3)
- `cargo test -p takka-cli --bin takka -- commands::e2e::smoke_lifecycle_spawned --nocapture`
  — passes, prints nothing (the e2e harness writes no daemon log).

Evidence replay performed locally (all on Linux, cleaned up afterwards):
two brokers with distinct `--socket-path` values coexist; a broker whose
listening socket's *path* is removed/re-bound by another process keeps
serving; a broker whose path is garbage-bound still binds; a broker exits
only through the two documented paths and can never die silent. None of
these reproduce the CI signature.

## Decision needed (BLOCKED)

I will not push a guessed transport fix. The three viable directions, in
order of cost:

1. **(recommended, cheap) Make the Windows run honest first.** Add
   `--no-fail-fast` to the CI test step (the brief's method needs the
   `takka-runtime` lib test result to know whether the *server* seam is
   proven on a real Windows host) and make the smoke fixture's endpoints
   unique per test (unique pipe stem / fully-qualified `\\.\pipe\`), which
   is a defensible, transport-neutral fix for the `os error 5` races and
   costs nothing on unix. Both need `.github/` + fixture edits — the brief
   forbids touching `.github/`, so this needs an explicit go-ahead.
2. **Instrument the failure and re-run once.** Have `evidence()` also
   report `pid_is_alive(pid)` for the broker's own pid (`tasklist`) plus
   the info file's *raw bytes* and length; that distinguishes "child alive
   but endpoint unreachable" from "another broker owns the pipe". One CI
   iteration, no behavioural change.
3. Stop here and leave the Windows leg red, documenting the investigation
   (this report) — the branch's only remaining gap would then be known and
   bounded.

Files changed in this task: **none** (investigation only). This report is
untracked.