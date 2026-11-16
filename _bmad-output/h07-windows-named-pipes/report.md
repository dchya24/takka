# H-07 — Windows named-pipe daemon transport: implementation report

**Branch:** `feature/h07-windows-named-pipes` (base `origin/dev` @ `6ac3330`)
**Commit:** `6c35823` — `feat: add Windows named-pipe transport to the daemon broker`
**Status:** DONE_WITH_CONCERNS — the Windows path is **compile-checked only** and has
never executed on a real Windows host; the CI smoke-test Windows leg was out of scope.
**Date:** 2026-11-18

## Scope decision (product)

The backlog item allowed either of two shapes. This PR takes the one the
spec explicitly demands: **a real named pipe, not a loopback TCP socket**.
A TCP listener on `127.0.0.1` is reachable by every local user and by any
process that can bind the port, which widens the local attack surface the
auth-token handshake exists to close. Records the decision in the module
docs of both seams so nobody "simplifies" it back to TCP later.

## Where the transport seam lives now

The broker logic (framing, one-active-connection rule, event drain,
`Ready` filter, stale-pid guard, bounded join) is unchanged and
transport-agnostic. The platform surface is four items, each with a
`unix` and a `windows` implementation:

**`takka-runtime/src/daemon.rs` → `mod platform`** (server side)

| Seam item        | unix                             | Windows                                         |
| ---------------- | -------------------------------- | ----------------------------------------------- |
| `LocalListener`  | `UnixListener` (non-blocking)    | `CreateNamedPipeW` server instances             |
| `LocalStream`    | `UnixStream`                     | duplex pipe handle (`Read`/`Write` impls)       |
| `open_connection`| `UnixStream::connect`            | `CreateFileW` + `WaitNamedPipeW` retry           |
| `write_info_file`| mode 0600                        | inherited user-only ACL                          |

Plus `fill_random` (`/dev/urandom` vs `ProcessPrng`) for the auth token,
and `pid_is_alive` (`/proc` / `kill -0` vs `tasklist`).

**`takka-cli/src/client.rs` → `mod transport`** (client side)

One async function returning a single duplex object
(`Box<dyn DuplexStream>`):

- unix: `tokio::net::UnixStream`
- Windows: a `CreateFileW`-opened pipe handle bridged into tokio via
  `tokio::fs::File::from_std`, opened with
  `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`
  so a process that renames the pipe cannot impersonate the user.

`DaemonConnection` now holds that one endpoint plus a per-line buffer
instead of pre-split halves. A Windows pipe handle is a single
`AsyncRead + AsyncWrite` value, and the client only ever alternates
write-request / read-frame, so splitting bought nothing.

`RuntimeMode::Daemon`, `AgentConnection::Daemon(..)`, `connect_daemon`,
`connect_daemon_at`, `spawn_daemon_process`, `wait_for_daemon_info`,
`shutdown_daemon_at` and `Commands::attach_daemon_inner` moved from
`#[cfg(unix)]` to `#[cfg(any(unix, windows))]`; the actionable-message
stubs are now `#[cfg(not(any(unix, windows)))]`. `takka-runtime`'s
`run_daemon`, `parse_daemon_flags` and `daemon_config_dir` moved the same
way, and `daemon_config_dir` falls back to `USERPROFILE` on Windows.

## Auth-token reuse (unchanged, single implementation)

The handshake was **not** duplicated. On both platforms:

1. the broker generates a 16-byte secret from the OS CSPRNG and writes it
   into the discovery file (`write_info_file`, mode 0600 on unix,
   user-only inherited ACL on Windows);
2. `connect_daemon_at` reads it via the existing `parse_daemon_info` and
   passes it to the existing `perform_handshake(.., Some(info.token))`;
3. `Init { daemon_token }` travels over the pipe exactly as over the
   socket; `RuntimeEngine` rejects a mismatch with `unauthorized` before
   touching session state.

`shutdown_daemon_at`'s busy/refusal retry loop is likewise shared.

## Named-pipe specifics worth reviewing

- **Instance arming.** A named pipe whose every instance has served a
  client is *closed* to new clients ("file not found") until another
  instance is created. The broker therefore keeps one extra unconnected
  instance armed and arms the replacement *before* blocking in
  `ConnectNamedPipe`. Without this, a second attach (or the shutdown
  retry loop) would see a connect error instead of the broker's
  `"daemon in use"` frame. This is the one Windows behaviour with no unix
  analogue and the most likely source of a first-run bug.
- **Naming.** `DaemonConfig.socket_path` keeps its name. On Windows it is
  read as a pipe name or a full `\\.\pipe\<name>` path;
  `platform::pipe_name` qualifies a bare name with `\\.\pipe\takka-<stem>`.
  The CLI's client seam accepts both spellings (bare or fully qualified).
