//! Test support: scripted provider + small helpers for orchestrator
//! integration tests.
#![allow(dead_code)]

use takka_core::providers::{
    ChatChunk, ChatMessageResponse, ChatRequest, ChatResponse, LLMProvider, ProviderError,
    ProviderResult, StreamResult, ToolCallDelta, ToolCallFunction, ToolCallResponse,
};
use std::sync::Mutex;

/// Provider that returns scripted `ChatResponse`s in order, one per
/// `chat()` call. When the queue is empty, returns an error so a
/// runaway loop fails loudly instead of hanging.
pub struct ScriptedProvider {
    responses: Mutex<Vec<ChatResponse>>,
}

impl ScriptedProvider {
    pub fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

impl LLMProvider for ScriptedProvider {
    fn provider_type(&self) -> &str {
        "fake"
    }
    fn provider_id(&self) -> &str {
        "fake"
    }
    fn chat(&self, _req: ChatRequest) -> ProviderResult<ChatResponse> {
        let mut q = self.responses.lock().unwrap();
        if q.is_empty() {
            return Err(ProviderError::new(
                "ScriptedProvider: no scripted response left",
            ));
        }
        Ok(q.remove(0))
    }
    fn chat_stream(&self, _req: ChatRequest) -> StreamResult<ChatChunk, ProviderError> {
        Err(ProviderError::new("streaming not supported in test"))
    }
}

/// Build a single `ToolCallResponse` for a tool call.
pub fn tool_call(id: &str, name: &str, args: &serde_json::Value) -> ToolCallResponse {
    ToolCallResponse {
        id: id.to_string(),
        call_type: "function".to_string(),
        function: ToolCallFunction {
            name: name.to_string(),
            arguments: args.to_string(),
        },
    }
}

/// Build a `ChatResponse` containing exactly one tool call. The
/// assistant's `content` is empty (matches what most providers send
/// alongside a tool-call turn).
pub fn tool_call_response(id: &str, name: &str, args: &serde_json::Value) -> ChatResponse {
    ChatResponse {
        id: format!("resp-{}", id),
        model: "gpt-4o-mini".to_string(),
        message: ChatMessageResponse {
            role: "assistant".to_string(),
            content: None,
            tool_calls: vec![tool_call(id, name, args)],
        },
        finish_reason: Some("tool_calls".to_string()),
        usage: None,
    }
}

/// Build a `ChatResponse` containing multiple tool calls in one turn.
pub fn multi_tool_call_response(calls: Vec<ToolCallResponse>) -> ChatResponse {
    ChatResponse {
        id: "resp-multi".to_string(),
        model: "gpt-4o-mini".to_string(),
        message: ChatMessageResponse {
            role: "assistant".to_string(),
            content: None,
            tool_calls: calls,
        },
        finish_reason: Some("tool_calls".to_string()),
        usage: None,
    }
}

/// Build a final-text `ChatResponse` (no tool calls). The orchestrator
/// will treat this as the loop terminator.
pub fn text_response(s: &str) -> ChatResponse {
    ChatResponse {
        id: "resp-text".to_string(),
        model: "gpt-4o-mini".to_string(),
        message: ChatMessageResponse {
            role: "assistant".to_string(),
            content: Some(s.to_string()),
            tool_calls: vec![],
        },
        finish_reason: Some("stop".to_string()),
        usage: None,
    }
}

/// Provider that returns scripted `ChatResponse`s in order, one per
/// `chat()` call, AND records how many tools each request offered.
/// Used by the graceful-finalization tests to assert that tools are
/// stripped on the finalization turn.
pub struct RecordingProvider {
    responses: Mutex<Vec<ChatResponse>>,
    /// One entry per `chat()` call: the tool count offered in that request.
    pub tool_counts: Mutex<Vec<usize>>,
}

impl RecordingProvider {
    pub fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            tool_counts: Mutex::new(Vec::new()),
        }
    }
}

impl LLMProvider for RecordingProvider {
    fn provider_type(&self) -> &str {
        "recording"
    }
    fn provider_id(&self) -> &str {
        "recording"
    }
    fn chat(&self, req: ChatRequest) -> ProviderResult<ChatResponse> {
        self.tool_counts.lock().unwrap().push(req.tools.len());
        let mut q = self.responses.lock().unwrap();
        if q.is_empty() {
            return Err(ProviderError::new(
                "RecordingProvider: no scripted response left",
            ));
        }
        Ok(q.remove(0))
    }
    fn chat_stream(&self, _req: ChatRequest) -> StreamResult<ChatChunk, ProviderError> {
        Err(ProviderError::new(
            "streaming not supported in RecordingProvider",
        ))
    }
}

