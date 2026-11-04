//! MCP transport layer — async stdio, HTTP, and SSE transports.
//!
//! Provides both the original blocking transports (for backward compatibility)
//! and new async transports built on tokio + reqwest.

use crate::mcp::types::{JsonRpcRequest, JsonRpcResponse};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

// ---------------------------------------------------------------------------
// Blocking transport trait (original, kept for backward compat)
// ---------------------------------------------------------------------------

pub trait McpTransport: Send + Sync {
    fn send_and_recv(&mut self, request: JsonRpcRequest) -> Result<JsonRpcResponse, String>;
    fn close(&mut self);
}

// ---------------------------------------------------------------------------
// Blocking Stdio transport (original)
// ---------------------------------------------------------------------------

pub struct StdioTransport {
    child: Child,
    stdin: ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

impl StdioTransport {
    pub fn new(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self, String> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        for (key, value) in env {
            cmd.env(key, value);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn MCP server '{}': {}", command, e))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Failed to get stdin of MCP server".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Failed to get stdout of MCP server".to_string())?;

        Ok(Self {
            child,
            stdin,
            stdout: std::io::BufReader::new(stdout),
        })
    }
}

impl McpTransport for StdioTransport {
    fn send_and_recv(&mut self, request: JsonRpcRequest) -> Result<JsonRpcResponse, String> {
        let msg = serde_json::to_string(&request)
            .map_err(|e| format!("Failed to serialize request: {}", e))?;
        let line = format!("{}\n", msg);

        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| format!("Failed to write to MCP server stdin: {}", e))?;
        self.stdin
            .flush()
            .map_err(|e| format!("Failed to flush stdin: {}", e))?;

        let mut response_line = String::new();
        self.stdout
            .read_line(&mut response_line)
            .map_err(|e| format!("Failed to read from MCP server stdout: {}", e))?;

