# MCP Client Implementation Plan

> **Status:** ✅ **Completed** (May 4, 2026)

**Goal:** Add MCP (Model Context Protocol) client support to core-agentic, allowing the agent to discover and call tools from external MCP servers via both stdio and HTTP/SSE transports.

**Architecture:** A new `mcp` module implements the MCP client protocol. Each remote MCP server is represented by an `McpConnection` that handles discovery (`tools/list`) and invocation (`tools/call`) via JSON-RPC 2.0. An `McpToolAdapter` wraps each remote tool as a `crate::tool::Tool` so it plugs into the existing `ToolRegistry`. Two transports are supported: `StdioTransport` (spawns a child process, communicates over stdin/stdout) and `HttpTransport` (POST to HTTP endpoint with optional SSE for streaming).

**Tech Stack:** Rust, serde_json (JSON-RPC), reqwest::blocking (HTTP), std::process (stdio)

---

### Task 1: MCP Types and JSON-RPC Core

**Files:**
- Create: `src/mcp/mod.rs`
- Create: `src/mcp/types.rs`

**Step 1: Create the mcp module directory**

```bash
mkdir -p src/mcp
```

**Step 2: Write `src/mcp/types.rs`**

```rust
//! MCP protocol types - JSON-RPC 2.0 messages and MCP-specific schemas

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn new(id: u64, method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    pub protocol_version: String,
    pub capabilities: serde_json::Value,
    pub client_info: ClientInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    pub protocol_version: String,
    pub capabilities: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_info: Option<ServerInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolSchema {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsListResult {
    pub tools: Vec<McpToolSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallParams {
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    pub content: Vec<ToolCallContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallContent {
    #[serde(rename = "type")]
    pub content_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
}

impl McpServerConfig {
    pub fn stdio(command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            command: Some(command.into()),
            args: Some(args),
            env: None,
            url: None,
            headers: None,
        }
    }

    pub fn http(url: impl Into<String>) -> Self {
        Self {
            command: None,
            args: None,
            env: None,
            url: Some(url.into()),
            headers: None,
        }
    }

    pub fn is_stdio(&self) -> bool {
        self.command.is_some()
    }

    pub fn is_http(&self) -> bool {
        self.url.is_some()
    }
}
```

**Step 3: Write `src/mcp/mod.rs`**

```rust
//! MCP (Model Context Protocol) client module

pub mod types;
pub mod transport;
pub mod client;
pub mod tool_adapter;

pub use client::McpClient;
pub use tool_adapter::McpToolAdapter;
pub use types::McpServerConfig;
```

**Step 4: Verify it compiles**

Run: `cargo check`
Expected: Compiles with warnings about unused modules (transport, client, tool_adapter not yet created)

---

### Task 2: Transport Trait and Stdio Transport

**Files:**
- Create: `src/mcp/transport.rs`

**Step 1: Write `src/mcp/transport.rs`**

