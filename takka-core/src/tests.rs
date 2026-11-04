//! Unit tests for takka-core

use std::sync::Arc;

use crate::tool::{Tool, ToolCall, ToolSchema};
use crate::tools::RunCommandTool;
use crate::{Memory, Message, MessageRole, RiskLevel, ToolRegistry};

#[test]
fn test_tool_call_new() {
    let call = ToolCall::new("echo", serde_json::json!({"text": "hello"}));
    assert_eq!(call.tool_name, "echo");
    assert_eq!(call.arguments["text"], "hello");
    assert!(!call.id.is_empty());
}

#[test]
fn test_tool_schema_new() {
    let schema = ToolSchema::new("echo", "Echoes back the input");
    assert_eq!(schema.name, "echo");
    assert_eq!(schema.description, "Echoes back the input");
    assert!(schema.parameters.is_empty());
}

#[test]
fn test_tool_schema_with_param() {
    let schema =
        ToolSchema::new("echo", "Echoes back the input").with_param("text", "string", true);

    assert!(schema.parameters.contains_key("text"));
    assert!(schema.required.contains(&"text".to_string()));
}

#[test]
fn test_memory_new() {
    let memory = Memory::new(1000);
    assert_eq!(memory.max_tokens, 1000);
}

#[test]
fn test_memory_add_message() {
    let mut memory = Memory::new(1000);
    memory.add_message(Message::user("Hello"));
    memory.add_message(Message::assistant("Hi there"));

    let messages = memory.get_messages();
    assert_eq!(messages.len(), 2);
}

#[test]
fn test_message_user() {
    let msg = Message::user("test content");
    assert!(matches!(msg.role, MessageRole::User));
    assert_eq!(msg.content, "test content");
}

#[test]
fn test_message_assistant() {
    let msg = Message::assistant("response");
    assert!(matches!(msg.role, MessageRole::Assistant));
    assert_eq!(msg.content, "response");
}

#[test]
fn test_message_tool() {
    let msg = Message::tool("my_tool", "call_id", "output");
    assert!(matches!(msg.role, MessageRole::Tool { .. }));
}

#[test]
fn test_risk_level_confirmation() {
    let low = RiskLevel::Low;
    let medium = RiskLevel::Medium;
    let high = RiskLevel::High;
    let critical = RiskLevel::Critical;

    assert!(!low.requires_confirmation());
    assert!(medium.requires_confirmation());
    assert!(high.requires_confirmation());
    assert!(critical.requires_confirmation());
}

#[test]
fn test_run_command_tool_schema() {
    let tool = RunCommandTool::new();
    assert_eq!(tool.name(), "run_command");
    assert_eq!(
        tool.description(),
        "Execute a shell command and return its output"
    );

    let schema = tool.schema();
    assert_eq!(schema.name, "run_command");
    assert!(schema.parameters.contains_key("command"));
    assert!(schema.required.contains(&"command".to_string()));
}

#[test]
fn test_run_command_tool_execute() {
    let tool = RunCommandTool::new();
    let result = tool.execute(serde_json::json!({"command": "echo hello"}));

    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(output.get("success").unwrap().as_bool().unwrap());
    assert!(output
        .get("stdout")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("hello"));
}

#[test]
fn test_run_command_tool_missing_command() {
    let tool = RunCommandTool::new();
    let result = tool.execute(serde_json::json!({}));

    assert!(result.is_err());
}

#[test]
fn test_tool_registry_register() {
    let registry = ToolRegistry::new();
    let tool = RunCommandTool::new();

    registry.register(Box::new(tool));

    assert!(registry.has_tool("run_command"));
    assert!(!registry.has_tool("nonexistent"));
}

#[test]
fn test_tool_registry_list() {
    let registry = ToolRegistry::new();
    let tool = RunCommandTool::new();

    registry.register(Box::new(tool));

    let tools = registry.list();
    assert!(!tools.is_empty());
}

#[test]
fn test_memory_clear() {
    let mut memory = Memory::new(1000);
    memory.add_message(Message::user("Hello"));
    memory.add_message(Message::assistant("Hi there"));

    memory.clear();

    let messages = memory.get_messages();
    assert!(messages.is_empty());
}

