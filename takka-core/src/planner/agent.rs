//! Planner agent — LLM plan creation, approval, execution, and replanning.

use serde::{Deserialize, Serialize};

use super::{Plan, PlanResult, PlanStatus, Step, StepStatus};
use crate::events::EventEmitter;
use crate::providers::{ChatMessageRequest, ChatRequest, LLMProvider};
use crate::sync;
use crate::tool::Tool;
use crate::tool_registry::ToolRegistry;
use crate::AgenticError;

/// Configuration for the planner agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannerConfig {
    /// Maximum number of steps allowed in a single plan.
    pub max_steps: usize,
    /// Whether plans require user approval before execution.
    pub require_approval: bool,
    /// Maximum number of re-plan attempts on failure before giving up.
    pub max_replan_attempts: usize,
}

impl Default for PlannerConfig {
    fn default() -> Self {
        Self {
            max_steps: 20,
            require_approval: true,
            max_replan_attempts: 3,
        }
    }
}

/// Callback type for plan approval. Returns `true` to approve, `false` to reject.
pub type ApprovalCallback = Box<dyn Fn(&Plan) -> bool + Send + Sync>;

/// The planner agent uses an LLM to decompose goals into executable plans.
pub struct PlannerAgent {
    provider: std::sync::Arc<dyn LLMProvider>,
    config: PlannerConfig,
    events: EventEmitter,
    approval_callback: std::sync::Mutex<Option<ApprovalCallback>>,
}

impl PlannerAgent {
    pub fn new(provider: std::sync::Arc<dyn LLMProvider>) -> Self {
        Self {
            provider,
            config: PlannerConfig::default(),
            events: EventEmitter::new(),
            approval_callback: std::sync::Mutex::new(None),
        }
    }

    /// Construct a PlannerAgent from the user-facing planner loop config.
    /// Maps `PlannerLoopConfig` fields to the internal `PlannerConfig`.
    pub fn from_config(
        provider: std::sync::Arc<dyn LLMProvider>,
        planner_cfg: &crate::config::PlannerLoopConfig,
    ) -> Self {
        Self {
            provider,
            config: PlannerConfig {
                max_steps: planner_cfg.max_steps,
                require_approval: planner_cfg.require_approval,
                max_replan_attempts: planner_cfg.max_replan_attempts,
            },
            events: EventEmitter::new(),
            approval_callback: std::sync::Mutex::new(None),
        }
    }

    pub fn with_config(mut self, config: PlannerConfig) -> Self {
        self.config = config;
        self
    }

    pub fn set_approval_callback<F>(&self, callback: F)
    where
        F: Fn(&Plan) -> bool + Send + Sync + 'static,
    {
        let mut cb = sync::lock(&self.approval_callback);
        *cb = Some(Box::new(callback));
    }

    /// Subscribe to events emitted by this planner (plan progress,
    /// tool calls, completion, etc.).
    pub fn on<F>(&self, handler: F)
    where
        F: Fn(crate::events::Event) + Send + Sync + 'static,
    {
        self.events.on(handler);
    }

    // ---- Plan creation via LLM ----

