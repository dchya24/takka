#!/usr/bin/env bash
#
# ACP v1 conformance check for `takka-runtime --acp` (Fase 4).
#
# The Zed project's official "Test Compatibility Kit" is not published on
# crates.io/npm (checked: `agent-client-protocol-tck` is 404 on both), so this
# script is a hand-written conformance run over the v1 wire contract instead of
# the official kit. It drives the REAL binary over stdio, exactly like a client
# (Zed) would, and asserts the documented behaviour of every implemented
# method. Design: docs/superpowers/specs/2026-11-10-takka-acp-agent-design.md
#
# Hermetic: the session engine is pinned to a discard-port provider through
# `TAKKA_TEST_ACP_CONFIG` (a test-only env pin — see acp::serve_with_config), so
# the ambient `~/.config/takka/config.json` is never read and no real provider
# is ever called. `127.0.0.1:9` refuses instantly, which is all these checks
# need (boot succeeds; turns fail fast and cleanly).
#
# Usage: scripts/acp-conformance.sh [--verbose]
# Exit:  0 = every check passed, 1 = at least one check failed.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT" || exit 1

VERBOSE=0
[ "${1:-}" = "--verbose" ] && { VERBOSE=1; ACP_CONFORMANCE_VERBOSE=1; }

PASS=0
FAIL=0
FAILED_CHECKS=""

ok() {
    PASS=$((PASS + 1))
    printf 'PASS  %s\n' "$1"
}

bad() {
    FAIL=$((FAIL + 1))
    FAILED_CHECKS="$FAILED_CHECKS
  - $1"
    printf 'FAIL  %s\n' "$1"
    [ -n "${2:-}" ] && printf '      %s\n' "$2"
}

# Timeout helper: GNU coreutils calls it `timeout`, macOS (brew coreutils) ships
# `gtimeout`, and a bare macOS install has neither — in that case the command
# runs unguarded (every check below is still bounded by the caller's own reads).
TIMEOUT_BIN=""
if command -v timeout >/dev/null 2>&1; then
    TIMEOUT_BIN="timeout"
elif command -v gtimeout >/dev/null 2>&1; then
    TIMEOUT_BIN="gtimeout"
fi

bounded() {
    local secs="$1"
    shift
    if [ -n "$TIMEOUT_BIN" ]; then
        "$TIMEOUT_BIN" "$secs" "$@"
    else
        "$@"
    fi
}

# --- build the binary under test -------------------------------------------
echo "== building takka-runtime =="
if ! bounded 600 cargo build -p takka-runtime >/tmp/acp-conformance-build.log 2>&1; then
    echo "FATAL: cargo build -p takka-runtime failed; see /tmp/acp-conformance-build.log"
    exit 1
fi
BIN="$REPO_ROOT/target/debug/takka-runtime"
[ -x "$BIN" ] || { echo "FATAL: $BIN missing after build"; exit 1; }