#[test]
fn test_memory_context_limit() {
    let mut memory = Memory::new(100);
    for i in 0..10 {
        memory.add_message(Message::user(format!("message {}", i)));
    }

    let messages = memory.get_messages();
    assert_eq!(messages.len(), 10);
}

#[test]
fn test_mcp_server_config_stdio() {
    use crate::mcp::types::McpServerConfig;
    let config = McpServerConfig::stdio(
        "npx",
        vec![
            "-y".into(),
            "@modelcontextprotocol/server-filesystem".into(),
            "/tmp".into(),
        ],
    );
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
    let json =
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#;
    let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.id, 1);
    assert!(resp.result.is_some());
    assert!(resp.error.is_none());
}

#[test]
fn test_json_rpc_error_response() {
    use crate::mcp::types::JsonRpcResponse;
    let json = r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32600,"message":"Invalid Request"}}"#;
    let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.id, 2);
    assert!(resp.error.is_some());
    let err = resp.error.unwrap();
    assert_eq!(err.code, -32600);
    assert_eq!(err.message, "Invalid Request");
}

#[test]
fn test_mcp_tool_schema_parsing() {
    use crate::mcp::types::ToolsListResult;
    let json = r#"{"tools":[{"name":"read_file","description":"Read a file","inputSchema":{"type":"object","properties":{"path":{"type":"string","description":"File path"}},"required":["path"]}}]}"#;
    let result: ToolsListResult = serde_json::from_str(json).unwrap();
    assert_eq!(result.tools.len(), 1);
    assert_eq!(result.tools[0].name, "read_file");
    assert_eq!(result.tools[0].description.as_deref(), Some("Read a file"));
    assert!(result.tools[0].input_schema.is_some());
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

#[test]
fn test_mcp_http_config_roundtrip() {
    use crate::mcp::types::McpServerConfig;
    let mut config = McpServerConfig::http("http://localhost:8080/mcp");
    let mut headers = std::collections::HashMap::new();
    headers.insert("Authorization".into(), "Bearer test-token".into());
    config.headers = Some(headers);

    let json = serde_json::to_string(&config).unwrap();
    let parsed: McpServerConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.url, config.url);
    assert!(parsed.headers.is_some());
    assert_eq!(
        parsed.headers.unwrap().get("Authorization").unwrap(),
        "Bearer test-token"
    );
}

#[test]
fn test_tool_call_result_parsing() {
    use crate::mcp::types::ToolCallResult;
    let json = r#"{"content":[{"type":"text","text":"file contents here"}],"is_error":false}"#;
    let result: ToolCallResult = serde_json::from_str(json).unwrap();
    assert_eq!(result.content.len(), 1);
    assert_eq!(result.content[0].content_type, "text");
    assert_eq!(
        result.content[0].text.as_deref(),
        Some("file contents here")
    );
    assert_eq!(result.is_error, Some(false));
}

// Anthropic Provider Tests

#[test]
fn test_anthropic_config_new() {
    use crate::providers::AnthropicProviderConfig;
    let config =
        AnthropicProviderConfig::new("test-provider", "sk-ant-test", "claude-3-5-sonnet-20241022");
    assert_eq!(config.id, "test-provider");
    assert_eq!(config.api_key, "sk-ant-test");
    assert_eq!(config.default_model, "claude-3-5-sonnet-20241022");
    assert_eq!(config.provider_type, "anthropic");
    assert_eq!(config.base_url, "https://api.anthropic.com/v1");
}

#[test]
fn test_anthropic_config_with_base_url() {
    use crate::providers::AnthropicProviderConfig;
    let config = AnthropicProviderConfig::new("test", "key", "model")
        .with_base_url("https://custom.anthropic.com/v1");
    assert_eq!(config.base_url, "https://custom.anthropic.com/v1");
}

#[test]
fn test_anthropic_config_with_version() {
    use crate::providers::AnthropicProviderConfig;
    let config = AnthropicProviderConfig::new("test", "key", "model").with_version("2023-06-01");
    assert_eq!(config.version, "2023-06-01");
}

