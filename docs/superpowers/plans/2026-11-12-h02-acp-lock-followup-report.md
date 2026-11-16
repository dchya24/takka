# H-02 follow-up: ACP `.lock().expect(...)` sweep — report

Branch: `fix/acp-lock-expect-sweep` (base HEAD: `0c0f476`, = `origin/dev`)
Commit: `1acfa82 fix: recover poisoned ACP mutexes instead of cascading panics`
Status: **DONE**

## 1. What was implemented

PR #21 (H-02) added `takka_core::sync::lock` — `m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())`
— and converted 119 `.lock().unwrap()` sites. `takka-runtime/src/acp/` was left out, and after
PR #23 (H-04) split it into `acp/{mod,framing,service,session,session_config,prompt,translate,permissions,test_support}.rs`
it still carried the same poisoning-cascade class behind `.lock().expect("...")`.

Every site in `acp/` was converted to `takka_core::sync::lock`, imported once as
`use takka_core::sync::lock;` in `acp/mod.rs` (every submodule reaches it via `use super::*`).

### 1.1 Site-by-site judgment

The brief asked for a per-site decision between (a) bookkeeping state → recover, and
(b) a genuine local invariant → keep `expect("...")`.

**Kept as `expect`: 0 sites.** Every mutex in this module guards bookkeeping whose
recovered state is always safe to use:

| Lock | What it guards | Why recovery is safe |
| --- | --- | --- |
| `AcpService::sessions` | `HashMap<String, Arc<Session>>` | Single insert / remove / drain on a plain map. A half-finished operation left it with or without one entry — never a torn multi-field invariant. Never `expect`ed for correctness; capacity checks read `.len()`. |
| `Session::events` | `Option<Receiver<ProtocolEvent>>` | One `take()` / one re-park per turn boundary. A panic between them leaves `None`, which every caller already handles as a typed error (`session engine stream is not available`) — the same path a dead engine takes. |
| `Session::current_model` / `current_mode` | `String` | A single `*guard = value` assignment. A recovered guard yields either the old or the new id; both are valid states the client can act on. |
| `Session::turn` / `engine` | `Option<JoinHandle<()>>` | Written once, taken once at EOF. A recovered `None` is exactly what the bounded join already tolerates (nothing to join). |
| `PermissionWaiters::inner` | `HashMap<String, PermissionWaiter>` | Insert / remove / drain of single entries. A recovered map may still hold a stale waiter, whose `SyncSender` the parked turn treats as a deny (fail-closed). The `insert` comment ("fresh id from `next_outbound_id` — never a collision") is about id uniqueness, not a lock invariant; ids come from an `AtomicU64`. |
| `Writer::out` | `Mutex<BufWriter<Stdout>>` | A write failure is already logged and swallowed (the client is gone → EOF on the next read). A recovered `BufWriter` mid-frame is the same failure mode as a partial write, which the `write_frame_line` error path already covers. |
| `VecSink::lines` (test only) | `Mutex<Vec<Value>>` | Test harness bookkeeping; a recovered vector only ever lacks one appended frame. |

Non-sites left alone, as the brief required:

