//! Async MCP client — manages connection to a single MCP server.
//!
//! Provides both the original blocking `McpClient` (backward compat)
//! and the new `AsyncMcpClient` built on tokio.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::mcp::transport::{
    AsyncHttpTransport, AsyncMcpTransport, AsyncSseTransport, AsyncStdioTransport, HttpTransport,
    McpTransport, StdioTransport,
};
use crate::mcp::types::*;
use crate::tool::{ToolError, ToolParam, ToolSchema};

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Blocking client (original, backward compat)
// ---------------------------------------------------------------------------

pub struct McpClient {
    transport: Box<dyn McpTransport>,
    server_info: Option<ServerInfo>,
    tools_cache: Vec<McpToolSchema>,
}

impl McpClient {
    pub fn connect(config: &McpServerConfig) -> Result<Self, String> {
        let transport: Box<dyn McpTransport> = if config.is_stdio() {
            let command = config
                .command
                .as_ref()
                .ok_or("Missing command for stdio transport")?;
            let args = config.args.as_ref().cloned().unwrap_or_default();
            let env = config.env.as_ref().cloned().unwrap_or_default();
            Box::new(StdioTransport::new(command, &args, &env)?)
        } else if config.is_http() {
            let url = config
                .url
                .as_ref()
                .ok_or("Missing URL for HTTP transport")?;
            let headers = config.headers.as_ref().cloned().unwrap_or_default();
            Box::new(HttpTransport::new(url, headers)?)
        } else {
            return Err(
                "MCP server config must have either 'command' (stdio) or 'url' (http)".into(),
            );
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
                name: "takka-core".into(),
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
            return Err(format!(
                "MCP initialize error (code {}): {}",
                error.code, error.message
            ));
        }

        let init_result: InitializeResult =
            serde_json::from_value(response.result.ok_or("No result in initialize response")?)
                .map_err(|e: serde_json::Error| {
                    format!("Failed to parse initialize result: {}", e)
                })?;

        self.server_info = init_result.server_info;

        let initialized = JsonRpcRequest::new(next_id(), "notifications/initialized", None);
        let _ = self.transport.send_and_recv(initialized);

        Ok(())
    }

    fn discover_tools(&mut self) -> Result<Vec<McpToolSchema>, String> {
        let request = JsonRpcRequest::new(next_id(), "tools/list", None);
        let response = self.transport.send_and_recv(request)?;

        if let Some(error) = response.error {
            return Err(format!(
                "MCP tools/list error (code {}): {}",
                error.code, error.message
            ));
        }

        let result: ToolsListResult =
            serde_json::from_value(response.result.ok_or("No result in tools/list response")?)
                .map_err(|e: serde_json::Error| {
                    format!("Failed to parse tools/list result: {}", e)
                })?;

        Ok(result.tools)
    }

    pub fn server_info(&self) -> Option<&ServerInfo> {
        self.server_info.as_ref()
    }

    pub fn tools(&self) -> &[McpToolSchema] {
        &self.tools_cache
    }

    pub fn tool_schemas(&self) -> Vec<ToolSchema> {
        Self::convert_tool_schemas(&self.tools_cache)
    }

    pub fn call_tool(
        &mut self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let params = ToolCallParams {
            name: name.to_string(),
            arguments,
        };

        let request = JsonRpcRequest::new(
            next_id(),
            "tools/call",
            Some(
                serde_json::to_value(&params)
                    .map_err(|e| ToolError::new(format!("Serialize error: {}", e)))?,
            ),
        );

        let response = self
            .transport
            .send_and_recv(request)
            .map_err(ToolError::new)?;

        if let Some(error) = response.error {
            return Err(ToolError::new(format!(
                "MCP tool call error (code {}): {}",
                error.code, error.message
            )));
        }

        let call_result: ToolCallResult = serde_json::from_value(
            response
                .result
                .ok_or_else(|| ToolError::new("No result in tool call response"))?,
        )
        .map_err(|e: serde_json::Error| {
            ToolError::new(format!("Failed to parse tool call result: {}", e))
        })?;

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

    /// Convert MCP tool schemas to local ToolSchema (shared by both sync and async clients).
    fn convert_tool_schemas(tools_cache: &[McpToolSchema]) -> Vec<ToolSchema> {
        tools_cache
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
}

// ===========================================================================
// Async client
// ===========================================================================

/// Configuration for auto-reconnection behavior.
#[derive(Debug, Clone)]
pub struct ReconnectConfig {
    /// Maximum number of reconnection attempts before giving up.
    pub max_attempts: usize,
    /// Base delay in milliseconds between attempts (doubles each retry).
    pub base_delay_ms: u64,
    /// Maximum delay in milliseconds between attempts.
    pub max_delay_ms: u64,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay_ms: 500,
            max_delay_ms: 10_000,
        }
    }
}

