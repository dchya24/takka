//! `spawn_subagent` tool.
//!
//! Spawns a subagent that runs the **same** agentic loop but with an
//! **isolated** conversation history. The parent only sees the final text
//! the subagent returns. This is the architecture doc's "subagent" pattern:
//! complex tasks are decomposed into subtasks whose context doesn't pollute
//! the parent.
//!
//! What's shared with the parent:
//! - The `ToolRegistry` (so subagents have the same tools).
//! - The `LLMProvider` (same network connection, same auth).
//! - Safety policy (blocklist, sandbox) is enforced via a fresh `Safety`
//!   inside the new orchestrator; the parent's permission mode is copied.
//!
//! What's isolated:
//! - The conversation history (fresh `Memory`).
//! - Tool result truncation / autocompact / cancel state.
//!
//! Subagents have a tighter `max_iterations` than the main agent (default
//! 12) to bound runaway token usage.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use crate::providers::LLMProvider;
use crate::safety::PermissionMode;
use crate::tool::{
    Concurrency, Mutability, SideEffects, Tool, ToolError, ToolMetadata, ToolParam, ToolResult,
    ToolSchema,
};
use crate::tool_registry::ToolRegistry;

/// Default cap on subagent iterations. Tighter than the main agent's loop
/// because subagents handle scoped subtasks.
pub const DEFAULT_SUBAGENT_MAX_ITERATIONS: u32 = 12;

/// Execution limits for a spawned subagent (P2-3). Every limit is a
/// separate guard so a runaway child can only burn one resource class.
#[derive(Debug, Clone)]
pub struct SubagentPolicy {
    /// Maximum spawn nesting depth. Depth 0 = top-level agent spawns;
    /// a child at `max_depth` is refused.
    pub max_depth: u32,
    /// Hard cap on child loop iterations (the model may still request
    /// fewer via the `max_iterations` argument — never more).
    pub max_iterations: u32,
    /// Child context token budget (memory `max_tokens`), bounding how
    /// much conversation a child may accumulate.
    pub max_tokens: u32,
    /// Maximum children this tool may run *concurrently*.
    pub max_children: usize,
    /// Wall-clock timeout for one child run.
    pub timeout: std::time::Duration,
}

impl Default for SubagentPolicy {
    fn default() -> Self {
        Self {
            max_depth: 3,
            max_iterations: DEFAULT_SUBAGENT_MAX_ITERATIONS,
            max_tokens: 64_000,
            max_children: 4,
            timeout: std::time::Duration::from_secs(600),
        }
    }
}

pub struct SpawnSubagentTool {
    provider: Arc<dyn LLMProvider>,
    tools: ToolRegistry,
    /// Model used by spawned subagents (defaults to the parent's model).
    model: String,
    /// Permission mode applied to subagents at construction. Mirrors the
    /// parent's mode at the moment the tool was registered.
    mode: PermissionMode,
    /// Cancel flag shared with the parent so a parent cancel kills the
    /// subagent too. Matches the architecture doc's "linked abort signal".
    parent_cancel: Option<Arc<AtomicBool>>,
    max_iterations: u32,
    /// Current spawn depth: 0 when registered on a top-level agent.
    depth: u32,
    /// Execution limits (P2-3).
    policy: SubagentPolicy,
    /// Concurrently running children (guard for `policy.max_children`).
    active_children: Arc<AtomicUsize>,
}

impl SpawnSubagentTool {
    pub fn new(
        provider: Arc<dyn LLMProvider>,
        tools: ToolRegistry,
        model: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            tools,
            model: model.into(),
            mode: PermissionMode::Default,
            parent_cancel: None,
            max_iterations: DEFAULT_SUBAGENT_MAX_ITERATIONS,
            depth: 0,
            policy: SubagentPolicy::default(),
            active_children: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Spawn depth of this tool instance (0 = top-level registry).
    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }

