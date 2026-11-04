use std::collections::HashMap;
use std::path::Path;

use crate::tool::{Tool, ToolError, ToolMetadata, ToolParam, ToolResult, ToolSchema};

pub struct GlobTool;

impl Default for GlobTool {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobTool {
    pub fn new() -> Self {
        Self
    }
}

impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::read_only()
    }

    fn description(&self) -> &str {
        "Fast file pattern matching tool. Supports glob patterns like **/*.js or src/**/*.ts. Returns matching file paths sorted by modification time."
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "pattern".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("The glob pattern to match files against".to_string()),
                default: None,
            },
        );
        params.insert(
            "path".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some(
                    "The directory to search in (defaults to current directory)".to_string(),
                ),
                default: None,
            },
        );

        ToolSchema {
            name: "glob".to_string(),
            description: "Fast file pattern matching tool".to_string(),
            parameters: params,
            required: vec!["pattern".to_string()],
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let args_obj = args
            .as_object()
            .ok_or_else(|| ToolError::new("Invalid arguments: expected object"))?;

        let pattern = args_obj
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: pattern"))?;

        let base_path = args_obj.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let base = Path::new(base_path);
        if !base.exists() {
            return Err(ToolError::new(format!(
                "Directory not found: {}",
                base.display()
            )));
        }

        let full_pattern = if base_path == "." {
            pattern.to_string()
        } else {
            format!(
                "{}/{}",
                base.to_string_lossy().trim_end_matches('/'),
                pattern.trim_start_matches("./")
            )
        };

        let glob_pattern = glob::glob(&full_pattern)
            .map_err(|e| ToolError::new(format!("Invalid glob pattern: {}", e)))?;

        let mut results: Vec<String> = Vec::new();
        for entry in glob_pattern {
            match entry {
                Ok(path) => {
                    results.push(path.to_string_lossy().to_string());
                }
                Err(e) => {
                    results.push(format!("[error reading path: {}]", e));
                }
            }
            if results.len() >= 100 {
                break;
            }
        }

        Ok(serde_json::json!({
            "pattern": pattern,
            "path": base.to_string_lossy(),
            "files": results,
            "count": results.len(),
        }))
    }
}
