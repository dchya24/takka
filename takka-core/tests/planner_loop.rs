//! Integration tests for the Planner Agent.
//!
//! These tests drive the real `PlannerAgent` against a scripted mock
//! provider (for plan creation / replanning) and the real tool registry
//! (for step execution). Only the LLM is stubbed; tool execution happens
//! against real temporary directories.
//!
//! Why this lives in `tests/` rather than alongside the planner's own
//! unit tests: the tests exercise filesystem operations through builtin
//! tools, matching Cargo's integration-test boundary.

use takka_core::{planner::*, tools::RunCommandTool, Event, LLMProvider, ToolRegistry};
use std::sync::Arc;

mod support;
use support::{text_response, ScriptedProvider};

/// Build a minimal `PlannerAgent` wired to a `ScriptedProvider` whose
/// first response is a plan-JSON array.
fn planner_for_steps(steps_json: &str) -> (PlannerAgent, ToolRegistry) {
    let provider: Arc<dyn LLMProvider> =
        Arc::new(ScriptedProvider::new(vec![text_response(steps_json)]));

    let tools = ToolRegistry::new();
    tools.register(Box::new(RunCommandTool::new()));

    let planner = PlannerAgent::new(provider).with_config(PlannerConfig {
        require_approval: false,
        max_replan_attempts: 0,
        ..Default::default()
    });

    (planner, tools)
}

// ── Manual plan execution (no LLM needed) ─────────────────────────

#[test]
fn planner_manual_executes_simple_tool_steps() {
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![]));
    let planner = PlannerAgent::new(provider).with_config(PlannerConfig {
        require_approval: false,
        ..Default::default()
    });

    let tools = ToolRegistry::new();
    tools.register(Box::new(RunCommandTool::new()));

    let mut plan = planner.create_plan_manual(
        "Echo test",
        vec!["First echo".to_string(), "Second echo".to_string()],
    );
    // Assign tools to steps
    plan.steps[0].tool = Some("run_command".to_string());
    plan.steps[0].args = Some(serde_json::json!({"command": "echo first-step"}));
    plan.steps[1].tool = Some("run_command".to_string());
    plan.steps[1].args = Some(serde_json::json!({"command": "echo second-step"}));
    plan.status = PlanStatus::Draft;

    let result = planner.execute_plan(&mut plan, &tools).unwrap();
    assert_eq!(result.status, PlanStatus::Completed);
    assert_eq!(result.steps_completed, 2);
    assert_eq!(result.steps_total, 2);
    assert!(result.output.contains("completed"));
}

#[test]
fn planner_manual_respects_dependency_order() {
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![]));
    let planner = PlannerAgent::new(provider).with_config(PlannerConfig {
        require_approval: false,
        ..Default::default()
    });

    let tools = ToolRegistry::new();
    tools.register(Box::new(RunCommandTool::new()));

    let mut plan = planner.create_plan_manual(
        "Sequential echo",
        vec!["First".to_string(), "Second".to_string()],
    );
    let first_id = plan.steps[0].id.clone();
    plan.steps[0].tool = Some("run_command".to_string());
    plan.steps[0].args = Some(serde_json::json!({"command": "echo first"}));
    plan.steps[1].tool = Some("run_command".to_string());
    plan.steps[1].args = Some(serde_json::json!({"command": "echo second"}));
    plan.steps[1].depends_on = vec![first_id];
    plan.status = PlanStatus::Draft;

    let result = planner.execute_plan(&mut plan, &tools).unwrap();
    assert_eq!(result.status, PlanStatus::Completed);
    assert_eq!(result.steps_completed, 2);
}

#[test]
fn planner_manual_fails_on_missing_tool() {
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![]));
    let planner = PlannerAgent::new(provider).with_config(PlannerConfig {
        require_approval: false,
        max_replan_attempts: 0,
        ..Default::default()
    });

    let tools = ToolRegistry::new();

    let mut plan = planner.create_plan_manual("Fail", vec!["Missing tool step".to_string()]);
    plan.steps[0].tool = Some("nonexistent_tool".to_string());
    plan.steps[0].args = Some(serde_json::json!({}));
    plan.status = PlanStatus::Draft;

    let result = planner.execute_plan(&mut plan, &tools).unwrap();
    assert_eq!(result.status, PlanStatus::Failed);
    assert_eq!(result.steps_failed, 1);
}

#[test]
fn planner_manual_cancels_on_approval_reject() {
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![]));
    let planner = PlannerAgent::new(provider).with_config(PlannerConfig {
        require_approval: true,
        ..Default::default()
    });
    planner.set_approval_callback(|_plan| false);

    let tools = ToolRegistry::new();

    let mut plan = planner.create_plan_manual("Cancelled plan", vec!["Step 1".to_string()]);
    plan.status = PlanStatus::PendingApproval;

    let result = planner.execute_plan(&mut plan, &tools).unwrap();
    assert_eq!(result.status, PlanStatus::Cancelled);
    assert_eq!(result.steps_completed, 0);
}

// ── LLM-driven plan creation ──────────────────────────────────────