/// Async MCP client with auto-reconnection and health checking.
pub struct AsyncMcpClient {
    transport: Box<dyn AsyncMcpTransport>,
    config: McpServerConfig,
    server_info: Option<ServerInfo>,
    tools_cache: Vec<McpToolSchema>,
    reconnect_config: ReconnectConfig,
    connected: bool,
}

impl AsyncMcpClient {
    /// Connect to an MCP server using the given configuration.
    /// Automatically selects the appropriate transport (stdio, HTTP, or SSE).
    pub async fn connect(config: &McpServerConfig) -> Result<Self, String> {
        let transport = Self::create_transport(config).await?;

        let mut client = Self {
            transport,
            config: config.clone(),
            server_info: None,
            tools_cache: Vec::new(),
            reconnect_config: ReconnectConfig::default(),
            connected: false,
        };

        client.handshake().await?;
        client.connected = true;

        Ok(client)
    }

    /// Connect with custom reconnection settings.
    pub async fn connect_with_reconnect(
        config: &McpServerConfig,
        reconnect: ReconnectConfig,
    ) -> Result<Self, String> {
        let mut client = Self::connect(config).await?;
        client.reconnect_config = reconnect;
        Ok(client)
    }

    /// Create the appropriate async transport based on config.
    async fn create_transport(
        config: &McpServerConfig,
    ) -> Result<Box<dyn AsyncMcpTransport>, String> {
        if config.is_stdio() {
            let command = config
                .command
                .as_ref()
                .ok_or("Missing command for stdio transport")?;
            let args = config.args.as_ref().cloned().unwrap_or_default();
            let env = config.env.as_ref().cloned().unwrap_or_default();
            Ok(Box::new(
                AsyncStdioTransport::new(command, &args, &env).await?,
            ))
        } else if config.is_http() {
            let url = config
                .url
                .as_ref()
                .ok_or("Missing URL for HTTP transport")?;
            let headers = config.headers.as_ref().cloned().unwrap_or_default();

            // Try SSE transport first; fall back to plain HTTP
            match AsyncSseTransport::new(url, headers.clone()) {
                Ok(sse) => Ok(Box::new(sse)),
                Err(_) => Ok(Box::new(AsyncHttpTransport::new(url, headers)?)),
            }
        } else {
            Err("MCP server config must have either 'command' (stdio) or 'url' (http)".into())
        }
    }

    /// Perform the MCP initialization handshake.
    async fn handshake(&mut self) -> Result<(), String> {
        let params = InitializeParams {
            protocol_version: "2024-11-05".into(),
            capabilities: serde_json::json!({}),
            client_info: ClientInfo {
                name: "takka-core".into(),
                version: "0.1.0".into(),
            },
        };

        let request = JsonRpcRequest::new(
            next_id(),
            "initialize",
            Some(serde_json::to_value(&params).map_err(|e| format!("Serialize error: {}", e))?),
        );

        let response = self.transport.send_and_recv(request).await?;

        if let Some(error) = response.error {
            return Err(format!(
                "MCP initialize error (code {}): {}",
                error.code, error.message
            ));
        }

        let init_result: InitializeResult =
            serde_json::from_value(response.result.ok_or("No result in initialize response")?)
                .map_err(|e: serde_json::Error| {
                    format!("Failed to parse initialize result: {}", e)
                })?;

        self.server_info = init_result.server_info;

        // Send initialized notification
        let initialized = JsonRpcRequest::new(next_id(), "notifications/initialized", None);
        let _ = self.transport.send_and_recv(initialized).await;

        // Discover tools
        self.tools_cache = self.discover_tools().await?;

        Ok(())
    }

