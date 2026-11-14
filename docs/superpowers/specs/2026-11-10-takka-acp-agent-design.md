# Design: Fase 4 — ACP (Agent Client Protocol) Integration

**Status:** Draft v1 (English, per documented convention) — awaiting approval
**Scope:** takka becomes an [ACP](https://agentclientprotocol.com) **Agent** so code editors (Zed
first; any ACP Client) can boot `takka` and drive sessions over stdin/stdout. This
replaces the previous Fase 4 plan ("SDK for other languages") — decision 2026-11-10.
**Acuan:** `docs/ROADMAP.md` §Open work (Fase 4 = ACP; Fase 3b deferred), ACP v1 docs
(overview / initialization / prompt-turn / tool-calls / session-setup / transports),
Zed reference crate [`agent-client-protocol`](https://crates.io/crates/agent-client-protocol)
(v5.x, MIT/Apache-2 dual), discipline precedent from the Fase 3a daemon spec (engine never
changes; hosts adapt around `Transport`).

---

## 1. Context & Goal

takka today speaks one wire protocol: the takka v2 JSONL protocol, consumed by its own
CLI (`AgentConnection` in-process/embedded/spawned/daemon). Gaps the ecosystem cares about:
there is no standard way for a code editor (Zed, or any ACP Client) to embed takka as the
coding agent in the editor's own UI.

ACP solves exactly this: the Client boots the agent as a subprocess and speaks
JSON-RPC 2.0 over newline-delimited stdio (verified: ACP v1 *Transports* — messages
newline-delimited, no embedded newlines, client must never write non-ACP frames to stdin).
Adopting ACP gives takka:

- editor embedding without any per-language SDK,
- standard session/prompt/tool-call/permission shapes the client UI already renders,
- listing in the ACP Registry (later; out of scope here).

The prior "SDK" plan is cancelled: an SDK would have required mapping takka's protocol
into each language by hand; ACP is one mapping maintained by a third-party protocol with
multiple working Clients, and it sits entirely beside the engine (zero protocol-bump risk
on the takka v2 protocol that the CLI and daemon depend on).

## 2. Decisions (draft)

| # | Decision | Choice | Rationale |
|---|---|---|---|
| D1 | Role | takka implements the **Agent** side only; Clients are ecosystems (Zed et al.) | The user's use case is "driven from an editor"; the agent side is the only embedding surface that matters |
| D2 | Transport | Subprocess over stdio, newline-delimited JSON-RPC 2.0 | ACP baseline transport (v1); portable all-3-OS; no socket/token/discovery plumbing — precisely what Phase 3b would have needed |
| D3 | Protocol version | **ACP v1 first**, `initialize` negotiation answers `min(requested_client_version, 1)` — v2 connection (v2 = draft; "support both" is the official guidance) gets an explicit follow-up behind version negotiation | v1 Clients dominate today (Zed + adapters). v2 history notes (per migration guide): *session/prompt response = ack, updates are upserts, client fs/terminal APIs dropped, capability re-org* — retrofit later with negotiation without breaking v1 peers |
| D4 | Types source | Depend on Zed's `agent-client-protocol` crate (v5.x) for JSON-RPC envelope, capability + tool-call types | Sheer spec surface (content blocks, permission options, tool kinds, stop reasons); hand-rolling serde types is high-drift-risk. Crate is freshly updated (2026-11-09) |
| D5 | Entry point | `takka-runtime --acp` (new flag in the standalone `takka-runtime` binary); adapter lives in `takka-runtime/src/acp.rs` | Mirrors the daemon precedent: hosts live around the engine, the engine is unmodified. `takka-runtime` binary already ships in all installer assets (v0.5+). Config/credentials stay in `~/.config/takka/config.json` (never in repo/env) |
| D6 | Session model | **One `RuntimeEngine` per ACP session** (`session/new` → engine thread + `MemoryTransport`), bounded by `--acp-max-sessions` (default 4); one session processes one prompt turn at a time (v1 rule; second `session/prompt` mid-turn → JSON-RPC error) | ACP allows multiple concurrent sessions per connection; takka's engine is single-session by design. engine-per-session gives multi-session *without* touching the engine — cleanly separate from Fase 3b (one engine, many clients on takka protocol v3) |
| D7 | Capabilities advertised (v1) | `loadSession: true` + `sessionCapabilities.resume` (stored-session lifecycle over the checkpoint store, pulled forward 2026-11-14), `promptCapabilities`: `image: true` (vision joins already exist), `audio: false`, `embeddedContext: false`; `mcpCapabilities`: `http:false`, `sse:false`; `authMethods: []` — takka reads its user config natively; `fs.readTextFile/writeTextFile: false`, `terminal: false` | takka owns filesystem access and command execution with its own safety layer (risk scoring, allowlists, audit log). The Client must not proxy file/terminal calls for takka (that's the ACP *trust* model decision: editor grants file system, agent self-manages safety) |
| D8 | Permission mapping | `ConfirmationRequest { action, description, risk_level, target }` becomes ONE ACP `session/request_permission`; options: `allow_once` ("Allow"), `allow_always` ("Always allow"), `reject_once` ("Reject"). `allow_always` persists the `target` command on the safety engine's dynamic auto-approve list for the rest of the session (in-memory, not written to config); chained/destructive commands keep prompting | ACP permission options are y/n/always shaped; the pending-confirmation channel already blocks a tool boundary, so the answer slots straight into `ConfirmResponse` semantics. The dynamic list (`Safety::allow_command`, 2026-11-13) removes the every-command prompt friction. `question` tool stays mid-turn via v1 `elicitation` as phase-2 |
| D9 | Modes | `default / plan / yolo` advertised as ACP v1 **session modes** (SessionMode `{ id, name, description }`; `session/set_mode` → `InitOverrides.permission_mode`); mode-change laws honored only at turn boundaries | Direct 1:1 with takka's PermissionMode; plan-mode restrictions already enforced engine-side |
| D10 | No engine changes | `takka-core` untouched (engine, protocol, transports); adapter translates ACP ⇄ `MemoryTransport` events | Same discipline that landed daemon mode; no protocol bump, e2e smoke keeps green |
| D11 | Per-session cwd | **Single-cwd adapter (option c):** `session/new.cwd` MUST equal the process cwd; a mismatch is rejected with an actionable error ("restart the agent from project directory X"); Zed/ACP Clients spawn one agent process per project, so this matches how Clients already work | Threading per-session cwd through all 16 path-resolving tools (option b) is the first engine-adjacent change since daemon mode — touches path sandbox, glob, grep, run_command; too big to smuggle into this phase. Silent tolerance (option a) would read/write the wrong directory — rejected as incorrect |
| D12 | Turn-limit stop reasons | takka v2 protocol gains **additive** events: `Event::TurnLimitExceeded { kind: max_tokens | max_turn_requests }` emitted when the loop cap / token budget ends a turn (no version bump — mirrors how `SessionAdopted` and `Usage` were added); the ACP adapter maps them to ACP `stopReason: max_tokens` / `max_turn_requests`; non-ACP frontends may surface a notice | ACP exposes distinct reasons; silently degrading both to `end_turn` would hide the real cause (user decision 2026-11-10: "tambahkan saja event tambahan v2") |

## 3. Target architecture

```text
Zed (or any ACP Client)
   │  boots subprocess, stdin/stdout JSON-RPC (newline-framed)
   ▼
takka-runtime --acp
┌────────────────────────────────────────────────────────────┐
│  acp.rs (new, adapter)                [agent-client-protocol crate]
│  ┌─────────────────────── per session ───────────────────┐
│  │ session#1              session#2            session#N │
│  │ EngineThread = RuntimeEngine(MemoryTransport)          │
│  │   + InitOverrides (merged client cwd/mode/config)      │
│  └────────────────────────────────────────────────────────┘
│  events: thinking→agent_thought_chunk, assistant_delta→agent_message_chunk,
│  tool_started→tool_call(in_progress), tool_output→tool_call_update(+final),
│  ConfirmationRequest→session/request_permission,
│  Done→stopReason, cooperative cancel←session/cancel
└────────────────────────────────────────────────────────────┘
```

New modules (all in one place, mirroring the daemon structure):

| Component | Location | Responsibility |
|---|---|---|
| `--acp` flag + framing loop | `takka-runtime/src/main.rs` | start adapter, line-buffer JSON-RPC I/O via `agent-client-protocol` crate |
| ACP Agent trait impl (`initialize`, `session/new`, `session/prompt`, `session/cancel`) | `takka-runtime/src/acp.rs` | negotiate v1, spawn/reap session engines, dispatch prompts, cancel |
| Event translator (pure function) | `takka-runtime/src/acp.rs` | `ProtocolEvent` (takka) → ACP `session/update`; unit-testable without a provider |
| Permission bridging | same | `ConfirmationRequest` → `request_permission` (outbound JSON-RPC request!) → `ConfirmResponse` |
| Session registry + timeout/bounded-concurrency | same | ≤ max-sessions, drop idle engines, engine panic → ACP `error` + session teardown |

Note: ACP permission + fs/terminal are **bidirectional** — the agent calls the Client.
The adapter's framing loop must therefore be concurrent: I/O pump thread + per-session
updaters, mirroring daemon's slot/drain ideas but in stdio form.

## 4. Event & method mapping (v1)

### Client → Agent

| ACP | takka | Notes |
|---|---|---|
| `initialize` | — | pick `protocolVersion: 1`; respond capabilities per D7/D9 |
| `session/new { cwd, mcpServers }` | put session + engine spawn | `cwd` recorded; **per-session cwd is an open question (see §7)**; `mcpServers` accepted-but-unused in phase 1 (client-configured MCP lands in phase 2) |
| `session/prompt { sessionId, prompt }` | `Request::Run` | content blocks: `text` → user message; `image {data,mimeType}` → `Attachment` (PNG/JPEG/GIF/WebP); `resource_link` `file://` → inlined (text files as fenced blocks bounded at 128 KiB, image files as `Attachment`s, directories as a path note — the editor's @-file mentions; 2026-11-14); `resource`/`audio`/non-file URIs → error "unsupported content type" (embeddedContext stays false) |
| `session/cancel` | cooperative cancel `Arc<AtomicBool>` | mid-turn only; silence if idle per spec |
| `session/set_mode { modeId }` | `InitOverrides.permission_mode` (applied next turn) | D9 |

### Agent → Client notifications (`session/update`)

| takka event | ACP update | Notes |
|---|---|---|
| `thinking` (activated by caps) | `agent_thought_chunk` | gate by negotiated prompt caps` |
| `assistant_delta` | `agent_message_chunk` | pass-through, throttle not required (client can render) |
| `tool_started { name, args }` | `tool_call` `in_progress` | kind mapping (ACP ToolKind consts: read/edit/delete/move/search/execute/think/fetch/switch_mode/other): `read_file/read_mem` → `read`; `glob/grep/search` → `search`; `write_file/edit_file/apply_patch` → `edit`; `run_command/run_script` → `execute`; `fetch/web_search` → `fetch`; `spawn_subagent/question/todowrite/update_memory` → `other` |
| `tool_output` (stream) | `tool_call_update` + `content` blocks | streamed output as text content block |
| final tool result (`success`, `truncated`, `duration_ms`) | `tool_call_update` → `completed` / `failed` (+ `rawOutput`) | failure = non-success result or safety rejection; `todo_changed` surfaces inside map per takka widget contract only if trivial — otherwise skip in phase 1 |
| `ConfirmationRequest` | `session/request_permission` (agent→client request) | D8 |
| `Done { result }` | `session/prompt` response `stopReason: end_turn` | |
| cooperative cancel completes | `stopReason: cancelled` | |
| safety hard-block turn | `stopReason: refusal` | `warn`/`error` events also surface as `error notice` update |
| engine panic | JSON-RPC `error` for prompt + session teardown | "runtime died" UX matches daemon |
| `Usage` / tool timeline details | **not mapped in phase 1** | `_meta` extension candidate later |

`stopReason` for turn limits: with D12's additive `Event::TurnLimitExceeded`, the adapter
maps `max_tokens` → `stopReason: max_tokens` and `max_turn_requests` →
`stopReason: max_turn_requests` instead of degrading to `end_turn`.

## 5. Data flow

1. **Boot:** Client spawns `takka-runtime --acp` (registered command in client config,
   e.g. Zed: `{ "command": "takka-runtime", "args": ["--acp"] }`).
2. **Initialize:** Client `initialize` (may request v2) → adapter answers v1 + caps →
   Client decides; then optionally `authenticate` is skipped (`authMethods: []`) —
   credentials come from the user's `~/.config/takka/config.json` at engine init; if none,
   first prompt surfaces a notice + refusal instead of discovering auth flows.
3. **Session:** `session/new` → registry claims a slot (else JSON-RPC error with `code`
   "session capacity"), engine thread launched with `MemoryTransport`, `Init` performed
   with overrides (config path default, permission mode from modeId), `InitOk` awaited.
4. **Prompt turn:** `session/prompt` → content mapped → `Run` → stream updates →
   perm request(s) if gates fire → `Done`/cancel/panic resolution → response stopReason.
5. **Turn boundary:** second prompt while busy → error `session busy`; `set_mode`
   applied between turns.
6. **Teardown:** connection EOF → cancel all engines, graceful join (bounded joins —
   reuse the daemon `join_bounded` pattern), process exits 0. SIGINT → same path.

## 6. Testing strategy (hermetic, mirrors CI discipline)

No real provider/network in unit tests (ambient config never read — follow the
hermetic pinning from `hermetic_runtime_config` in takka-cli daemon tests):

| S | Scenario | Expect |
|---|---|---|
| T-01 | `initialize` requesting v2, then requesting v1 | both answered `protocolVersion: 1` + capability set per D7 (pure unit test of init-responder) |
| T-02 | event translator unit tests | `thinking`→`agent_thought_chunk`, `assistant_delta`→`agent_message_chunk`, `tool_started` per-kind mapping, `tool_output` stream→update, final→`completed`/`failed`, `Done`→end_turn, cancel→cancelled, blocklist→refusal+notice |
| T-03 | session lifecycle hermetically | `session/new` (temp-dir config via `InitOverrides.config_path`) → engine boots → `session/prompt` against pinned unreachable provider (`127.0.0.1:9`) → turn fails cleanly → second prompt still works (no poisoned session) |
| T-04 | over-capacity | N+1-th `session/new` → JSON-RPC error `session capacity reached` |
| T-05 | cancel mid-turn | user message → immediately `session/cancel` → prompt resolves `cancelled`; state machine recovers for next prompt |
| T-06 | permission flow | scripted `ConfirmationRequest` (unit-level) → outbound `request_permission` JSON asserted; `cancel` outcome maps to `ConfirmResponse(false)` |
| T-07 | shutdown/EOF | client closes stdin → all engines joined within bound; process exits 0 |
| T-08 | CI sanity | `--acp` flag compiles on all 3 OS; Windows gets no new cfg faces (adapter is stdio, fully portable) |

The Zed project ships a **Test Compatibility Kit** for ACP agents; after bootstrap we run
it once manually and record the result into the implementation report (not a CI gate in
this phase — no `.github/` change is allowed without asking first).

## 7. Out of scope (phase 2 backlog) & open questions

**Phase 2 backlog (tracked when phase 1 lands):**

- Per-session cwd (option b): thread a per-session cwd base through tool path
  resolution in takka-core (path sandbox, glob, grep, run_command), so one adapter
  process may host sessions across projects — the real fix behind D11's phase-1
  restriction
- Turn-limit wiring (D12) into the CLI/TUI status line (the additive event may surface
  as a notice only in non-ACP frontends at first)
- ~~`session/load` / `loadSession` (map the `.config/takka/sessions` store to ACP session IDs)~~ — **landed 2026-11-14 (pulled forward)**: `session/load` + `session/resume` map the checkpoint store to ACP session ids; the ACP session id doubles as the checkpoint filename, so a client's saved id keeps working across agent restarts
- Client-provided `mcpServers` at `session/new` (takka-core already has an MCP client — wire config through)
- `available_commands_update`: expose takka slash commands to the editor's "/" menu
- v1 `elicitation` for the `question` tool (structured ask UI)
- `EmbeddedResource` prompt blocks (`resource` with inline text/blob payloads) — `resource_link` is already inlined (2026-11-14, pulled forward)
- ACP v2 support behind explicit negotiation (per the v1+v2 side-by-side guide)
- `Usage` details + context-compaction notifications via `_meta`/future ACP fields

**Not planned for now (decision 2026-11-10):** ACP Registry listing.

## Resolved decisions (user, 2026-11-10)

1. **Per-session cwd → option (c), single-cwd adapter** (D11). Phase 1 validates
   `session/new.cwd == process.cwd()` and rejects mismatches with an actionable error;
   option (b) (per-session cwd threading in takka-core) moves to the phase-2 backlog as
   the real fix; option (a) (silent tolerance) is rejected outright — it would silently
   operate in the wrong directory.
2. **Turn-limit stop reasons → additive takka v2 events** (D12): the adapter maps the
   real reasons (`max_tokens`, `max_turn_requests`) instead of degrading to `end_turn`.
   See D12 for the event shape and the no-bump rationale.
3. **ACP Registry listing → not planned for now** (explicit user decision; revisit only
   if an ecosystem need appears).

## 8. Risks

| Risk | Mitigation |
|---|---|
| crates `agent-client-protocol` v5.x majors fast | pin = spec-exact version in Cargo.toml; migration upstream notes followed |
| stdin/stdout never interleaved with logs | engine's memory transport emits **no stderr**; tracing writer forced to stderr-only (same discipline daemon lives by) |
| multiple prompts racing a session | explicit turn-state flag; busy → protocol error (not queueing — ACP clients serialize turns) |
| engine-per-session session token cost | bounded max-sessions + idle reaping; note: not sharing one engine until 3b |

---

*Decision note 2026-11-10: replaces the "Fase 4 — SDK bahasa lain" plan; cycle order
recorded as D housekeeping → ACP → issue #2 → issue #4 (see ROADMAP Open work).*
