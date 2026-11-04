//! Tool trait and related types

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type ToolResult<T> = std::result::Result<T, ToolError>;

#[derive(Debug, thiserror::Error)]
#[error("Tool error: {0}")]
pub struct ToolError(pub String);

impl ToolError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: HashMap<String, ToolParam>,
    pub required: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolParam {
    pub param_type: String,
    pub description: Option<String>,
    pub default: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
}

impl ToolCall {
    pub fn new(tool_name: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            tool_name: tool_name.into(),
            arguments,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultValue {
    pub tool_call_id: String,
    pub output: serde_json::Value,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Capability model
// ---------------------------------------------------------------------------
// A tool is not just a function: it is a *capability* with metadata the
// scheduler uses to decide how (and whether) calls may run. This is the
// seam P0-2 of the hardening plan builds on — `ToolRegistry::execute_batch`
// and the orchestrator's capability analysis consume these.

/// Mutability of a tool: does it change observable state?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mutability {
    /// Reads state only; safe to run concurrently with other read-only
    /// tools. Maps to the old `is_read_only() == true`.
    #[default]
    ReadOnly,
    /// Changes state (filesystem, shell, network, memory). Runs alone,
    /// sequentially, never batched with other calls.
    Mutating,
}

/// Concurrency policy for execution scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Concurrency {
    /// May run in parallel with other `ParallelSafe` tools.
    #[default]
    ParallelSafe,
    /// Must not share an execution batch with any other tool.
    Exclusive,
}

/// Side effects a tool can have, used for risk/UX classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SideEffects {
    #[default]
    None,
    /// Reads from the filesystem (never writes).
    FsRead,
    /// Writes to the filesystem.
    FsWrite,
    /// Executes shell commands / subprocesses.
    Shell,
    /// Performs network I/O.
    Network,
    /// Interacts with the user (questions, confirmation).
    UserFacing,
}

/// Static metadata describing a tool's execution contract.
///
/// The scheduler uses this instead of name-based hardcoding: a tool
/// declares its own mutability, concurrency class, idempotency, risk
/// level, and side effects. Registry-level analysis (parallel vs
/// sequential vs confirmation vs denied) reads from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolMetadata {
    pub mutability: Mutability,
    pub concurrency: Concurrency,
    pub idempotent: bool,
    /// Static risk floor contributed by this tool class (0–100).
    /// The safety engine still scores per-call args; this is the
    /// tool-level baseline.
    pub risk: u8,
    pub side_effects: SideEffects,
}

impl Default for ToolMetadata {
    /// Conservative default: mutating, exclusive, not idempotent, zero
    /// risk floor, no side effects. Used for unknown tools and for
    /// `unwrap_or_default()` at lookup sites — never assume a tool is
    /// safe to parallelize.
    fn default() -> Self {
        Self {
            mutability: Mutability::Mutating,
            concurrency: Concurrency::Exclusive,
            idempotent: false,
            risk: 0,
            side_effects: SideEffects::None,
        }
    }
}

impl ToolMetadata {
    /// Metadata for a read-only, parallel-safe tool. The common case
    /// for file/state inspection tools.
    pub const fn read_only() -> Self {
        Self {
            mutability: Mutability::ReadOnly,
            concurrency: Concurrency::ParallelSafe,
            idempotent: true,
            risk: 0,
            side_effects: SideEffects::FsRead,
        }
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> ToolSchema;
    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value>;

    /// Static capability metadata. The scheduler consumes this for
    /// batching, risk floors, and side-effect classification.
    ///
    /// The single source of truth (Fase D: `is_read_only` removed).
    /// Every implementation declares its own contract — conservative
    /// default is `Mutating + Exclusive`, never assume safety.
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::default()
    }

    /// Stream progressive output to `on_progress` as the tool runs.
    ///
    /// Default: run [`Self::execute`] atomically and ignore the callback.
    /// Tools that produce long-running output (e.g. run_command) override
    /// this to report deltas live; non-streaming tools are untouched.
    fn execute_streaming(
        &self,
        args: serde_json::Value,
        _on_progress: &dyn Fn(&str),
    ) -> ToolResult<serde_json::Value> {
        self.execute(args)
    }
}

impl ToolSchema {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters: HashMap::new(),
            required: vec![],
        }
    }

    pub fn with_param(
        self,
        name: impl Into<String>,
        param_type: impl Into<String>,
        required: bool,
    ) -> Self {
        let name = name.into();
        let mut params = self.parameters;
        params.insert(
            name.clone(),
            ToolParam {
                param_type: param_type.into(),
                description: None,
                default: None,
            },
        );
        let mut required_list = self.required;
        if required {
            required_list.push(name);
        }
        Self {
            name: self.name,
            description: self.description,
            parameters: params,
            required: required_list,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // Tool yang hanya mengimplementasikan `execute` (bukan meng-override
    // `execute_streaming`) tetap berfungsi: default mengembalikan hasil yang
    // sama dan tidak pernah memanggil on_progress.
    #[test]
    fn execute_streaming_defaults_to_execute() {
        struct Basic;
        impl Tool for Basic {
            fn name(&self) -> &str {
                "basic"
            }
            fn description(&self) -> &str {
                ""
            }
            fn schema(&self) -> ToolSchema {
                ToolSchema::new("basic", "")
            }
            fn execute(&self, _: serde_json::Value) -> ToolResult<serde_json::Value> {
                Ok(serde_json::json!({ "ok": 1 }))
            }
        }

        let tool = Basic;
        let callbacks = Arc::new(AtomicUsize::new(0));
        let c = callbacks.clone();
        // Closure harus Fn (bukan FnMut) supaya bisa di-coerce ke
        // `&dyn Fn(&str)`.
        let result = tool
            .execute_streaming(serde_json::json!({}), &move |_| {
                c.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        assert_eq!(result, serde_json::json!({ "ok": 1 }));
        assert_eq!(
            callbacks.load(Ordering::SeqCst),
            0,
            "fallback must not invoke on_progress"
        );
    }
}