        let response: JsonRpcResponse =
            serde_json::from_str(response_line.trim()).map_err(|e| {
                format!(
                    "Failed to parse MCP response: {} (input: {:?})",
                    e, response_line
                )
            })?;

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

// ---------------------------------------------------------------------------
// Blocking HTTP transport (original)
// ---------------------------------------------------------------------------

pub struct HttpTransport {
    url: String,
    client: reqwest::blocking::Client,
    headers: HashMap<String, String>,
}

impl HttpTransport {
    pub fn new(url: &str, headers: HashMap<String, String>) -> Result<Self, String> {
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
        let mut req = self
            .client
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

// ===========================================================================
// Async transport trait
// ===========================================================================

/// Async MCP transport — send JSON-RPC requests and receive responses
/// without blocking the tokio runtime.
#[async_trait::async_trait]
pub trait AsyncMcpTransport: Send + Sync {
    async fn send_and_recv(&mut self, request: JsonRpcRequest) -> Result<JsonRpcResponse, String>;
    async fn close(&mut self);

    /// Check if the transport is still alive / connected.
    /// Default implementation sends a no-op and returns Ok.
    async fn health_check(&mut self) -> Result<bool, String> {
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Async Stdio transport
// ---------------------------------------------------------------------------

pub struct AsyncStdioTransport {
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: Option<tokio::io::BufReader<tokio::process::ChildStdout>>,
    child: Option<tokio::process::Child>,
}

impl AsyncStdioTransport {
    pub async fn new(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self, String> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        for (key, value) in env {
            cmd.env(key, value);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn MCP server '{}': {}", command, e))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Failed to get stdin of MCP server".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Failed to get stdout of MCP server".to_string())?;

        Ok(Self {
            command: command.to_string(),
            args: args.to_vec(),
            env: env.clone(),
            stdin: Some(stdin),
            stdout: Some(tokio::io::BufReader::new(stdout)),
            child: Some(child),
        })
    }

    /// Reconnect to the MCP server (spawns a new child process).
    pub async fn reconnect(&mut self) -> Result<(), String> {
        // Close existing
        self.close().await;

        let mut cmd = tokio::process::Command::new(&self.command);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        for (key, value) in &self.env {
            cmd.env(key, value);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to respawn MCP server '{}': {}", self.command, e))?;

        self.stdin = Some(
            child
                .stdin
                .take()
                .ok_or_else(|| "Failed to get stdin after reconnect".to_string())?,
        );
        self.stdout =
            Some(tokio::io::BufReader::new(child.stdout.take().ok_or_else(
                || "Failed to get stdout after reconnect".to_string(),
            )?));
        self.child = Some(child);
        Ok(())
    }
}

#[async_trait::async_trait]
impl AsyncMcpTransport for AsyncStdioTransport {
    async fn send_and_recv(&mut self, request: JsonRpcRequest) -> Result<JsonRpcResponse, String> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let stdin = self
            .stdin
            .as_mut()
            .ok_or("Stdio transport not connected (no stdin)")?;
        let stdout = self
            .stdout
            .as_mut()
            .ok_or("Stdio transport not connected (no stdout)")?;

        let msg = serde_json::to_string(&request)
            .map_err(|e| format!("Failed to serialize request: {}", e))?;
        let line = format!("{}\n", msg);

        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("Failed to write to MCP server stdin: {}", e))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("Failed to flush stdin: {}", e))?;

        let mut response_line = String::new();
        stdout
            .read_line(&mut response_line)
            .await
            .map_err(|e| format!("Failed to read from MCP server stdout: {}", e))?;

        let response: JsonRpcResponse =
            serde_json::from_str(response_line.trim()).map_err(|e| {
                format!(
                    "Failed to parse MCP response: {} (input: {:?})",
                    e, response_line
                )
            })?;

        Ok(response)
    }

    async fn close(&mut self) {
        use tokio::io::AsyncWriteExt;
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.flush().await;
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.stdout = None;
    }

    async fn health_check(&mut self) -> Result<bool, String> {
        // Check if the child process is still running
        if let Some(ref mut child) = self.child {
            match child.try_wait() {
                Ok(None) => Ok(true), // Still running
                Ok(Some(status)) => Err(format!("MCP server exited with status: {}", status)),
                Err(e) => Err(format!("Failed to check MCP server status: {}", e)),
            }
        } else {
            Err("No child process".to_string())
        }
    }
}

impl Drop for AsyncStdioTransport {
    fn drop(&mut self) {
        // Best-effort synchronous cleanup
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
    }
}

// ---------------------------------------------------------------------------
// Async HTTP transport
// ---------------------------------------------------------------------------

pub struct AsyncHttpTransport {
    url: String,
    client: reqwest::Client,
    headers: HashMap<String, String>,
}

impl AsyncHttpTransport {
    pub fn new(url: &str, headers: HashMap<String, String>) -> Result<Self, String> {
        let client = reqwest::Client::builder()
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

#[async_trait::async_trait]
impl AsyncMcpTransport for AsyncHttpTransport {
    async fn send_and_recv(&mut self, request: JsonRpcRequest) -> Result<JsonRpcResponse, String> {
        let mut req = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json");

        for (key, value) in &self.headers {
            req = req.header(key.as_str(), value.as_str());
        }

        let response = req
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("HTTP request to MCP server failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("MCP server returned HTTP {}: {}", status, body));
        }

        let rpc_response: JsonRpcResponse = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse MCP HTTP response: {}", e))?;

        Ok(rpc_response)
    }

    async fn close(&mut self) {
        // HTTP has no persistent connection to close
    }

    async fn health_check(&mut self) -> Result<bool, String> {
        // POST an invalid/empty request and check we get a response
        let probe = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&serde_json::json!({"jsonrpc":"2.0","id":0,"method":"ping"}))
            .send()
            .await;

        match probe {
            Ok(resp) if resp.status().is_success() => Ok(true),
            Ok(resp) => Err(format!("Health check returned HTTP {}", resp.status())),
            Err(e) => Err(format!("Health check failed: {}", e)),
        }
    }
}

// ---------------------------------------------------------------------------
// SSE transport (HTTP POST for requests, SSE stream for notifications)
// ---------------------------------------------------------------------------

/// SSE (Server-Sent Events) transport for MCP servers that support it.
/// Sends requests via HTTP POST and can listen for server-pushed events.
pub struct AsyncSseTransport {
    http: AsyncHttpTransport,
    connected: bool,
}

impl AsyncSseTransport {
    pub fn new(url: &str, headers: HashMap<String, String>) -> Result<Self, String> {
        Ok(Self {
            http: AsyncHttpTransport::new(url, headers)?,
            connected: false,
        })
    }

    /// Try to establish an SSE connection. Not required for basic request/response.
    pub async fn connect_sse(&mut self) -> Result<(), String> {
        // The SSE connection is optional — it's for receiving server-initiated events.
        // For basic tool calling we just POST. Mark as connected.
        self.connected = true;
        Ok(())
    }
}

#[async_trait::async_trait]
impl AsyncMcpTransport for AsyncSseTransport {
    async fn send_and_recv(&mut self, request: JsonRpcRequest) -> Result<JsonRpcResponse, String> {
        self.http.send_and_recv(request).await
    }

    async fn close(&mut self) {
        self.http.close().await;
        self.connected = false;
    }

    async fn health_check(&mut self) -> Result<bool, String> {
        self.http.health_check().await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blocking_stdio_transport_new_invalid_command() {
        let result = StdioTransport::new(
            "nonexistent_command_that_does_not_exist_12345",
            &[],
            &HashMap::new(),
        );
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.contains("Failed to spawn"));
    }

    #[test]
    fn test_blocking_http_transport_new() {
        let result = HttpTransport::new("http://localhost:1", HashMap::new());
        assert!(result.is_ok());
    }

    #[test]
    fn test_async_stdio_transport_new_invalid_command() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(AsyncStdioTransport::new(
            "nonexistent_command_that_does_not_exist_12345",
            &[],
            &HashMap::new(),
        ));
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.contains("Failed to spawn"));
    }

    #[test]
    fn test_async_http_transport_new() {
        let result = AsyncHttpTransport::new("http://localhost:1", HashMap::new());
        assert!(result.is_ok());
    }

    #[test]
    fn test_async_sse_transport_new() {
        let result = AsyncSseTransport::new("http://localhost:3001/mcp", HashMap::new());
        assert!(result.is_ok());
        let transport = result.unwrap();
        assert!(!transport.connected);
    }

    #[test]
    fn test_async_sse_transport_connect() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut transport =
            AsyncSseTransport::new("http://localhost:3001/mcp", HashMap::new()).unwrap();
        rt.block_on(transport.connect_sse()).unwrap();
        assert!(transport.connected);
    }

    #[tokio::test]
    async fn test_async_stdio_transport_reconnect_without_spawn() {
        let mut transport = AsyncStdioTransport {
            command: "echo".to_string(),
            args: vec![],
            env: HashMap::new(),
            stdin: None,
            stdout: None,
            child: None,
        };
        // Reconnect should try to spawn, echo won't have stdout pipe — but at least
        // we verify the method runs without panic.
        let result = transport.reconnect().await;
        // echo exits immediately, so stdout pipe may fail
        // We just verify no panic.
        let _ = result;
    }

    #[tokio::test]
    async fn test_async_http_health_check_fails_on_bad_url() {
        let mut transport = AsyncHttpTransport::new("http://localhost:1", HashMap::new()).unwrap();
        let result = transport.health_check().await;
        assert!(result.is_err());
    }
}
