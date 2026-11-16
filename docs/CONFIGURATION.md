# 📄 Configuration Schema Documentation

## 1. Config File Structure

This file defines available AI providers, their models, and enabled tools.

### Structure

```json
{
  "providers": [
    {
      "name": "string",
      "type": "openai-compatible",
      "api_base": "string (URL)",
      "api_key": "string",
      "models": [
        {
          "model": "string",
          "display_name": "string",
          "temperature": "number",
          "max_tokens": "number"
        }
      ]
    }
  ],
  "safety": {
    "auto_approve_low_risk": true,
    "blocked_commands": ["string"]
  },
  "output": {
    "color": true,
    "stream": true,
    "show_thoughts": true,
    "show_tool_calls": true
  },
  "agent": {
    "max_iterations": 30,
    "max_cost_usd": null
  },
  "mcp_servers": {
    "<server_name>": {
      "command": "string",
      "args": ["string"],
      "env": { "KEY": "VALUE" },
      "url": "string (URL)",
      "headers": { "Header-Name": "value" }
    }
  }
}
```

---

### Field Explanation

#### `providers`

List of AI providers.

* `name` → Provider name (e.g., `"z.ai"`, `"OpenAI"`)
* `type` → API compatibility type (currently `"openai-compatible"`)
* `api_base` → Base URL for API requests
* `api_key` → Authentication key
* `models` → List of available models under this provider

#### `models`

Defines each model configuration:

* `model` → Internal model ID (used in API calls)
* `display_name` → Human-readable name (optional, auto-generated if missing)
* `temperature` → Sampling temperature (default: `0.7`)
* `max_tokens` → Maximum token limit per request (default: `8192`)

---

#### `safety`

Security and approval settings:

* `auto_approve_low_risk` → Automatically approve low-risk tool calls (default: `true`)
* `blocked_commands` → List of command patterns to block
* `require_confirmation` → Ask before medium+ risk actions (default: `true`)
* `auto_approve_commands` → `run_command` prefixes that run **without a
  prompt** even though `run_command` carries a risk floor. Defaults cover
  read-only inspection (`git status`, `git log`, `git diff`, `ls`, `cat`,
  `grep`, `rg`, `find`, `cargo check`, …) and local, reflog-undoable git edits
  (`git add`, `git commit`, `git stash`, `git switch -c`, …). Matching is
  strict: a command is auto-approved only if **every** chained segment
  (`&&`, `;`) is on the list, has no blocking flag (`--force`, `--hard`, `-D`,
  `--delete`, `-exec`, …) and no pipeline/substitution/redirection (`|`,
  `` ` ``, `$(`, `>`, `<`). `git push`, `git reset`, `git rebase`, `git clean`
  and unknown commands still prompt. Set to `[]` to prompt for everything.

  Plan mode (`--mode plan`) uses the same list to decide what counts as
  read-only: inspection runs, everything that mutates state is denied.

---

#### `output`

CLI output preferences:

* `color` → Enable colored output (default: `true`)
* `stream` → Enable streaming responses (default: `true`)
* `show_thoughts` → Show the model's pre-tool reasoning as a DIM
  "thinking" block above the tool activity (default: `true`). Long blocks are
  capped at 12 lines. Override per session with `--no-thoughts`, or toggle
  live in the REPL with `/thoughts`
* `/timeline` (REPL) → `detailed` (default) or `compact` tool-timeline detail;
  `compact` prints one row per tool with no nested output, errors or diffs
* `show_tool_calls` → Show tool call details (default: `true`)

---

#### `agent`

Agent-loop knobs. Every key is optional; omitting the block keeps the
built-in defaults.

* `max_iterations` → Hard cap on agent-loop iterations for one user turn
  (default: `30`). The last allowed turn is a forced finalization: tools are
  stripped and the model must answer in text, so hitting the cap still yields
  a result.
* `max_cost_usd` → Spend ceiling for **one user turn**, in US dollars
  (default: `null` = unlimited, i.e. the behaviour before this key existed).

  The cost of every provider call is priced against a built-in list-price
  table (USD per million tokens, including prompt-cache reads/writes) and
  accumulated for the run. When the running total reaches the ceiling, the
  loop stops with `cost_limit_exceeded` **after the crossing turn completes** —
  a tool batch that turn requested still runs, and an in-flight/provider
  stream is never cut mid-response. The work already paid for is returned as
  the answer; the stop is a policy outcome, not a failure.

  The counter restarts at every turn, so `max_cost_usd: 0.50` allows up to
  about $0.50 per prompt, not per session. Models with no entry in the price
  table cannot be priced, so the ceiling stays unevaluated for them
  (fail-open) rather than stopping a run on a guess.

  The REPL and TUI show the remaining budget in their status lines while a
  ceiling is set, and the stop notice tells you to raise or remove the key.

**Example — cap each turn at 25 cents:**

```json
"agent": {
  "max_cost_usd": 0.25
}
```

---

#### `mcp_servers`

Map of external MCP (Model Context Protocol) servers whose tools are discovered and registered at startup. Each entry key is an arbitrary name you choose.

Two transport modes:

**stdio** (spawn a child process):
* `command` → Executable to run (e.g., `"npx"`, `"python"`)
* `args` → Arguments passed to the command
* `env` → Optional environment variables for the child process

**HTTP** (POST JSON-RPC to an endpoint):
* `url` → MCP server URL (e.g., `"http://localhost:3001/mcp"`)
* `headers` → Optional HTTP headers (e.g., `{"Authorization": "Bearer <token>"}`)

Use **either** stdio (`command` + `args`) **or** HTTP (`url`), not both.

**Example — stdio server:**

```json
"mcp_servers": {
  "filesystem": {
    "command": "npx",
    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/home/user/projects"]
  }
}
```

**Example — HTTP server:**

```json
"mcp_servers": {
  "my-api": {
    "url": "http://localhost:3001/mcp",
    "headers": {
      "Authorization": "Bearer my-secret-token"
    }
  }
}
```

---

## 2. Output Mapping

This represents how models should be displayed externally (e.g., UI dropdown).

### Structure

```json
[
  {
    "name": "string",
    "model": "string"
  }
]
```

---

### Field Explanation

* `name` → Display name shown to users
* `model` → Corresponding model identifier

---

## 3. Example Mapping

### Input (Config)

* Provider: `z.ai`
* Models:

  * `glm-4.7`
  * `glm-5`

### Output

```json
[
  {
    "name": "GLM-4.7 Free (z.ai)",
    "model": "GLM-4.7"
  },
  {
    "name": "GLM-5 Free (z.ai)",
    "model": "GLM-5"
  }
]
```

---

## 4. Notes & Recommendations

* Normalize model naming (e.g., `glm-4.7` → `GLM-4.7`) for consistent UI display.
* Keep `display_name` as the source of truth for user-facing labels.
* Avoid hardcoding API keys in config files for production—use environment variables instead.
* Ensure `api_base` endpoints are valid and follow OpenAI-compatible format.
* Default model usage is providers[0].models[0] if not specified by the user.