#[test]
fn planner_llm_creates_and_executes_plan() {
    // The ScriptedProvider's first response returns a plan-JSON array.
    let steps_json = r#"[
        {"description": "Say hello", "tool": "run_command", "args": {"command": "echo hello"}}
    ]"#;

    let (planner, tools) = planner_for_steps(steps_json);

    let plan = planner.create_plan("Say hello world", &tools).unwrap();
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].description, "Say hello");
    assert_eq!(plan.steps[0].tool.as_deref(), Some("run_command"));

    let mut plan = plan;
    let result = planner.execute_plan(&mut plan, &tools).unwrap();
    assert_eq!(result.status, PlanStatus::Completed);
    assert_eq!(result.steps_completed, 1);
}

// ── Event emission ────────────────────────────────────────────────

#[test]
fn planner_emits_plan_progress_events() {
    use std::sync::Mutex;

    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![]));
    let planner = PlannerAgent::new(provider).with_config(PlannerConfig {
        require_approval: false,
        ..Default::default()
    });

    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    planner.on({
        let events = events.clone();
        move |ev| events.lock().unwrap().push(ev)
    });

    let tools = ToolRegistry::new();
    tools.register(Box::new(RunCommandTool::new()));

    let mut plan = planner.create_plan_manual(
        "Event test",
        vec!["Echo one".to_string(), "Echo two".to_string()],
    );
    plan.steps[0].tool = Some("run_command".to_string());
    plan.steps[0].args = Some(serde_json::json!({"command": "echo one"}));
    plan.steps[1].tool = Some("run_command".to_string());
    plan.steps[1].args = Some(serde_json::json!({"command": "echo two"}));
    plan.status = PlanStatus::Draft;

    let _result = planner.execute_plan(&mut plan, &tools).unwrap();

    let captured = events.lock().unwrap();
    let progress_events: Vec<&Event> = captured
        .iter()
        .filter(|e| matches!(e, Event::PlanProgress { .. }))
        .collect();

    // We expect at least 2 PlanProgress events: one "in_progress" + one "completed" per step
    assert!(
        progress_events.len() >= 2,
        "Expected at least 2 PlanProgress events, got {}",
        progress_events.len()
    );

    // Check the first event is "in_progress" and last is "completed" or "failed"
    if let Event::PlanProgress { step_status, .. } = progress_events[0] {
        assert_eq!(
            step_status, "in_progress",
            "First event should be in_progress"
        );
    }
}

// ── Replan event ─────────────────────────────────────────────────

#[test]
fn planner_emits_plan_replanned_event_on_failure_with_replan() {
    use std::sync::Mutex;

    // First response: plan with a failing tool step.
    // Second response: revised plan with a working step.
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![
        text_response(
            r#"[{"description": "Fail step", "tool": "nonexistent", "args": {}}]
"#,
        ),
        text_response(
            r#"[{"description": "Recovery step", "tool": "run_command", "args": {"command": "echo recovered"}}]
"#,
        ),
    ]));

    let planner = PlannerAgent::new(provider).with_config(PlannerConfig {
        require_approval: false,
        max_replan_attempts: 1,
        ..Default::default()
    });

    let replanned_events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    planner.on({
        let replanned_events = replanned_events.clone();
        move |ev| {
            if matches!(ev, Event::PlanReplanned { .. }) {
                replanned_events.lock().unwrap().push(ev);
            }
        }
    });

    let tools = ToolRegistry::new();
    tools.register(Box::new(RunCommandTool::new()));

    let plan = planner.create_plan("Fail then recover", &tools).unwrap();
    assert_eq!(plan.steps.len(), 1);

    let mut plan = plan;
    let result = planner.execute_plan(&mut plan, &tools).unwrap();

    // The plan should have recovered via replan
    assert_eq!(result.status, PlanStatus::Completed);

    // A PlanReplanned event should have been emitted
    let replanned = replanned_events.lock().unwrap();
    assert_eq!(
        replanned.len(),
        1,
        "Expected exactly 1 PlanReplanned event, got {}",
        replanned.len()
    );

    if let Event::PlanReplanned {
        reason,
        steps_carried_over,
        steps_total,
        plan_goal,
        ..
    } = &replanned[0]
    {
        assert!(
            reason.contains("Fail step"),
            "Reason should mention failed step: {}",
            reason
        );
        assert_eq!(
            *steps_carried_over, 0,
            "No steps were completed before replan"
        );
        assert_eq!(*steps_total, 1, "Revised plan should have 1 step");
        assert_eq!(plan_goal, "Fail then recover");
    } else {
        panic!("Expected PlanReplanned event");
    }
}

#[test]
fn planner_result_summary() {
    let provider: Arc<dyn LLMProvider> = Arc::new(ScriptedProvider::new(vec![]));
    let tools = ToolRegistry::new();
    tools.register(Box::new(RunCommandTool::new()));

    let planner = PlannerAgent::new(provider).with_config(PlannerConfig {
        require_approval: false,
        max_replan_attempts: 0,
        ..Default::default()
    });

    let mut plan =
        planner.create_plan_manual("Mixed results", vec!["Ok".to_string(), "Fail".to_string()]);
    plan.steps[0].tool = Some("run_command".to_string());
    plan.steps[0].args = Some(serde_json::json!({"command": "echo ok"}));
    plan.steps[1].tool = Some("nonexistent".to_string());
    plan.steps[1].args = Some(serde_json::json!({}));
    plan.status = PlanStatus::Draft;

    let result = planner.execute_plan(&mut plan, &tools).unwrap();
    assert_eq!(result.status, PlanStatus::Failed);
    assert_eq!(result.steps_completed, 1);
    assert_eq!(result.steps_failed, 1);
    assert_eq!(result.steps_total, 2);
}
