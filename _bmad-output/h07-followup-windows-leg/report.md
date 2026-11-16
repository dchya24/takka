# H-07 follow-up — Windows smoke leg, syntect cleanup, CI check

**Branch:** `chore/ci-windows-leg-and-cleanup` (base `origin/dev` @ `0c0f476`)
**Status:** DONE_WITH_CONCERNS — items 1 and 2 are complete; item 3 turned up a
**pre-existing, hard blocker** on the Windows CI leg that is outside this
task's scope (see "Blocker" below).
**Date:** 2026-11-16

## Summary

| Item | Result |
| --- | --- |
| 1. Enable `daemon_smoke_tests` on Windows | **Done** — `cfg(all(test, any(unix, windows)))`, fixture was already transport-agnostic; comment-only portability notes added |
| 2. Drop unused `syntect` dev-dependency | **Done** — confirmed unused, removed; `Cargo.lock` loses only syntect + its exclusive transitive deps |
| 3. CI Windows leg | **No edit needed — but the leg cannot pass.** `ci.yml` needs nothing: the `test` job already runs `cargo test --workspace --all-targets` on `windows-latest` with no skip/ignore/filter. The leg fails **before any test runs**, at link time, for a pre-existing reason (see Blocker) |

## Blocker found in CI (pre-existing, not introduced here)

The `Test (windows-latest)` job on `dev` **already fails**, and has failed for
every run in the visible history — independent of this branch:

```
LINK : fatal error LNK1181: cannot open input file 'bcryptprimitives.lib'
error: could not compile `takka-runtime` (bin "takka-runtime" test)
```