- `framing.rs:103` `stdin.lock()` — a `StdinLock` stdio handle, not a mutex; it cannot poison.
- Any `RwLock` / tokio / `parking_lot` primitive — none exist in this module.
- The `.lock()` calls **inside the new tests** (`assert!(m.lock().is_err(), ...)`,
  `poison()`'s `m.lock().expect("guard to poison")`) — these deliberately bypass
  the helper to *prove* the mutex really is poisoned, and to create the poison.

### 1.2 Counts per file

| File | `.lock()` sites before | of which production | converted | kept as `expect` |
| --- | --- | --- | --- | --- |
| `acp/session_config.rs` | 17 | 17 | 17 | 0 |
| `acp/session.rs` | 9 | 8 | 9 | 0 |
| `acp/prompt.rs` | 5 | 5 | 5 | 0 |
| `acp/permissions.rs` | 4 | 4 | 4 | 0 |
| `acp/test_support.rs` | 4 | 0 (test-only crate paths) | 4 | 0 |
| `acp/service.rs` | 3 | 3 | 3 | 0 |
| `acp/framing.rs` | 3 | 1 | 3 | 0 |
| **Total** | **45** | **38** | **45** | **0** |

Three of the 45 sat in test *bodies* (`session.rs:725`, `framing.rs:208`, both
`expect("sink lock")`); they were routed through the helper too for consistency.
Production code therefore went from 38 `.lock().expect("...")` sites to 0. `framing.rs`'s
third `.lock()` is `stdin.lock()` (a stdio handle, not a mutex) and is untouched.

## 2. Tests (TDD)

The helper `poison(&mutex)` in `acp/test_support.rs` takes a guard and panics while
holding it under `catch_unwind` — that marks the mutex poisoned while keeping the
test process clean. It also asserts `m.lock().is_err()` before returning, so a test
can never silently "pass" on an unpoisoned lock.

New tests, each driving **production** code after the poison:

| Test | Poisons | Drives |
| --- | --- | --- |
| `acp::service::tests::a_poisoned_session_registry_stays_usable` | the live `AcpService::sessions` | `session_new`, `route_frame` (`session/prompt` for a live and an unknown session), a second `session_new`, then `shutdown` |
| `acp::session::tests::a_poisoned_session_survives_the_handlers` | a live session's `events`, `current_model`, `current_mode`, `turn`, `engine` | `session/prompt` → the turn thread takes/re-parks the stream, parks the turn handle, answers; asserts `busy` is cleared and the stream is back |
| `acp::permissions::tests::a_poisoned_waiter_map_still_answers_and_denies` | `PermissionWaiters::inner` | `insert` / `remove` / `deny_session` / `deny_all` |
| `acp::permissions::tests::a_poisoned_sink_lock_still_records_frames` | `VecSink::lines` | `send_line`, `response_for`, `updates` |

All four use the hermetic config (`hermetic_config_path()`, provider pinned to
`127.0.0.1:9`), so they never read the ambient `~/.config/takka/config.json`.

### Red evidence (before conversion)

Temporarily reverting `lock(&self.sessions)` back to
`self.sessions.lock().expect("session registry lock")` in `session.rs`/`service.rs`:

```
test acp::service::tests::a_poisoned_session_registry_stays_usable ... FAILED
thread ... panicked at takka-runtime/src/acp/session.rs:337:49:
session registry lock: PoisonError { .. }
test result: FAILED. 0 passed; 1 failed
```

That is the exact cascade the fix removes: a poisoned registry killed a plain
`session/prompt` routing lookup. Before conversion, the permission/sink tests failed
the same way at `permissions.rs:52` (`permission waiters lock`).

### Green after conversion

```
cargo test -p takka-runtime --bin takka-runtime acp::
test result: ok. 53 passed; 0 failed; 0 ignored; 0 measured; 14 filtered out
```

## 3. Verification

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace --all-targets` | **1108 passed, 0 failed** (all 11 targets) |

No daemon smoke test flaked; no rerun was needed.

Baseline (`0c0f476`) before the change: fmt/clippy clean, 1104 tests passing.
+4 tests after.

## 4. Files changed

- `takka-runtime/src/acp/mod.rs` — one import (`use takka_core::sync::lock;`).
- `takka-runtime/src/acp/session.rs` — 9 conversions + 1 new test.
- `takka-runtime/src/acp/session_config.rs` — 17 conversions.
- `takka-runtime/src/acp/prompt.rs` — 5 conversions.
- `takka-runtime/src/acp/permissions.rs` — 4 conversions + 2 new tests.
- `takka-runtime/src/acp/service.rs` — 3 conversions + 1 new test.
- `takka-runtime/src/acp/framing.rs` — 3 conversions (1 production, 2 test-body).
- `takka-runtime/src/acp/test_support.rs` — 4 conversions + the `poison` helper.

No behaviour change beyond poisoning recovery: every edit is a lock acquisition
rewritten to the helper (plus rustfmt reflow of the now-shorter chains). Nothing
outside `takka-runtime/src/acp/` was touched; `.github/` untouched; no pre-existing
working-tree changes were staged (the tree was clean at start and only these eight
files were added).

## 5. Self-review notes

- **The brief's "poison from a panicking thread" wording.** A spawned-thread
  poisoner cannot work here: the live connection shares its `Arc<Mutex<…>>` with
  the serve thread, so a test can never own it while it exists. Poisoning in place
  via `catch_unwind` produces an identical `PoisonError` state, and the new tests
  still poison the *real* service/session instances and then call the *real*
  handlers. The one deviation from a literal reading of the brief: no thread is
  spawned; the poison is applied on the test thread. This is strictly stronger for
  coverage (it reaches production code paths a fresh lock could not).
- No accessor methods were added to `Session` just to make it testable — the tests
  call the same inline `lock(&field)` sites the handlers use.
- `acp/test_support.rs` is compiled into the test binary only, so its conversions
  are dead code in production; they were included for consistency with the
  "every non-test `Mutex` acquisition" rule in `sync.rs` (and `VecSink` is reachable
  from `framing`'s non-test-gated `Writer` sibling in shape).
- One earlier draft added a `Session::test_stub` constructor and
  `AcpService::poisoned_session_registry` seam; both were removed as YAGNI once the
  tests drove real instances.