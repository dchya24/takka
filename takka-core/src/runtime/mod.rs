//! Agent runtime subsystem (P1-1) + headless runtime protocol (PRD).
//!
//! - [`AgentRuntime`] / [`AgentLoop`] — lifecycle owner around an
//!   orchestrator with a pluggable decision loop.
//! - [`protocol`] — transport-neutral wire types (versioned requests
//!   and events) for headless frontends.
//!
//! Engine + transport land with the decoupling commits that follow.

pub mod engine;
pub mod protocol;
pub mod transport;

// `AgentRuntime` / `AgentLoop` split (P1-1).
//
// Prinsip dari rencana pematangan: **Runtime mengelola lifecycle,
// loop mengelola decision.**
//
// - [`AgentRuntime`] — lifecycle: sessions, event envelope,
//   checkpoints, cancel, pause/resume, status. Frontends (CLI, daemon
//   JSONL, TUI, kanban) memanggil runtime, bukan orchestrator.
// - [`AgentLoop`] — decision: bagaimana model didorong menuju jawaban.
//   [`StandardLoop`] mendelegasikan ke loop LLM→tool→observation yang
//   sudah teruji; varian lain (`PlanningLoop`, `InteractiveLoop`, …)
//   tinggal di-plug tanpa conditional di runtime.
//
// ```text
// Frontend ──► AgentRuntime ──► AgentLoop ──► Orchestrator internals
//                  │                             (tools/safety/memory)
//                  └── events/checkpoints/cancel/pause
// ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::attachments::Attachment;
use crate::orchestrator::{Orchestrator, OrchestratorState};
use crate::providers::LLMProvider;
use crate::safety::PermissionMode;
use crate::tool_registry::ToolRegistry;
use crate::AgenticError;

/// One decision-loop implementation.
///
/// Implementations receive an orchestrator already wired with tools,
/// safety, memory, and events; they decide how to drive it toward an
/// answer for `input` and must return the final assistant text.
pub trait AgentLoop: Send + Sync {
    /// Human-readable name, surfaced in status metadata.
    fn name(&self) -> &str;

    /// Drive the orchestrator until `input` is answered.
    ///
    /// Contract: implementors must NOT wrap the call in the session
    /// lifecycle envelope — the runtime owns it. The standard
    /// implementation therefore calls the orchestrator's inner loop,
    /// not [`Orchestrator::run`].
    fn run(
        &self,
        orchestrator: &Orchestrator,
        input: &str,
        attachments: Vec<Attachment>,
    ) -> Result<String, AgenticError>;

    /// Streaming variant: forward assistant text deltas through
    /// `on_chunk` while the loop runs.
    fn run_stream<'a>(
        &'a self,
        orchestrator: &'a Orchestrator,
        input: &'a str,
        attachments: Vec<Attachment>,
        on_chunk: &'a mut (dyn FnMut(String) + Send),
    ) -> Pin<Box<dyn Future<Output = Result<String, AgenticError>> + Send + 'a>>;
}

/// The proven synchronous LLM→tool→observation loop.
///
/// Delegates to the orchestrator's inner loop (no lifecycle envelope —
/// that belongs to [`AgentRuntime`]).
pub struct StandardLoop;

impl AgentLoop for StandardLoop {
    fn name(&self) -> &str {
        "standard"
    }

    fn run(
        &self,
        orchestrator: &Orchestrator,
        input: &str,
        attachments: Vec<Attachment>,
    ) -> Result<String, AgenticError> {
        orchestrator.run_with_attachments_inner(input, attachments)
    }

    fn run_stream<'a>(
        &'a self,
        orchestrator: &'a Orchestrator,
        input: &'a str,
        attachments: Vec<Attachment>,
        on_chunk: &'a mut (dyn FnMut(String) + Send),
    ) -> Pin<Box<dyn Future<Output = Result<String, AgenticError>> + Send + 'a>> {
        Box::pin(async move {
            orchestrator
                .run_stream_with_attachments_inner(input, attachments, &mut *on_chunk)
                .await
        })
    }
}

/// Snapshot of runtime state for frontends (kanban, daemon status).
#[derive(Debug, Clone)]
pub struct RuntimeStatus {
    /// Tracked checkpoint session, if persistence is attached.
    pub session_id: Option<String>,
    /// Current lifecycle state (P1-2 machine).
    pub state: OrchestratorState,
    /// Whether the loop is parked by [`AgentRuntime::pause`].
    pub paused: bool,
    /// Configured model name.
    pub model: String,
    /// Which [`AgentLoop`] implementation serves runs.
    pub loop_name: String,
}

/// Lifecycle owner around an orchestrator + a pluggable decision loop.
///
/// Every entry point drives the same envelope: `SessionStarted` →
/// loop → terminal state + `SessionCompleted`/`SessionFailed`, with
/// checkpoints written by the orchestrator when a store is attached.
pub struct AgentRuntime {
    orchestrator: Arc<Orchestrator>,
    agent_loop: Box<dyn AgentLoop>,
}