```rust
//! MCP transport layer - stdio and HTTP

use crate::mcp::types::{JsonRpcRequest, JsonRpcResponse};
use std::io::{BufRead, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub trait McpTransport: Send + Sync {
    fn send_and_recv(&mut self, request: JsonRpcRequest) -> Result<JsonRpcResponse, String>;
    fn close(&mut self);
}

pub struct StdioTransport {
    child: Child,
    stdin: ChildStdin,
    stdout: std::io::BufReader<ChildStdout>,
}

impl StdioTransport {
    pub fn new(command: &str, args: &[String], env: &std::collections::HashMap<String, String>) -> Result<Self, String> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        for (key, value) in env {
            cmd.env(key, value);
        }

        let mut child = cmd.spawn().map_err(|e| format!("Failed to spawn MCP server '{}': {}", command, e))?;

        let stdin = child.stdin.take().ok_or("Failed to get stdin of MCP server")?;
        let stdout = child.stdout.take().ok_or("Failed to get stdout of MCP server")?;

        Ok(Self {
            child,
            stdin,
            stdout: std::io::BufReader::new(stdout),
        })
    }
}

impl McpTransport for StdioTransport {
    fn send_and_recv(&mut self, request: JsonRpcRequest) -> Result<JsonRpcResponse, String> {
        let msg = serde_json::to_string(&request).map_err(|e| format!("Failed to serialize request: {}", e))?;
        let line = format!("{}\n", msg);

        self.stdin.write_all(line.as_bytes())
            .map_err(|e| format!("Failed to write to MCP server stdin: {}", e))?;
        self.stdin.flush()
            .map_err(|e| format!("Failed to flush stdin: {}", e))?;

        let mut response_line = String::new();
        self.stdout.read_line(&mut response_line)
            .map_err(|e| format!("Failed to read from MCP server stdout: {}", e))?;

        let response: JsonRpcResponse = serde_json::from_str(response_line.trim())
            .map_err(|e| format!("Failed to parse MCP response: {} (input: {:?})", e, response_line))?;

        Ok(response)
    }

    fn close(&mut self) {
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.close();
    }
}

pub struct HttpTransport {
    url: String,
    client: reqwest::blocking::Client,
    headers: std::collections::HashMap<String, String>,
}

impl HttpTransport {
    pub fn new(url: &str, headers: std::collections::HashMap<String, String>) -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

        Ok(Self {
            url: url.to_string(),
            client,
            headers,
        })
    }
}

impl McpTransport for HttpTransport {
    fn send_and_recv(&mut self, request: JsonRpcRequest) -> Result<JsonRpcResponse, String> {
        let mut req = self.client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json");

        for (key, value) in &self.headers {
            req = req.header(key.as_str(), value.as_str());
        }

        let response = req
            .json(&request)
            .send()
            .map_err(|e| format!("HTTP request to MCP server failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            return Err(format!("MCP server returned HTTP {}: {}", status, body));
        }

        let rpc_response: JsonRpcResponse = response
            .json()
            .map_err(|e| format!("Failed to parse MCP HTTP response: {}", e))?;

        Ok(rpc_response)
    }

    fn close(&mut self) {}
}
```

**Step 2: Verify it compiles**

Run: `cargo check`

---

### Task 3: MCP Client

**Files:**
- Create: `src/mcp/client.rs`

**Step 1: Write `src/mcp/client.rs`**

