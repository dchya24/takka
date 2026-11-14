//! Run command tool - executes shell commands

use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::process::{self, DEFAULT_TIMEOUT_SECS, MAX_TIMEOUT_SECS};
use crate::tool::{
    Concurrency, Mutability, SideEffects, Tool, ToolError, ToolMetadata, ToolParam, ToolResult,
    ToolSchema,
};

pub struct RunCommandTool {
    cancel: Option<Arc<AtomicBool>>,
}

impl Default for RunCommandTool {
    fn default() -> Self {
        Self::new()
    }
}

impl RunCommandTool {
    pub fn new() -> Self {
        Self { cancel: None }
    }

    /// Observe the orchestrator's cooperative cancel flag while running.
    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = Some(cancel);
        self
    }
}

impl Tool for RunCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its output"
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "command".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("The command to execute".to_string()),
                default: None,
            },
        );
        params.insert(
            "cwd".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("Working directory for command execution".to_string()),
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

        ToolSchema {
            name: "run_command".to_string(),
            description: "Execute a shell command and return its output".to_string(),
            parameters: params,
            required: vec!["command".to_string()],
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        self.run_command_impl(args, None)
    }

    fn execute_streaming(
        &self,
        args: serde_json::Value,
        on_progress: &dyn Fn(&str),
    ) -> ToolResult<serde_json::Value> {
        self.run_command_impl(args, Some(on_progress))
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

impl RunCommandTool {
    /// Shared implementation: parse args, run bounded, map to the JSON
    /// contract both `execute` and `execute_streaming` return.
    fn run_command_impl(
        &self,
        args: serde_json::Value,
        on_progress: Option<&dyn Fn(&str)>,
    ) -> ToolResult<serde_json::Value> {
        let args_obj = args
            .as_object()
            .ok_or_else(|| ToolError::new("Invalid arguments: expected object"))?;

        let command = args_obj
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: command"))?;

        let cwd = args_obj.get("cwd").and_then(|v| v.as_str());
        let timeout_secs = args_obj
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        let mut cmd = Self::build_command(command, cwd);
        let output = process::run_bounded(
            &mut cmd,
            process::clamp_timeout(timeout_secs),
            process::OUTPUT_CAP_BYTES,
            self.cancel.as_deref(),
            on_progress,
        )
        .map_err(|e| ToolError::new(format!("Failed to execute command: {}", e)))?;

        Ok(serde_json::json!({
            "success": output.success,
            "exit_code": output.exit_code,
            "stdout": output.stdout,
            "stderr": output.stderr,
        }))
    }

    /// Build a `sh -c` / `cmd /C` `Command` for the given command string.
    fn build_command(command: &str, cwd: Option<&str>) -> Command {
        if cfg!(target_os = "windows") {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", command]);
            if let Some(dir) = cwd {
                cmd.current_dir(dir);
            }
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", command]);
            if let Some(dir) = cwd {
                cmd.current_dir(dir);
            }
            cmd
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn run_command_honours_timeout() {
        let tool = RunCommandTool::new();
        let started = std::time::Instant::now();
        let result = tool
            .execute(serde_json::json!({
                "command": "sleep 5",
                "timeout": 1,
            }))
            .unwrap();
        assert_eq!(result["success"], false);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "command timeout was ignored"
        );
    }

    #[test]
    fn run_command_streams_and_keeps_json() {
        let tool = RunCommandTool::new();
        let deltas: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let d2 = deltas.clone();
        let result = tool
            .execute_streaming(
                serde_json::json!({
                    "command": "printf 'hello\\nworld\\n'",
                }),
                &move |s| d2.lock().unwrap().push(s.to_string()),
            )
            .unwrap();
        assert_eq!(result["success"], true);
        let stdout = result["stdout"].as_str().unwrap();
        assert!(
            stdout.contains("hello"),
            "stdout missing 'hello': {}",
            stdout
        );
        // Deltas harus terisi — setidaknya satu callback dipanggil.
        assert!(!deltas.lock().unwrap().is_empty());
        // Kontrak JSON dipertahankan (exit_code ada).
        assert!(result.get("exit_code").is_some());
    }
}