    /// Ask the LLM to create a plan for the given goal, given the available tools.
    pub fn create_plan(&self, goal: &str, tools: &ToolRegistry) -> Result<Plan, AgenticError> {
        let tool_defs = tools.tool_definitions();
        let tool_descriptions: Vec<String> = tool_defs
            .iter()
            .map(|t| format!("- {}: {}", t.function.name, t.function.description))
            .collect();

        let system_prompt = format!(
            "You are a planning agent. Given a goal and a list of available tools, \
             create a step-by-step plan to achieve the goal.\n\n\
             ## Available Tools\n{}\n\n\
             ## Output Format\n\
             Respond with a JSON array of steps. Each step is an object with:\n\
             - `description` (string): what this step does\n\
             - `tool` (string, optional): tool name to use\n\
             - `args` (object, optional): arguments for the tool\n\
             - `depends_on` (array of integers, optional): 0-based indices of steps that must complete first\n\n\
             Respond ONLY with the JSON array, no other text. Example:\n\
             ```json\n\
             [\n  \
               {{\"description\": \"Read the current file\", \"tool\": \"read_file\", \"args\": {{\"path\": \"src/main.rs\"}}}},\n  \
               {{\"description\": \"Fix the bug\", \"depends_on\": [0]}}\n\
             ]\n\
             ```",
            tool_descriptions.join("\n")
        );

        let request = ChatRequest::new(
            "planner",
            vec![
                ChatMessageRequest::system(&system_prompt),
                ChatMessageRequest::user(goal),
            ],
        );

        let response = self
            .provider
            .chat(request)
            .map_err(|e| AgenticError::Provider(e.to_string()))?;

        let content = response.message.content.unwrap_or_default();

        let steps = Self::parse_plan_response(&content)?;

        if steps.len() > self.config.max_steps {
            return Err(AgenticError::Config(format!(
                "Plan has {} steps, exceeding max_steps limit of {}",
                steps.len(),
                self.config.max_steps
            )));
        }

        let mut plan = Plan::new(goal).with_steps(steps);
        plan.status = if self.config.require_approval {
            PlanStatus::PendingApproval
        } else {
            PlanStatus::Draft
        };

        self.events.emit(crate::events::Event::System {
            message: format!(
                "Plan created with {} step(s) for goal: {}",
                plan.steps.len(),
                goal
            ),
        });
        // P0-3 lifecycle signal: frontends key plan-UI visibility off
        // this event, the System message stays for text-based logs.
        self.events.emit(crate::events::Event::PlanCreated {
            steps_total: plan.steps.len(),
        });

        Ok(plan)
    }

    /// Create a plan manually (without LLM) from a list of step descriptions.
    pub fn create_plan_manual(&self, goal: &str, step_descriptions: Vec<String>) -> Plan {
        let steps: Vec<Step> = step_descriptions.into_iter().map(Step::new).collect();

        let mut plan = Plan::new(goal).with_steps(steps);
        plan.status = if self.config.require_approval {
            PlanStatus::PendingApproval
        } else {
            PlanStatus::Draft
        };
        // P0-3 lifecycle signal — same emission as the LLM path.
        self.events.emit(crate::events::Event::PlanCreated {
            steps_total: plan.steps.len(),
        });
        plan
    }

    /// Parse the LLM response into steps.
    pub(super) fn parse_plan_response(content: &str) -> Result<Vec<Step>, AgenticError> {
        // Try to extract JSON from the content — the LLM might wrap it in ```json ... ```
        let json_str = Self::extract_json(content);

        #[derive(Deserialize)]
        struct RawStep {
            description: String,
            tool: Option<String>,
            args: Option<serde_json::Value>,
            depends_on: Option<Vec<usize>>,
        }

        let raw_steps: Vec<RawStep> = serde_json::from_str(&json_str).map_err(|e| {
            AgenticError::Config(format!(
                "Failed to parse plan steps from LLM response: {}. Content: {}",
                e,
                &json_str[..json_str.len().min(200)]
            ))
        })?;

        // First pass: create all steps and build index mapping
        let mut steps: Vec<Step> = Vec::with_capacity(raw_steps.len());
        let mut index_to_id: Vec<String> = Vec::with_capacity(raw_steps.len());

        for raw in &raw_steps {
            let step = Step::new(&raw.description);
            index_to_id.push(step.id.clone());
            steps.push(step);
        }

        // Second pass: resolve depends_on indices to step IDs
        for (i, raw) in raw_steps.iter().enumerate() {
            if let Some(dep_indices) = &raw.depends_on {
                for &dep_idx in dep_indices {
                    if dep_idx < index_to_id.len() {
                        steps[i].depends_on.push(index_to_id[dep_idx].clone());
                    }
                }
            }

            // Set tool and args
            if let Some(tool) = &raw.tool {
                steps[i].tool = Some(tool.clone());
                steps[i].args = raw.args.clone();
            }
        }

        Ok(steps)
    }