    /// Replace the default execution policy.
    pub fn with_policy(mut self, policy: SubagentPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_mode(mut self, mode: PermissionMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.parent_cancel = Some(cancel);
        self
    }

    pub fn with_max_iterations(mut self, max: u32) -> Self {
        self.max_iterations = max.max(1);
        self
    }
}

const SUBAGENT_SYSTEM_PROMPT: &str = r#"You are a focused subagent spawned by a parent AI coding agent to handle a scoped subtask.

You have the same tools as the parent (read_file, edit_file, list_files, etc.) but a FRESH conversation history. The parent does not see your tool calls; it only sees the final text you return.

Rules:
1. Complete the task you were given. Don't try to coordinate with the parent.
2. Be concise in your final answer. The parent uses your text as a summary.
3. Don't ask clarifying questions. If the task is ambiguous, make a reasonable assumption and note it.
4. Stop as soon as you have an answer. Don't keep using tools speculatively.
"#;

impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        "spawn_subagent"
    }

    fn description(&self) -> &str {
        "Spawn a subagent with an isolated conversation history to handle a focused subtask. \
         Use this when a step would pollute the main context (large file exploration, multi-step \
         refactor, etc.). The parent only sees the subagent's final text answer."
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "task".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some(
                    "The subtask description. Be specific and self-contained: the subagent has no memory of the parent conversation.".to_string(),
                ),
                default: None,
            },
        );
        params.insert(
            "max_iterations".to_string(),
            ToolParam {
                param_type: "number".to_string(),
                description: Some(
                    "Optional cap on subagent loop iterations. Defaults to a small value."
                        .to_string(),
                ),
                default: Some(serde_json::json!(DEFAULT_SUBAGENT_MAX_ITERATIONS)),
            },
        );

        ToolSchema {
            name: "spawn_subagent".to_string(),
            description: "Spawn a subagent with isolated context for a focused subtask."
                .to_string(),
            parameters: params,
            required: vec!["task".to_string()],
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let args_obj = args
            .as_object()
            .ok_or_else(|| ToolError::new("Invalid arguments: expected object"))?;

        let task = args_obj
            .get("task")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: task"))?;

        if task.trim().is_empty() {
            return Err(ToolError::new("task must not be empty"));
        }

        let requested = args_obj
            .get("max_iterations")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(self.max_iterations);
        // Policy caps the requested budget (P2-3): the model may ask
        // for fewer iterations, never more.
        let max_iter = requested.min(self.policy.max_iterations);

        // Depth guard: refuse spawns beyond the policy's nesting limit.
        if self.depth >= self.policy.max_depth {
            return Err(ToolError::new(format!(
                "Subagent depth limit reached ({}/{}): nested spawn refused",
                self.depth, self.policy.max_depth
            )));
        }

        // Concurrency guard: bounded children per tool instance.
        if self.active_children.load(Ordering::SeqCst) >= self.policy.max_children {
            return Err(ToolError::new(format!(
                "Subagent concurrency limit reached ({}/{}): spawn refused",
                self.active_children.load(Ordering::SeqCst),
                self.policy.max_children
            )));
        }
        self.active_children.fetch_add(1, Ordering::SeqCst);

        // Child registry: rebuild the spawn tool one level deeper so
        // the depth guard compounds down the tree.
        let child_tools = self.tools.clone();
        child_tools.unregister("spawn_subagent");
        child_tools.register(Box::new(
            SpawnSubagentTool::new(
                self.provider.clone(),
                child_tools.clone(),
                self.model.clone(),
            )
            .with_depth(self.depth + 1)
            .with_policy(self.policy.clone()),
        ) as Box<dyn crate::tool::Tool + Send + Sync>);

        // Child cancel flag: shared handle so the timeout (and a
        // mirrored parent cancel) can abort the child at its next
        // loop/tool boundary.
        let child_cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        if let Some(parent) = &self.parent_cancel {
            let parent = parent.clone();
            let child_cancel = child_cancel.clone();
            let done = done.clone();
            std::thread::spawn(move || loop {
                if parent.load(Ordering::SeqCst) {
                    child_cancel.store(true, Ordering::SeqCst);
                    break;
                }
                if done.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            });
        }

        // Build a fresh child runtime (P1-1): same AgentRuntime path as
        // top-level runs — lifecycle envelope, pause/cancel plumbing —
        // with a fresh conversation, inherited toolset, and bounded
        // iterations. We use the synchronous `run()` so we don't need a
        // Tokio runtime context; spawn_subagent itself runs inside
        // Tool::execute, which the orchestrator may call from a
        // spawn_blocking thread.
        let spawn_config = crate::runtime::ChildSpawn::new(
            task,
            self.model.clone(),
            SUBAGENT_SYSTEM_PROMPT,
            self.mode,
            max_iter,
        )
        .with_parent_cancel(Some(child_cancel.clone()))
        .with_memory_token_budget(self.policy.max_tokens);

        // Timeout guard: run the child on its own thread and impose the
        // policy's wall-clock limit. On timeout the child's cancel flag
        // flips so the abandoned thread unwinds at its next boundary.
        let (tx, rx) = mpsc::channel();
        let provider = self.provider.clone();
        let spawn_tools = child_tools;

        let worker_cancel = child_cancel.clone();
        std::thread::spawn(move || {
            let result = crate::runtime::AgentRuntime::spawn(provider, spawn_tools, spawn_config);
            let _ = tx.send(result);
            done.store(true, Ordering::SeqCst);
            let _ = worker_cancel;
        });

        let answer = match rx.recv_timeout(self.policy.timeout) {
            Ok(result) => result.map_err(|e| {
                let preview: String = task.chars().take(80).collect();
                ToolError::new(format!("Subagent failed: {} (task='{}')", e, preview))
            })?,
            Err(_) => {
                child_cancel.store(true, Ordering::SeqCst);
                return Err(ToolError::new(format!(
                    "Subagent timed out after {:?} (task='{}')",
                    self.policy.timeout,
                    task.chars().take(80).collect::<String>()
                )));
            }
        };
        self.active_children.fetch_sub(1, Ordering::SeqCst);

        Ok(serde_json::json!({
            "success": true,
            "answer": answer,
            "max_iterations": max_iter,
        }))
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            mutability: Mutability::Mutating,
            concurrency: Concurrency::Exclusive,
            idempotent: false,
            risk: 20,
            side_effects: SideEffects::Shell,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{
        ChatChunk, ChatMessageResponse, ChatRequest, ChatResponse, LLMProvider, ProviderError,
        ProviderResult, StreamResult,
    };
    use std::sync::Mutex;

    /// Minimal fake provider for subagent testing.
    struct ScriptedProvider {
        responses: Mutex<Vec<ChatResponse>>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<ChatResponse>) -> Self {
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
                return Err(ProviderError::new("no scripted response"));
            }
            Ok(q.remove(0))
        }
        fn chat_stream(&self, _req: ChatRequest) -> StreamResult<ChatChunk, ProviderError> {
            Err(ProviderError::new("streaming not supported in test"))
        }
    }

    fn text_response(s: &str) -> ChatResponse {
        ChatResponse {
            id: "test".into(),
            model: "fake".into(),
            message: ChatMessageResponse {
                role: "assistant".into(),
                content: Some(s.into()),
                tool_calls: vec![],
            },
            finish_reason: Some("stop".into()),
            usage: None,
        }
    }

    #[test]
    fn rejects_empty_task() {
        let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![]));
        let tool = SpawnSubagentTool::new(provider, ToolRegistry::new(), "fake");
        let err = tool
            .execute(serde_json::json!({"task": "   "}))
            .expect_err("empty task should fail");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn returns_subagent_answer_to_parent() {
        let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![text_response(
            "42 is the answer",
        )]));
        let tool = SpawnSubagentTool::new(provider, ToolRegistry::new(), "fake");
        let result = tool
            .execute(serde_json::json!({"task": "what is the meaning of life?"}))
            .expect("subagent should succeed");
        assert_eq!(result["success"], true);
        assert_eq!(result["answer"], "42 is the answer");
    }

    #[test]
    fn respects_max_iterations_argument() {
        let provider: Arc<dyn LLMProvider> =
            Arc::new(ScriptedProvider::new(vec![text_response("done")]));
        let tool = SpawnSubagentTool::new(provider, ToolRegistry::new(), "fake");
        let result = tool
            .execute(serde_json::json!({"task": "x", "max_iterations": 1}))
            .expect("should succeed");
        assert_eq!(result["max_iterations"], 1);
    }
}
