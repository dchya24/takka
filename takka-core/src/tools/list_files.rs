//! List files tool

use std::collections::HashMap;
use std::path::Path;

use crate::tool::{Tool, ToolError, ToolMetadata, ToolParam, ToolResult, ToolSchema};

pub struct ListFilesTool;

impl Default for ListFilesTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ListFilesTool {
    pub fn new() -> Self {
        Self
    }
}

impl Tool for ListFilesTool {
    fn name(&self) -> &str {
        "list_files"
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::read_only()
    }

    fn description(&self) -> &str {
        "List files in a directory"
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "path".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("Directory path to list".to_string()),
                default: None,
            },
        );

        ToolSchema {
            name: "list_files".to_string(),
            description: "List files in a directory".to_string(),
            parameters: params,
            required: vec!["path".to_string()],
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let args_obj = args
            .as_object()
            .ok_or_else(|| ToolError::new("Invalid arguments: expected object"))?;

        let path = args_obj.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let path = Path::new(path);

        if !path.exists() {
            return Err(ToolError::new(format!(
                "Directory not found: {}",
                path.display()
            )));
        }

        if !path.is_dir() {
            return Err(ToolError::new(format!(
                "Not a directory: {}",
                path.display()
            )));
        }

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
            } else if file_type.is_file() {
                "file"
            } else if file_type.is_symlink() {
                "symlink"
            } else {
                "other"
            };

            entries.push(serde_json::json!({
                "name": file_name,
                "type": entry_type,
            }));
        }

        entries.sort_by(|a, b| {
            let a_type = a.get("type").and_then(|v| v.as_str()).unwrap_or("other");
            let b_type = b.get("type").and_then(|v| v.as_str()).unwrap_or("other");
            if a_type == "directory" && b_type != "directory" {
                std::cmp::Ordering::Less
            } else if a_type != "directory" && b_type == "directory" {
                std::cmp::Ordering::Greater
            } else {
                a.get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .cmp(b.get("name").and_then(|v| v.as_str()).unwrap_or(""))
            }
        });

        Ok(serde_json::json!({
            "path": path.to_string_lossy(),
            "entries": entries,
            "count": entries.len(),
        }))
    }
}