```rust
//! MCP client - manages connection to a single MCP server

use std::sync::atomic::{AtomicU64, Ordering};

use crate::mcp::transport::{HttpTransport, McpTransport, StdioTransport};
use crate::mcp::types::*;
use crate::tool::{ToolError, ToolSchema, ToolParam};
use std::collections::HashMap;

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed)
}

pub struct McpClient {
    transport: Box<dyn McpTransport>,
    server_info: Option<ServerInfo>,
    tools_cache: Vec<McpToolSchema>,
}

impl McpClient {
    pub fn connect(config: &McpServerConfig) -> Result<Self, String> {
        let transport: Box<dyn McpTransport> = if config.is_stdio() {
            let command = config.command.as_ref().ok_or("Missing command for stdio transport")?;
            let args = config.args.as_ref().cloned().unwrap_or_default();
            let env = config.env.as_ref().cloned().unwrap_or_default();
            Box::new(StdioTransport::new(command, &args, &env)?)
        } else if config.is_http() {
            let url = config.url.as_ref().ok_or("Missing URL for HTTP transport")?;
            let headers = config.headers.as_ref().cloned().unwrap_or_default();
            Box::new(HttpTransport::new(url, headers)?)
        } else {
            return Err("MCP server config must have either 'command' (stdio) or 'url' (http)".into());
        };

        let mut client = Self {
            transport,
            server_info: None,
            tools_cache: Vec::new(),
        };

        client.initialize()?;
        client.tools_cache = client.discover_tools()?;

        Ok(client)
    }

    fn initialize(&mut self) -> Result<(), String> {
        let params = InitializeParams {
            protocol_version: "2024-11-05".into(),
            capabilities: serde_json::json!({}),
            client_info: ClientInfo {
                name: "core-agentic".into(),
                version: "0.1.0".into(),
            },
        };

        let request = JsonRpcRequest::new(
            next_id(),
            "initialize",
            Some(serde_json::to_value(params).map_err(|e| format!("Serialize error: {}", e))?),
        );

        let response = self.transport.send_and_recv(request)?;

        if let Some(error) = response.error {
            return Err(format!("MCP initialize error (code {}): {}", error.code, error.message));
        }

        let init_result: InitializeResult = response
            .result
            .ok_or("No result in initialize response")?
            .try_into()
            .map_err(|e: serde_json::Error| format!("Failed to parse initialize result: {}", e))?;

        self.server_info = init_result.server_info;

        let initialized = JsonRpcRequest::new(next_id(), "notifications/initialized", None);
        let _ = self.transport.send_and_recv(initialized);

        Ok(())
    }

    fn discover_tools(&mut self) -> Result<Vec<McpToolSchema>, String> {
        let request = JsonRpcRequest::new(next_id(), "tools/list", None);
        let response = self.transport.send_and_recv(request)?;

        if let Some(error) = response.error {
            return Err(format!("MCP tools/list error (code {}): {}", error.code, error.message));
        }

        let result: ToolsListResult = response
            .result
            .ok_or("No result in tools/list response")?
            .try_into()
            .map_err(|e: serde_json::Error| format!("Failed to parse tools/list result: {}", e))?;

        Ok(result.tools)
    }

    pub fn server_info(&self) -> Option<&ServerInfo> {
        self.server_info.as_ref()
    }

    pub fn tools(&self) -> &[McpToolSchema] {
        &self.tools_cache
    }

    pub fn tool_schemas(&self) -> Vec<ToolSchema> {
        self.tools_cache
            .iter()
            .map(|mcp_tool| {
                let mut params = HashMap::new();
                let mut required = Vec::new();

                if let Some(schema) = &mcp_tool.input_schema {
                    if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
                        for (name, def) in props {
                            let param_type = def
                                .get("type")
                                .and_then(|t| t.as_str())
                                .unwrap_or("string")
                                .to_string();
                            let description = def
                                .get("description")
                                .and_then(|d| d.as_str())
                                .map(|s| s.to_string());
                            let default = def.get("default").cloned();

                            params.insert(
                                name.clone(),
                                ToolParam {
                                    param_type,
                                    description,
                                    default,
                                },
                            );
                        }
                    }

                    if let Some(req) = schema.get("required").and_then(|r| r.as_array()) {
                        for r in req {
                            if let Some(s) = r.as_str() {
                                required.push(s.to_string());
                            }
                        }
                    }
                }

                ToolSchema {
                    name: mcp_tool.name.clone(),
                    description: mcp_tool.description.clone().unwrap_or_default(),
                    parameters: params,
                    required,
                }
            })
            .collect()
    }

    pub fn call_tool(&mut self, name: &str, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let params = ToolCallParams {
            name: name.to_string(),
            arguments,
        };

        let request = JsonRpcRequest::new(
            next_id(),
            "tools/call",
            Some(serde_json::to_value(&params).map_err(|e| ToolError::new(format!("Serialize error: {}", e)))?),
        );

        let response = self.transport.send_and_recv(request).map_err(|e| ToolError::new(e))?;

        if let Some(error) = response.error {
            return Err(ToolError::new(format!(
                "MCP tool call error (code {}): {}",
                error.code, error.message
            )));
        }

        let call_result: ToolCallResult = response
            .result
            .ok_or_else(|| ToolError::new("No result in tool call response"))?
            .try_into()
            .map_err(|e: serde_json::Error| ToolError::new(format!("Failed to parse tool call result: {}", e)))?;

        let text_parts: Vec<&str> = call_result
            .content
            .iter()
            .filter_map(|c| {
                if c.content_type == "text" {
                    c.text.as_deref()
                } else {
                    None
                }
            })
            .collect();

        Ok(serde_json::json!({
            "content": text_parts.join("\n"),
            "is_error": call_result.is_error.unwrap_or(false),
        }))
    }

    pub fn close(&mut self) {
        self.transport.close();
    }
}
```

**Step 2: Verify it compiles**

Run: `cargo check`

---

### Task 4: MCP Tool Adapter

**Files:**
- Create: `src/mcp/tool_adapter.rs`

This is the key piece - adapts an MCP remote tool into the local `Tool` trait so it can be registered in `ToolRegistry`.

**Step 1: Write `src/mcp/tool_adapter.rs`**

