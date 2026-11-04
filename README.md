# Agentic

Agentic is a Rust-based coding agent that can inspect a codebase, edit files,
run commands, use external tools, and work through multi-step development tasks
from your terminal. It combines a reusable agent runtime with a standalone CLI
for one-shot tasks, an interactive REPL, and a full-screen TUI.

> Agentic is under active development. Review commands and file changes before
> approving them, especially when using permissive execution modes.

## Highlights

- **Agentic workflows** — iterative tool use, streaming responses, cancellation,
  planning, replanning, and subagent delegation.
- **Developer tools** — file read/write/edit, glob and grep, command execution,
  web access, memory, questions, task tracking, and patch application.
- **Multiple providers** — OpenAI-compatible and Anthropic-compatible APIs with
  configurable models and capabilities.
- **Terminal interfaces** — one-shot commands, an inline interactive REPL, and
  a full-screen `ratatui` TUI.
- **Context and persistence** — project instructions, persistent memory,
  session history, token-aware compaction, and prompt caching support.
- **Extensibility** — MCP servers and discoverable `SKILL.md` skills.
- **Safety controls** — permission modes, risk scoring, command blocklists,
  path and URL policies, rate limits, audit logs, and diff previews.

## Workspace

| Crate | Purpose |
|---|---|
| [`core-agentic`](core-agentic/) | Agent loop, providers, tools, memory, safety, MCP, planning, and skills. |
| [`agentic-cli`](agentic-cli/) | The `agentic` executable, configuration commands, REPL, and TUI. |

## Supported Platforms

Prebuilt GitHub Release binaries are currently available for:

| Platform | Architecture | Status |
|---|---|---|
| Linux | x86_64 | Supported |
| Linux | ARM64 / aarch64 | **Not supported yet** |
| Windows | x86_64 | Supported |
| macOS | Intel / x86_64 | Supported |
| macOS | Apple Silicon / aarch64 | Supported |

The installers stop with an actionable error on unsupported platforms. They do
not fall back to compiling untrusted source automatically.

## Installation

The installation scripts download the matching archive from the latest GitHub
Release, verify it against the published SHA-256 checksum manifest, and install
Agentic without administrator privileges.

Default destinations:

- Linux/macOS: `${AGENTIC_INSTALL_DIR:-$HOME/.local/bin}`
- Windows: `%LOCALAPPDATA%\Programs\agentic\bin`

The installer installs **two** binaries into the same directory:

- `agentic` — the CLI (parser, renderer, REPL/TUI)
- `agentic-runtime` — the headless agent daemon the CLI spawns for every
  run (protocol: JSON Lines over stdin/stdout). The CLI looks for the
  runtime next to its own binary, or via the `AGENTIC_RUNTIME_BIN`
  environment variable.

Since v0.6.0, the CLI defaults to **embedded mode**: the agent engine runs
as a thread inside the `agentic` process, so `agentic-runtime` does not need
to be on PATH at all. Use `agentic --runtime spawn` to run the agent in a
separate daemon process instead (the `agentic-runtime` binary is discovered
next to `agentic`, or via the `AGENTIC_RUNTIME_BIN` environment variable).

### Linux and macOS

Download and inspect the installer first (recommended):

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/dchya24/agentic/dev/scripts/install.sh \
  -o install.sh
less install.sh
sh install.sh
```

Optional one-liner:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/dchya24/agentic/dev/scripts/install.sh | sh
```

Install a specific release or destination:

```sh
AGENTIC_VERSION=v0.3.2 AGENTIC_INSTALL_DIR="$HOME/bin" sh install.sh
```

### Windows

Download and inspect the installer first (recommended):

```powershell
Invoke-WebRequest `
  https://raw.githubusercontent.com/dchya24/agentic/dev/scripts/install.ps1 `
  -OutFile install.ps1
Get-Content .\install.ps1
.\install.ps1
```

Optional one-liner:

```powershell
irm https://raw.githubusercontent.com/dchya24/agentic/dev/scripts/install.ps1 | iex
```

Install a specific release or destination:

```powershell
.\install.ps1 -Version v0.3.2 -InstallDir "$HOME\bin"
```

The Windows installer adds its directory only to the current user's `PATH`.
The POSIX installer prints the required `PATH` export without editing shell
startup files. Open a new terminal if instructed after installation.

### Build from source

A Rust toolchain is only required when building from source:

```sh
git clone https://github.com/dchya24/agentic.git
cd agentic
cargo install --path agentic-cli
```

## Configuration

Binary installation and configuration are intentionally separate. Installing
or upgrading Agentic **does not create, overwrite, or delete your config,
sessions, memory, or skills**.

Default config locations:

| Platform | Path |
|---|---|
| Linux/macOS | `~/.config/agentic/config.json` |
| Windows | `%USERPROFILE%\.config\agentic\config.json` |

Run the guided wizard explicitly:

```sh
agentic config init --interactive
agentic config validate
```

Or ask the installer to run the wizard only after a successful installation:

```sh
sh install.sh --init
```

```powershell
.\install.ps1 -Init
```

API keys can be referenced through environment variables instead of being
stored directly in `config.json`:

```sh
export OPENAI_API_KEY="sk-..."
```

```json
{
  "providers": [
    {
      "name": "openai",
      "type": "openai-compatible",
      "api_base": "https://api.openai.com/v1",
      "api_key": "$OPENAI_API_KEY",
      "models": [{ "model": "gpt-4o" }]
    }
  ]
}
```

See [Configuration](docs/CONFIGURATION.md) for the complete schema and provider
examples.

## Quick Start

```sh
# Run one task
agentic run "explain the architecture of this repository"

# Start the inline REPL
agentic interactive

# Start the full-screen interface
agentic tui

# Attach to the long-lived daemon (auto-starts one if absent),
# or run a one-off task through it via --runtime daemon
agentic attach
agentic --runtime daemon run "explain the architecture of this repository"
```

`agentic attach` drops you into the REPL routed through the running daemon's
engine; the daemon keeps the session alive across attach/detach cycles, and
`agentic attach --shutdown` stops it. On Windows use `--runtime embedded`
(the default) or `--runtime spawn` instead — daemon mode is Unix-only for now.

Useful configuration and discovery commands:

```sh
agentic config show
agentic config path
agentic skill list
agentic --help
```

## Known Limitations and Roadmap

- [ ] Daemon mode (`--runtime daemon` / `agentic attach`) is Unix-only for now;
      the Windows named-pipe transport is still a TODO — use `--runtime embedded`
      or `--runtime spawn` on Windows.
- [ ] Publish Linux ARM64/aarch64 and Windows ARM64 prebuilt releases.
- [ ] Distribute through package managers such as Homebrew, Scoop, Winget,
      and community Linux repositories.
- [ ] Add Windows Authenticode and macOS code signing/notarization.
- [ ] Ship shell-completion packages for common shells.

The authoritative implementation status and longer-term work live in the
[project roadmap](docs/ROADMAP.md).

## Documentation

- [CLI reference and examples](agentic-cli/README.md)
- [Configuration guide](docs/CONFIGURATION.md)
- [Architecture](AGENT_ARCHITECTURE.md)
- [Roadmap](docs/ROADMAP.md)
- [Release process](docs/RELEASING.md)
- [Contributing](CONTRIBUTING.md)
- [Security policy](SECURITY.md)

## License

Agentic is available under the [MIT License](LICENSE).