- **Security descriptor.** `CreateNamedPipeW` is given a NULL
  `SECURITY_ATTRIBUTES`, so Windows derives the ACL from the creating
  process's token — the analogue of the unix socket living in a
  user-owned directory.
- **Connect errors.** Only `ERROR_PIPE_BUSY` is retried (bounded to 5 s).
  Every other failure surfaces immediately, which matters: the CLI turns
  a failed connect into "stale discovery file → respawn once" or the
  actionable no-daemon error, and a blanket retry would turn that into a
  5-second stall.

## No new dependencies

Confirmed: `Cargo.toml` / `Cargo.lock` are untouched by the commit.
- The runtime reaches `kernel32` / `bcryptprimitives` through local
  `extern "system"` blocks (two call sites' worth of symbols) rather than
  adding `windows-sys`.
- The CLI import list grew only with `tokio::fs::File` (already a
  dependency) and its own local `extern` block.

`tokio::net::windows::named_pipe` was considered for the CLI and rejected
in favour of the raw-handle bridge: `ClientOptions::open` has no way to
expose the busy-retry semantics the shutdown path needs, whereas
`CreateFileW` + `WaitNamedPipeW` matches the runtime broker exactly.

Note the two seeds deliberately differ in *execution model*: the runtime
broker uses blocking `ConnectNamedPipe` on its thread-based accept loop
(consistent with the rest of `serve`, which is a `std::thread` design),
while the CLI uses tokio's blocking-pool bridge. Neither introduces a new
thread-per-connection cost.

## Tests

### Replaced: the "unsupported platform" guard test

`takka-runtime/src/daemon.rs::windows_guard_tests` ("daemon mode not
supported on Windows yet") is **deleted**. The message constant remains
(now platform-agnostic) because hosts that are neither unix nor windows
still need it; its contract test lives in
`takka-cli/src/client/daemon_info_tests.rs::daemon_mode_unsupported_message_is_actionable`,
plus a drift guard (`takka_runtime_exports_the_same_unsupported_message`)
that greps the runtime source for the same literal.

### New real transport tests

Runtime (`takka-runtime/src/daemon.rs`, module now
`#[cfg(all(test, any(unix, windows)))]`):

| Test | What it proves |
| --- | --- |
| `transport_round_trips_bytes_both_ways` | client connects, listener accepts, bytes flow both directions — the exact seam `serve` uses |
| `connecting_to_a_dead_endpoint_fails` | a dead endpoint fails fast (the stale-discovery signal) |
| every pre-existing broker test | `serve_round_trips_basic_run`, `an_idle_connection_is_not_mistaken_for_a_detach`, `second_connection_is_refused`, `reattach_adopts_session`, `shutdown_unlinks_files`, `stale_info_file_is_cleaned`, `drain_filter_drops_only_the_startup_ready` now run against the seam on both hosts |

CLI (`takka-cli/src/client/daemon_info_tests.rs::transport_seam_tests`):

| Test | What it proves |
| --- | --- |
| `connecting_to_a_dead_endpoint_fails` | the client seam under `connect_daemon_at` fails on a dead endpoint |

`accepted_connections_are_forced_blocking` stays `#[cfg(unix)]` — it
asserts the *inherited* `O_NONBLOCK` flag of a BSD `accept`, which has no
named-pipe analogue (pipe handles are synchronous). The Windows
equivalent of its intent is covered by the round-trip and idle-connection
tests, which never see a spurious detach.

### Test runs (Linux, this machine)

```
cargo fmt --all -- --check                       PASS
cargo clippy --workspace --all-targets -- -D warnings   PASS (clean)
cargo test --workspace --all-targets             PASS
  330 (takka-cli unit)  594+330+30+8+4+30+6+10  (other suites)
   62 (takka-runtime unit, was 61)
    1 (acp_smoke)        3 (protocol_smoke)
bash scripts/acp-conformance.sh                  24/24 PASS
```

Deltas vs baseline: takka-cli unit 328 → 330 (+2), takka-runtime 61 → 62
(+1 net: +2 transport tests, −1 deleted guard test).

## Compile-checked vs untested

**Compile-checked for Windows:**

```
cargo check --target x86_64-pc-windows-gnu -p takka-runtime --all-targets   PASS (no warnings)
cargo check --target x86_64-pc-windows-gnu -p takka-cli                    PASS (no warnings)
cargo check --target x86_64-pc-windows-gnu --workspace --all-targets       PASS (no warnings)
```

The third command had to be run with `syntect` temporarily commented out
of `takka-cli`'s dev-dependencies (restored immediately; `Cargo.toml` and
`Cargo.lock` are unmodified in the commit). `syntect` is an **unused**
dev-dependency — nothing in `takka-cli` references it, and
`widgets/code_highlight.rs` documents deliberately avoiding it — but
pulling it in drags `onig_sys`, whose build script needs
`x86_64-w64-mingw32-gcc`, which is not installed here. **Without that
temporary edit, `--all-targets` cannot be cross-checked on this machine
at all, so this is a pre-existing repo/environment limitation, not
something this change introduced.**

`rustup target list --installed` reports `x86_64-pc-windows-gnu` only;
no `-msvc` target is installed, so `-gnu` was used.

**Untested — stated plainly:**

- No Windows machine was available. **None of the Windows code has ever
  run.** It is parsed, type-checked and borrow-checked by rustc and that
  is all.
- Specifically unverified at runtime: pipe instance arming under the
  refusal and shutdown-retry paths, `SECURITY_IDENTIFICATION` open
  semantics, `ProcessPrng` availability on older Windows 10 builds,
  `tasklist` CSV parsing in `pid_is_alive`, discovery-file ACL
  inheritance, and the CLI's `tokio::fs::File` bridge over a duplex pipe.
- The first Windows CI run should be treated as the real smoke test. If
  the CI smoke matrix gains its Windows leg (follow-up below), the
  runtime's and CLI's transport tests will execute there automatically
  because they are no longer `#[cfg(unix)]`.

## Follow-ups

1. **CI Windows leg (blocked on sign-off).** The backlog's acceptance
    criterion — "daemon smoke test matrix gains a Windows leg" — is
    **not** met here: `.github/` is explicitly out of scope without
    separate approval. The `test` job in `ci.yml` already lists
    `windows-latest` in its matrix, so the new transport tests will begin
    running there on the next push **without** any CI edit; what is
    missing is a *daemon-specific* leg (the smoke module in
    `takka-cli/src/commands/daemon.rs` is still `#[cfg(all(test, unix))]`).
2. **Drop the unused `syntect` dev-dependency** so
    `cargo check --target …-windows-gnu --all-targets` works without a
    C cross-compiler. Out of scope for H-07 (it is a manifest change with
    its own review question), but it is the single thing blocking
    routine Windows cross-checks.
3. **Run on a real Windows host** before claiming parity anywhere
    user-facing (CHANGELOG, README feature matrix).
4. **Un-gate the CLI daemon smoke module** on Windows once (3) passes:
    `daemon_smoke_tests` is `#[cfg(all(test, unix))]`; its fixtures are
    already endpoint-agnostic apart from `.sock` naming.

## Files changed

| File | Change |
| --- | --- |
| `takka-runtime/src/daemon.rs` | +`mod platform` (unix + windows), `LocalListener`/`LocalStream`/`open_connection`/`write_info_file`/`fill_random`/`pid_is_alive` seams, async-free accept loop unchanged otherwise, tests un-gated and two transport tests added, guard test deleted |
| `takka-runtime/src/main.rs` | `run_daemon`/`parse_daemon_flags`/`daemon_config_dir`/`flag_value` now `any(unix, windows)`; `USERPROFILE` fallback |
| `takka-cli/src/client.rs` | +`mod transport` (unix + windows), `DuplexStream`/`Duplex`, `DaemonConnection` over one duplex endpoint, daemon path un-gated |
| `takka-cli/src/client/daemon_info_tests.rs` | guard test rewritten as the platform-agnostic message contract + drift guard, `transport_seam_tests` added |
| `takka-cli/src/commands/daemon.rs` | `attach_daemon_inner` un-gated to `any(unix, windows)`, stub now `not(any(unix, windows))` |

## Self-review findings

- **Bug caught and fixed before commit:** the first version of the
  Windows client connect retried *any* non-success error for the full 5 s
  deadline, which would have turned the "stale discovery file → respawn"
  path into a 5-second stall. Now only `ERROR_PIPE_BUSY` retries. The
  runtime's `open_connection` had a related inversion (it tested
  `!= ERROR_PIPE_BUSY` before testing the deadline) and was rewritten
  with the same rule.
- **YAGNI:** removed a `wide_to_os_string` helper (and its `OsStringExt`
  import) that existed only to keep an import "load-bearing"; removed a
  `PipeStream` trait that nothing used.
- **Not fixed, deliberately:** `DaemonConfig.socket_path` and
  `DaemonInfo.socket_path` still say "socket" while carrying a pipe name
  on Windows. Renaming touches the discovery-file schema pinned by
  `daemon_info_parses_fields` and is a cosmetic change with cross-crate
  risk; the docs on both structs call out the dual meaning instead.
- **Accepted limitation:** on Windows a `ConnectNamedPipe` blocks its
  thread, so the broker's shutdown latency while no client is waiting
  is bounded by the accept poll interval, same as unix. A shutdown with
  an idle client attached relies on the pre-existing
  `SHUTDOWN_JOIN_TIMEOUT` detach rule (2 s) exactly as unix does — this
  is unchanged behaviour, not new Windows-specific code.
- **No secrets committed.** The hermetic `127.0.0.1:9` provider config
  used by the tests was already there; nothing new touches credentials.