    /// Discover available tools from the MCP server.
    async fn discover_tools(&mut self) -> Result<Vec<McpToolSchema>, String> {
        let request = JsonRpcRequest::new(next_id(), "tools/list", None);
        let response = self.transport.send_and_recv(request).await?;

        if let Some(error) = response.error {
            return Err(format!(
                "MCP tools/list error (code {}): {}",
                error.code, error.message
            ));
        }

        let result: ToolsListResult =
            serde_json::from_value(response.result.ok_or("No result in tools/list response")?)
                .map_err(|e: serde_json::Error| {
                    format!("Failed to parse tools/list result: {}", e)
                })?;

        Ok(result.tools)
    }

    // ---- Public API ----

    /// Check if the client is connected.
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Get server info (name, version) if available.
    pub fn server_info(&self) -> Option<&ServerInfo> {
        self.server_info.as_ref()
    }

    /// Get cached tool schemas from the MCP server.
    pub fn tools(&self) -> &[McpToolSchema] {
        &self.tools_cache
    }

    /// Get tool schemas converted to the local `ToolSchema` format.
    pub fn tool_schemas(&self) -> Vec<ToolSchema> {
        McpClient::convert_tool_schemas(&self.tools_cache)
    }

    /// Call a tool on the MCP server.
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let params = ToolCallParams {
            name: name.to_string(),
            arguments,
        };

        let request = JsonRpcRequest::new(
            next_id(),
            "tools/call",
            Some(
                serde_json::to_value(&params)
                    .map_err(|e| ToolError::new(format!("Serialize error: {}", e)))?,
            ),
        );

        let response = self
            .transport
            .send_and_recv(request)
            .await
            .map_err(ToolError::new)?;

        if let Some(error) = response.error {
            return Err(ToolError::new(format!(
                "MCP tool call error (code {}): {}",
                error.code, error.message
            )));
        }

        let call_result: ToolCallResult = serde_json::from_value(
            response
                .result
                .ok_or_else(|| ToolError::new("No result in tool call response"))?,
        )
        .map_err(|e: serde_json::Error| {
            ToolError::new(format!("Failed to parse tool call result: {}", e))
        })?;

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

    /// Check if the underlying transport is healthy.
    pub async fn health_check(&mut self) -> Result<bool, String> {
        self.transport.health_check().await
    }

    /// Attempt to reconnect to the MCP server with exponential backoff.
    pub async fn reconnect(&mut self) -> Result<(), String> {
        let mut delay = self.reconnect_config.base_delay_ms;

        for attempt in 1..=self.reconnect_config.max_attempts {
            log::info!(
                "MCP reconnection attempt {}/{}",
                attempt,
                self.reconnect_config.max_attempts
            );

            // Close existing connection
            self.transport.close().await;
            self.connected = false;

            // Wait with backoff
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            delay = (delay * 2).min(self.reconnect_config.max_delay_ms);

            // Create new transport
            match Self::create_transport(&self.config).await {
                Ok(mut transport) => {
                    // Re-initialize
                    match Self::handshake_with(&mut transport).await {
                        Ok((server_info, tools_cache)) => {
                            self.transport = transport;
                            self.server_info = server_info;
                            self.tools_cache = tools_cache;
                            self.connected = true;
                            log::info!("MCP reconnection successful on attempt {}", attempt);
                            return Ok(());
                        }
                        Err(e) => {
                            log::warn!("MCP handshake failed on attempt {}: {}", attempt, e);
                            transport.close().await;
                        }
                    }
                }
                Err(e) => {
                    log::warn!(
                        "MCP transport creation failed on attempt {}: {}",
                        attempt,
                        e
                    );
                }
            }
        }

        Err(format!(
            "Failed to reconnect after {} attempts",
            self.reconnect_config.max_attempts
        ))
    }

