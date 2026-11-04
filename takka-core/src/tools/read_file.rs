//! Read file tool

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::file_tracker::FileTracker;
use crate::tool::{Tool, ToolError, ToolMetadata, ToolParam, ToolResult, ToolSchema};

pub struct ReadFileTool {
    tracker: Option<Arc<FileTracker>>,
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadFileTool {
    pub fn new() -> Self {
        Self { tracker: None }
    }

    /// Build a [`ReadFileTool`] that records mtimes into a shared
    /// [`FileTracker`] for staleness detection in edit_file.
    pub fn with_tracker(tracker: Arc<FileTracker>) -> Self {
        Self {
            tracker: Some(tracker),
        }
    }
}

impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::read_only()
    }

    fn description(&self) -> &str {
        "Read the contents of a file"
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "path".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("Path to the file to read".to_string()),
                default: None,
            },
        );
        params.insert(
            "offset".to_string(),
            ToolParam {
                param_type: "number".to_string(),
                description: Some("The line number to start reading from (1-indexed)".to_string()),
                default: None,
            },
        );
        params.insert(
            "limit".to_string(),
            ToolParam {
                param_type: "number".to_string(),
                description: Some("The maximum number of lines to read".to_string()),
                default: None,
            },
        );

        ToolSchema {
            name: "read_file".to_string(),
            description: "Read the contents of a file. Supports reading a range of lines with offset and limit.".to_string(),
            parameters: params,
            required: vec!["path".to_string()],
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

        let offset = args_obj
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        let limit = args_obj
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        let path = Path::new(path);

        if !path.exists() {
            return Err(ToolError::new(format!(
                "File not found: {}",
                path.display()
            )));
        }

        if path.is_dir() {
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(path)
                .map_err(|e| ToolError::new(format!("Failed to read directory: {}", e)))?
            {
                let entry =
                    entry.map_err(|e| ToolError::new(format!("Failed to read entry: {}", e)))?;
                let file_name = entry.file_name().to_string_lossy().to_string();
                let file_type = entry
                    .file_type()
                    .map_err(|e| ToolError::new(format!("Failed to get file type: {}", e)))?;
                let entry_type = if file_type.is_dir() {
                    "directory"
                } else {
                    "file"
                };
                entries.push(format!("{} ({})", file_name, entry_type));
            }
            return Ok(serde_json::json!({
                "path": path.to_string_lossy(),
                "type": "directory",
                "entries": entries,
            }));
        }

        let content = std::fs::read_to_string(path)
            .map_err(|e| ToolError::new(format!("Failed to read file: {}", e)))?;

        // Record mtime so edit_file can detect external modifications.
        if let Some(t) = &self.tracker {
            t.mark_read(path);
        }

        let total_lines = content.lines().count();

        let lines: Vec<String> = content
            .lines()
            .enumerate()
            .filter(|(i, _)| {
                if let Some(off) = offset {
                    *i >= off.saturating_sub(1)
                } else {
                    true
                }
            })
            .take(limit.unwrap_or(2000))
            .map(|(i, line)| format!("{}: {}", i + 1, line))
            .collect();

        Ok(serde_json::json!({
            "path": path.to_string_lossy(),
            "content": lines.join("\n"),
            "total_lines": total_lines,
            "lines_shown": lines.len(),
        }))
    }
}
