//! `question` tool — ask the user questions during execution.
//!
//! The agent calls this when it needs to gather preferences, clarify
//! instructions, or get a decision on implementation choices. Unlike
//! the confirmation handler (which is yes/no), `question` supports:
//!
//! - Free-text answers (the user types anything)
//! - Multiple-choice selection (the user picks from options)
//! - Multi-select (the user picks one or more options)
//!
//! **Mechanism**: Because `Tool::execute` is synchronous and must return
//! a `ToolResult<Value>`, the question tool uses a callback pattern
//! (the same approach as the orchestrator's `confirmation_handler`).
//! The CLI/TUI registers a handler at startup; the tool invokes it
//! when `execute` runs. If no handler is registered, the tool returns
//! a fallback response so the agent doesn't stall.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::tool::{
    Concurrency, Mutability, SideEffects, Tool, ToolError, ToolMetadata, ToolParam, ToolResult,
    ToolSchema,
};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single question the agent wants to ask the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionPrompt {
    /// The question text shown to the user.
    pub question: String,
    /// Short header / label for the question (used in TUI rendering).
    #[serde(default)]
    pub header: Option<String>,
    /// Pre-defined options the user can choose from. Empty = free text.
    #[serde(default)]
    pub options: Vec<String>,
    /// Allow the user to type a custom answer instead of picking an option.
    #[serde(default)]
    pub custom: bool,
    /// Allow selecting multiple options (checkbox style).
    #[serde(default)]
    pub multiple: bool,
}

/// The user's answer to a single question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionAnswer {
    /// The question that was asked (echoed back for context).
    pub question: String,
    /// The user's selected option(s) or free-text answer.
    pub answer: Vec<String>,
    /// Whether the user chose to skip / dismiss this question.
    #[serde(default)]
    pub skipped: bool,
}

/// Callback type for handling questions. Returns one `QuestionAnswer`
/// per `QuestionPrompt`.
///
/// The handler receives the full list of questions and returns the
/// same number of answers. Implementations may show them one at a time
/// or batched, synchronously or via a UI loop — that's the caller's
/// concern.
///
/// **Important for host integrations**: `handle` is invoked from the
/// middle of a tool execution while the host may still be rendering
/// progress elsewhere (e.g. the CLI's streaming "Thinking…" spinner
/// thread). Implementations that present an interactive prompt must
/// suspend that other rendering for the duration of the prompt, or the
/// two writers will corrupt each other's output. The CLI handler does
/// this by parking the spinner thread behind a gate while dialoguer is
/// on screen.
pub trait QuestionHandler: Send + Sync {
    fn handle(&self, questions: &[QuestionPrompt]) -> Vec<QuestionAnswer>;
}

/// Wire a per-instance question handler into a registry (P2-4):
/// replaces the registered `question` tool with one that routes to
/// `handler`. `None` installs the skip-all fallback — right for
/// non-interactive runs.
pub fn install_question_handler(
    tools: &crate::tool_registry::ToolRegistry,
    handler: Option<Arc<dyn QuestionHandler>>,
) {
    tools.unregister("question");
    let tool = match handler {
        Some(h) => QuestionTool::new().with_handler(h),
        None => QuestionTool::new(),
    };
    tools.register(Box::new(tool));
}

// ---------------------------------------------------------------------------
// Fallback handler (when no UI is connected)
// ---------------------------------------------------------------------------