```rust
//! Adapter to wrap MCP remote tools as local Tool trait implementations

use std::sync::Mutex;

use crate::mcp::client::McpClient;
use crate::mcp::types::McpToolSchema;
use crate::tool::{Tool, ToolError, ToolResult, ToolSchema};

pub struct McpToolAdapter {
    tool_name: String,
    tool_description: String,
    schema: ToolSchema,
    client: Mutex<McpClient>,
}

impl McpToolAdapter {
    pub fn new(mcp_tool: &McpToolSchema, schema: ToolSchema, client: McpClient) -> Self {
        Self {
            tool_name: mcp_tool.name.clone(),
            tool_description: mcp_tool.description.clone().unwrap_or_default(),
            schema,
            client: Mutex::new(client),
        }
    }

    pub fn from_client(client: McpClient) -> Vec<Box<dyn Tool + Send + Sync>> {
        let schemas = client.tool_schemas();
        let tools = client.tools().to_vec();

        tools
            .into_iter()
            .zip(schemas.into_iter())
            .map(|(mcp_tool, schema)| {
                let adapter = Self::new(&mcp_tool, schema, McpClient {
                    transport: todo!(),
                    server_info: client.server_info().cloned(),
                    tools_cache: vec![mcp_tool.clone()],
                });
                Box::new(adapter) as Box<dyn Tool + Send + Sync>
            })
            .collect()
    }
}

impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.tool_description
    }

    fn schema(&self) -> ToolSchema {
        self.schema.clone()
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let mut client = self.client.lock().map_err(|e| ToolError::new(format!("Lock error: {}", e)))?;
        client.call_tool(&self.tool_name, args)
    }
}
```

Wait - `McpClient` contains a `Box<dyn McpTransport>` which is not `Clone`. We can't share one client across multiple adapters. The right approach: wrap the client in `Arc<Mutex<>>` and share it.

**Step 1 (revised): Write `src/mcp/tool_adapter.rs`**

```rust
//! Adapter to wrap MCP remote tools as local Tool trait implementations

use std::sync::{Arc, Mutex};

use crate::mcp::client::McpClient;
use crate::mcp::types::McpToolSchema;
use crate::tool::{Tool, ToolError, ToolResult, ToolSchema};

type SharedClient = Arc<Mutex<McpClient>>;

pub struct McpToolAdapter {
    tool_name: String,
    tool_description: String,
    schema: ToolSchema,
    client: SharedClient,
}

pub fn mcp_tools_from_config(config: &crate::mcp::types::McpServerConfig) -> Result<Vec<Box<dyn Tool + Send + Sync>>, String> {
    let client = McpClient::connect(config)?;
    let tools = client.tools().to_vec();
    let schemas = client.tool_schemas();
    let shared = Arc::new(Mutex::new(client));

    Ok(tools
        .into_iter()
        .zip(schemas.into_iter())
        .map(|(mcp_tool, schema)| {
            let adapter = McpToolAdapter {
                tool_name: mcp_tool.name.clone(),
                tool_description: mcp_tool.description.clone().unwrap_or_default(),
                schema,
                client: shared.clone(),
            };
            Box::new(adapter) as Box<dyn Tool + Send + Sync>
        })
        .collect())
}

impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.tool_description
    }

    fn schema(&self) -> ToolSchema {
        self.schema.clone()
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let mut client = self.client.lock().map_err(|e| ToolError::new(format!("Lock error: {}", e)))?;
        client.call_tool(&self.tool_name, args)
    }
}
```

**Step 2: Verify it compiles**

Run: `cargo check`

---

### Task 5: Wire Into lib.rs and Add Registry Helper

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/tool_registry.rs`

**Step 1: Add mcp module to `src/lib.rs`**

Add after line 18 (`pub mod tools;`):

```rust
pub mod mcp;
```

Add to re-exports after line 28:

```rust
pub use mcp::{McpClient, McpServerConfig, McpToolAdapter};
```

**Step 2: Add convenience method to `ToolRegistry`**

Add to `src/tool_registry.rs` impl block:

```rust
pub fn register_mcp_server(&self, config: &crate::mcp::types::McpServerConfig) -> Result<(), String> {
    let tools = crate::mcp::tool_adapter::mcp_tools_from_config(config)?;
    for tool in tools {
        self.register(tool);
    }
    Ok(())
}
```

**Step 3: Verify it compiles**

Run: `cargo check`

---

### Task 6: Update Config for MCP Servers

**Files:**
- Modify: `src/config.rs`

**Step 1: Add mcp_servers field to Config**

In `src/config.rs`, add to the `Config` struct (after line 11):

```rust
#[serde(default)]
pub mcp_servers: std::collections::HashMap<String, crate::mcp::types::McpServerConfig>,
```

**Step 2: Verify it compiles**

Run: `cargo check`

---

### Task 7: Tests

**Files:**
- Modify: `src/tests.rs`
- Create: `src/mcp/test_utils.rs` (optional mock transport for testing)

**Step 1: Add MCP unit tests to `src/tests.rs`**

```rust
#[test]
fn test_mcp_server_config_stdio() {
    use crate::mcp::types::McpServerConfig;
    let config = McpServerConfig::stdio("npx", vec!["-y".into(), "@modelcontextprotocol/server-filesystem".into(), "/tmp".into()]);
    assert!(config.is_stdio());
    assert!(!config.is_http());
    assert_eq!(config.command.as_deref(), Some("npx"));
    assert_eq!(config.args.as_ref().unwrap().len(), 3);
}

