# Agentic Tools Reference

The `core-agentic/src/tools/` directory contains **18 registered builtin tools**.
Each tool implements the `Tool` trait (`core-agentic/src/tool.rs`) with
`name()`, `description()`, `schema()`, `execute()`, and `is_read_only()`.

## Architecture

| File                    | Purpose                                                                 |
| ----------------------- | ----------------------------------------------------------------------- |
| `tool.rs`               | `Tool` trait, `ToolSchema`, `ToolParam`, `ToolCall`, `ToolResultValue`  |
| `tool_registry.rs`      | `ToolRegistry` — register, execute, list, `RwLock`-based concurrency   |
| `mod.rs`                | `builtin_tools()` — assembles the standard 18-tool set                  |

### Tool classification

| Category | Tools | `is_read_only` |
|----------|-------|:-:|
| File read | `read_file`, `list_files`, `glob`, `grep`, `search_files` | ✅ |
| VCS read | `git_status`, `git_diff` | ✅ |
| Web read | `fetch`, `web_search` | ✅ |
| Interactive | `question` | ✅ |
| File write | `write_file`, `edit_file`, `apply_patch` | ❌ |
| Execution | `run_command`, `run_script`, `run_tests` | ❌ |
| Agent | `spawn_subagent`, `update_memory`, `todowrite` | ❌ |

Read-only tools may be batched and executed concurrently by the orchestrator's
`handle_tool_calls_parallel` path. Mutating tools always run sequentially.

---

## Tool List

### run_command

Execute a shell command and return its output.

| Parameter | Type   | Required | Description                          |
| --------- | ------ | -------- | ------------------------------------ |
| command   | string | yes      | The command to execute               |
| cwd       | string | no       | Working directory for execution      |
| timeout   | number | no       | Optional timeout in milliseconds     |

---

### read_file

Read the contents of a file. Supports text files. Returns content with line tracking.
Integrates with `FileTracker` for staleness detection (edit_file rejects edits to files modified externally since last read).

| Parameter | Type   | Required | Description               |
| --------- | ------ | -------- | ------------------------- |
| path      | string | yes      | Absolute path to the file |
| offset    | number | no       | Start line (1-indexed)    |
| limit     | number | no       | Max lines to return       |

---

### edit_file

Performs exact string replacements in files. Rejects edits to stale files
(files modified externally since the agent last read them).

| Parameter   | Type    | Required | Description                                                    |
| ----------- | ------- | -------- | -------------------------------------------------------------- |
| file_path   | string  | yes      | Absolute path to the file to modify                            |
| old_string  | string  | yes      | The text to replace                                            |
| new_string  | string  | yes      | The text to replace it with (must differ from `old_string`)    |
| replace_all | boolean | no       | Replace all occurrences (default: first match only)            |

---

### write_file

Write content to a file. Overwrites existing files. Creates parent directories.

| Parameter | Type   | Required | Description                        |
| --------- | ------ | -------- | ---------------------------------- |
| path      | string | yes      | Absolute path to the file          |
| content   | string | yes      | Content to write                   |

---

### apply_patch

Apply a unified diff to one or more files atomically. Multi-file changes
expressed as a single tool call. All-or-nothing: failure halfway through
leaves the disk untouched.

| Parameter | Type   | Required | Description                              |
| --------- | ------ | -------- | ---------------------------------------- |
| patch     | string | yes      | Full unified diff text describing changes |

---

### list_files

List files in a directory.

| Parameter | Type   | Required | Description                     |
| --------- | ------ | -------- | ------------------------------- |
| path      | string | no       | Directory path (defaults to .)  |

---

### glob

Fast file pattern matching. Returns matching file paths sorted by modification time.

| Parameter | Type   | Required | Description                            |
| --------- | ------ | -------- | -------------------------------------- |
| pattern   | string | yes      | Glob pattern (e.g. `**/*.rs`)          |
| path      | string | no       | Directory to search in (defaults to .) |

---

### grep

Fast content search using regular expressions.

| Parameter | Type   | Required | Description                                              |
| --------- | ------ | -------- | -------------------------------------------------------- |
| pattern   | string | yes      | Regex pattern to search for                              |
| path      | string | no       | Directory to search in (defaults to .)                   |
| include   | string | no       | File pattern filter (e.g. `*.rs`, `*.{ts,tsx}`)          |

---

### search_files

Full-text content search across files. Case-insensitive.

| Parameter | Type   | Required | Description                                        |
| --------- | ------ | -------- | -------------------------------------------------- |
| query     | string | yes      | Text to search for                                 |
| path      | string | no       | Directory to search in (defaults to .)             |

---

### run_script

Execute multi-line scripts with optional timeout.

| Parameter | Type   | Required | Description                              |
| --------- | ------ | -------- | ---------------------------------------- |
| script    | string | yes      | Script content to execute                |
| cwd       | string | no       | Working directory                        |
| timeout   | number | no       | Optional timeout in milliseconds         |

---

### run_tests

Auto-detect the project's test runner and execute it. Returns structured output
(passed / failed / duration / stdout). Detection: Cargo → npm/pnpm/yarn → pytest → go test.

| Parameter | Type   | Required | Description                                  |
| --------- | ------ | -------- | -------------------------------------------- |
| filter    | string | no       | Test filter forwarded to the underlying runner |
| workdir   | string | no       | Working directory (defaults to .)              |

---

### git_status