/// Provider that returns scripted `ChatResponse`s in order, one per
/// `chat()` call, AND captures every incoming `ChatRequest`. Used by the
/// steering tests to assert what context the orchestrator actually sent
/// to the provider on each turn.
pub struct CapturingScriptedProvider {
    responses: Mutex<Vec<ChatResponse>>,
    /// One entry per `chat()` call, in call order.
    pub requests: Mutex<Vec<ChatRequest>>,
}

impl CapturingScriptedProvider {
    pub fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl LLMProvider for CapturingScriptedProvider {
    fn provider_type(&self) -> &str {
        "capturing"
    }
    fn provider_id(&self) -> &str {
        "capturing"
    }
    fn chat(&self, req: ChatRequest) -> ProviderResult<ChatResponse> {
        self.requests.lock().unwrap().push(req);
        let mut q = self.responses.lock().unwrap();
        if q.is_empty() {
            return Err(ProviderError::new(
                "CapturingScriptedProvider: no scripted response left",
            ));
        }
        Ok(q.remove(0))
    }
    fn chat_stream(&self, _req: ChatRequest) -> StreamResult<ChatChunk, ProviderError> {
        Err(ProviderError::new(
            "streaming not supported in CapturingScriptedProvider",
        ))
    }
}

/// Streaming counterpart of [`CapturingScriptedProvider`]: scripts
/// `ChatResponse`s for `chat_stream()` calls (same single-chunk encoding
/// as [`StreamingScriptedProvider`]) AND records every incoming
/// `ChatRequest`. Used by the runtime steering tests to assert what the
/// engine's run actually sent to the provider on each turn.
pub struct CapturingStreamingScriptedProvider {
    responses: Mutex<Vec<ChatResponse>>,
    /// One entry per `chat_stream()` call, in call order.
    pub requests: Mutex<Vec<ChatRequest>>,
}

impl CapturingStreamingScriptedProvider {
    pub fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl LLMProvider for CapturingStreamingScriptedProvider {
    fn provider_type(&self) -> &str {
        "capturing-stream"
    }
    fn provider_id(&self) -> &str {
        "capturing-stream"
    }
    fn chat(&self, _req: ChatRequest) -> ProviderResult<ChatResponse> {
        Err(ProviderError::new(
            "streaming only — use chat_stream for CapturingStreamingScriptedProvider",
        ))
    }
    fn chat_stream(&self, req: ChatRequest) -> StreamResult<ChatChunk, ProviderError> {
        self.requests.lock().unwrap().push(req);
        let mut q = self.responses.lock().unwrap();
        if q.is_empty() {
            return Err(ProviderError::new(
                "CapturingStreamingScriptedProvider: no scripted response left",
            ));
        }
        let resp = q.remove(0);

        // Single chunk carrying either text delta or full tool calls
        // (same encoding as StreamingScriptedProvider).
        let mut chunks = Vec::new();
        if resp.message.tool_calls.is_empty() {
            chunks.push(Ok(ChatChunk {
                id: resp.id.clone(),
                delta: resp.message.content.clone().unwrap_or_default(),
                finish_reason: resp.finish_reason.clone(),
                tool_calls: vec![],
                usage: resp.usage.clone(),
            }));
        } else {
            let tool_calls: Vec<ToolCallDelta> = resp
                .message
                .tool_calls
                .iter()
                .map(|tc| ToolCallDelta {
                    index: 0,
                    id: Some(tc.id.clone()),
                    function_name: Some(tc.function.name.clone()),
                    function_arguments: Some(tc.function.arguments.clone()),
                })
                .collect();
            chunks.push(Ok(ChatChunk {
                id: resp.id.clone(),
                delta: resp.message.content.clone().unwrap_or_default(),
                finish_reason: resp.finish_reason.clone(),
                tool_calls,
                usage: resp.usage.clone(),
            }));
        }

        Ok(Box::pin(futures::stream::iter(chunks)))
    }
}

/// Provider that streams scripted `ChatResponse`s in order, one per
/// `chat_stream()` call. Text content is streamed as a single delta;
/// tool calls are delivered as one chunk carrying the full arguments.
/// When the queue is empty, returns an error so a runaway loop fails
/// loudly instead of hanging.
pub struct StreamingScriptedProvider {
    responses: Mutex<Vec<ChatResponse>>,
}

impl StreamingScriptedProvider {
    pub fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

impl LLMProvider for StreamingScriptedProvider {
    fn provider_type(&self) -> &str {
        "fake-stream"
    }
    fn provider_id(&self) -> &str {
        "fake-stream"
    }
    fn chat(&self, _req: ChatRequest) -> ProviderResult<ChatResponse> {
        Err(ProviderError::new(
            "streaming only — use chat_stream for StreamingScriptedProvider",
        ))
    }
    fn chat_stream(&self, _req: ChatRequest) -> StreamResult<ChatChunk, ProviderError> {
        let mut q = self.responses.lock().unwrap();
        if q.is_empty() {
            return Err(ProviderError::new(
                "StreamingScriptedProvider: no scripted response left",
            ));
        }
        let resp = q.remove(0);

        // Single chunk carrying either text delta or full tool calls.
        let mut chunks = Vec::new();
        if resp.message.tool_calls.is_empty() {
            chunks.push(Ok(ChatChunk {
                id: resp.id.clone(),
                delta: resp.message.content.clone().unwrap_or_default(),
                finish_reason: resp.finish_reason.clone(),
                tool_calls: vec![],
                usage: resp.usage.clone(),
            }));
        } else {
            let tool_calls: Vec<ToolCallDelta> = resp
                .message
                .tool_calls
                .iter()
                .map(|tc| ToolCallDelta {
                    index: 0,
                    id: Some(tc.id.clone()),
                    function_name: Some(tc.function.name.clone()),
                    function_arguments: Some(tc.function.arguments.clone()),
                })
                .collect();
            chunks.push(Ok(ChatChunk {
                id: resp.id.clone(),
                delta: resp.message.content.clone().unwrap_or_default(),
                finish_reason: resp.finish_reason.clone(),
                tool_calls,
                usage: resp.usage.clone(),
            }));
        }

        let stream = futures::stream::iter(chunks);
        Ok(Box::pin(stream))
    }
}

/// Per-test temp dir under the system tmp. Avoids collisions with
/// parallel tests via timestamp + pid.
pub fn tempdir() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("agentic-orch-it-{}-{}", pid, nanos));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Test-only tool that sleeps for a configurable duration before
/// returning a trivial JSON value. Marked `is_read_only = true` so the
/// orchestrator's batched executor schedules it alongside other reads.
///
/// Used by the concurrent-batching test to verify that N parallel
/// invocations finish in roughly one slot's wall-time, not N × slot.
pub struct SlowReadTool {
    name: String,
    sleep: std::time::Duration,
}

impl SlowReadTool {
    pub fn new(name: impl Into<String>, sleep: std::time::Duration) -> Self {
        Self {
            name: name.into(),
            sleep,
        }
    }
}

impl takka_core::tool::Tool for SlowReadTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "slow read-only test tool"
    }
    fn schema(&self) -> takka_core::tool::ToolSchema {
        takka_core::tool::ToolSchema::new(self.name.clone(), "slow read-only test tool")
    }
    fn execute(
        &self,
        _args: serde_json::Value,
    ) -> takka_core::tool::ToolResult<serde_json::Value> {
        std::thread::sleep(self.sleep);
        Ok(serde_json::json!({"ok": true, "name": self.name}))
    }
    fn metadata(&self) -> takka_core::tool::ToolMetadata {
        takka_core::tool::ToolMetadata::read_only()
    }
}