    /// Extract JSON array from content that might be wrapped in markdown fences.
    pub(super) fn extract_json(content: &str) -> String {
        let trimmed = content.trim();

        // Try to find ```json ... ``` block
        if let Some(start) = trimmed.find("```json") {
            let json_start = start + 7;
            if let Some(end) = trimmed[json_start..].find("```") {
                return trimmed[json_start..json_start + end].trim().to_string();
            }
        }

        // Try to find ``` ... ``` block
        if let Some(start) = trimmed.find("```") {
            let json_start = start + 3;
            if let Some(end) = trimmed[json_start..].find("```") {
                return trimmed[json_start..json_start + end].trim().to_string();
            }
        }

        // Try the content as-is (find the JSON array)
        if let Some(start) = trimmed.find('[') {
            if let Some(end) = trimmed.rfind(']') {
                return trimmed[start..=end].to_string();
            }
        }

        trimmed.to_string()
    }

    // ---- Plan approval ----

    /// Request approval for a plan. Returns `true` if approved.
    pub fn request_approval(&self, plan: &Plan) -> bool {
        self.events.emit(crate::events::Event::ConfirmationRequest {
            target: None,
            action: "plan_approval".to_string(),
            description: format!(
                "Plan: {} ({} steps)\n{}",
                plan.goal,
                plan.steps.len(),
                plan.steps
                    .iter()
                    .enumerate()
                    .map(|(i, s)| format!("  {}. {}", i + 1, s.description))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            risk_level: "medium".to_string(),
        });

        let cb = sync::lock(&self.approval_callback);
        if let Some(ref callback) = *cb {
            callback(plan)
        } else {
            // Default: approve if no callback set
            true
        }
    }

    /// Approve a plan, transitioning it to Draft (ready for execution).
    pub fn approve_plan(&self, plan: &mut Plan) -> Result<(), AgenticError> {
        if plan.status != PlanStatus::PendingApproval {
            return Err(AgenticError::Config(format!(
                "Plan is in '{}' status, expected 'pending_approval'",
                plan.status
            )));
        }
        plan.status = PlanStatus::Draft;
        plan.touch();
        Ok(())
    }

    /// Reject a plan, transitioning it to Cancelled.
    pub fn reject_plan(&self, plan: &mut Plan) -> Result<(), AgenticError> {
        if plan.status != PlanStatus::PendingApproval {
            return Err(AgenticError::Config(format!(
                "Plan is in '{}' status, expected 'pending_approval'",
                plan.status
            )));
        }
        plan.status = PlanStatus::Cancelled;
        plan.touch();
        Ok(())
    }

    /// Emit a PlanProgress event reflecting the current plan state.
    fn emit_plan_progress(
        &self,
        plan: &Plan,
        step_id: &str,
        step_description: &str,
        step_status: &str,
    ) {
        let (pending, completed, failed, _skipped) = plan.step_summary();
        self.events.emit(crate::events::Event::PlanProgress {
            plan_id: plan.id.clone(),
            plan_goal: plan.goal.clone(),
            step_id: step_id.to_string(),
            step_description: step_description.to_string(),
            step_status: step_status.to_string(),
            steps_total: plan.steps.len(),
            steps_completed: completed,
            steps_failed: failed,
            steps_pending: pending,
        });
    }

    // ---- Plan execution ----

    /// Execute a plan step by step using the provided tools.
    /// Steps with dependencies wait until their dependencies are completed.
    pub fn execute_plan(
        &self,
        plan: &mut Plan,
        tools: &ToolRegistry,
    ) -> Result<PlanResult, AgenticError> {
        if plan.status == PlanStatus::PendingApproval {
            if !self.request_approval(plan) {
                plan.status = PlanStatus::Cancelled;
                plan.touch();
                return Ok(PlanResult::from_plan(plan));
            }
            plan.status = PlanStatus::Draft;
        }

        if plan.status == PlanStatus::Cancelled {
            return Ok(PlanResult::from_plan(plan));
        }

        plan.status = PlanStatus::Executing;
        plan.touch();

        let mut replan_attempts = 0;

        while let Some(next) = plan.next_executable_step() {
            let step_id = next.id.clone();
            let step_desc = next.description.clone();
            let step_tool = next.tool.clone();
            let step_args = next.args.clone();

            // Mark step as in progress
            {
                let s = plan.get_step_mut(&step_id).unwrap();
                s.status = StepStatus::InProgress;
            }
            plan.touch();

            // P0-3 step lifecycle: index is the step's position in the
            // declared plan order, not the execution order (dependencies
            // may reorder execution).
            let step_index = plan
                .steps
                .iter()
                .position(|s| s.id == step_id)
                .unwrap_or_default();
            self.events.emit(crate::events::Event::StepStarted {
                index: step_index,
                total: plan.steps.len(),
                description: step_desc.clone(),
            });

            self.emit_plan_progress(plan, &step_id, &step_desc, "in_progress");

            self.events.emit(crate::events::Event::ToolCall {
                tool_name: step_tool.clone().unwrap_or_else(|| "none".to_string()),
                arguments: step_args.clone().unwrap_or(serde_json::json!(null)),
            });

            // Execute the step
            let result = if let Some(tool_name) = &step_tool {
                if tool_name == "spawn_subagent" {
                    // Delegate to subagent: creates a fresh subagent with
                    // isolated context to execute this step as a subtask.
                    let subagent = crate::tools::SpawnSubagentTool::new(
                        self.provider.clone(),
                        tools.clone(),
                        // Use a generic model label for the subagent.
                        "planner-subagent",
                    );
                    // The step description serves as the subagent task.
                    let sub_args = serde_json::json!({"task": step_desc});
                    match subagent.execute(sub_args) {
                        Ok(output) => (
                            StepStatus::Completed,
                            Some(
                                serde_json::to_string(&output)
                                    .unwrap_or_else(|_| output.to_string()),
                            ),
                        ),
                        Err(e) => (StepStatus::Failed, Some(format!("Subagent error: {}", e))),
                    }
                } else {
                    let args = step_args.clone().unwrap_or(serde_json::json!({}));
                    match tools.execute_by_name(tool_name, &args) {
                        Ok(output) => (
                            StepStatus::Completed,
                            Some(
                                serde_json::to_string(&output)
                                    .unwrap_or_else(|_| output.to_string()),
                            ),
                        ),
                        Err(e) => (StepStatus::Failed, Some(format!("Tool error: {}", e))),
                    }
                }
            } else {
                // No tool specified — mark as completed (manual/LLM-driven step)
                (StepStatus::Completed, None)
            };

            let (new_status, result_text) = result;

            // Update step
            {
                let s = plan.get_step_mut(&step_id).unwrap();
                s.status = new_status;
                s.result = result_text.clone();
            }
            plan.touch();

            self.events.emit(crate::events::Event::StepCompleted {
                index: step_index,
                total: plan.steps.len(),
                status: new_status.to_string(),
            });

            self.events.emit(crate::events::Event::ToolOutput {
                tool_name: step_tool.unwrap_or_else(|| "none".to_string()),
                output: serde_json::json!({
                    "step": step_desc,
                    "status": new_status.to_string(),
                    "result": result_text,
                }),
                error: None,
                tool_call_id: step_id.clone(),
                duration_ms: 0,
                success: true,
                truncated: false,
            });

            self.emit_plan_progress(plan, &step_id, &step_desc, &new_status.to_string());

            // Handle failure
            if new_status == StepStatus::Failed {
                // Try to re-plan
                if replan_attempts < self.config.max_replan_attempts {
                    replan_attempts += 1;
                    match self.replan(plan) {
                        Ok(new_plan) => {
                            *plan = new_plan;
                            plan.status = PlanStatus::Executing;
                            continue;
                        }
                        Err(_) => {
                            // Re-planning failed, mark remaining steps as skipped
                            Self::skip_remaining(plan);
                            break;
                        }
                    }
                } else {
                    // Max replan attempts reached, skip remaining
                    Self::skip_remaining(plan);
                    break;
                }
            }
        }

        // Determine final status
        let (pending, completed, failed, skipped) = plan.step_summary();
        if failed > 0 || skipped > 0 && pending > 0 {
            plan.status = PlanStatus::Failed;
        } else if pending == 0 && failed == 0 {
            plan.status = PlanStatus::Completed;
        } else {
            plan.status = PlanStatus::Failed;
        }
        plan.touch();

        self.events.emit(crate::events::Event::Done {
            result: format!(
                "Plan {}: {} completed, {} failed, {} skipped",
                plan.status, completed, failed, skipped
            ),
        });

        Ok(PlanResult::from_plan(plan))
    }

    /// Mark all pending steps as skipped.
    fn skip_remaining(plan: &mut Plan) {
        for step in &mut plan.steps {
            if step.status == StepStatus::Pending {
                step.status = StepStatus::Skipped;
            }
        }
    }

    /// Ask the LLM to re-plan around a failure. Returns a new plan with updated steps.
    pub fn replan(&self, plan: &Plan) -> Result<Plan, AgenticError> {
        let completed_steps: Vec<&Step> = plan
            .steps
            .iter()
            .filter(|s| s.status == StepStatus::Completed)
            .collect();
        let failed_steps: Vec<&Step> = plan
            .steps
            .iter()
            .filter(|s| s.status == StepStatus::Failed)
            .collect();

        let completed_summary: Vec<String> = completed_steps
            .iter()
            .map(|s| format!("- ✅ {}", s.description))
            .collect();
        let failed_summary: Vec<String> = failed_steps
            .iter()
            .map(|s| {
                format!(
                    "- ❌ {} (error: {})",
                    s.description,
                    s.result.as_deref().unwrap_or("unknown")
                )
            })
            .collect();

        let prompt = format!(
            "The following plan encountered failures. Please create a revised plan.\n\n\
             ## Original Goal\n{}\n\n\
             ## Completed Steps\n{}\n\n\
             ## Failed Steps\n{}\n\n\
             Please create a new plan that accounts for what's already done and avoids the failures. \
             Respond with the same JSON array format.",
            plan.goal,
            if completed_summary.is_empty() {
                "(none)".to_string()
            } else {
                completed_summary.join("\n")
            },
            if failed_summary.is_empty() {
                "(none)".to_string()
            } else {
                failed_summary.join("\n")
            },
        );

        let request = ChatRequest::new(
            "planner",
            vec![
                ChatMessageRequest::system(
                    "You are a planning agent. Create a revised plan in JSON array format. \
                 Respond ONLY with the JSON array.",
                ),
                ChatMessageRequest::user(&prompt),
            ],
        );

        let response = self
            .provider
            .chat(request)
            .map_err(|e| AgenticError::Provider(e.to_string()))?;

        let content = response.message.content.unwrap_or_default();
        let new_steps = Self::parse_plan_response(&content)?;

        // Preserve completed steps, replace the rest
        let mut revised = plan.clone();
        revised.steps = completed_steps
            .into_iter()
            .cloned()
            .chain(new_steps)
            .collect();
        revised.touch();

        self.events.emit(crate::events::Event::PlanReplanned {
            plan_id: revised.id.clone(),
            plan_goal: revised.goal.clone(),
            reason: format!(
                "Step '{}' failed",
                failed_steps
                    .first()
                    .map(|s| s.description.as_str())
                    .unwrap_or("unknown")
            ),
            steps_carried_over: completed_summary.len(),
            steps_total: revised.steps.len(),
        });

        Ok(revised)
    }
}
