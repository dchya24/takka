//! Run command tool - executes shell commands

use std::collections::HashMap;
use std::process::Command;

use crate::tool::{
    Concurrency, Mutability, SideEffects, Tool, ToolError, ToolMetadata, ToolParam, ToolResult,
    ToolSchema,
};

pub struct RunCommandTool;

impl Default for RunCommandTool {
    fn default() -> Self {
        Self::new()
    }
}

impl RunCommandTool {
    pub fn new() -> Self {
        Self
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

        ToolSchema {
            name: "run_command".to_string(),
            description: "Execute a shell command and return its output".to_string(),
            parameters: params,
            required: vec!["command".to_string()],
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let args_obj = args
            .as_object()
            .ok_or_else(|| ToolError::new("Invalid arguments: expected object"))?;

        let command = args_obj
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: command"))?;

        let cwd = args_obj.get("cwd").and_then(|v| v.as_str());

        let output = Self::build_command(command, cwd)
            .output()
            .map_err(|e| ToolError::new(format!("Failed to execute command: {}", e)))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        Ok(serde_json::json!({
            "success": output.status.success(),
            "exit_code": output.status.code().unwrap_or(-1),
            "stdout": stdout,
            "stderr": stderr,
        }))
    }

    fn execute_streaming(
        &self,
        args: serde_json::Value,
        on_progress: &dyn Fn(&str),
    ) -> ToolResult<serde_json::Value> {
        let args_obj = args
            .as_object()
            .ok_or_else(|| ToolError::new("Invalid arguments: expected object"))?;

        let command = args_obj
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: command"))?;

        let cwd = args_obj.get("cwd").and_then(|v| v.as_str());

        let mut child = Self::build_command(command, cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| ToolError::new(format!("Failed to execute command: {}", e)))?;

        use std::io::{BufRead, BufReader};

        // Streaming stdout, lalu stderr (berurutan). Interleave dua stream
        // tidak ditangani di Fase 1 — run_command jarang memakai keduanya
        // sekaligus, dan hasil akhir tetap akurat di `stdout`/`stderr`.
        let stdout_acc = if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            let mut acc = String::new();
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        on_progress(&line);
                        acc.push_str(&line);
                        acc.push('\n');
                    }
                    Err(_) => break,
                }
            }
            acc
        } else {
            String::new()
        };

        let stderr_acc = if let Some(stderr) = child.stderr.take() {
            let reader = BufReader::new(stderr);
            let mut acc = String::new();
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        on_progress(&line);
                        acc.push_str(&line);
                        acc.push('\n');
                    }
                    Err(_) => break,
                }
            }
            acc
        } else {
            String::new()
        };

        let status = child
            .wait()
            .map_err(|e| ToolError::new(format!("Failed to wait command: {}", e)))?;

        Ok(serde_json::json!({
            "success": status.success(),
            "exit_code": status.code().unwrap_or(-1),
            "stdout": stdout_acc,
            "stderr": stderr_acc,
        }))
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