/// Read-only test tool that tracks peak concurrent executions via shared
/// atomics. A deterministic concurrency proof: after a run, `max` holds the
/// largest number of simultaneously-executing copies. No wall-clock asserts,
/// so it can't flake under parallel test load.
pub struct ConcurrentReadTool {
    name: String,
    sleep: std::time::Duration,
    pub current: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub max: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl ConcurrentReadTool {
    pub fn new(name: &str, sleep: std::time::Duration) -> Self {
        Self {
            name: name.to_string(),
            sleep,
            current: Default::default(),
            max: Default::default(),
        }
    }
}

impl takka_core::tool::Tool for ConcurrentReadTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "concurrency-tracking read-only test tool"
    }
    fn schema(&self) -> takka_core::tool::ToolSchema {
        takka_core::tool::ToolSchema::new(self.name.clone(), "concurrency test")
    }
    fn execute(
        &self,
        _args: serde_json::Value,
    ) -> takka_core::tool::ToolResult<serde_json::Value> {
        use std::sync::atomic::Ordering;
        let cur = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(cur, Ordering::SeqCst);
        std::thread::sleep(self.sleep);
        self.current.fetch_sub(1, Ordering::SeqCst);
        Ok(serde_json::json!({ "ok": true, "name": self.name }))
    }
    fn metadata(&self) -> takka_core::tool::ToolMetadata {
        takka_core::tool::ToolMetadata::read_only()
    }
}
