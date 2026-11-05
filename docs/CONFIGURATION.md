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
