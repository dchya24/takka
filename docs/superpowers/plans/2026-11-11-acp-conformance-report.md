# ACP v1 conformance report — `takka-runtime --acp`

**Date:** 2026-11-11
**Scope:** Fase 4 ACP adapter (T-00 … T-07, D12, D9 set_mode).
**Artifact:** `scripts/acp-conformance.sh`

## 1. The official Test Compatibility Kit is not obtainable

Spec §6 says: *"The Zed project ships a Test Compatibility Kit for ACP agents; after
bootstrap we run it once manually and record the result."* That kit is **not
published** to either public registry. Verified on this machine:

```
$ npm view @zed-industries/agent-client-protocol-tck version
npm error 404 Not Found - GET https://registry.npmjs.org/@zed-industries%2f...
$ npm view agent-client-protocol-tck version
npm error 404 Not Found
$ npm view @agentclientprotocol/tck version
npm error 404 Not Found
$ cargo search agent-client-protocol        # nothing named *-tck
agent-client-protocol = "5.2.0"             # protocol types + traits
agent-client-protocol-schema = "1.17.0"     # the schema crate we depend on
agent-client-protocol-derive / -tokio / -rmcp ...
```

The only Zed-published npm package is the **client** library
(`@zed-industries/agent-client-protocol@0.4.5`), not a conformance kit. So no official
TCK run can be recorded, and installing an unvetted third-party "tck" package was
rejected as a supply-chain risk for a release gate.

## 2. What replaced it

`scripts/acp-conformance.sh` — a hand-written conformance run that drives the **real
binary** over stdio exactly like an ACP client does, and asserts the documented
behaviour of every method this phase implements. Properties:

- **Hermetic**: the session engine is pinned to a discard-port provider via
  `TAKKA_TEST_ACP_CONFIG` (the test-only seam in `acp::serve_with_config`), so the
  ambient `~/.config/takka/config.json` is never read and no real provider is called.
  `127.0.0.1:9` refuses instantly, which is all the checks need.
- **Real transport**: stdin is a **FIFO** (a regular file would give the agent a
  sticky EOF after the first frames — the first version of the script got this wrong
  and the failures were visible as unanswered requests), stdout is captured per run.
- **Bounded**: every wait has a deadline (GNU `timeout`/`gtimeout` when available,
  otherwise the script's own poll deadlines); the whole run finishes in seconds.
- **Portable**: POSIX shell, no `jq` dependency (JSON is parsed with `python3`), works
  on Linux and macOS.
- **Exit code meaningful**: `0` only when every check passes.

## 3. Result — 14/14 passed

Run on branch `feature/acp-sessions-t03-t07` (output as produced):

```
== building takka-runtime ==
PASS  initialize answers protocolVersion 1 (client asked for 2)
PASS  initialize advertises image prompts
PASS  initialize advertises loadSession: false
PASS  initialize reports agentInfo.name = takka
PASS  session/new returns a sessionId
PASS  session/new advertises default as the current mode
PASS  session/new advertises the plan mode
PASS  session/new with a foreign cwd is invalid_params
PASS  the cwd rejection names the project directory to restart from
PASS  session/prompt resolves (error or stopReason) against the unreachable provider
PASS  a second prompt still works on the same session (no poisoned session)
PASS  session/set_mode switches the mode (empty result, no error)
PASS  an unknown method is JSON-RPC method not found
PASS  the agent exits 0 after stdin EOF

== 14 passed, 0 failed ==
ACP v1 conformance: OK
```

### Observed frames (relevant excerpts)

`initialize` — a v2 request is answered with v1 and the D7 capability set:

```json
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":false,"promptCapabilities":{"image":true,"audio":false,"embeddedContext":false},"mcpCapabilities":{"http":false,"sse":false},"sessionCapabilities":{},"auth":{}},"authMethods":[],"agentInfo":{"name":"takka","title":"takka","version":"0.1.0"}}}
```

`session/new` — id plus the D9 mode state:

```json
{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-1","modes":{"currentModeId":"default","availableModes":[{"id":"default","name":"Default",...},{"id":"plan","name":"Plan",...},{"id":"yolo","name":"Yolo",...}]}}}
```

D11 rejection — invalid params, actionable message (the client is told which
directory to restart from):

```json
{"jsonrpc":"2.0","id":3,"error":{"code":-32602,"message":"restart the agent from project directory /Works/.../agentic (session cwd / does not match)","data":{...,"expected":"/Works/.../agentic","received":"/"}}}
```

`session/prompt` against the unreachable provider — the turn fails cleanly as an
internal error, and the **same session** answers a second prompt (no poisoning):

```json
{"jsonrpc":"2.0","id":4,"error":{"code":-32603,"message":"Provider error: ... error sending request for url (http://127.0.0.1:9/chat/completions)"}}
{"jsonrpc":"2.0","id":5,"error":{"code":-32603,"message":"Provider error: ..."}}
```

`session/set_mode` (D9) — applied on the idle session, empty ACP result:

```json
{"jsonrpc":"2.0","id":6,"result":{}}
```

Unknown method — `-32601`; EOF — exit status `0`.

## 4. Findings / honest gaps

1. **A failed turn surfaces as a JSON-RPC `error`, not as `stopReason: refusal`.**
   The spec's mapping table says a *safety hard-block* is `refusal`; a provider
   failure is a different thing. Today both end up as `-32603`. Behaviour is honest
   (the client sees a real error, the session survives), but a client cannot
   distinguish "provider unreachable" from "agent broken" without reading the message
   text. Candidate follow-up: map provider-level failures to `stopReason: refusal`
   with a notice, keeping `-32603` for genuine adapter faults.
2. **`session/set_mode` is refused while a turn is in flight** (`session busy`)
   rather than deferred to the turn boundary. This follows D9 ("laws honored only at
   turn boundaries") in the strict direction: the client is told no instead of being
   made to believe a mode is active when it is not yet.
3. **`session/cancel` mid-permission gate** is covered by unit tests (T-05), not by
   this script: the hermetic provider never reaches a tool call, so a permission
   prompt cannot be staged end-to-end without a scripted provider (the fake-engine
   seam in `acp.rs` covers it deterministically).
4. **`session/load`, `session/fork`, `session/list`, MCP servers** are out of phase 1
   (D7 `loadSession: false`) and are therefore not exercised; `session/load` is
   asserted to fail with an honest "phase 2" error in the unit tests.

## 5. Recommendation

- Keep `scripts/acp-conformance.sh` as the manual gate recorded here (spec §6 wanted
  one manual run, not a CI gate; `.github/` is out of bounds without explicit
  approval anyway).
- Re-run it after any adapter change; it takes seconds and needs no credentials.
- Revisit the official TCK if Zed publishes one; if a Zed-hosted editor is available
  locally, a real Zed session (open project → chat → approve a command) remains the
  strongest manual check and is still outstanding.