    /// Perform a handshake with an arbitrary transport, returning server_info and tools.
    async fn handshake_with(
        transport: &mut Box<dyn AsyncMcpTransport>,
    ) -> Result<(Option<ServerInfo>, Vec<McpToolSchema>), String> {
        let params = InitializeParams {
            protocol_version: "2024-11-05".into(),
            capabilities: serde_json::json!({}),
            client_info: ClientInfo {
                name: "takka-core".into(),
                version: "0.1.0".into(),
            },
        };

        let request = JsonRpcRequest::new(
            next_id(),
            "initialize",
            Some(serde_json::to_value(&params).map_err(|e| format!("Serialize error: {}", e))?),
        );

        let response = transport.send_and_recv(request).await?;

        if let Some(error) = response.error {
            return Err(format!(
                "MCP initialize error (code {}): {}",
                error.code, error.message
            ));
        }

        let init_result: InitializeResult =
            serde_json::from_value(response.result.ok_or("No result in initialize response")?)
                .map_err(|e: serde_json::Error| {
                    format!("Failed to parse initialize result: {}", e)
                })?;

        let server_info = init_result.server_info;

        // Send initialized notification
        let initialized = JsonRpcRequest::new(next_id(), "notifications/initialized", None);
        let _ = transport.send_and_recv(initialized).await;

        // Discover tools
        let request = JsonRpcRequest::new(next_id(), "tools/list", None);
        let response = transport.send_and_recv(request).await?;

        if let Some(error) = response.error {
            return Err(format!(
                "MCP tools/list error (code {}): {}",
                error.code, error.message
            ));
        }

        let result: ToolsListResult =
            serde_json::from_value(response.result.ok_or("No result in tools/list response")?)
                .map_err(|e: serde_json::Error| {
                    format!("Failed to parse tools/list result: {}", e)
                })?;

        Ok((server_info, result.tools))
    }

    /// Refresh the tool list from the server.
    pub async fn refresh_tools(&mut self) -> Result<(), String> {
        self.tools_cache = self.discover_tools().await?;
        Ok(())
    }

    /// Close the connection.
    pub async fn close(&mut self) {
        self.transport.close().await;
        self.connected = false;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reconnect_config_default() {
        let config = ReconnectConfig::default();
        assert_eq!(config.max_attempts, 3);
        assert_eq!(config.base_delay_ms, 500);
        assert_eq!(config.max_delay_ms, 10_000);
    }

    #[test]
    fn test_reconnect_config_serialization() {
        let config = ReconnectConfig {
            max_attempts: 5,
            base_delay_ms: 1000,
            max_delay_ms: 30_000,
        };
        // ReconnectConfig is Clone but not Serialize — just verify fields
        let cloned = config.clone();
        assert_eq!(cloned.max_attempts, 5);
    }

    #[test]
    fn test_convert_tool_schemas() {
        let schemas = McpClient::convert_tool_schemas(&[McpToolSchema {
            name: "test_tool".to_string(),
            description: Some("A test tool".to_string()),
            input_schema: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path"
                    },
                    "verbose": {
                        "type": "boolean",
                        "default": false
                    }
                },
                "required": ["path"]
            })),
        }]);

        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "test_tool");
        assert_eq!(schemas[0].description, "A test tool");
        assert!(schemas[0].parameters.contains_key("path"));
        assert!(schemas[0].parameters.contains_key("verbose"));
        assert_eq!(schemas[0].required, vec!["path"]);
    }

    #[test]
    fn test_convert_tool_schemas_empty() {
        let schemas = McpClient::convert_tool_schemas(&[]);
        assert!(schemas.is_empty());
    }

    #[test]
    fn test_convert_tool_schemas_no_input_schema() {
        let schemas = McpClient::convert_tool_schemas(&[McpToolSchema {
            name: "no_schema".to_string(),
            description: None,
            input_schema: None,
        }]);
        assert_eq!(schemas.len(), 1);
        assert!(schemas[0].parameters.is_empty());
        assert!(schemas[0].required.is_empty());
    }
}