#[test]
fn test_mcp_server_config_http() {
    use crate::mcp::types::McpServerConfig;
    let config = McpServerConfig::http("http://localhost:3001/mcp");
    assert!(config.is_http());
    assert!(!config.is_stdio());
    assert_eq!(config.url.as_deref(), Some("http://localhost:3001/mcp"));
}

#[test]
fn test_json_rpc_request_serialization() {
    use crate::mcp::types::JsonRpcRequest;
    let req = JsonRpcRequest::new(1, "initialize", Some(serde_json::json!({"test": true})));
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("\"jsonrpc\":\"2.0\""));
    assert!(json.contains("\"id\":1"));
    assert!(json.contains("\"method\":\"initialize\""));
}

#[test]
fn test_json_rpc_response_deserialization() {
    use crate::mcp::types::JsonRpcResponse;
    let json = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#;
    let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.id, 1);
    assert!(resp.result.is_some());
    assert!(resp.error.is_none());
}

#[test]
fn test_mcp_tool_schema_conversion() {
    use crate::mcp::types::{McpToolSchema, ToolsListResult};
    let json = r#"{"tools":[{"name":"read_file","description":"Read a file","inputSchema":{"type":"object","properties":{"path":{"type":"string","description":"File path"}},"required":["path"]}}]}"#;
    let result: ToolsListResult = serde_json::from_str(json).unwrap();
    assert_eq!(result.tools.len(), 1);
    assert_eq!(result.tools[0].name, "read_file");
}

#[test]
fn test_mcp_server_config_serialization_roundtrip() {
    use crate::mcp::types::McpServerConfig;
    let config = McpServerConfig::stdio("my-server", vec!["--arg1".into()]);
    let json = serde_json::to_string(&config).unwrap();
    let parsed: McpServerConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.command, config.command);
    assert_eq!(parsed.args, config.args);
}
```

**Step 2: Run tests**

Run: `cargo test`
Expected: All tests pass

---

### Task 8: Full Build Verification

**Step 1: Run full check**

Run: `cargo check && cargo test`

**Step 2: Commit**

```bash
git add src/mcp/ src/lib.rs src/tool_registry.rs src/config.rs src/tests.rs
git commit -m "feat: add MCP client with stdio and HTTP transports"
```

---

## File Summary

| File | Action | Purpose |
|------|--------|---------|
| `src/mcp/mod.rs` | Create | Module entry point + re-exports |
| `src/mcp/types.rs` | Create | JSON-RPC 2.0 + MCP protocol types |
| `src/mcp/transport.rs` | Create | `McpTransport` trait, `StdioTransport`, `HttpTransport` |
| `src/mcp/client.rs` | Create | `McpClient` - connect, discover tools, call tools |
| `src/mcp/tool_adapter.rs` | Create | `McpToolAdapter` + `mcp_tools_from_config()` helper |
| `src/lib.rs` | Modify | Add `mcp` module + re-exports |
| `src/tool_registry.rs` | Modify | Add `register_mcp_server()` helper |
| `src/config.rs` | Modify | Add `mcp_servers` field |
| `src/tests.rs` | Modify | Add MCP unit tests |

## Usage After Implementation

```rust
use core_agentic::{ToolRegistry, McpServerConfig};

let registry = ToolRegistry::new();

// Register local builtin tools
for tool in core_agentic::tools::builtin_tools() {
    registry.register(tool);
}

// Register MCP server via stdio
let fs_config = McpServerConfig::stdio(
    "npx",
    vec!["-y".into(), "@modelcontextprotocol/server-filesystem".into(), "/tmp".into()],
);
registry.register_mcp_server(&fs_config)?;

// Register MCP server via HTTP
let http_config = McpServerConfig::http("http://localhost:3001/mcp");
registry.register_mcp_server(&http_config)?;

// Now all tools (local + remote) are available via the registry
```
