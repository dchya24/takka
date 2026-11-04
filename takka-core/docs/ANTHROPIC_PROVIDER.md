# Anthropic Provider Documentation

This provider enables using Anthropic's Claude models via the Anthropic API with the core-agentic library.

## Overview

The Anthropic provider implements the `LLMProvider` trait and supports:
- Claude 3.5 Sonnet, Claude 3 Opus, and other Claude models
- Streaming responses via Server-Sent Events (SSE)
- Tool/function calling
- Retry logic with exponential backoff
- Configurable base URL (for proxy/custom endpoints)

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
core-agentic = { version = "0.1", features = ["anthropic"] }
```

## Configuration

The `AnthropicProviderConfig` struct is used to configure the provider:

```rust
use core_agentic::providers::AnthropicProviderConfig;

// Basic configuration
let config = AnthropicProviderConfig::new(
    "anthropic",                          // Provider ID
    "sk-ant-api03-...",                   // API Key
    "claude-3-5-sonnet-20241022"         // Default Model
);

// With custom base URL (e.g., for proxy)
let config = AnthropicProviderConfig::new(
    "anthropic-proxy",
    "sk-ant-api03-...",
    "claude-3-opus-20240229"
)
.with_base_url("https://proxy.example.com/v1");

// With custom API version
let config = AnthropicProviderConfig::new(
    "anthropic",
    "sk-ant-api03-...",
    "claude-3-5-sonnet-20241022"
)
.with_version("2023-06-01");
```

### Retry Configuration

The provider automatically retries requests on failures with exponential backoff:

```rust
use core_agentic::providers::{AnthropicProviderConfig, RetryConfig};

let config = AnthropicProviderConfig::new("anthropic", "api-key", "model");

// The default retry config is:
// - max_retries: 3
// - base_delay_ms: 1000
// - max_delay_ms: 30000

// You can customize this if needed:
let retry_config = RetryConfig {
    max_retries: 5,
    base_delay_ms: 2000,
    max_delay_ms: 60000,
};

let mut config = AnthropicProviderConfig::new("anthropic", "api-key", "model");
config.retry = retry_config;
```

## Usage

### Creating the Provider

```rust
use core_agentic::providers::{AnthropicProvider, AnthropicProviderConfig};

let config = AnthropicProviderConfig::new(
    "anthropic",
    std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY must be set"),
    "claude-3-5-sonnet-20241022"
);

let provider = AnthropicProvider::new(config);
```

### Chat Completions

```rust
use core_agentic::providers::{ChatRequest, ChatMessageRequest};

let request = ChatRequest::new(
    "claude-3-5-sonnet-20241022",
    vec![
        ChatMessageRequest::user("What is the capital of France?"),
    ]
);

let response = provider.chat(request)?;
println!("Response: {}", response.message.content.unwrap());
```

#### Custom System Prompt

Override the default system prompt per-request:

```rust
let request = ChatRequest::new(
    "claude-3-5-sonnet-20241022",
    vec![
        ChatMessageRequest::user("Explain ownership in Rust"),
    ]
)
.with_system_prompt("You are a Rust language expert. Be concise and use code examples.");

let response = provider.chat(request)?;
```

> **Note:** The Anthropic API uses a dedicated `system` field in the request body rather than
> a system-role message. The provider handles this automatically — it extracts any system
> prompt set via `with_system_prompt()` and sends it in the correct Anthropic format.
> If no custom prompt is set, `DEFAULT_SYSTEM_PROMPT` is used.

### Streaming Responses

```rust
use futures::StreamExt;

let mut request = ChatRequest::new(
    "claude-3-5-sonnet-20241022",
    vec![
        ChatMessageRequest::user("Tell me a short story."),
    ]
);

let stream = provider.chat_stream(request)?;

let mut full_response = String::new();
tokio::pin!(stream);