Runs checked: `43792220762` (dev, post-PR-#26 merge), `43791656616`,
`43789169842`, and the PR #26 run. All fail on the Windows leg.

### Root cause

`bcryptprimitives.dll` ships without an import library, so it can only be
linked two ways:

1. `#[link(name = "bcryptprimitives", kind = "raw-dylib")]` — emits no
   import-library name to the linker. This is exactly what `getrandom`
   does (`getrandom-0.4.2/src/backends/windows.rs`, with the comment
   "As bcryptprimitives.dll lacks an import library, we use 'raw-dylib'").
2. Pass an explicit `.def`/import lib.

`takka-runtime/src/daemon.rs:892` (introduced by H-07 / PR #26) uses the
plain form:

```rust
#[link(name = "bcryptprimitives")]
extern "system" { fn ProcessPrng(data: *mut u8, length: usize) -> i32; }
```

which asks MSVC `link.exe` for `bcryptprimitives.lib` — a file that does not
exist. The H-07 report's "compile-checked" evidence could not catch this: the
cross-check used `x86_64-pc-windows-gnu`, whose `rust-lld` link flavour does
not resolve the same import library, and `cargo check` never links at all.

### Why more than a one-line fix

`kind = "raw-dylib"` is only available on **MSVC** targets (rustc rejects it
on the GNU targets), while CI compiles with `dtolnay/rust-toolchain@stable`,
i.e. whatever stable currently is. On this machine:

```
rustc 1.97.1 (8bab26f4f 2026-07-14)
```

`getrandom 0.4.2` declares `rust-version = "1.85"` and relies on `raw-dylib`
being stable on that MSVC target. Under the local 1.97.1 toolchain, though, a
`raw-dylib`-using compilation of `windows-link 0.2.1` **already fails on the
git-hub-hosted runner's configuration** (also pulled into this workspace by
`windows-targets 0.52.6` via the CLI). That is a toolchain-vs-dependency
incompatibility, not something this task should paper over.

Consequences:

- A local, blind `kind = "raw-dylib"` patch **cannot be verified here** — the
  local `x86_64-pc-windows-gnu` target is the only Windows target installed
  and it must not use `raw-dylib`, so there is no way to test the fix on this
  machine. CI is the only verifier, exactly like the smoke suite itself.
- The fix has three candidate shapes (below), and two of them are far beyond
  "make the Windows leg run the daemon smoke tests".

### Candidate fixes (needs a decision)

1. **Minimal, runtime-only:** replace the hand-rolled `ProcessPrng` binding
   with `getrandom` (already in the runtime's *graph* via the shipped
   `uuid`/`tempfile` deps on Windows — but **not currently declared** as a
   `takka-runtime` dependency, so this adds a manifest entry), or re-declare
   the binding with `kind = "raw-dylib"` guarded `#[cfg(target_env = "msvc")]`
   plus a GNU variant. Both need CI iteration to confirm.
2. **Toolchain-level:** the runner needs a toolchain/dependency pairing where
   the std dependency `windows-link 0.2.1` actually links on MSVC. That is a
   `Cargo.lock` / toolchain policy question (the workspace has no
   `rust-toolchain` pin and CI uses `@stable`), not a daemon-transport fix.
3. **Defer:** leave the Windows leg red, land items 1 and 2 now (the smoke
   suite is compile-checked and will start running the moment the link problem
   is fixed), and file the link failure as its own backlog item.

I deliberately did **not** guess:
- no code change to the transport seam without a way to verify it,
- no `.github/` or toolchain-policy change (out of this task's mandate),
- no `#[ignore]`/`--skip` on the Windows leg (that would be exactly the
  "silently not running" outcome item 3 asked me to prevent).

## Item 1 — what the gate change looks like

`takka-cli/src/commands/daemon.rs`:

```diff
-#[cfg(all(test, unix))]
+///
+/// Windows runs the same tests over the named-pipe transport (H-07): ...
+#[cfg(all(test, any(unix, windows)))]
 mod daemon_smoke_tests {
```

The gate is **kept**, not dropped: hosts with neither transport (no
`connect_daemon_at`, e.g. wasm) still exclude the module.

### Why the fixture needed no behavioural port

Audited every platform-sensitive spot in the module:

| Concern | Unix today | Windows | Action |
| --- | --- | --- | --- |
| socket path vs pipe name | `dir/test.sock` | same path → runtime's `platform::pipe_name` derives `\\.\pipe\takka-test` from the stem | none needed |
| discovery file | tempdir `daemon.json` | same | none needed |
| `/tmp` literal | none — `std::env::temp_dir()` | same call | none needed |
| `tasklist` parsing | n/a | in `takka-runtime`'s `pid_is_alive` (already shipped, H-07) | not touched here |
| `sleep 0` | used in `takka-runtime`'s stale-pid test | that test lives in the runtime crate (already `cfg(any(unix, windows))` from H-07); CLI smokes contain no `sleep` | none needed |

The only Windows-relevant hazard found was **child stdio handles**: cargo's
test harness captures stdout/stderr through inherited pipes. The fixture
already redirects both to files in the fixture dir, so the daemon never holds
the harness's handles — documented in place so nobody "simplifies" it back to
`Stdio::null()`/inheritance.

Changes are therefore **comments plus the gate** (diff is ~30 lines, all
commentary): SUN_LEN on unix, pipe naming/ACL from the temp path on Windows,
and why the child's stdio stays off the harness handles. `POLL_TIMEOUT`
comment now also notes a cold Windows runner is not faster than macOS.

## Item 2 — syntect

Unused, confirmed three ways:

```
$ grep -rn "syntect" --include=*.rs --include=*.toml .
takka-cli/src/widgets/code_highlight.rs:11-12  (only the "why we avoid it" doc comment)
takka-cli/Cargo.toml:77                        syntect = "5.2"
$ cargo tree -p takka-cli -e normal | grep -c syntect   → 0
$ cargo tree -p takka-cli -e dev    | grep -i syntect   → syntect v5.3.0
```

`takka-cli/Cargo.toml` `[dev-dependencies]` now:

```diff
 [dev-dependencies]
 futures = "0.3"
 tui-textarea = "0.7"
-syntect = "5.2"
```

`Cargo.lock`: **131 pure deletions, zero insertions** — `syntect` plus its
exclusive transitive deps (`onig`, `onig_sys`, `plist`, `quick-xml`,
`flate2`, `miniz_oxide`, `adler2`, `simd-adler32`, `crc32fast`, `bincode`,
`yaml-rust`, `linked-hash-map`), minus the now-single `"syntect",` line in
`takka-cli`'s dependency list.

Note: `cargo update -p syntect` additionally re-resolved six unrelated
`windows-sys` edges (some going **backwards**, `0.61.2 → 0.52.0`). Those were
reverted by hand so the lock diff stays purely the removal; verified with
`cargo check --workspace --all-targets --locked` and a second
`cargo metadata` pass (no rewrite).

`takka-cli/Cargo.lock` is a stale orphan manifest snapshot already drifting
from the workspace lock (it predates the crate rename and is not used by the
workspace build); left untouched.

Effect: `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets`
now works without a MinGW C cross-compiler — the H-07 follow-up #2 goal.

## Item 3 — CI verification (no edit made)

`ci.yml`, `test` job:

```yaml
  test:
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo test --workspace --all-targets
```

Checked for anything that could skip the smoke suite on Windows:

- no `--skip`/`--features`/`--package` filter,
- no `#[ignore]` or `cfg_attr(…, ignore)` anywhere in the smoke module or the
  runtime's daemon tests,
- `.cargo/config.toml` does not exist (no `rustflags`/target overrides),
- `takka-cli/Cargo.toml` declares no `required-features`/`harness = false`
  that would change discovery,
- the CLI binary test target is a lib+bin unit build, so
  `cargo test --workspace --all-targets` builds `takka` *and* the sibling
  `takka-runtime` binary that `runtime_binary()` locates by ancestor scan.

**Conclusion: no `.github/` edit is warranted.** The gap is the linker error
above, which fails the job before the test binary is ever produced.

## Verification run

Baseline (before any change, on `0c0f476`):

```
cargo fmt --all -- --check                              PASS
cargo clippy --workspace --all-targets -- -D warnings    PASS
cargo test --workspace --all-targets                     run 1: FAIL (adopt_smoke flake)
                                                         run 2: PASS (known socket-slot flake)
```

After each commit:

```
cargo fmt --all -- --check                              PASS
cargo clippy --workspace --all-targets -- -D warnings    PASS
cargo test --workspace --all-targets                     PASS
  336 takka-cli unit   607 takka-core unit   36 acp_smoke   8 daemon_info
   4 handshake   30 protocol_smoke   6 e2e misc   10 installers   63 takka-runtime unit
   1 acp_smoke   3 protocol_smoke
cargo test -p takka-cli --bin takka daemon_smoke_tests   PASS (9/9)
```

Windows cross-check (the best available evidence without a Windows host):

```
cargo check --target x86_64-pc-windows-gnu --workspace --all-targets          PASS
RUSTFLAGS="-D warnings" cargo check --target x86_64-pc-windows-gnu \
    -p takka-cli -p takka-runtime --all-targets                                PASS (no warnings)
cargo clippy --target x86_64-pc-windows-gnu -p takka-cli -p takka-runtime \
    --all-targets -- -D warnings                                               PASS
cargo check --workspace --all-targets --locked                                PASS
```

`cargo check` does **not** link, so it proves the Windows *smoke module
compiles*, not that the transport or the link works. The first CI run remains
the real test — and it currently cannot get that far (Blocker).

## Commits

```
7dd3f73 chore: drop the unused syntect dev-dependency
6adc173 test: run the daemon smoke suite on Windows too
```

No push, no merge. Nothing outside the task was staged (`db.conf`,
`gen_xdb.py`, `master_xdb.kjb`, CSV files, `.full_mode` were never touched —
the working tree was clean at start and is clean now).

## Self-review findings

- **Item 2 lock hygiene:** the default `cargo update -p syntect` would have
  dragged 6 unrelated `windows-sys` re-resolutions into a "remove a dev-dep"
  commit. Reverted by hand; verified `--locked` builds and that an extra
  `cargo metadata` run does not rewrite the file.
- **YAGNI:** I first extracted a shared `commands/daemon_fixture.rs` for the
  daemon smoke + e2e suites, then found the e2e suite only shares the
  hermetic-config *content*, not the process/endpoint helpers. Deleted the
  module; the helpers stayed inside `daemon_smoke_tests` and only the gate +
  comments changed.
- **Not fixed, deliberately:** the runtime's `fill_random` `#[link]` (the
  Blocker). Changing it without a way to verify the link on this machine would
  be a guess, and the fix spans a manifest/toolchain decision this task did
  not authorize.
- **Noticed, out of scope:** `takka-cli/src/client.rs` has a doubled
  `#[cfg(test)]` above `mod handshake_tests` (harmless).