impl AgentRuntime {
    /// Wrap an orchestrator with the [`StandardLoop`].
    pub fn new(orchestrator: Arc<Orchestrator>) -> Self {
        Self {
            orchestrator,
            agent_loop: Box::new(StandardLoop),
        }
    }

    /// Wrap an orchestrator with a custom decision loop.
    pub fn with_loop(orchestrator: Arc<Orchestrator>, agent_loop: Box<dyn AgentLoop>) -> Self {
        Self {
            orchestrator,
            agent_loop,
        }
    }

    /// Shared handle to the wrapped orchestrator (for direct
    /// configuration: model, safety, session store, event handlers).
    pub fn orchestrator(&self) -> &Arc<Orchestrator> {
        &self.orchestrator
    }

    /// Run a turn to completion.
    pub fn run(&self, input: &str) -> Result<String, AgenticError> {
        self.run_with_attachments(input, Vec::new())
    }

    /// Run a turn to completion with attachments.
    pub fn run_with_attachments(
        &self,
        input: &str,
        attachments: Vec<Attachment>,
    ) -> Result<String, AgenticError> {
        self.orchestrator.session_begin();
        let result = self.agent_loop.run(&self.orchestrator, input, attachments);
        self.orchestrator.session_terminal(&result);
        result
    }

    /// Run a turn with streaming assistant deltas.
    pub async fn run_stream(
        &self,
        input: &str,
        mut on_chunk: impl FnMut(String) + Send,
    ) -> Result<String, AgenticError> {
        self.orchestrator.session_begin();
        let result = self
            .agent_loop
            .run_stream(&self.orchestrator, input, Vec::new(), &mut on_chunk)
            .await;
        self.orchestrator.session_terminal(&result);
        result
    }

    /// Request cancellation: the loop aborts at the next iteration /
    /// tool boundary with `AgenticError::Cancelled`.
    pub fn cancel(&self) {
        self.orchestrator.cancel();
    }

    /// Park the loop at the next iteration boundary. Blocking tool
    /// calls finish first.
    pub fn pause(&self) {
        self.orchestrator.pause();
    }

    /// Release a parked loop.
    pub fn resume(&self) {
        self.orchestrator.resume();
    }

    /// Observe runtime status without touching the loop.
    pub fn status(&self) -> RuntimeStatus {
        let orch = &self.orchestrator;
        RuntimeStatus {
            session_id: orch.session_id(),
            state: orch.get_state(),
            paused: orch.is_paused(),
            model: orch.model().to_string(),
            loop_name: self.agent_loop.name().to_string(),
        }
    }

    /// Spawn an isolated child runtime for a subagent task: fresh
    /// conversation, inherited toolset, bounded iterations, shared
    /// cancel flag, custom system prompt. The child takes the same
    /// `AgentRuntime` path as top-level runs (P1-1: "Subagent memakai
    /// `AgentRuntime::spawn()` yang sama").
    pub fn spawn(
        provider: Arc<dyn LLMProvider>,
        tools: ToolRegistry,
        config: ChildSpawn,
    ) -> Result<String, AgenticError> {
        let mut child = Orchestrator::new(provider, tools);
        child.set_model(config.model);
        child.set_max_iterations(config.max_iterations);
        child.set_system_prompt(config.system_prompt);
        child.set_permission_mode(config.mode);
        if let Some(flag) = config.parent_cancel {
            child.set_cancel_handle(flag);
        }
        if let Some(budget) = config.memory_token_budget {
            child.set_memory_token_budget(budget);
        }
        AgentRuntime::new(Arc::new(child)).run(&config.task)
    }
}

/// Wiring for a child runtime spawned by [`AgentRuntime::spawn`].
pub struct ChildSpawn {
    pub task: String,
    pub model: String,
    pub system_prompt: String,
    pub mode: PermissionMode,
    pub max_iterations: u32,
    pub parent_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Child context token budget (`Memory::max_tokens`). `None` keeps
    /// the default.
    pub memory_token_budget: Option<u32>,
}

impl ChildSpawn {
    pub fn new(
        task: impl Into<String>,
        model: impl Into<String>,
        system_prompt: impl Into<String>,
        mode: PermissionMode,
        max_iterations: u32,
    ) -> Self {
        Self {
            task: task.into(),
            model: model.into(),
            system_prompt: system_prompt.into(),
            mode,
            max_iterations,
            parent_cancel: None,
            memory_token_budget: None,
        }
    }

    /// Bound the child's context window (subagent policy, P2-3).
    pub fn with_memory_token_budget(mut self, max_tokens: u32) -> Self {
        self.memory_token_budget = Some(max_tokens);
        self
    }

    /// Share the parent's cancel flag so Ctrl+C kills children too.
    pub fn with_parent_cancel(mut self, flag: Option<Arc<std::sync::atomic::AtomicBool>>) -> Self {
        self.parent_cancel = flag;
        self
    }
}
