//! Run script tool — execute multi-line scripts with optional timeout.
//!
//! Unlike `run_command` which runs a single command, this tool writes
//! a script to a temp file and executes it, supporting multi-line scripts.

use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::process::{self, DEFAULT_TIMEOUT_SECS, MAX_TIMEOUT_SECS};
use crate::tool::{
    Concurrency, Mutability, SideEffects, Tool, ToolError, ToolMetadata, ToolParam, ToolResult,
    ToolSchema,
};

pub struct RunScriptTool {
    cancel: Option<Arc<AtomicBool>>,
}

impl Default for RunScriptTool {
    fn default() -> Self {
        Self::new()
    }
}

impl RunScriptTool {
    pub fn new() -> Self {
        Self { cancel: None }
    }

    /// Observe the orchestrator's cooperative cancel flag while running.
    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = Some(cancel);
        self
    }
}

impl Tool for RunScriptTool {
    fn name(&self) -> &str {
        "run_script"
    }

    fn description(&self) -> &str {
        "Execute a multi-line script (bash/sh) and return its output. Supports timeout."
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "script".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("The script content to execute".to_string()),
                default: None,
            },
        );
        params.insert(
            "cwd".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("Working directory for script execution".to_string()),
                default: None,
            },
        );
        params.insert(
            "timeout".to_string(),
            ToolParam {
                param_type: "number".to_string(),
                description: Some(format!(
                    "Timeout in seconds (default {}, max {})",
                    DEFAULT_TIMEOUT_SECS, MAX_TIMEOUT_SECS
                )),
                default: Some(serde_json::json!(DEFAULT_TIMEOUT_SECS)),
            },
        );
        params.insert(
            "interpreter".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some(
                    "Interpreter to use (default 'sh'). Use 'bash', 'python3', etc.".to_string(),
                ),
                default: Some(serde_json::json!("sh")),
            },
        );

        ToolSchema {
            name: "run_script".to_string(),
            description: "Execute a multi-line script and return its output.".to_string(),
            parameters: params,
            required: vec!["script".to_string()],
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        self.run_script_impl(args, None)
    }

    fn execute_streaming(
        &self,
        args: serde_json::Value,
        on_progress: &dyn Fn(&str),
    ) -> ToolResult<serde_json::Value> {
        self.run_script_impl(args, Some(on_progress))
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            mutability: Mutability::Mutating,
            concurrency: Concurrency::Exclusive,
            idempotent: false,
            risk: 40,
            side_effects: SideEffects::Shell,
        }
    }
}

impl RunScriptTool {
    /// Shared implementation: parse args, write temp script, run the
    /// interpreter, clean up. Streams each output line to `on_progress`
    /// when provided; otherwise runs atomically (same output as before).
    fn run_script_impl(
        &self,
        args: serde_json::Value,
        on_progress: Option<&dyn Fn(&str)>,
    ) -> ToolResult<serde_json::Value> {
        let args_obj = args
            .as_object()
            .ok_or_else(|| ToolError::new("Invalid arguments: expected object"))?;

        let script = args_obj
            .get("script")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: script"))?;

        let cwd = args_obj.get("cwd").and_then(|v| v.as_str());
        let timeout_secs = args_obj
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let interpreter = args_obj
            .get("interpreter")
            .and_then(|v| v.as_str())
            .unwrap_or("sh");

        // Write script to temp file
        let suffix = match interpreter {
            "python3" | "python" => ".py",
            "node" => ".js",
            _ => ".sh",
        };

        let temp_dir = std::env::temp_dir().join("takka-core-scripts");
        std::fs::create_dir_all(&temp_dir)
            .map_err(|e| ToolError::new(format!("Failed to create temp dir: {}", e)))?;

        let script_id = uuid::Uuid::new_v4().to_string();
        let script_path = temp_dir.join(format!("{}{}", script_id, suffix));

        let mut file = std::fs::File::create(&script_path)
            .map_err(|e| ToolError::new(format!("Failed to create script file: {}", e)))?;
        file.write_all(script.as_bytes())
            .map_err(|e| ToolError::new(format!("Failed to write script: {}", e)))?;
        drop(file);

        // Execute
        let result =
            self.run_interpreter(interpreter, &script_path, cwd, timeout_secs, on_progress);

        // Cleanup
        let _ = std::fs::remove_file(&script_path);

        result
    }
}