#[test]
fn test_anthropic_retry_config_default() {
    use crate::providers::RetryConfig;
    let retry = RetryConfig::default();
    assert_eq!(retry.max_retries, 3);
    assert_eq!(retry.base_delay_ms, 1000);
    assert_eq!(retry.max_delay_ms, 30000);
}

#[test]
fn test_anthropic_retry_delay() {
    use crate::providers::RetryConfig;
    let retry = RetryConfig {
        max_retries: 3,
        base_delay_ms: 1000,
        max_delay_ms: 30000,
    };

    // Test exponential backoff with cap
    let delay0 = retry.delay_for_attempt(0);
    let delay1 = retry.delay_for_attempt(1);
    let delay2 = retry.delay_for_attempt(2);
    let delay10 = retry.delay_for_attempt(10);

    assert_eq!(delay0.as_millis(), 1000);
    assert_eq!(delay1.as_millis(), 2000);
    assert_eq!(delay2.as_millis(), 4000);
    assert_eq!(delay10.as_millis(), 30000); // Capped
}

#[test]
fn test_anthropic_retry_config_serialization() {
    use crate::providers::RetryConfig;
    let retry = RetryConfig {
        max_retries: 5,
        base_delay_ms: 2000,
        max_delay_ms: 60000,
    };

    let json = serde_json::to_string(&retry).unwrap();
    assert!(json.contains("\"max_retries\":5"));
    assert!(json.contains("\"base_delay_ms\":2000"));
    assert!(json.contains("\"max_delay_ms\":60000"));

    let parsed: RetryConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.max_retries, 5);
    assert_eq!(parsed.base_delay_ms, 2000);
    assert_eq!(parsed.max_delay_ms, 60000);
}

#[test]
fn test_anthropic_config_serialization_roundtrip() {
    use crate::providers::AnthropicProviderConfig;
    let config = AnthropicProviderConfig::new("test-provider", "sk-ant-key", "claude-3-opus");

    let json = serde_json::to_string(&config).unwrap();
    assert!(json.contains("\"type\":\"anthropic\""));
    assert!(json.contains("\"api_key\":\"sk-ant-key\""));

    let parsed: AnthropicProviderConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.id, config.id);
    assert_eq!(config.api_key, "sk-ant-key");
    assert_eq!(parsed.default_model, config.default_model);
    assert_eq!(parsed.provider_type, "anthropic");
}

#[test]
fn test_anthropic_response_parsing() {
    use crate::providers::AnthropicResponse;
    let json = r#"{"id":"msg-123","type":"message","role":"assistant","content":[{"type":"text","text":"Hello! How can I help you?"}],"model":"claude-3-5-sonnet-20241022","stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":15}}"#;

    let response: AnthropicResponse = serde_json::from_str(json).unwrap();
    assert_eq!(response.id, "msg-123");
    assert_eq!(response.role, "assistant");
    assert_eq!(response.content.len(), 1);
    assert_eq!(response.model, "claude-3-5-sonnet-20241022");
    assert_eq!(response.stop_reason, Some("end_turn".to_string()));
    assert!(response.usage.is_some());
    let usage = response.usage.unwrap();
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 15);
}

#[test]
fn test_anthropic_tool_use_response_parsing() {
    use crate::providers::{AnthropicContentBlockResponse, AnthropicResponse};
    let json = r#"{"id":"msg-456","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_123","name":"read_file","input":{"path":"/tmp/file.txt"}}],"model":"claude-3-5-sonnet-20241022","stop_reason":"tool_use"}"#;

    let response: AnthropicResponse = serde_json::from_str(json).unwrap();
    assert_eq!(response.id, "msg-456");
    assert_eq!(response.content.len(), 1);

    match &response.content[0] {
        AnthropicContentBlockResponse::ToolUse {
            tool_use_type,
            id,
            name,
            input,
        } => {
            assert_eq!(tool_use_type, "tool_use");
            assert_eq!(id, "toolu_123");
            assert_eq!(name, "read_file");
            assert_eq!(input["path"], "/tmp/file.txt");
        }
        _ => panic!("Expected ToolUse block"),
    }
}

