//! Tool for the agent to write notes into the persistent memory file.
//!
//! The agent calls this when it learns something worth remembering across
//! sessions: user preferences, project conventions, decisions made, etc.

use std::collections::HashMap;

use crate::memory_file::{append_project_memory, append_user_memory};
use crate::tool::{
    Concurrency, Mutability, SideEffects, Tool, ToolError, ToolMetadata, ToolParam, ToolResult,
    ToolSchema,
};

pub struct UpdateMemoryTool;

impl Default for UpdateMemoryTool {
    fn default() -> Self {
        Self::new()
    }
}

impl UpdateMemoryTool {
    pub fn new() -> Self {
        Self
    }
}

impl Tool for UpdateMemoryTool {
    fn name(&self) -> &str {
        "update_memory"
    }

    fn description(&self) -> &str {
        "Append a note to the persistent agent memory. Use this for facts \
         worth remembering across sessions: user preferences, project \
         conventions, decisions, or important context."
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "content".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some(
                    "Markdown text to append. Will be wrapped with a timestamp.".to_string(),
                ),
                default: None,
            },
        );
        params.insert(
            "scope".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some(
                    "Where to write: 'user' (global, ~/.config/agentic/memory.md) \
                     or 'project' (./.agentic/memory.md). Defaults to 'user'."
                        .to_string(),
                ),
                default: Some(serde_json::json!("user")),
            },
        );

        ToolSchema {
            name: "update_memory".to_string(),
            description: "Append a note to persistent memory (user-global or project-local)."
                .to_string(),
            parameters: params,
            required: vec!["content".to_string()],
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let args_obj = args
            .as_object()
            .ok_or_else(|| ToolError::new("Invalid arguments: expected object"))?;

        let content = args_obj
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: content"))?;

        if content.trim().is_empty() {
            return Err(ToolError::new("content must not be empty"));
        }

        let scope = args_obj
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("user");

        let path = match scope {
            "user" | "global" => append_user_memory(content)
                .map_err(|e| ToolError::new(format!("Failed to write user memory: {}", e)))?,
            "project" | "local" => {
                let cwd = std::env::current_dir()
                    .map_err(|e| ToolError::new(format!("Failed to read cwd: {}", e)))?;
                append_project_memory(&cwd, content)
                    .map_err(|e| ToolError::new(format!("Failed to write project memory: {}", e)))?
            }
            other => {
                return Err(ToolError::new(format!(
                    "Invalid scope '{}': expected 'user' or 'project'",
                    other
                )));
            }
        };

        Ok(serde_json::json!({
            "scope": scope,
            "path": path.to_string_lossy(),
            "appended_chars": content.len(),
            "success": true,
        }))
    }

    fn metadata(&self) -> ToolMetadata {
        // Writes to a file the user controls; treat as mutating so it
        // doesn't get scheduled in a parallel batch with reads.
        ToolMetadata {
            mutability: Mutability::Mutating,
            concurrency: Concurrency::Exclusive,
            idempotent: false,
            risk: 10,
            side_effects: SideEffects::FsWrite,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_memory_rejects_empty_content() {
        let t = UpdateMemoryTool::new();
        let err = t
            .execute(serde_json::json!({"content": "   "}))
            .expect_err("should reject empty");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn update_memory_rejects_unknown_scope() {
        let t = UpdateMemoryTool::new();
        let err = t
            .execute(serde_json::json!({"content": "hi", "scope": "bogus"}))
            .expect_err("should reject scope");
        assert!(err.to_string().contains("Invalid scope"));
    }

    #[test]
    fn update_memory_writes_user_scope() {
        let dir = std::env::temp_dir().join("update_memory_user_scope");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("notes.md");
        std::env::set_var("AGENTIC_MEMORY_PATH", &target);

        let t = UpdateMemoryTool::new();
        let result = t
            .execute(serde_json::json!({"content": "remember this", "scope": "user"}))
            .expect("should succeed");
        assert_eq!(result["success"], true);
        assert!(target.is_file());
        let content = std::fs::read_to_string(&target).unwrap();
        assert!(content.contains("remember this"));

        std::env::remove_var("AGENTIC_MEMORY_PATH");
    }
}
