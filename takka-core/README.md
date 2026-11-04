# core-agentic

Rust library for AI agent orchestration with multi-step task execution, tool system, LLM provider integration, memory management, and safety features.

## Features

- **LLM Providers** — Pluggable provider trait with built-in support for OpenAI, Anthropic, and Z.ai
- **Tool System** — Register and execute tools with automatic schema generation
- **Agent Loop** — Orchestrator with retry, streaming, and tool-call handling
- **Planner** — LLM-powered task decomposition into executable plans with dependencies
- **Memory** — Conversation memory with configurable context windows
- **Safety** — Risk assessment, confirmation prompts, command allow/deny lists
- **MCP** — Model Context Protocol client (stdio, HTTP, SSE transports)

## Quick Start

```rust
use core_agentic::{
    Orchestrator, ToolRegistry,
    OpenAIProvider, OpenAIProviderConfig,
};

// 1. Create a provider
let config = OpenAIProviderConfig::new(
    "openai",
    "https://api.openai.com/v1",
    "sk-...",
    "gpt-4o",
);
let provider = std::sync::Arc::new(OpenAIProvider::new(config));

// 2. Register tools
let tools = ToolRegistry::new();
tools.register(Box::new(core_agentic::RunCommandTool::new()));
tools.register(Box::new(core_agentic::ReadFileTool::new()));

// 3. Run the orchestrator
let mut orchestrator = Orchestrator::new(provider, tools);
orchestrator.set_system_prompt("You are a Rust expert assistant.");

let result = orchestrator.run("List files in the current directory")?;
println!("{}", result);
```

## System Prompts

System prompts define the AI assistant's persona and behavior. They are **configurable at multiple levels**:

### Default System Prompt

When no custom prompt is provided, all providers use `DEFAULT_SYSTEM_PROMPT`:

> *"You are an intelligent coding assistant. You help users with software development tasks including writing, reviewing, refactoring, and debugging code. You provide clear explanations, follow best practices, and write clean, maintainable code. When uncertain, you ask for clarification rather than guessing."*

### Per-Request Override

Override the system prompt for a single chat request:

```rust
use core_agentic::providers::{ChatRequest, ChatMessageRequest};

let request = ChatRequest::new("gpt-4o", vec![
    ChatMessageRequest::user("Explain ownership in Rust"),
])
.with_system_prompt("You are a Rust language expert. Be concise and use code examples.");

let response = provider.chat(request)?;
```

### Orchestrator-Level

Set a system prompt for all requests made through the orchestrator:

```rust
let mut orchestrator = Orchestrator::new(provider, tools);
orchestrator.set_system_prompt("You are a security-focused code reviewer.");
```

### Agent Config

Configure via `AgentConfig`:

```rust
use core_agentic::AgentConfig;

let config = AgentConfig::new("reviewer", "openai", "gpt-4o")
    .with_system_prompt("You review code for security vulnerabilities.");
```

### Global Config File

Set in `~/.config/agentic/config.json`:

```json
{
  "providers": [...],
  "system_prompt": "You are a helpful coding assistant specialized in Rust."
}
```

### Priority Order

```
ChatRequest::with_system_prompt()  →  highest priority
         │
Orchestrator::set_system_prompt()
         │
AgentConfig::system_prompt
         │
Config::system_prompt (config.json)
         │
DEFAULT_SYSTEM_PROMPT              →  fallback
```

## Providers

### OpenAI-Compatible

Supports any OpenAI-compatible API (OpenAI, Groq, Together, local models, etc.):

```rust
use core_agentic::OpenAIProviderConfig;

let config = OpenAIProviderConfig::new(
    "my-provider",
    "https://api.openai.com/v1",
    "sk-...",
    "gpt-4o",
);
```

### Anthropic (Claude)

See [Anthropic Provider Docs](docs/ANTHROPIC_PROVIDER.md).

### Z.ai

```rust
use core_agentic::ZaiProviderConfig;

let config = ZaiProviderConfig::new("api-key", "z-1")
    .with_base_url("https://api.z.ai/v1");
```

### Failover

Chain multiple providers with automatic failover:

```rust
use core_agentic::FailoverProvider;

let failover = FailoverProvider::new(vec![
    Arc::new(primary_provider),
    Arc::new(fallback_provider),
]);
```

## Tools

### Built-in Tools

| Tool | Description |
|------|-------------|
| `RunCommandTool` | Execute shell commands |
| `ReadFileTool` | Read file contents |
| `WriteFileTool` | Write files |
| `EditFileTool` | Edit files with exact string replacement |
| `ListFilesTool` | List directory contents |
| `GlobTool` | File pattern matching |
| `GrepTool` | Regex content search |
| `SearchFilesTool` | Search files in directory |
| `RunScriptTool` | Run script files |

### Custom Tools

Implement the `Tool` trait:

```rust
use core_agentic::{Tool, ToolResult, ToolError};

struct MyTool;

impl Tool for MyTool {
    fn id(&self) -> &str { "my_tool" }
    fn name(&self) -> &str { "My Tool" }
    fn description(&self) -> &str { "Does something useful" }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "input": { "type": "string" }
            },
            "required": ["input"]
        })
    }

    fn execute(&self, args: &serde_json::Value) -> Result<ToolResult, ToolError> {
        let input = args["input"].as_str().unwrap_or("");
        Ok(ToolResult::text(format!("Processed: {}", input)))
    }
}
```

## Planner

Decompose goals into step-by-step plans with dependencies:

```rust
use core_agentic::{PlannerAgent, ToolRegistry};

let planner = PlannerAgent::new(provider);
let tools = ToolRegistry::new();
// ... register tools ...

let mut plan = planner.create_plan("Build a REST API with CRUD endpoints", &tools)?;

for step in &plan.steps {
    println!("• {}", step.description);
}

let result = planner.execute_plan(&mut plan, &tools)?;
```

## Configuration

Config file: `~/.config/agentic/config.json`

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
          "max_tokens": 8192
        }
      ]
    }
  ],
  "system_prompt": "You are a helpful coding assistant.",
  "safety": {
    "auto_approve_low_risk": true,
    "blocked_commands": ["rm -rf /", "mkfs"]
  },
  "output": {
    "color": true,
    "stream": true,
    "show_thoughts": true,
    "show_tool_calls": true
  }
}
```

Environment variable substitution is supported: `"api_key": "$OPENAI_API_KEY"` resolves from the environment.

## License

MIT
