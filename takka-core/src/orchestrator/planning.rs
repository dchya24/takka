//! Plan-then-execute wiring (O-05a, opt-in).
//!
//! With plan mode enabled, a fresh run spends ONE extra provider call
//! (via [`PlannerAgent`]) to turn the goal into a step list. The steps
//! are dumped into the session todo store, which drives everything
//! downstream:
//! - `Event::TodoChanged` surfaces the checklist to the UI;
//! - the model updates progress itself via the existing `todowrite`
//!   tool (option B: no heuristic step tracking);
//! - [`Orchestrator::plan_progress_message`] prepends done/current
//!   state to every request so the loop stops re-deriving the plan;
//! - the checkpoint persists plan + todos (`session.plan` /
//!   `session.task_state`) and resume restores both.
//!
//! Planner errors are logged and the run continues reactively — a
//! failed plan call must never break the turn.

use crate::events::Event;
use crate::planner::{PlannerAgent, PlannerConfig};
use crate::providers::ChatMessageRequest;
use crate::tools::todowrite::{self, TodoItem, TodoPriority, TodoStatus};
use crate::sync;

use super::Orchestrator;

impl Orchestrator {
    /// Create the session plan for `goal`: one planner call, steps
    /// dumped into the todo store, plan serialized onto the
    /// orchestrator for checkpointing.
    pub(super) fn create_session_plan(&self, goal: &str) {
        let planner = PlannerAgent::new(self.provider.clone()).with_config(PlannerConfig {
            require_approval: false,
            ..PlannerConfig::default()
        });
        // Forward planner lifecycle events (plan created, etc.) onto the
        // orchestrator's emitter, same as the runtime engine does.
        {
            let events = self.events.clone();
            planner.on(move |event| events.emit(event));
        }
        match planner.create_plan(goal, &self.tools) {
            Ok(plan) => {
                *sync::lock(&self.plan) = serde_json::to_value(&plan).ok();
                let todos: Vec<TodoItem> = plan
                    .steps
                    .iter()
                    .map(|step| TodoItem {
                        content: step.description.clone(),
                        status: TodoStatus::Pending,
                        priority: TodoPriority::Medium,
                    })
                    .collect();
                if !todos.is_empty() {
                    {
                        let mut list = sync::lock(&todowrite::TODO_LIST);
                        *list = todos.clone();
                    }
                    self.events.emit(Event::TodoChanged { todos });
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "plan-mode: plan creation failed; continuing reactively");
            }
        }
    }

    /// Transient trailing message carrying the plan's done/current state
    /// (not saved to memory). `None` when no checklist exists.
    pub(super) fn plan_progress_message(&self) -> Option<ChatMessageRequest> {
        let todos = todowrite::current_todos();
        if todos.is_empty() {
            return None;
        }
        let total = todos.len();
        let done = todos
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count();
        let current = todos
            .iter()
            .find(|t| t.status == TodoStatus::InProgress)
            .or_else(|| todos.iter().find(|t| t.status == TodoStatus::Pending))
            .map(|t| t.content.clone());
        Some(ChatMessageRequest::user(format!(
            "[plan] {done}/{total} plan steps done. Current: {}. Follow the \
             plan; keep it updated with the todowrite tool.",
            current.unwrap_or_else(|| "(none)".to_string())
        )))
    }
}
