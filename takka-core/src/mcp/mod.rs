//! MCP (Model Context Protocol) client module
//!
//! Provides both blocking and async transports for connecting to MCP servers.
//!
//! - **Blocking**: `McpClient`, `McpToolAdapter` (original, backward compat)
//! - **Async**: `AsyncMcpClient`, `AsyncMcpToolAdapter` (tokio-based, with auto-reconnect)

pub mod client;
pub mod tool_adapter;
pub mod transport;
pub mod types;

// Blocking API (backward compat)
pub use client::McpClient;
pub use tool_adapter::McpToolAdapter;
pub use types::McpServerConfig;

// Async API
pub use client::{AsyncMcpClient, ReconnectConfig};
pub use tool_adapter::AsyncMcpToolAdapter;
pub use transport::{
    AsyncHttpTransport, AsyncMcpTransport, AsyncSseTransport, AsyncStdioTransport,
};
