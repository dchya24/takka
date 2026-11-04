//! Search files tool — full-text content search across files.
//!
//! Walks a directory tree, reads each text file, and returns lines
//! matching the query string (case-insensitive).

use std::collections::HashMap;

use crate::tool::{Tool, ToolError, ToolMetadata, ToolParam, ToolResult, ToolSchema};

pub struct SearchFilesTool;

impl Default for SearchFilesTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchFilesTool {
    pub fn new() -> Self {
        Self
    }
}

impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::read_only()
    }

    fn description(&self) -> &str {
        "Search for text content across files in a directory. Returns matching file paths, line numbers, and matching lines."
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "query".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("The text to search for".to_string()),
                default: None,
            },
        );
        params.insert(
            "path".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("Directory to search in (defaults to cwd)".to_string()),
                default: None,
            },
        );
        params.insert(
            "include".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("File pattern to include (e.g. '*.rs', '*.ts')".to_string()),
                default: None,
            },
        );
        params.insert(
            "max_results".to_string(),
            ToolParam {
                param_type: "number".to_string(),
                description: Some("Maximum number of results to return (default 50)".to_string()),
                default: Some(serde_json::json!(50)),
            },
        );

        ToolSchema {
            name: "search_files".to_string(),
            description: "Search for text content across files in a directory.".to_string(),
            parameters: params,
            required: vec!["query".to_string()],
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let args_obj = args
            .as_object()
            .ok_or_else(|| ToolError::new("Invalid arguments: expected object"))?;

        let query = args_obj
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: query"))?;

        let search_path = args_obj.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let include_pattern = args_obj
            .get("include")
            .and_then(|v| v.as_str())
            .unwrap_or("**/*");

        let max_results = args_obj
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(50) as usize;

        let query_lower = query.to_lowercase();
        let mut results: Vec<serde_json::Value> = Vec::new();

        let glob_pattern = format!("{}/{}", search_path, include_pattern);
        let paths = glob::glob(&glob_pattern)
            .map_err(|e| ToolError::new(format!("Invalid glob pattern: {}", e)))?;

        for entry in paths {
            if results.len() >= max_results {
                break;
            }

            let path = entry.map_err(|e| ToolError::new(format!("Glob error: {}", e)))?;

            if !path.is_file() {
                continue;
            }

            // Skip binary-ish files and hidden dirs
            let path_str = path.to_string_lossy();
            if path_str.contains("/.git/")
                || path_str.contains("/node_modules/")
                || path_str.contains("/target/")
            {
                continue;
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue, // Skip binary/unreadable files
            };

            for (line_num, line) in content.lines().enumerate() {
                if results.len() >= max_results {
                    break;
                }

                if line.to_lowercase().contains(&query_lower) {
                    results.push(serde_json::json!({
                        "path": path_str,
                        "line": line_num + 1,
                        "content": line.trim(),
                    }));
                }
            }
        }

        Ok(serde_json::json!({
            "query": query,
            "results": results,
            "total": results.len(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_files_schema() {
        let tool = SearchFilesTool::new();
        let schema = tool.schema();
        assert_eq!(schema.name, "search_files");
        assert!(schema.parameters.contains_key("query"));
        assert!(schema.required.contains(&"query".to_string()));
    }

    #[test]
    fn test_search_files_missing_query() {
        let tool = SearchFilesTool::new();
        let result = tool.execute(serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_search_files_in_current_dir() {
        let tool = SearchFilesTool::new();
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let result = tool.execute(serde_json::json!({
            "query": "search_files",
            "path": format!("{manifest_dir}/src/tools"),
            "include": "*.rs",
            "max_results": 5,
        }));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(!output["results"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_search_files_no_results() {
        let tool = SearchFilesTool::new();
        let result = tool.execute(serde_json::json!({
            "query": "THIS_STRING_ABSOLUTELY_DOES_NOT_EXIST_ANYWHERE_XYZZY",
            "path": "src",
            "include": "nonexistent_extension_xyz*.rs",
        }));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output["total"], 0);
    }
}
