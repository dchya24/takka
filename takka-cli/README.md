# Agentic CLI

AI agent orchestration command-line interface using [core-agentic](https://github.com/nutec/agentic/tree/main/agentic-cli).

## Installation

Prebuilt binaries and user-local installers for Linux, macOS, and Windows are
documented in the repository's [main installation guide](../README.md#installation).
The installers verify GitHub Release archives with SHA-256 and do not alter
existing Agentic configuration.

To build from source instead:

```bash
cargo install --path agentic-cli
```

Or build without installing:

```bash
cd agentic-cli
cargo build --release
# Binary at target/release/agentic
```

## Quick Start

```bash
# Set API key
export OPENAI_API_KEY="sk-your-key"

# Run a single task
agentic run "list files in current directory"

# Interactive REPL with @ file completion and sessions
agentic interactive

# TUI mode
agentic tui
```

## Usage

### Commands

| Command | Description |
|---------|------------|
| `agentic run <task>` | Run a single task |
| `agentic interactive` | Start interactive REPL mode |
| `agentic tui` | Start full TUI mode |
| `agentic config show` | Display current configuration |
| `agentic config init --interactive` | Guided config wizard |
| `agentic version` | Show version |

### Options

| Option | Description |
|--------|------------|
| `-c, --config <PATH>` | Custom config file |
| `-v, --verbose <LEVEL>` | Verbose output (error/warn/info/debug/trace) |
| `--debug` | Enable debug tracing |
| `--color <auto|always|never>` | Color output control |

## Configuration

### Environment Variables

```bash
export OPENAI_API_KEY="your-api-key"
export OPENAI_BASE_URL="https://api.openai.com/v1"
```

### Config File

Location: `~/.config/agentic/config.json` or custom via `--config`

```json
{
  "providers": [
    {
      "name": "openai",
      "type": "openai-compatible",
      "api_base": "https://api.openai.com/v1",
      "api_key": "$OPENAI_API_KEY",
      "models": [
        {
          "model": "gpt-4o",
          "display_name": "GPT-4o",
          "temperature": 0.7,
          "max_tokens": 4096
        }
      ]
    }
  ],
  "safety": {
    "auto_approve_low_risk": true,
    "blocked_commands": ["rm -rf /", "mkfs", "dd if="]
  },
  "output": {
    "color": true,
    "stream": true,
    "show_thoughts": true,
    "show_tool_calls": true
  }
}
```

### Environment Variable Substitution

Use `$VAR` in config to reference environment variables:

```json
{
  "providers": [{
    "api_key": "$OPENAI_API_KEY"
  }]
}
```

## Interactive Mode

The REPL uses [reedline](https://github.com/nushell/reedline) with rich features:

### Slash Commands

| Command | Alias | Description |
|---------|-------|-------------|
| `/help` | `/h` | Show help message |
| `/new` | `/n`, `n` | Start a new session |
| `/models` | `/m` | Pick model interactively (fuzzy search) |
| `/models <name>` | | Switch model by name (auto-complete) |
| `/sessions` | `/ss` | List previous sessions |
| `/sessions <id>` | | Resume a previous session |
| `/history` | `/hist` | Show conversation history |
| `/image <path>` | `/img` | Attach image for next turn |
| `/config` | `/cfg` | Show current configuration |
| `/tools` | `/t` | List available tools |
| `/stats` | | Show session statistics |
| `/mcp` | | Show MCP server status |
| `/plan <goal>` | `/p` | Create a plan for a goal |
| `/search <query>` | `/find` | Search conversation memory |
| `/provider <name>` | `/prov` | Switch or show provider |
| `/quit` | `/q`, `exit` | Exit interactive mode |

### Completion & Navigation

- **`@` file completion** — type `@` to see all project files recursively
  - Respects `.gitignore` (node_modules, target, .git, etc. excluded automatically)
  - Type `@src/` to browse files under `src/`
  - Type `@chat` to search files matching "chat"
  - **Auto-detects images** — `@photo.png` attaches as vision input (PNG/JPEG/GIF/WebP)
- **`/` command completion** — type `/` to see all slash commands with descriptions
- **`/models ` completion** — type `/models ` then Tab for model name suggestions
- **Inline hints** — fish-style autocomplete suggestions
- **In-memory history** — Ctrl+R search within session
- **Syntax highlighting** — `/` commands in yellow, `@` file refs in blue

### Image Attachments

Attach images to conversations via multiple methods:

```bash
# Method 1: @ reference (auto-detected from file contents, not extension)
> analyze @screenshot.png

# Method 2: Slash command (queue for next turn)
> /image ~/Pictures/photo.jpg

# Method 3: Drag & drop from file manager into terminal
# Terminal inserts path, prefix with @ to auto-attach:
> analyze @/tmp/dropped-image.png

# Method 4: URL
> /image https://example.com/diagram.png
```

Supported formats: PNG, JPEG, GIF, WebP (max 20 MB per image, max 10 per message).

Model must support vision — check for 👁 indicator in `/models` or status bar.

## Sessions

Sessions are automatically saved and can be resumed across runs.

### Session Lifecycle

```
agentic interactive
  → Session created (auto-titled from first user message)
  → Each turn: auto-saved to ~/.config/agentic/sessions/
  → /new → saves current session, creates fresh one
  → /sessions → list all sessions (most recent first)
  → /sessions <id> → resume a previous session
  → /quit → auto-save before exit
```

### Session Storage

Sessions are stored as JSON files in `~/.config/agentic/sessions/`:

```
~/.config/agentic/
├── config.json
└── sessions/
    ├── ses_18a3b2c_001f.json
    ├── ses_18a3b4d_002a.json
    └── ses_18a3b5e_003b.json
```

Each session captures:
- **Messages** — full conversation history (user + assistant)
- **Metadata** — title, working directory, provider, model
- **Stats** — cost, token counts (input/output)
- **Timestamps** — created_at, updated_at

### Example

```bash
agentic interactive

# Work on something
> help me fix the auth module in user.service.ts
# ... agent responds ...

# Start fresh (old session auto-saved)
/new

# List previous sessions
/sessions
# 1. help me fix the auth module in user.service.ts  2 msgs  1m ago
#    ses_18a3b2c_001f · /home/user/project · openai/gpt-4o

# Resume a session
/sessions ses_18a3b2c_001f
# ✓ Resumed: help me fix the auth module (2 messages)
```

## Model Picker

`/models` opens an interactive fuzzy-searchable picker (powered by [dialoguer](https://crates.io/crates/dialoguer)):

- Type to filter by display name, model ID, or provider
- ↑↓ arrow keys to navigate
- Enter to select, Esc to cancel
- Shows vision capability (👁) and active model (●)

Direct switch without picker:
```bash
/models gpt-4o          # exact or partial match
/models claude<Tab>     # auto-complete dropdown
```

## Available Tools

When running tasks, the agent has access to:

| Tool | Description |
|------|-------------|
| `run_command` | Execute shell commands |
| `read_file` | Read file contents |
| `write_file` | Write content to files |
| `edit_file` | Edit files with exact string replacement |
| `list_files` | List directory contents |
| `glob` | File pattern matching |
| `grep` | Regex content search |

## Examples

```bash
# Single task
agentic run "hello world"

# File operations
agentic run "create a hello.txt file with 'hello world'"

# List files
agentic run "show me all files in src directory"

# Interactive with session management
agentic interactive

# Debug mode
agentic run "list files" -v trace

# Custom config
agentic -c ~/my-config.json interactive
```

## Building

Requirements:
- Rust 1.70+
- Cargo

```bash
# Development
cargo build

# Release (optimized)
cargo build --release

# Run tests
cargo test
```

## License

MIT