Structured `git status --porcelain` output. Returns parsed entries with path, index status, and worktree status.

| Parameter | Type   | Required | Description                      |
| --------- | ------ | -------- | -------------------------------- |
| workdir   | string | no       | Working directory (defaults to .) |

---

### git_diff

Structured `git diff` output with configurable scope. Capped at 25K chars.

| Parameter | Type   | Required | Description                                               |
| --------- | ------ | -------- | --------------------------------------------------------- |
| staged    | boolean | no      | Show staged changes instead of unstaged (default: false) |
| workdir   | string  | no      | Working directory (defaults to .)                         |

---

### fetch

Fetch a URL and return cleaned text content. HTML is stripped to plain text.
Process-wide in-memory cache avoids re-fetching the same URL within a session.

| Parameter | Type   | Required | Description                                        |
| --------- | ------ | -------- | -------------------------------------------------- |
| url       | string | yes      | URL to fetch (http/https only)                     |
| max_chars | number | no       | Max output chars (default: 25,000)                 |
| raw       | boolean | no      | Skip HTML cleaning (default: false)               |

---

### web_search

Search the web. Multi-backend: Tavily → Brave → DuckDuckGo (fallback).

| Parameter    | Type   | Required | Description                             |
| ------------ | ------ | -------- | --------------------------------------- |
| query        | string | yes      | Search query                            |
| max_results  | number | no       | Max results (default: 5, hard cap: 20)  |

---

### spawn_subagent

Spawn a subagent with an isolated conversation history. The parent only sees
the subagent's final text answer. Shared: provider, tools, safety policy, cancel flag.
Isolated: conversation memory, truncation/compaction state.

| Parameter       | Type   | Required | Description                                    |
| --------------- | ------ | -------- | ---------------------------------------------- |
| task            | string | yes      | Self-contained task description                 |
| max_iterations  | number | no       | Loop cap for the subagent (default: 12)         |

---

### update_memory

Append a timestamped note to persistent memory (user-global or project-local).
Loaded into the system prompt on session start.

| Parameter | Type   | Required | Description                                                          |
| --------- | ------ | -------- | -------------------------------------------------------------------- |
| content   | string | yes      | Markdown text to append                                              |
| scope     | string | no       | `user` (global `~/.config/agentic/memory.md`) or `project` (`.agentic/memory.md`). Default: `user` |

---

### question

Ask the user one or more questions during execution. Supports free-text,
multiple choice, multi-select, and custom answers. Uses a callback pattern:
the CLI/TUI registers a `QuestionHandler` at startup.

**If no handler is registered**, the tool returns a `skipped: true` answer
for every question — the agent can proceed without blocking.

| Parameter | Type  | Required | Description                                                                                                |
| --------- | ----- | -------- | ---------------------------------------------------------------------------------------------------------- |
| questions | array | yes      | Array of `{question (string, required), header? (string), options? (string[]), custom? (bool), multiple? (bool)}` |

**Returns:**
```jsonc
{
  "answers": [{ "question": "...", "answer": ["choice"], "skipped": false }],
  "total": 1,
  "skipped": 0,
  "answered": 1
}
```

**CLI integration required:** Register a handler via `set_question_handler()`.
See [milestone-4-interactive-tools-02062026.md](../../docs/milestone-4-interactive-tools-02062026.md).

---

### todowrite

Create and manage a structured task list for the session. Each call **replaces**
the entire list — send the full updated array every time.

Session-scoped: the list lives in memory and is lost on process exit.
Persist important tasks via `update_memory` if cross-session continuity is needed.

| Parameter | Type  | Required | Description                                                                                              |
| --------- | ----- | -------- | -------------------------------------------------------------------------------------------------------- |
| todos     | array | yes      | Full updated list. Each item: `{content (string, required), status? (string), priority? (string)}`. Max 50 items. |

**Status values:** `pending` · `in_progress` (aliases: `active`, `in-progress`) · `completed` (alias: `done`) · `cancelled` (aliases: `canceled`, `skipped`)

**Priority values:** `low` · `medium` (aliases: `normal`) · `high` (aliases: `important`, `critical`)

**Returns:**
```jsonc
{
  "total": 3,
  "completed": 1,
  "in_progress": 1,
  "pending": 1,
  "progress_pct": 33
}
```

**CLI integration required:** Register a change handler via `set_todo_change_handler()`
for UI rendering. See [milestone-4-interactive-tools-02062026.md](../../docs/milestone-4-interactive-tools-02062026.md).

---

## Conditional availability

These tools are always registered in `builtin_tools()`. Conditional gating is handled at the CLI layer:

| Tool | CLI condition |
| ---- | ------------- |
| `question` | Only effective when a `QuestionHandler` is registered. Falls back to skip-all when no handler is present. |
| `fetch` / `web_search` | Subject to `UrlPolicy` (domain allowlist, IP-literal block). |
| `apply_patch` | Works on all models. When a model emits `apply_patch`, the tool applies it. When a model prefers `edit_file`/`write_file`, those are used instead. |

## Not implemented (intentional)

These tools from the opencode reference are **not planned** for agentic:

| Tool | Reason |
| ---- | ------ |
| `lsp` | Out of scope per architecture spec. The agent uses `read_file` + `grep` + `glob` for code navigation. |
| `codesearch` | Out of scope. The agent uses `web_search` for external documentation lookup. |
| `multiedit` | Covered by `apply_patch` (multi-file unified diff) and `edit_file` with `replace_all`. |