/// Answer produced when no question handler is registered. Returns a
/// skip for every question so the agent can proceed without blocking.
fn fallback_answers(questions: &[QuestionPrompt]) -> Vec<QuestionAnswer> {
    questions
        .iter()
        .map(|q| QuestionAnswer {
            question: q.question.clone(),
            answer: vec![],
            skipped: true,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tool implementation
// ---------------------------------------------------------------------------

pub struct QuestionTool {
    /// Per-instance handler (P2-4/D): each registry/orchestrator wires
    /// its own UI. `None` returns the skip-all fallback (non-interactive
    /// contract).
    handler: Option<Arc<dyn QuestionHandler>>,
}

impl QuestionTool {
    pub fn new() -> Self {
        Self { handler: None }
    }

    /// Attach this instance's UI handler — the per-session path that
    /// replaces the process-global registration.
    pub fn with_handler(mut self, handler: Arc<dyn QuestionHandler>) -> Self {
        self.handler = Some(handler);
        self
    }
}

impl Default for QuestionTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for QuestionTool {
    fn name(&self) -> &str {
        "question"
    }

    fn description(&self) -> &str {
        "Ask the user one or more questions during execution. Use this to gather \
         preferences, clarify ambiguous instructions, get decisions on implementation \
         choices, or present options. Each question can have pre-defined options \
         (multiple choice), accept free-text, or both. Returns the user's answers \
         or a 'skipped' marker if the user dismissed the question."
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "questions".to_string(),
            ToolParam {
                param_type: "array".to_string(),
                description: Some(
                    "Array of questions to ask. Each question has: \
                     question (string, required) — the question text, \
                     header (string, optional) — short label, \
                     options (string[], optional) — pre-defined choices, \
                     custom (boolean, optional) — allow free-text answer, \
                     multiple (boolean, optional) — allow multi-select."
                        .to_string(),
                ),
                default: None,
            },
        );

        ToolSchema {
            name: "question".to_string(),
            description: "Ask the user questions during execution.".to_string(),
            parameters: params,
            required: vec!["questions".to_string()],
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let args_obj = args
            .as_object()
            .ok_or_else(|| ToolError::new("Invalid arguments: expected object"))?;

        let questions_val = args_obj
            .get("questions")
            .ok_or_else(|| ToolError::new("Missing required parameter: questions"))?;

        let questions: Vec<QuestionPrompt> = serde_json::from_value(questions_val.clone())
            .map_err(|e| ToolError::new(format!("Invalid questions format: {}", e)))?;

        if questions.is_empty() {
            return Err(ToolError::new("questions array must not be empty"));
        }

        // Validate each question has a question text.
        for (i, q) in questions.iter().enumerate() {
            if q.question.trim().is_empty() {
                return Err(ToolError::new(format!(
                    "Question at index {} has empty question text",
                    i
                )));
            }
        }

        // Resolution order (P2-4, clean cutover): the per-instance
        // handler wired by `with_handler`; without one the tool returns
        // the skip-all fallback (right for non-interactive runs).
        let answers = if let Some(h) = &self.handler {
            h.handle(&questions)
        } else {
            tracing::warn!("question tool has no handler attached; returning skip-all fallback");
            fallback_answers(&questions)
        };

        // Sanity: answers must match questions count.
        if answers.len() != questions.len() {
            return Err(ToolError::new(format!(
                "Handler returned {} answers for {} questions",
                answers.len(),
                questions.len()
            )));
        }

        let skipped_count = answers.iter().filter(|a| a.skipped).count();

        Ok(serde_json::json!({
            "answers": answers,
            "total": questions.len(),
            "skipped": skipped_count,
            "answered": questions.len() - skipped_count,
        }))
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            mutability: Mutability::ReadOnly,
            concurrency: Concurrency::Exclusive,
            idempotent: false,
            risk: 0,
            side_effects: SideEffects::UserFacing,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_questions_array() {
        let tool = QuestionTool::new();
        let err = tool
            .execute(serde_json::json!({"questions": []}))
            .expect_err("should reject empty");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn rejects_empty_question_text() {
        let tool = QuestionTool::new();
        let err = tool
            .execute(serde_json::json!({"questions": [{"question": "  "}]}))
            .expect_err("should reject empty text");
        assert!(err.to_string().contains("empty question text"));
    }

    #[test]
    fn returns_skip_all_when_no_handler() {
        // No handler attached → skip-all (non-interactive contract).
        let tool = QuestionTool::new();
        let result = tool
            .execute(serde_json::json!({
                "questions": [
                    {"question": "What framework?", "options": ["react", "vue"]},
                    {"question": "TypeScript?"}
                ]
            }))
            .expect("should succeed");

        assert_eq!(result["total"], 2);
        assert_eq!(result["skipped"], 2);
        assert_eq!(result["answered"], 0);

        let answers = result["answers"].as_array().unwrap();
        assert!(answers[0]["skipped"].as_bool().unwrap());
        assert!(answers[1]["skipped"].as_bool().unwrap());
    }

    #[test]
    fn invokes_attached_handler() {
        struct TestHandler;
        impl QuestionHandler for TestHandler {
            fn handle(&self, questions: &[QuestionPrompt]) -> Vec<QuestionAnswer> {
                questions
                    .iter()
                    .enumerate()
                    .map(|(i, q)| QuestionAnswer {
                        question: q.question.clone(),
                        answer: vec![format!("answer-{}", i)],
                        skipped: false,
                    })
                    .collect()
            }
        }

        let tool = QuestionTool::new().with_handler(Arc::new(TestHandler));
        let result = tool
            .execute(serde_json::json!({
                "questions": [
                    {"question": "Color?", "options": ["red", "blue"]},
                    {"question": "Name?"}
                ]
            }))
            .expect("should succeed");

        assert_eq!(result["total"], 2);
        assert_eq!(result["skipped"], 0);
        assert_eq!(result["answered"], 2);

        let answers = result["answers"].as_array().unwrap();
        assert_eq!(answers[0]["answer"][0], "answer-0");
        assert_eq!(answers[1]["answer"][0], "answer-1");
    }

    #[test]
    fn parses_all_question_fields() {
        let qp: QuestionPrompt = serde_json::from_value(serde_json::json!({
            "question": "Pick colors",
            "header": "Colors",
            "options": ["red", "green", "blue"],
            "custom": true,
            "multiple": true
        }))
        .unwrap();

        assert_eq!(qp.question, "Pick colors");
        assert_eq!(qp.header.as_deref(), Some("Colors"));
        assert_eq!(qp.options, vec!["red", "green", "blue"]);
        assert!(qp.custom);
        assert!(qp.multiple);
    }

    #[test]
    fn defaults_optional_fields() {
        let qp: QuestionPrompt = serde_json::from_value(serde_json::json!({
            "question": "Simple?"
        }))
        .unwrap();

        assert!(qp.header.is_none());
        assert!(qp.options.is_empty());
        assert!(!qp.custom);
        assert!(!qp.multiple);
    }
}

// -----------------------------------------------------------------
// P2-4: per-instance handler wiring
// -----------------------------------------------------------------

#[cfg(test)]
mod per_instance_tests {
    use super::*;
    use crate::ToolRegistry;

    struct EchoHandler;
    impl QuestionHandler for EchoHandler {
        fn handle(&self, questions: &[QuestionPrompt]) -> Vec<QuestionAnswer> {
            questions
                .iter()
                .map(|q| QuestionAnswer {
                    question: q.question.clone(),
                    answer: vec!["echoed".to_string()],
                    skipped: false,
                })
                .collect()
        }
    }

    #[test]
    fn instance_handler_answers_without_global_slot() {
        // Per-instance handler is the ONLY resolution path (P2-4 → D).
        let tool = QuestionTool::new().with_handler(Arc::new(EchoHandler));
        let result = tool
            .execute(serde_json::json!({
                "questions": [{"question": "Depth?"}]
            }))
            .unwrap();
        assert_eq!(result["answers"][0]["answer"][0], "echoed");
        assert_eq!(result["skipped"], 0);
    }

    #[test]
    fn install_question_handler_swaps_registry_tool() {
        let tools = ToolRegistry::new();
        tools.register(Box::new(QuestionTool::new()));
        assert!(tools.has_tool("question"));

        // Interactive path: install the handler.
        core_tools_install(&tools, Some(Arc::new(EchoHandler)));
        let out = tools
            .execute(crate::tool::ToolCall::new(
                "question",
                serde_json::json!({"questions": [{"question": "Pick one"}]}),
            ))
            .unwrap();
        let out = out.output;
        assert_eq!(out["answered"], 1);
        assert_eq!(out["answers"][0]["answer"][0], "echoed");

        // Non-interactive path: None restores the skip-all tool.
        core_tools_install(&tools, None);
        let out = tools
            .execute(crate::tool::ToolCall::new(
                "question",
                serde_json::json!({"questions": [{"question": "Pick one"}]}),
            ))
            .unwrap();
        assert_eq!(out.output["skipped"], 1);
    }

    fn core_tools_install(tools: &ToolRegistry, handler: Option<Arc<dyn QuestionHandler>>) {
        crate::tools::question::install_question_handler(tools, handler);
    }
}