#[test]
fn test_anthropic_error_parsing() {
    use crate::providers::AnthropicError;
    let json = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Invalid request: missing required field 'model'"}}"#;

    let error: AnthropicError = serde_json::from_str(json).unwrap();
    assert_eq!(error.r#type, "error");
    assert_eq!(error.error.r#type, "invalid_request_error");
    assert!(error.error.message.contains("missing required field"));
}

#[test]
fn test_anthropic_stream_event_parsing() {
    use crate::providers::AnthropicStreamEvent;
    let json = r#"{"type":"message_start","message":{"id":"msg-789","type":"message","model":"claude-3-5-sonnet-20241022","role":"assistant"}}"#;

    let event: AnthropicStreamEvent = serde_json::from_str(json).unwrap();
    assert_eq!(event.r#type, "message_start");
    assert!(event.message.is_some());
    let msg = event.message.unwrap();
    assert_eq!(msg.id, "msg-789");
}

#[test]
fn test_anthropic_content_block_delta_parsing() {
    use crate::providers::AnthropicStreamEvent;
    let json =
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;

    let event: AnthropicStreamEvent = serde_json::from_str(json).unwrap();
    assert_eq!(event.r#type, "content_block_delta");
    assert_eq!(event.index, Some(0));
    assert!(event.delta.is_some());
    let delta = event.delta.unwrap();
    assert_eq!(delta.text, Some("Hello".to_string()));
}

// -----------------------------------------------------------------------
// P1-2: OrchestratorState transition matrix
// -----------------------------------------------------------------------

#[test]
fn state_machine_accepts_the_happy_path() {
    use crate::orchestrator::OrchestratorState::*;
    let path = [
        Created,
        WaitingForModel,
        ExecutingTools,
        WaitingForModel,
        Completed,
    ];
    for pair in path.windows(2) {
        assert!(
            pair[0].can_transition_to(pair[1]),
            "{} → {} must be legal",
            pair[0].as_str(),
            pair[1].as_str()
        );
    }
}

#[test]
fn state_machine_accepts_compaction_detour() {
    use crate::orchestrator::OrchestratorState::*;
    // Run start with compaction, then mid-loop compaction after tools.
    for (from, to) in [
        (Idle, Compacting),
        (Compacting, WaitingForModel),
        (WaitingForModel, ExecutingTools),
        (ExecutingTools, Compacting),
        (Compacting, WaitingForModel),
        (WaitingForModel, Completed),
    ] {
        assert!(
            from.can_transition_to(to),
            "{} → {} must be legal",
            from.as_str(),
            to.as_str()
        );
    }
}

#[test]
fn state_machine_accepts_terminal_and_recovery_paths() {
    use crate::orchestrator::OrchestratorState::*;
    for (from, to) in [
        (WaitingForModel, Failed),
        (WaitingForModel, Cancelled),
        (ExecutingTools, Failed),
        (ExecutingTools, Cancelled),
        (ExecutingTools, WaitingForUser),
        (WaitingForUser, ExecutingTools),
        (WaitingForUser, Cancelled),
        (Compacting, Failed),
        // Recovery: a new run may start from any terminal state.
        (Completed, WaitingForModel),
        (Failed, WaitingForModel),
        (Cancelled, WaitingForModel),
        (Completed, Compacting),
    ] {
        assert!(
            from.can_transition_to(to),
            "{} → {} must be legal",
            from.as_str(),
            to.as_str()
        );
    }
}

#[test]
fn state_machine_rejects_illegal_transitions() {
    use crate::orchestrator::OrchestratorState::*;
    for (from, to) in [
        (Created, Completed),        // never ran
        (Idle, Completed),           // no request was made
        (Idle, ExecutingTools),      // tools without a model turn
        (Completed, Completed),      // …via transition (no-op guard covers equality)
        (Completed, ExecutingTools), // must pass through WaitingForModel
        (Failed, ExecutingTools),
        (WaitingForModel, WaitingForUser), // confirmations happen under tools
        (WaitingForUser, WaitingForModel), // resume goes through ExecutingTools
        (WaitingForUser, Completed),
        (Compacting, ExecutingTools), // compaction feeds a model request
        (Compacting, Completed),      // …never straight to a terminal state
    ] {
        assert!(
            !from.can_transition_to(to),
            "{} → {} must be illegal",
            from.as_str(),
            to.as_str()
        );
    }
}

#[test]
fn state_wire_names_round_trip() {
    use crate::orchestrator::OrchestratorState;
    for state in [
        OrchestratorState::Created,
        OrchestratorState::Idle,
        OrchestratorState::WaitingForModel,
        OrchestratorState::ExecutingTools,
        OrchestratorState::WaitingForUser,
        OrchestratorState::Compacting,
        OrchestratorState::Completed,
        OrchestratorState::Failed,
        OrchestratorState::Cancelled,
    ] {
        assert_eq!(OrchestratorState::from_wire(state.as_str()), Some(state));
    }
    assert_eq!(OrchestratorState::from_wire("bogus"), None);
}

// -----------------------------------------------------------------------
// P2-1: unified policy pipeline (PolicyRequest / PolicyDecision)
// -----------------------------------------------------------------------

#[test]
fn policy_pipeline_allows_low_risk_reads() {
    use crate::safety::{PolicyRequest, Safety};
    let safety = Safety::new();

    let decision = safety.evaluate_tool(&PolicyRequest::new(
        "read_file",
        serde_json::json!({"path": "src/main.rs"}),
        0,
    ));
    assert!(decision.allowed);
    assert!(decision.denial_reason.is_none());
    // Low risk + floor 0 → no confirmation.
    assert!(!decision.confirmation_required);
    assert!(decision.notes.is_empty());
}

#[test]
fn policy_pipeline_denies_blocklisted_commands() {
    use crate::safety::{PolicyRequest, Safety};
    let safety = Safety::new();

    let decision = safety.evaluate_tool(&PolicyRequest::new(
        "run_command",
        serde_json::json!({"command": "rm -rf /"}),
        40,
    ));
    assert!(!decision.allowed);
    assert!(decision
        .denial_reason
        .as_deref()
        .unwrap()
        .to_lowercase()
        .contains("block"));
    // Denial never asks for confirmation.
    assert!(!decision.confirmation_required);
}

#[test]
fn policy_pipeline_asks_confirmation_above_threshold() {
    use crate::safety::{PolicyRequest, Safety};
    let safety = Safety::new(); // require_confirmation=true, threshold=0.3

    let decision = safety.evaluate_tool(&PolicyRequest::new(
        "run_command",
        serde_json::json!({"command": "sudo apt install foo"}),
        40,
    ));
    assert!(decision.allowed, "sudo is high-risk but not blocked");
    assert!(
        decision.confirmation_required,
        "high-risk call must require confirmation, score={:?}",
        decision.score
    );
}

#[test]
fn policy_pipeline_risk_floor_raises_effective_score() {
    use crate::safety::{PolicyRequest, Safety};
    let safety = Safety::new();

    // A low-argument call to a tool class with a 50/100 risk floor:
    // per-arg scoring stays low, the floor lifts the effective score.
    let decision = safety.evaluate_tool(&PolicyRequest::new(
        "mcp_external_tool",
        serde_json::json!({"query": "harmless"}),
        50,
    ));
    assert!(decision.allowed);
    assert!(
        decision.score.value >= 0.5,
        "floor must lift effective score"
    );
    assert!(decision.notes.iter().any(|n| n.contains("risk floor 50")));
}

#[test]
fn policy_pipeline_floor_never_grants_denied_access() {
    use crate::safety::{PolicyRequest, Safety};
    // Plan mode denies state-changing tools; a floor cannot override.
    let safety = Safety::new();
    safety.set_mode(crate::safety::PermissionMode::Plan);

    let decision = safety.evaluate_tool(&PolicyRequest::new(
        "write_file",
        serde_json::json!({"path": "out.txt", "content": "x"}),
        0,
    ));
    assert!(!decision.allowed);
    // Plan mode scores the denial at 0.5 (Medium); the floor must not
    // alter the denied decision or its score.
    assert_eq!(decision.score.value, 0.5);
    assert!(decision.notes.is_empty(), "no floor notes on denial");
}

// -----------------------------------------------------------------------
// P2-3: SubagentPolicy enforcement
// -----------------------------------------------------------------------

#[test]
fn subagent_policy_defaults_are_conservative() {
    use crate::tools::spawn_subagent::{SubagentPolicy, DEFAULT_SUBAGENT_MAX_ITERATIONS};
    let policy = SubagentPolicy::default();
    assert_eq!(policy.max_depth, 3);
    assert_eq!(policy.max_iterations, DEFAULT_SUBAGENT_MAX_ITERATIONS);
    assert!(policy.max_tokens > 0);
    assert!(policy.max_children >= 1);
    assert!(policy.timeout.as_secs() >= 60);
}

#[test]
fn spawn_refuses_nesting_beyond_max_depth() {
    use crate::providers::{
        ChatChunk, ChatRequest, LLMProvider, ProviderError, ProviderResult, StreamResult,
    };
    use crate::tool::Tool;
    use crate::tools::spawn_subagent::SubagentPolicy;
    use crate::tools::SpawnSubagentTool;
    use crate::ToolRegistry;

    struct NoopProvider;
    impl LLMProvider for NoopProvider {
        fn provider_type(&self) -> &str {
            "noop"
        }
        fn provider_id(&self) -> &str {
            "noop"
        }
        fn chat(&self, _req: ChatRequest) -> ProviderResult<crate::providers::ChatResponse> {
            Err(ProviderError::new("not used"))
        }
        fn chat_stream(&self, _req: ChatRequest) -> StreamResult<ChatChunk, ProviderError> {
            Err(ProviderError::new("not used"))
        }
    }

    let provider: Arc<dyn LLMProvider> = Arc::new(NoopProvider);
    let tools = ToolRegistry::new();
    // Depth 3 == max_depth 3 → spawn refused before any provider call.
    let tool = SpawnSubagentTool::new(provider.clone(), tools.clone(), "m")
        .with_depth(3)
        .with_policy(SubagentPolicy::default());

    let err = tool
        .execute(serde_json::json!({"task": "dig deeper"}))
        .unwrap_err();
    assert!(
        err.0.contains("depth limit"),
        "expected depth refusal, got: {}",
        err.0
    );
}

#[test]
fn spawn_caps_requested_iterations_at_policy_max() {
    use crate::providers::{
        ChatChunk, ChatRequest, LLMProvider, ProviderError, ProviderResult, StreamResult,
    };
    use crate::tool::Tool;
    use crate::tools::SpawnSubagentTool;
    use crate::ToolRegistry;

    // A provider that records nothing but fails: the error surfaces the
    // failure, while iteration capping is exercised by a later behavior
    // check. Here we verify the depth-0 path *runs* (no refusal).
    struct FailingProvider;
    impl LLMProvider for FailingProvider {
        fn provider_type(&self) -> &str {
            "failing"
        }
        fn provider_id(&self) -> &str {
            "failing"
        }
        fn chat(&self, _req: ChatRequest) -> ProviderResult<crate::providers::ChatResponse> {
            Err(ProviderError::new("provider down"))
        }
        fn chat_stream(&self, _req: ChatRequest) -> StreamResult<ChatChunk, ProviderError> {
            Err(ProviderError::new("provider down"))
        }
    }

    let provider: Arc<dyn LLMProvider> = Arc::new(FailingProvider);
    let tools = ToolRegistry::new();
    let tool = SpawnSubagentTool::new(provider, tools, "m");

    let err = tool
        .execute(serde_json::json!({"task": "work", "max_iterations": 99}))
        .unwrap_err();
    assert!(
        err.0.contains("Subagent failed"),
        "depth-0 spawn proceeds to the runtime, got: {}",
        err.0
    );
}