impl RunScriptTool {
    fn run_interpreter(
        &self,
        interpreter: &str,
        script_path: &std::path::Path,
        cwd: Option<&str>,
        timeout_secs: u64,
        on_progress: Option<&dyn Fn(&str)>,
    ) -> ToolResult<serde_json::Value> {
        let mut cmd = Command::new(interpreter);
        cmd.arg(script_path);

        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        let output = process::run_bounded(
            &mut cmd,
            process::clamp_timeout(timeout_secs),
            process::OUTPUT_CAP_BYTES,
            self.cancel.as_deref(),
            on_progress,
        )
        .map_err(|e| ToolError::new(format!("Failed to execute script: {}", e)))?;

        Ok(serde_json::json!({
            "success": output.success,
            "exit_code": output.exit_code,
            "stdout": output.stdout,
            "stderr": output.stderr,
            "interpreter": interpreter,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_script_schema() {
        let tool = RunScriptTool::new();
        let schema = tool.schema();
        assert_eq!(schema.name, "run_script");
        assert!(schema.parameters.contains_key("script"));
        assert!(schema.required.contains(&"script".to_string()));
    }

    #[test]
    fn test_run_script_missing_script() {
        let tool = RunScriptTool::new();
        let result = tool.execute(serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_run_script_echo() {
        let tool = RunScriptTool::new();
        let result = tool.execute(serde_json::json!({
            "script": "echo hello\necho world",
            "interpreter": "sh",
        }));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output["success"], true);
        assert!(output["stdout"].as_str().unwrap().contains("hello"));
        assert!(output["stdout"].as_str().unwrap().contains("world"));
    }

    #[test]
    fn test_run_script_multiline() {
        let tool = RunScriptTool::new();
        let result = tool.execute(serde_json::json!({
            "script": "for i in 1 2 3; do echo \"line $i\"; done",
        }));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output["success"], true);
        let stdout = output["stdout"].as_str().unwrap();
        assert!(stdout.contains("line 1"));
        assert!(stdout.contains("line 2"));
        assert!(stdout.contains("line 3"));
    }

    #[test]
    fn test_run_script_failure() {
        let tool = RunScriptTool::new();
        let result = tool.execute(serde_json::json!({
            "script": "exit 42",
        }));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output["success"], false);
        assert_eq!(output["exit_code"], 42);
    }

    #[test]
    fn run_script_honours_timeout() {
        let tool = RunScriptTool::new();
        let started = std::time::Instant::now();
        let result = tool
            .execute(serde_json::json!({
                "script": "sleep 5",
                "timeout": 1,
            }))
            .unwrap();
        assert_eq!(result["success"], false);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "script timeout was ignored"
        );
    }

    #[test]
    fn run_script_streams_echo_lines() {
        use std::sync::{Arc, Mutex};
        let tool = RunScriptTool::new();
        let deltas: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let d = deltas.clone();
        let result = tool
            .execute_streaming(
                serde_json::json!({
                    "script": "echo one\necho two",
                    "interpreter": "sh",
                }),
                &move |line| d.lock().unwrap().push(line.to_string()),
            )
            .unwrap();
        assert_eq!(result["success"], true);
        let stdout = result["stdout"].as_str().unwrap();
        assert!(
            stdout.contains("one") && stdout.contains("two"),
            "bad stdout: {}",
            stdout
        );
        assert!(!deltas.lock().unwrap().is_empty(), "expected live deltas");
    }
}
