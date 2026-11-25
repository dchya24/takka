//! Planner agent — task decomposition, step planning, and execution tracking.
//!
//! The planner takes a high-level goal, decomposes it into ordered steps
//! (optionally with dependencies), tracks execution, and supports re-planning
//! on failure. Plan approval flows through the event system so the UI layer
//! can present plans to the user before execution begins.

mod agent;
#[cfg(test)]
mod tests;

pub use agent::{ApprovalCallback, PlannerAgent, PlannerConfig};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// Plan types
// ---------------------------------------------------------------------------

/// Unique identifier for a plan or step.
pub type PlanId = String;
pub type StepId = String;

/// Overall status of a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Plan is being constructed or edited.
    Draft,
    /// Plan is waiting for user approval before execution.
    PendingApproval,
    /// Plan steps are being executed.
    Executing,
    /// All steps completed successfully.
    Completed,
    /// One or more steps failed and the plan stopped.
    Failed,
    /// Plan was cancelled by the user.
    Cancelled,
}

impl fmt::Display for PlanStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanStatus::Draft => write!(f, "draft"),
            PlanStatus::PendingApproval => write!(f, "pending_approval"),
            PlanStatus::Executing => write!(f, "executing"),
            PlanStatus::Completed => write!(f, "completed"),
            PlanStatus::Failed => write!(f, "failed"),
            PlanStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// Status of a single step within a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Step is waiting to be executed.
    Pending,
    /// Step is currently running.
    InProgress,
    /// Step finished successfully.
    Completed,
    /// Step failed.
    Failed,
    /// Step was skipped (e.g. dependency failed).
    Skipped,
}

impl fmt::Display for StepStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StepStatus::Pending => write!(f, "pending"),
            StepStatus::InProgress => write!(f, "in_progress"),
            StepStatus::Completed => write!(f, "completed"),
            StepStatus::Failed => write!(f, "failed"),
            StepStatus::Skipped => write!(f, "skipped"),
        }
    }
}

/// A single step within a plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: StepId,
    /// Human-readable description of what this step does.
    pub description: String,
    /// Optional tool name to execute for this step.
    pub tool: Option<String>,
    /// Optional arguments for the tool.
    pub args: Option<serde_json::Value>,
    /// Current status of the step.
    pub status: StepStatus,
    /// Result output (set on completion or failure).
    pub result: Option<String>,
    /// IDs of steps that must complete before this step can run.
    pub depends_on: Vec<StepId>,
}

impl Step {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            description: description.into(),
            tool: None,
            args: None,
            status: StepStatus::Pending,
            result: None,
            depends_on: vec![],
        }
    }

    pub fn with_tool(mut self, tool: impl Into<String>, args: serde_json::Value) -> Self {
        self.tool = Some(tool.into());
        self.args = Some(args);
        self
    }

    pub fn depends_on(mut self, step_id: impl Into<String>) -> Self {
        self.depends_on.push(step_id.into());
        self
    }

    /// Check if all dependencies are satisfied (all completed).
    pub fn dependencies_met(&self, steps: &[Step]) -> bool {
        self.depends_on.iter().all(|dep_id| {
            steps
                .iter()
                .find(|s| &s.id == dep_id)
                .map(|s| s.status == StepStatus::Completed)
                .unwrap_or(false)
        })
    }
}

/// A plan consisting of multiple steps toward a goal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: PlanId,
    /// The high-level goal this plan aims to achieve.
    pub goal: String,
    /// Ordered list of steps.
    pub steps: Vec<Step>,
    /// Overall plan status.
    pub status: PlanStatus,
    /// When this plan was created.
    pub created_at: DateTime<Utc>,
    /// When this plan was last updated.
    pub updated_at: DateTime<Utc>,
}

impl Plan {
    pub fn new(goal: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            goal: goal.into(),
            steps: vec![],
            status: PlanStatus::Draft,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn with_steps(mut self, steps: Vec<Step>) -> Self {
        self.steps = steps;
        self.touch();
        self
    }

    pub fn add_step(&mut self, step: Step) {
        self.steps.push(step);
        self.touch();
    }

    /// Get a step by ID.
    pub fn get_step(&self, id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// Get a mutable step by ID.
    pub fn get_step_mut(&mut self, id: &str) -> Option<&mut Step> {
        self.steps.iter_mut().find(|s| s.id == id)
    }

    /// Get the next step that is pending and has all dependencies met.
    pub fn next_executable_step(&self) -> Option<&Step> {
        self.steps
            .iter()
            .find(|s| s.status == StepStatus::Pending && s.dependencies_met(&self.steps))
    }

    /// Return a summary of step statuses: (pending, completed, failed, skipped).
    pub fn step_summary(&self) -> (usize, usize, usize, usize) {
        let pending = self
            .steps
            .iter()
            .filter(|s| s.status == StepStatus::Pending)
            .count();
        let completed = self
            .steps
            .iter()
            .filter(|s| s.status == StepStatus::Completed)
            .count();
        let failed = self
            .steps
            .iter()
            .filter(|s| s.status == StepStatus::Failed)
            .count();
        let skipped = self
            .steps
            .iter()
            .filter(|s| s.status == StepStatus::Skipped)
            .count();
        (pending, completed, failed, skipped)
    }

    pub(super) fn touch(&mut self) {
        self.updated_at = Utc::now();
    }
}

/// Result of executing a plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanResult {
    pub plan_id: PlanId,
    pub status: PlanStatus,
    pub steps_completed: usize,
    pub steps_failed: usize,
    pub steps_skipped: usize,
    pub steps_total: usize,
    /// Final output / summary of the plan execution.
    pub output: String,
}

impl PlanResult {
    pub fn from_plan(plan: &Plan) -> Self {
        let (pending, completed, failed, skipped) = plan.step_summary();
        let output = match plan.status {
            PlanStatus::Completed => format!(
                "Plan completed: {} step(s) executed successfully.",
                completed
            ),
            PlanStatus::Failed => format!(
                "Plan failed: {} completed, {} failed, {} skipped.",
                completed, failed, skipped
            ),
            _ => format!("Plan status: {} ({} pending)", plan.status, pending),
        };
        Self {
            plan_id: plan.id.clone(),
            status: plan.status,
            steps_completed: completed,
            steps_failed: failed,
            steps_skipped: skipped,
            steps_total: plan.steps.len(),
            output,
        }
    }
}