while let Some(chunk) = stream.next().await {
    match chunk {
        Ok(chat_chunk) => {
            print!("{}", chat_chunk.delta);
            full_response.push_str(&chat_chunk.delta);
            
            if let Some(finish_reason) = chat_chunk.finish_reason {
                println!("\nFinished: {}", finish_reason);
                break;
            }
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            break;
        }
    }
}
```

### Tool Calling

Anthropic supports function/tool calling:

```rust
use core_agentic::providers::{ChatRequest, ChatMessageRequest, ToolDefinition, ToolFunction};

let request = ChatRequest::new(
    "claude-3-5-sonnet-20241022",
    vec![
        ChatMessageRequest::user("What's the weather in Tokyo?"),
    ]
)
.with_tools(vec![
    ToolDefinition {
        tool_type: "function".to_string(),
        function: ToolFunction {
            name: "get_weather".to_string(),
            description: "Get the current weather for a location".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": {
                        "type": "string",
                        "description": "The city name"
                    }
                },
                "required": ["location"]
            }),
        },
    },
]);

let response = provider.chat(request)?;

// Handle tool calls
if !response.message.tool_calls.is_empty() {
    for tool_call in &response.message.tool_calls {
        println!("Tool: {}", tool_call.function.name);
        println!("Args: {}", tool_call.function.arguments);
    }
}
```

## API Key

You need an Anthropic API key. Get one from: https://console.anthropic.com/

Set it as an environment variable:

```bash
export ANTHROPIC_API_KEY=sk-ant-api03-...
```

Or pass it directly:

```rust
let config = AnthropicProviderConfig::new(
    "anthropic",
    "sk-ant-api03-...",
    "claude-3-5-sonnet-20241022"
);
```

## Supported Models

| Model | Context Window |
|-------|---------------|
| claude-3-5-sonnet-20241022 | 200K |
| claude-3-5-sonnet-20240620 | 200K |
| claude-3-opus-20240229 | 200K |
| claude-3-sonnet-20240229 | 200K |
| claude-3-haiku-20240307 | 200K |

## Error Handling

The provider returns `ProviderError` for any issues:

```rust
use core_agentic::providers::ProviderError;

match provider.chat(request) {
    Ok(response) => {
        // Handle response
    }
    Err(ProviderError(msg)) => {
        eprintln!("Provider error: {}", msg);
        
        if msg.contains("401") {
            eprintln!("Check your API key");
        } else if msg.contains("429") {
            eprintln!("Rate limit exceeded");
        } else if msg.contains("500") {
            eprintln!("Server error, retrying...");
        }
    }
}
```

## Differences from OpenAI

The Anthropic provider handles these key differences from OpenAI:

1. **API Version**: Uses `anthropic-version` header instead of version in URL
2. **Authentication**: Uses `x-api-key` header instead of `Authorization: Bearer`
3. **System Messages**: Uses separate `system` field in request body
4. **Content Blocks**: Uses structured content blocks instead of simple text
5. **Tool Format**: Different schema for tool definitions
6. **Streaming**: Different SSE event format

All of these differences are handled transparently by the `AnthropicProvider` implementation.

## Configuration File Example

You can also configure the Anthropic provider via a configuration file:

```json
{
  "providers": [
    {
      "id": "anthropic",
      "name": "Anthropic",
      "type": "anthropic",
      "base_url": "https://api.anthropic.com/v1",
      "api_key": "sk-ant-api03-...",
      "default_model": "claude-3-5-sonnet-20241022",
      "version": "2023-06-01",
      "retry": {
        "max_retries": 3,
        "base_delay_ms": 1000,
        "max_delay_ms": 30000
      },
      "models": [
        {
          "id": "claude-3-5-sonnet-20241022",
          "name": "Claude 3.5 Sonnet",
          "context_window": 200000
        },
        {
          "id": "claude-3-opus-20240229",
          "name": "Claude 3 Opus",
          "context_window": 200000
        }
      ]
    }
  ]
}
```

## License

This provider implementation is part of core-agentic and follows the same license.
