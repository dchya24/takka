use std::collections::HashMap;
use std::path::Path;

use regex::Regex;

use crate::tool::{Tool, ToolError, ToolMetadata, ToolParam, ToolResult, ToolSchema};

pub struct GrepTool;

impl Default for GrepTool {
    fn default() -> Self {
        Self::new()
    }
}

impl GrepTool {
    pub fn new() -> Self {
        Self
    }
}

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::read_only()
    }

    fn description(&self) -> &str {
        "Fast content search tool using regular expressions. Returns file paths and line numbers with matches sorted by modification time."
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "pattern".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("The regex pattern to search for in file contents".to_string()),
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
        params.insert(
            "include".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("File pattern to include (e.g. *.js, *.{ts,tsx})".to_string()),
                default: None,
            },
        );

        ToolSchema {
            name: "grep".to_string(),
            description: "Fast content search tool using regular expressions".to_string(),
            parameters: params,
            required: vec!["pattern".to_string()],
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let args_obj = args
            .as_object()
            .ok_or_else(|| ToolError::new("Invalid arguments: expected object"))?;

        let pattern_str = args_obj
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: pattern"))?;

        let base_path = args_obj.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let include_pattern = args_obj.get("include").and_then(|v| v.as_str());

        let re = Regex::new(pattern_str)
            .map_err(|e| ToolError::new(format!("Invalid regex pattern: {}", e)))?;

        let base = Path::new(base_path);
        if !base.exists() {
            return Err(ToolError::new(format!(
                "Directory not found: {}",
                base.display()
            )));
        }

        let glob_str = match include_pattern {
            Some(p) => format!(
                "{}/{}",
                base.to_string_lossy().trim_end_matches("/"),
                p.trim_start_matches("./")
            ),
            None => format!("{}/**/*", base.to_string_lossy().trim_end_matches('/')),
        };

        let glob_pattern = glob::glob(&glob_str)
            .map_err(|e| ToolError::new(format!("Invalid file pattern: {}", e)))?;

        let mut results: Vec<serde_json::Value> = Vec::new();

        for entry in glob_pattern {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !entry.is_file() {
                continue;
            }

            let content = match std::fs::read_to_string(&entry) {
                Ok(c) => c,
                Err(_) => continue,
            };

            for (line_num, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    results.push(serde_json::json!({
                        "file": entry.to_string_lossy(),
                        "line": line_num + 1,
                        "content": line,
                    }));

                    if results.len() >= 200 {
                        break;
                    }
                }
            }

            if results.len() >= 200 {
                break;
            }
        }

        Ok(serde_json::json!({
            "pattern": pattern_str,
            "path": base.to_string_lossy(),
            "matches": results,
            "count": results.len(),
        }))
    }
}
