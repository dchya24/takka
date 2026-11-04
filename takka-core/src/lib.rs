//! Takka Core - Rust library for AI agent orchestration
//!
//! Provides multi-step task execution, tool system, LLM provider integration,
//! memory management, and safety features.

#[cfg(test)]
mod tests;

pub mod agent;
pub mod attachments;
pub mod capabilities;
pub mod config;
pub mod context;
pub mod diff_util;
pub mod events;
pub mod file_tracker;
pub mod mcp;
pub mod memory;
pub mod memory_file;
pub mod orchestrator;
pub mod planner;
pub mod prompts;
pub mod providers;
pub mod runtime;
pub mod safety;
pub mod session;
pub mod skills;
pub mod tool;
pub mod tool_registry;
pub mod tools;

// Re-export main types for public API
pub use agent::{Agent, AgentConfig};
pub use attachments::{
    Attachment, AttachmentError, AttachmentKind, AttachmentLimits, AttachmentSource,
    ALLOWED_IMAGE_MIME, DEFAULT_MAX_BYTES,
};
pub use capabilities::ModelCapabilities;
pub use config::{
    AgentLoopConfig, BreakpointStrategy, CacheConfig, Config, ModelConfig, ModelOutput,
    OutputConfig, PlannerLoopConfig, ProviderConfig, SafetyConfig, SkillsConfig,
};
pub use context::{BuildOptions, ContextBudget, ContextEngine};
pub use events::{Event, EventType};
pub use file_tracker::{FileTracker, Freshness};
pub use mcp::{
    AsyncHttpTransport, AsyncMcpClient, AsyncMcpToolAdapter, AsyncMcpTransport, AsyncSseTransport,
    AsyncStdioTransport, McpClient, McpServerConfig, McpToolAdapter, ReconnectConfig,
};
pub use memory::{
    ContextWindow, Memory, MemoryConfig, Message, MessageMetadata, MessageRole, SessionInfo,
    SummarizedContext,
};
pub use memory_file::{
    append_project_memory, append_user_memory, assemble_memory_section, find_project_memory,
    load_project_memory, load_user_memory, user_memory_path, PROJECT_MEMORY_FILE,
};
pub use orchestrator::{Orchestrator, OrchestratorState};
pub use planner::{Plan, PlanResult, PlanStatus, PlannerAgent, PlannerConfig, Step, StepStatus};
pub use prompts::{
    assemble_system_prompt, find_project_instructions, load_project_instructions,
    skills_system_section, DEFAULT_SYSTEM_PROMPT, PROJECT_INSTRUCTION_FILES,
};
pub use providers::LLMProvider;
pub use runtime::{AgentLoop, AgentRuntime, RuntimeStatus, StandardLoop};
pub use safety::{
    AuditDecision, AuditEntry, ConfirmationRequest, PermissionMode, RateLimit, RiskLevel,
    RiskScore, SafetyDecision, UrlPolicy,
};
pub use session::{AgentSession, SessionStore, SessionSummary, SESSION_FORMAT_VERSION};
pub use tool::{
    Concurrency, Mutability, SideEffects, Tool, ToolCall, ToolError, ToolMetadata, ToolResult,
    ToolResultValue, ToolSchema,
};
pub use tool_registry::{CapabilityAnalysis, ToolRegistry};

// Re-export skill system types
pub use skills::{
    discover_skills, DiscoveryConfig, Skill, SkillIndex, SkillLoader, SkillMetadata, SkillRegistry,
    SkillTool,
};

// Re-export tool implementations
pub use tools::{
    clear_todos,
    current_todos,
    ApplyPatchTool,
    EditFileTool,
    FetchTool,
    GitDiffTool,
    GitStatusTool,
    GlobTool,
    GrepTool,
    ListFilesTool,
    QuestionAnswer,
    QuestionHandler,
    QuestionPrompt,
    // Interactive tools
    QuestionTool,
    ReadFileTool,
    RunCommandTool,
    RunScriptTool,
    RunTestsTool,
    SearchFilesTool,
    SpawnSubagentTool,
    TodoChangeHandler,
    TodoItem,
    TodoPriority,
    TodoStatus,
    TodowriteTool,
    ToolDeps,
    UpdateMemoryTool,
    WebSearchTool,
    WriteFileTool,
};

// Re-export provider implementations
pub use providers::anthropic::AnthropicProvider;
pub use providers::openai::OpenAIProvider;
pub use providers::{ModelCapability, ModelInfo};

pub type Result<T> = std::result::Result<T, AgenticError>;

#[derive(Debug, thiserror::Error)]
pub enum AgenticError {
    #[error("Provider error: {0}")]
    Provider(String),

    #[error("Tool error: {0}")]
    Tool(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Cancelled by user")]
    Cancelled,
}
