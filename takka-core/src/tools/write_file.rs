//! Write file tool

use std::collections::HashMap;
use std::path::Path;

use crate::tool::{
    Concurrency, Mutability, SideEffects, Tool, ToolError, ToolMetadata, ToolParam, ToolResult,
    ToolSchema,
};

pub struct WriteFileTool;

impl Default for WriteFileTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteFileTool {
    pub fn new() -> Self {
        Self
    }
}

impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write content to a file"
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "path".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("Path to the file to write".to_string()),
                default: None,
            },
        );
        params.insert(
            "content".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("Content to write to the file".to_string()),
                default: None,
            },
        );

        ToolSchema {
            name: "write_file".to_string(),
            description: "Write content to a file".to_string(),
            parameters: params,
            required: vec!["path".to_string(), "content".to_string()],
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let args_obj = args
            .as_object()
            .ok_or_else(|| ToolError::new("Invalid arguments: expected object"))?;

        let path = args_obj
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: path"))?;

        let content = args_obj
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: content"))?;

        let path = Path::new(path);

        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| ToolError::new(format!("Failed to create directory: {}", e)))?;
            }
        }

        // Capture previous content (if any) so we can produce a real diff.
        // Treat "file does not exist" as an empty before-state so creates
        // surface as `+N` lines added.
        let before = std::fs::read_to_string(path).unwrap_or_default();
        let is_create = !path.exists() || before.is_empty();

        std::fs::write(path, content)
            .map_err(|e| ToolError::new(format!("Failed to write file: {}", e)))?;

        let path_label = path.to_string_lossy().to_string();
        let diff = crate::diff_util::unified_diff(&path_label, &before, content, 3);
        let stats = crate::diff_util::change_summary(&before, content);

        Ok(serde_json::json!({
            "path": path_label,
            "success": true,
            "bytes_written": content.len(),
            "created": is_create,
            "diff": diff,
            "lines_added": stats.added,
            "lines_removed": stats.removed,
        }))
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            mutability: Mutability::Mutating,
            concurrency: Concurrency::Exclusive,
            idempotent: true,
            risk: 25,
            side_effects: SideEffects::FsWrite,
        }
    }
}