# --- hermetic provider config ----------------------------------------------
WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/takka-acp-conformance.XXXXXX")" || exit 1
cleanup() {
    [ -n "${ACP_PID:-}" ] && kill "$ACP_PID" 2>/dev/null
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

CFG="$WORKDIR/config.json"
cat >"$CFG" <<'JSON'
{
  "providers": [
    {
      "name": "conformance-hermetic",
      "type": "openai-compatible",
      "api_base": "http://127.0.0.1:9",
      "api_key": "unused",
      "models": [
        { "model": "hermetic-model", "temperature": 0.0, "max_tokens": 64 }
      ]
    }
  ]
}
JSON

# stdin MUST be a FIFO, not a regular file: the agent's reader blocks on a pipe
# and only sees EOF when we close the writer. With a regular file it would hit
# EOF after the first frames and stop reading later ones.
IN="$WORKDIR/in.fifo"
OUT="$WORKDIR/out.jsonl"
mkfifo "$IN" || exit 1
: >"$OUT"

# The agent must be started FROM the project directory it will serve: the
# adapter is single-cwd by decision D11 and rejects a mismatched
# `session/new.cwd`.
cd "$REPO_ROOT" || exit 1
TAKKA_TEST_ACP_CONFIG="$CFG" "$BIN" --acp <"$IN" >"$OUT" 2>"$WORKDIR/stderr.log" &
ACP_PID=$!

# Hold the writer end open so the agent's stdin stays live until we close it.
exec 3>"$IN"

send() { printf '%s\n' "$1" >&3; }

# Wait (bounded) until the file contains a line whose compact JSON has the
# given request id, then print that line. Uses python3 for real JSON parsing so
# assertions never depend on field order.
await_id() {
    local id="$1" deadline_s="${2:-15}" waited=0
    while [ "$waited" -lt $((deadline_s * 20)) ]; do
        if python3 - "$OUT" "$id" <<'PY'
import json, sys
path, wanted = sys.argv[1], sys.argv[2]
try:
    lines = open(path, encoding="utf-8", errors="replace").read().splitlines()
except FileNotFoundError:
    sys.exit(1)
for line in lines:
    line = line.strip()
    if not line:
        continue
    try:
        frame = json.loads(line)
    except json.JSONDecodeError:
        continue
    if isinstance(frame, dict) and str(frame.get("id")) == wanted:
        # Diagnostics only: the pass/fail lines are the script's real output.
        import os
        if os.environ.get("ACP_CONFORMANCE_VERBOSE"):
            print(line, file=sys.stderr)
        sys.exit(0)
sys.exit(1)
PY
        then
            [ "$VERBOSE" = 1 ] && printf '      (id %s answered)\n' "$id"
            return 0
        fi
        sleep 0.05
        waited=$((waited + 1))
    done
    return 1
}

# Field assertion on the last answered frame for an id.
assert_field() {
    local id="$1" dotted="$2" expected="$3" label="$4"
    if python3 - "$OUT" "$id" "$dotted" "$expected" <<'PY'
import json, sys
path, wanted, dotted, expected = sys.argv[1:5]
frames = []
for line in open(path, encoding="utf-8", errors="replace").read().splitlines():
    line = line.strip()
    if not line:
        continue
    try:
        frame = json.loads(line)
    except json.JSONDecodeError:
        continue
    if isinstance(frame, dict) and str(frame.get("id")) == wanted:
        frames.append(frame)
if not frames:
    sys.exit(1)
value = frames[-1]
for part in dotted.split("."):
    if isinstance(value, list):
        value = value[int(part)]
    elif isinstance(value, dict):
        if part not in value:
            sys.exit(1)
        value = value[part]
    else:
        sys.exit(1)
sys.exit(0 if json.dumps(value) == json.dumps(json.loads(expected)) else 1)
PY
    then
        ok "$label"
    else
        bad "$label" "expected $dotted == $expected"
    fi
}

# --- 1) initialize negotiates v1 and advertises the D7 capability set ------
send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":2,"clientCapabilities":{"fs":{"readTextFile":true}},"clientInfo":{"name":"conformance","version":"0.0.0"}}}'
if await_id 1; then
    assert_field 1 "result.protocolVersion" '1' "initialize answers protocolVersion 1 (client asked for 2)"
    assert_field 1 "result.agentCapabilities.promptCapabilities.image" 'true' "initialize advertises image prompts"
    assert_field 1 "result.agentCapabilities.loadSession" 'true' "initialize advertises loadSession: true"
    assert_field 1 "result.agentCapabilities.sessionCapabilities.resume" '{}' "initialize advertises sessionCapabilities.resume"
    assert_field 1 "result.agentInfo.name" '"takka"' "initialize reports agentInfo.name = takka"
else
    bad "initialize is answered" "no response to id 1 within 15s"
fi

# --- 2) session/new boots a session and advertises the D9 modes ------------
CWD_JSON=$(python3 -c 'import json,os;print(json.dumps(os.getcwd()))')
send "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{\"cwd\":$CWD_JSON,\"mcpServers\":[]}}"
SESSION=""
if await_id 2; then
    SESSION=$(python3 - "$OUT" <<'PY'
import json, sys
for line in open(sys.argv[1], encoding="utf-8", errors="replace").read().splitlines():
    line = line.strip()
    if not line:
        continue
    try:
        frame = json.loads(line)
    except json.JSONDecodeError:
        continue
    if isinstance(frame, dict) and str(frame.get("id")) == "2" and isinstance(frame.get("result"), dict):
        print(frame["result"].get("sessionId", ""))
        break
PY
)
    if [ -n "$SESSION" ]; then
        ok "session/new returns a sessionId"
        assert_field 2 "result.modes.currentModeId" '"default"' "session/new advertises default as the current mode"
        assert_field 2 "result.modes.availableModes.1.id" '"plan"' "session/new advertises the plan mode"
        assert_field 2 "result.configOptions.0.id" '"model"' "session/new advertises the model config option"
        assert_field 2 "result.configOptions.0.currentValue" '"conformance-hermetic/hermetic-model"' "session/new reports the active model"
        assert_field 2 "result.configOptions.1.id" '"mode"' "session/new advertises the mode config option"
        assert_field 2 "result.configOptions.1.currentValue" '"default"' "session/new reports the default permission mode"
    else
        bad "session/new returns a sessionId" "no sessionId in the result"
    fi
else
    bad "session/new is answered" "no response to id 2 within 15s"
fi

# --- 3) D11: a foreign cwd is rejected with an actionable error -----------
send '{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/","mcpServers":[]}}'
if await_id 3; then
    assert_field 3 "error.code" '-32602' "session/new with a foreign cwd is invalid_params"
    if grep -q "restart the agent from project directory" "$OUT"; then
        ok "the cwd rejection names the project directory to restart from"
    else
        bad "the cwd rejection names the project directory to restart from" "message not found in the stream"
    fi
else
    bad "foreign-cwd session/new is answered" "no response to id 3 within 15s"
fi

# --- 4/5) prompts: fail cleanly, and the session survives ------------------
if [ -n "$SESSION" ]; then
    send "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"$SESSION\",\"prompt\":[{\"type\":\"text\",\"text\":\"hello\"}]}}"
    if await_id 4 30; then
        if python3 - "$OUT" <<'PY'
import json, sys
for line in open(sys.argv[1], encoding="utf-8", errors="replace").read().splitlines():
    line = line.strip()
    if not line:
        continue
    try:
        frame = json.loads(line)
    except json.JSONDecodeError:
        continue
    if isinstance(frame, dict) and str(frame.get("id")) == "4":
        # Either honest shape proves the turn resolved: an error result (the
        # pinned provider refuses instantly) or a stopReason.
        has_error = isinstance(frame.get("error"), dict)
        has_stop = isinstance(frame.get("result"), dict) and "stopReason" in frame["result"]
        sys.exit(0 if (has_error or has_stop) else 1)
sys.exit(1)
PY
        then
            ok "session/prompt resolves (error or stopReason) against the unreachable provider"
        else
            bad "session/prompt resolves (error or stopReason)" "response carried neither error nor stopReason"
        fi
    else
        bad "session/prompt is answered" "no response to id 4 within 30s"
    fi

    send "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"$SESSION\",\"prompt\":[{\"type\":\"text\",\"text\":\"again\"}]}}"
    if await_id 5 30; then
        ok "a second prompt still works on the same session (no poisoned session)"
    else
        bad "a second prompt still works on the same session" "no response to id 5 within 30s"
    fi

    # --- 6) D9: set_mode switches an idle session ------------------------
    send "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"session/set_mode\",\"params\":{\"sessionId\":\"$SESSION\",\"modeId\":\"plan\"}}"
    if await_id 6; then
        if python3 - "$OUT" <<'PY'
import json, sys
for line in open(sys.argv[1], encoding="utf-8", errors="replace").read().splitlines():
    line = line.strip()
    if not line:
        continue
    try:
        frame = json.loads(line)
    except json.JSONDecodeError:
        continue
    if isinstance(frame, dict) and str(frame.get("id")) == "6":
        sys.exit(0 if isinstance(frame.get("result"), dict) else 1)
sys.exit(1)
PY
        then
            ok "session/set_mode switches the mode (empty result, no error)"
        else
            bad "session/set_mode switches the mode" "got an error frame"
        fi
    else
        bad "session/set_mode is answered" "no response to id 6 within 15s"
    fi
else
    bad "prompt checks skipped" "no session was created"
fi

# --- 7) unknown method is method-not-found --------------------------------
send '{"jsonrpc":"2.0","id":7,"method":"session/_conformance_probe","params":{}}'
if await_id 7; then
    assert_field 7 "error.code" '-32601' "an unknown method is JSON-RPC method not found"
else
    bad "unknown method is answered" "no response to id 7 within 15s"
fi

# --- 8) stored-session lifecycle: session/load replays the checkpoint -----
# The checkpoint document is written straight into the hermetic store
# (derived from the config file's directory) — exactly what a previous
# session would have left behind.
SESS_DIR="$WORKDIR/config-sessions"
mkdir -p "$SESS_DIR"
cat >"$SESS_DIR/conformance-load-1.json" <<'JSON'
{
  "version": 1,
  "session_id": "conformance-load-1",
  "state": "completed",
  "model": "hermetic-model",
  "created_at": "2026-11-14T00:00:00Z",
  "updated_at": "2026-11-14T00:00:00Z",
  "messages": [
    { "id": "m1", "role": { "type": "user" }, "content": "hello from checkpoint", "timestamp": "2026-11-14T00:00:00Z" },
    { "id": "m2", "role": { "type": "assistant" }, "content": "checkpoint answer", "timestamp": "2026-11-14T00:00:00Z" }
  ],
  "checkpoint_count": 1
}
JSON
send "{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"session/load\",\"params\":{\"sessionId\":\"conformance-load-1\",\"cwd\":$CWD_JSON,\"mcpServers\":[]}}"
if await_id 8; then
    assert_field 8 "result.modes.currentModeId" '"default"' "session/load answers with the D9 mode surface"
    if python3 - "$OUT" <<'PY'
import json, sys
lines = [l.strip() for l in open(sys.argv[1], encoding="utf-8", errors="replace").read().splitlines() if l.strip()]
updates, response_at = [], None
for i, line in enumerate(lines):
    try:
        frame = json.loads(line)
    except json.JSONDecodeError:
        continue
    if frame.get("method") == "session/update":
        updates.append(frame)
    elif str(frame.get("id")) == "8" and "result" in frame:
        response_at = i
if response_at is None:
    sys.exit(1)
texts = []
for f in updates:
    update = f.get("params", {}).get("update", {})
    if update.get("sessionUpdate") in ("user_message_chunk", "agent_message_chunk"):
        texts.append((update.get("sessionUpdate"), update.get("content", {}).get("text")))
sys.exit(0 if texts == [("user_message_chunk", "hello from checkpoint"), ("agent_message_chunk", "checkpoint answer")] else 1)
PY
    then
        ok "session/load replays the stored history as session updates"
    else
        bad "session/load replays the stored history" "expected user+assistant chunks before the response"
    fi
else
    bad "session/load is answered" "no response to id 8 within 15s"
fi

# --- 9) session/set_config_option switches the mode -----------------------
send "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"session/set_config_option\",\"params\":{\"sessionId\":\"$SESSION\",\"configId\":\"mode\",\"value\":\"plan\"}}"
if await_id 9; then
    if python3 - "$OUT" <<'PY'
import json, sys
for line in open(sys.argv[1], encoding="utf-8", errors="replace").read().splitlines():
    line = line.strip()
    if not line:
        continue
    try:
        frame = json.loads(line)
    except json.JSONDecodeError:
        continue
    if isinstance(frame, dict) and str(frame.get("id")) == "9":
        sys.exit(0 if isinstance(frame.get("result"), dict) else 1)
sys.exit(1)
PY
    then
        ok "session/set_config_option(mode=plan) succeeds (result object, no error)"
    else
        bad "session/set_config_option(mode=plan)" "got an error frame"
    fi
    assert_field 9 "result.configOptions.1.currentValue" '"plan"' "session/set_config_option reports the new current mode"
else
    bad "session/set_config_option is answered" "no response to id 9 within 15s"
fi

# --- 10) resource_link @-file mentions are inlined -------------------------
printf 'the mentioned note\n' >"$WORKDIR/mention.md"
MENTION_URI="file://$WORKDIR/mention.md"
send "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"$SESSION\",\"prompt\":[{\"type\":\"text\",\"text\":\"look at this\"},{\"type\":\"resource_link\",\"name\":\"mention\",\"mime_type\":\"text/markdown\",\"uri\":\"$MENTION_URI\"}]}}"
if await_id 10; then
    if python3 - "$OUT" "$MENTION_URI" <<'PY'
import json, sys
path, uri = sys.argv[1:3]
for line in open(path, encoding="utf-8", errors="replace").read().splitlines():
    line = line.strip()
    if not line:
        continue
    try:
        frame = json.loads(line)
    except json.JSONDecodeError:
        continue
    if isinstance(frame, dict) and str(frame.get("id")) == "10":
        error = frame.get("error") or {}
        # The turn fails against the pinned discard provider (internal
        # error) — but an invalid-params error would mean the resource
        # link was rejected instead of inlined.
        sys.exit(0 if error.get("code") != -32602 else 1)
sys.exit(1)
PY
    then
        ok "session/prompt inlines a file:// resource_link (no invalid-params)"
    else
        bad "session/prompt inlines a file:// resource_link" "the resource_link was rejected"
    fi
else
    bad "resource_link prompt is answered" "no response to id 10 within 15s"
fi

# --- 11) EOF: the client closes stdin, the agent exits 0 ------------------
# Closing the FIFO writer is precisely what an ACP client does at shutdown.
exec 3>&-

waited=0
while kill -0 "$ACP_PID" 2>/dev/null && [ "$waited" -lt 300 ]; do
    sleep 0.05
    waited=$((waited + 1))
done
if kill -0 "$ACP_PID" 2>/dev/null; then
    bad "the agent exits after stdin EOF" "still running after EOF (15s)"
    kill "$ACP_PID" 2>/dev/null
else
    wait "$ACP_PID"
    STATUS=$?
    if [ "$STATUS" -eq 0 ]; then
        ok "the agent exits 0 after stdin EOF"
    else
        bad "the agent exits 0 after stdin EOF" "exit status $STATUS"
    fi
fi
ACP_PID=""

# --- summary -------------------------------------------------------------
echo
echo "== $PASS passed, $FAIL failed =="
if [ "$FAIL" -ne 0 ]; then
    printf 'failed checks:%s\n' "$FAILED_CHECKS"
    echo "(stderr from the agent: $WORKDIR/stderr.log — kept only on failure)"
    cp "$WORKDIR/stderr.log" /tmp/takka-acp-conformance-stderr.log 2>/dev/null
    exit 1
fi
echo "ACP v1 conformance: OK"
exit 0
