//! Tool registry for dynamic tool registration and execution

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::context::builder::truncate_tool_result;
use crate::providers::{ToolDefinition, ToolFunction};
use crate::tool::{Tool, ToolCall, ToolError, ToolResultValue};

pub struct ToolRegistry {
    /// `RwLock` lets multiple read-only tool calls proceed concurrently.
    /// Tool::execute takes &self, so no exclusive lock is needed during
    /// execution. We only acquire the write lock for register/unregister.
    tools: Arc<RwLock<HashMap<String, Box<dyn Tool + Send + Sync>>>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn register(&self, tool: Box<dyn Tool + Send + Sync>) {
        let mut tools = self.tools.write();
        tools.insert(tool.name().to_string(), tool);
    }

    pub fn unregister(&self, name: &str) -> Option<Box<dyn Tool + Send + Sync>> {
        let mut tools = self.tools.write();
        tools.remove(name)
    }

    pub fn list(&self) -> Vec<crate::tool::ToolSchema> {
        let tools = self.tools.read();
        tools.values().map(|t| t.schema()).collect()
    }

    pub fn execute(&self, call: ToolCall) -> Result<ToolResultValue, ToolError> {
        let tools = self.tools.read();

        let tool = tools
            .get(&call.tool_name)
            .ok_or_else(|| ToolError::new(format!("Tool not found: {}", call.tool_name)))?;

        let result = tool
            .execute(call.arguments)
            .map_err(|e| ToolError::new(e.to_string()))?;

        Ok(ToolResultValue {
            tool_call_id: call.id,
            output: result,
            error: None,
        })
    }

    pub fn has_tool(&self, name: &str) -> bool {
        let tools = self.tools.read();
        tools.contains_key(name)
    }

    /// Static capability metadata for the named tool. Unknown tools
    /// default to the conservative `Mutating + Exclusive` contract.
    pub fn metadata(&self, name: &str) -> crate::tool::ToolMetadata {
        let tools = self.tools.read();
        tools.get(name).map(|t| t.metadata()).unwrap_or_default()
    }

    /// Whether the named tool may be batched with other tools.
    pub fn concurrency_class(&self, name: &str) -> crate::tool::Concurrency {
        self.metadata(name).concurrency
    }

    pub fn tool_names(&self) -> Vec<String> {
        let tools = self.tools.read();
        tools.keys().cloned().collect()
    }

    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let tools = self.tools.read();
        tools
            .values()
            .map(|t| {
                let schema = t.schema();
                let mut properties = serde_json::Map::new();
                for (name, param) in &schema.parameters {
                    let mut prop = serde_json::json!({
                        "type": param.param_type,
                    });
                    if let Some(desc) = &param.description {
                        prop["description"] = serde_json::json!(desc);
                    }
                    properties.insert(name.clone(), prop);
                }
                ToolDefinition {
                    tool_type: "function".into(),
                    function: ToolFunction {
                        name: schema.name.clone(),
                        description: schema.description.clone(),
                        parameters: serde_json::json!({
                            "type": "object",
                            "properties": properties,
                            "required": schema.required,
                        }),
                    },
                }
            })
            .collect()
    }

    pub fn register_mcp_server(
        &self,
        config: &crate::mcp::types::McpServerConfig,
    ) -> Result<(), String> {
        let tools = crate::mcp::tool_adapter::mcp_tools_from_config(config)?;
        for tool in tools {
            self.register(tool);
        }
        Ok(())
    }

    pub fn execute_by_name(
        &self,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let tools = self.tools.read();
        let tool = tools
            .get(name)
            .ok_or_else(|| ToolError::new(format!("Tool not found: {}", name)))?;
        tool.execute(args.clone())
    }

    /// Execute a tool by name, streaming progress deltas through
    /// `on_progress`. For tools without a streaming override this routes
    /// to the default (atomic) execution and never calls `on_progress`.
    pub fn execute_streaming_by_name(
        &self,
        name: &str,
        args: &serde_json::Value,
        on_progress: &dyn Fn(&str),
    ) -> Result<serde_json::Value, ToolError> {
        let tools = self.tools.read();
        let tool = tools
            .get(name)
            .ok_or_else(|| ToolError::new(format!("Tool not found: {}", name)))?;
        tool.execute_streaming(args.clone(), on_progress)
    }

    // -----------------------------------------------------------------------
    // Capability analysis & batched execution
    // -----------------------------------------------------------------------
    // The scheduler asks the registry to classify a batch of calls and
    // then executes the parallel-safe slice. Mutating/exclusive tools
    // are always returned sequentially so the caller can interleave
    // them in original order.

    /// Classify a slice of tool names into parallel-safe vs exclusive
    /// groups. Read-only parallel-safe tools land in the first group;
    /// everything else lands in the second. Order is preserved within
    /// each group.
    pub fn capability_analysis(&self, names: &[&str]) -> CapabilityAnalysis {
        let tools = self.tools.read();
        let mut parallel_safe: Vec<usize> = Vec::new();
        let mut exclusive: Vec<usize> = Vec::new();
        let mut max_risk: u8 = 0;
        for (i, name) in names.iter().enumerate() {
            let meta = tools.get(*name).map(|t| t.metadata()).unwrap_or_default();
            max_risk = max_risk.max(meta.risk);
            match meta.concurrency {
                crate::tool::Concurrency::ParallelSafe => parallel_safe.push(i),
                crate::tool::Concurrency::Exclusive => exclusive.push(i),
            }
        }
        CapabilityAnalysis {
            parallel_safe,
            exclusive,
            max_risk,
        }
    }

    /// Plan execution batches for a sequence of calls (P0-2 of the
    /// hardening plan).
    ///
    /// Consecutive parallel-safe [`ScheduleEntry::Execute`] entries
    /// group into a single concurrent batch; exclusive tools and
    /// [`ScheduleEntry::PreResolved`] entries break runs and get
    /// singleton batches. Batches are returned in execution order and
    /// `indices` are positions in `entries`, so results stay aligned
    /// with the original tool-call order no matter which batch finishes
    /// first.
    pub fn plan_batches(&self, entries: &[ScheduleEntry]) -> Vec<ScheduleBatch> {
        let tools = self.tools.read();
        let class = |name: &str| {
            tools
                .get(name)
                .map(|t| t.metadata())
                .unwrap_or_default()
                .concurrency
        };

        let mut batches = Vec::new();
        let mut i = 0;
        while i < entries.len() {
            match &entries[i] {
                ScheduleEntry::PreResolved => {
                    batches.push(ScheduleBatch {
                        indices: vec![i],
                        parallel: false,
                    });
                    i += 1;
                }
                ScheduleEntry::Execute(name) => {
                    if class(name) == crate::tool::Concurrency::ParallelSafe {
                        let start = i;
                        let mut end = i + 1;
                        while end < entries.len() {
                            match &entries[end] {
                                ScheduleEntry::Execute(n)
                                    if class(n) == crate::tool::Concurrency::ParallelSafe =>
                                {
                                    end += 1
                                }
                                _ => break,
                            }
                        }
                        batches.push(ScheduleBatch {
                            indices: (start..end).collect(),
                            parallel: true,
                        });
                        i = end;
                    } else {
                        batches.push(ScheduleBatch {
                            indices: vec![i],
                            parallel: false,
                        });
                        i += 1;
                    }
                }
            }
        }
        batches
    }

    /// Execute a parallel batch of calls concurrently via scoped
    /// threads, returning outcomes **in the same order as `calls`**.
    ///
    /// A single call runs inline (cheaper than a spawn). A panicking
    /// tool becomes a synthetic error outcome for its own slot — the
    /// loop still gets a response for every call. Output longer than
    /// `max_chars` is truncated (same policy the orchestrator applies
    /// to its own executions).
    pub fn execute_batch(&self, calls: &[BatchCall], max_chars: usize) -> Vec<BatchOutcome> {
        let run_one = |call: &BatchCall| -> BatchOutcome {
            let start = std::time::Instant::now();
            let raw = match self.execute_by_name(&call.name, &call.args) {
                Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()),
                Err(e) => format!("Tool error: {}", e),
            };
            let truncated = raw.len() > max_chars;
            BatchOutcome {
                name: call.name.clone(),
                id: call.id.clone(),
                output: truncate_tool_result(&raw, max_chars),
                duration_ms: start.elapsed().as_millis() as u64,
                success: !raw.starts_with("Tool error"),
                truncated,
            }
        };

        if calls.len() <= 1 {
            return calls.iter().map(run_one).collect();
        }

        // Panics are caught per call so slot identity is never lost.
        std::thread::scope(|s| {
            let handles: Vec<_> = calls
                .iter()
                .map(|call| {
                    let call = call.clone();
                    s.spawn(move || {
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            run_one(&call)
                        })) {
                            Ok(outcome) => outcome,
                            Err(_) => BatchOutcome {
                                name: call.name,
                                id: call.id,
                                output: "Tool error: task panicked".to_string(),
                                duration_ms: 0,
                                success: false,
                                truncated: false,
                            },
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or_else(|_| BatchOutcome {
                        name: String::new(),
                        id: String::new(),
                        output: "Tool error: task panicked".to_string(),
                        duration_ms: 0,
                        success: false,
                        truncated: false,
                    })
                })
                .collect()
        })
    }
}

/// One entry the scheduler plans over: either a tool to execute, or a
/// call the caller already resolved (denied / skipped) that must never
/// run but still occupies its position in the result order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleEntry {
    Execute(String),
    PreResolved,
}

/// A group of entries that run together, produced by
/// [`ToolRegistry::plan_batches`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleBatch {
    /// Positions in the entry list, in order.
    pub indices: Vec<usize>,
    /// Whether the members may run concurrently. `false` marks a
    /// singleton batch (exclusive tool or pre-resolved slot).
    pub parallel: bool,
}

/// One executable call inside a batch.
#[derive(Debug, Clone)]
pub struct BatchCall {
    pub name: String,
    pub id: String,
    pub args: serde_json::Value,
}

/// Result of one executed call — what the orchestrator records as the
/// tool message and surfaces via `Event::ToolOutput`.
#[derive(Debug, Clone)]
pub struct BatchOutcome {
    pub name: String,
    pub id: String,
    /// Already truncated to the caller's max-chars policy.
    pub output: String,
    pub duration_ms: u64,
    pub success: bool,
    pub truncated: bool,
}

/// Result of [`ToolRegistry::capability_analysis`]: indices into the
/// original call list, grouped by concurrency class.
#[derive(Debug, Clone, Default)]
pub struct CapabilityAnalysis {
    /// Indices of calls that may run in parallel with each other.
    pub parallel_safe: Vec<usize>,
    /// Indices of calls that must run alone, sequentially.
    pub exclusive: Vec<usize>,
    /// Highest static tool-level risk floor in the slice (0–100). The
    /// safety engine still scores per-call args on top of this baseline.
    pub max_risk: u8,
}

impl Clone for ToolRegistry {
    fn clone(&self) -> Self {
        Self {
            tools: self.tools.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolError, ToolSchema};

    struct ReadTool;
    impl Tool for ReadTool {
        fn name(&self) -> &str {
            "reader"
        }
        fn description(&self) -> &str {
            ""
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema::new("reader", "")
        }
        fn execute(&self, _: serde_json::Value) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({"ok": true}))
        }
        fn metadata(&self) -> crate::tool::ToolMetadata {
            crate::tool::ToolMetadata::read_only()
        }
    }

    struct WriteTool;
    impl Tool for WriteTool {
        fn name(&self) -> &str {
            "writer"
        }
        fn description(&self) -> &str {
            ""
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema::new("writer", "")
        }
        fn execute(&self, _: serde_json::Value) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({"ok": true}))
        }
        // Declares the same contract as the builtin write_file tool.
        fn metadata(&self) -> crate::tool::ToolMetadata {
            crate::tool::ToolMetadata {
                mutability: crate::tool::Mutability::Mutating,
                concurrency: crate::tool::Concurrency::Exclusive,
                idempotent: false,
                risk: 25,
                side_effects: crate::tool::SideEffects::FsWrite,
            }
        }
    }

    #[test]
    fn read_only_flag_propagates_through_registry() {
        let reg = ToolRegistry::new();
        reg.register(Box::new(ReadTool));
        reg.register(Box::new(WriteTool));

        assert_eq!(
            reg.metadata("reader").mutability,
            crate::tool::Mutability::ReadOnly
        );
        assert_eq!(
            reg.metadata("writer").mutability,
            crate::tool::Mutability::Mutating
        );
    }

    #[test]
    fn unknown_tool_treated_as_mutating() {
        let reg = ToolRegistry::new();
        assert_eq!(
            reg.metadata("nonexistent").mutability,
            crate::tool::Mutability::Mutating,
            "unknown tools default to the conservative contract"
        );
    }

    #[test]
    fn builtin_read_tools_are_marked_read_only() {
        let reg = ToolRegistry::new();
        for tool in crate::tools::builtin_tools() {
            reg.register(tool);
        }
        for name in ["read_file", "list_files", "glob", "grep", "search_files"] {
            assert_eq!(
                reg.metadata(name).mutability,
                crate::tool::Mutability::ReadOnly,
                "{} should be read-only",
                name
            );
        }
        for name in ["write_file", "edit_file", "run_command", "run_script"] {
            assert_eq!(
                reg.metadata(name).mutability,
                crate::tool::Mutability::Mutating,
                "{} should be mutating",
                name
            );
        }
    }

    #[test]
    fn concurrent_read_only_calls_dont_deadlock() {
        // RwLock should allow concurrent reads. This test exercises the
        // exact pattern handle_tool_calls_parallel uses: clone + execute.
        use std::sync::Arc;
        let reg = Arc::new(ToolRegistry::new());
        reg.register(Box::new(ReadTool));

        let mut threads = Vec::new();
        for _ in 0..16 {
            let r = reg.clone();
            threads.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    r.execute_by_name("reader", &serde_json::json!({})).unwrap();
                }
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
    }

    // Tool yang override execute_streaming untuk memancarkan dua delta;
    // registry harus meneruskan callback apa adanya.
    struct Counter;
    impl Tool for Counter {
        fn name(&self) -> &str {
            "counter"
        }
        fn description(&self) -> &str {
            "dummy"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema::new("counter", "dummy")
        }
        fn execute(&self, _: serde_json::Value) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({ "ok": true }))
        }
        fn execute_streaming(
            &self,
            _: serde_json::Value,
            on_progress: &dyn Fn(&str),
        ) -> Result<serde_json::Value, ToolError> {
            on_progress("alpha");
            on_progress("beta");
            Ok(serde_json::json!({ "ok": true }))
        }
    }

    #[test]
    fn registry_forwards_streaming_deltas() {
        let reg = ToolRegistry::new();
        reg.register(Box::new(Counter));
        let deltas = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let d2 = deltas.clone();
        let result = reg
            .execute_streaming_by_name("counter", &serde_json::json!({}), &move |s| {
                d2.lock().push(s.to_string());
            })
            .unwrap();
        assert_eq!(result, serde_json::json!({ "ok": true }));
        assert_eq!(*deltas.lock(), vec!["alpha", "beta"]);
    }

    // -----------------------------------------------------------------------
    // Capability model (P0-2)
    // -----------------------------------------------------------------------

    /// Interactive tool: read-only but must run exclusively (never batched).
    struct QuestionLike;
    impl Tool for QuestionLike {
        fn name(&self) -> &str {
            "question_like"
        }
        fn description(&self) -> &str {
            ""
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema::new("question_like", "")
        }
        fn execute(&self, _: serde_json::Value) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({"ok": true}))
        }
        fn metadata(&self) -> crate::tool::ToolMetadata {
            crate::tool::ToolMetadata {
                mutability: crate::tool::Mutability::ReadOnly,
                concurrency: crate::tool::Concurrency::Exclusive,
                idempotent: false,
                risk: 0,
                side_effects: crate::tool::SideEffects::UserFacing,
            }
        }
    }

    #[test]
    fn trait_metadata_default_is_conservative() {
        // Fase D: no is_read_only derivation — a tool without an
        // explicit metadata() override gets Mutating + Exclusive, so
        // forgetting to declare a contract can never grant parallelism.
        struct UndeclaredTool;
        impl Tool for UndeclaredTool {
            fn name(&self) -> &str {
                "undeclared"
            }
            fn description(&self) -> &str {
                ""
            }
            fn schema(&self) -> ToolSchema {
                ToolSchema::new("undeclared", "")
            }
            fn execute(&self, _: serde_json::Value) -> Result<serde_json::Value, ToolError> {
                Ok(serde_json::json!({}))
            }
        }

        let meta = UndeclaredTool.metadata();
        assert_eq!(meta.mutability, crate::tool::Mutability::Mutating);
        assert_eq!(meta.concurrency, crate::tool::Concurrency::Exclusive);

        let reg = ToolRegistry::new();
        reg.register(Box::new(UndeclaredTool));
        assert_eq!(
            reg.concurrency_class("undeclared"),
            crate::tool::Concurrency::Exclusive
        );
    }

    #[test]
    fn capability_analysis_groups_by_concurrency() {
        let reg = ToolRegistry::new();
        reg.register(Box::new(ReadTool)); // parallel-safe
        reg.register(Box::new(WriteTool)); // exclusive
        reg.register(Box::new(QuestionLike)); // read-only but exclusive

        let names = ["reader", "writer", "question_like", "reader"];
        let analysis = reg.capability_analysis(&names);

        // Indices of the two readers.
        assert_eq!(analysis.parallel_safe, vec![0, 3]);
        // writer + question_like are exclusive, in original order.
        assert_eq!(analysis.exclusive, vec![1, 2]);
        // Risk floor: the highest tool-level baseline in the slice wins
        // (writer > question_like > reader).
        let write_meta = reg.metadata("writer");
        assert!(analysis.max_risk >= write_meta.risk);
    }

    #[test]
    fn capability_analysis_max_risk_reflects_highest_tool_floor() {
        // A read-only slice has a zero risk floor; adding one mutating
        // tool raises the analysis to that tool's floor.
        let reg = ToolRegistry::new();
        reg.register(Box::new(ReadTool));
        reg.register(Box::new(WriteTool));

        let read_only = reg.capability_analysis(&["reader", "reader"]);
        assert_eq!(read_only.max_risk, 0);

        let write_meta = reg.metadata("writer");
        assert!(write_meta.risk > 0, "test tool must declare a risk floor");
        let mixed = reg.capability_analysis(&["reader", "writer"]);
        assert_eq!(mixed.max_risk, write_meta.risk);
    }

    #[test]
    fn unknown_tool_analysis_is_exclusive() {
        let reg = ToolRegistry::new();
        let analysis = reg.capability_analysis(&["nope", "also_nope"]);
        assert!(analysis.parallel_safe.is_empty());
        assert_eq!(analysis.exclusive, vec![0, 1]);
    }

    #[test]
    fn builtin_question_tool_is_exclusive() {
        // Regression: `question` is read-only but must never batch with
        // other tools (it blocks on user input).
        let reg = ToolRegistry::new();
        for tool in crate::tools::builtin_tools() {
            reg.register(tool);
        }
        assert_eq!(
            reg.concurrency_class("question"),
            crate::tool::Concurrency::Exclusive
        );
        assert_eq!(
            reg.concurrency_class("read_file"),
            crate::tool::Concurrency::ParallelSafe
        );
    }

    // -----------------------------------------------------------------------
    // Batch scheduling & execution (P0-2)
    // -----------------------------------------------------------------------

    #[test]
    fn plan_batches_groups_consecutive_parallel_safe() {
        let reg = ToolRegistry::new();
        reg.register(Box::new(ReadTool));
        reg.register(Box::new(WriteTool));

        let entries = vec![
            ScheduleEntry::Execute("reader".into()),
            ScheduleEntry::Execute("reader".into()),
            ScheduleEntry::Execute("writer".into()),
            ScheduleEntry::Execute("reader".into()),
        ];
        let batches = reg.plan_batches(&entries);
        assert_eq!(batches.len(), 3);
        // The two consecutive readers form one parallel batch.
        assert_eq!(batches[0].indices, vec![0, 1]);
        assert!(batches[0].parallel);
        // The writer breaks the run and executes alone.
        assert_eq!(batches[1].indices, vec![2]);
        assert!(!batches[1].parallel);
        // A trailing reader is still parallel-capable, just alone.
        assert_eq!(batches[2].indices, vec![3]);
        assert!(batches[2].parallel);
    }

    #[test]
    fn plan_batches_pre_resolved_breaks_runs() {
        let reg = ToolRegistry::new();
        reg.register(Box::new(ReadTool));

        let entries = vec![
            ScheduleEntry::Execute("reader".into()),
            ScheduleEntry::PreResolved,
            ScheduleEntry::Execute("reader".into()),
        ];
        let batches = reg.plan_batches(&entries);
        // Never executed, but still occupies its position in the order.
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[1].indices, vec![1]);
        assert!(!batches[1].parallel);
    }

    #[test]
    fn plan_batches_unknown_tool_is_exclusive() {
        let reg = ToolRegistry::new();
        let batches = reg.plan_batches(&[
            ScheduleEntry::Execute("nope".into()),
            ScheduleEntry::Execute("nope".into()),
        ]);
        assert_eq!(batches.len(), 2);
        assert!(batches.iter().all(|b| !b.parallel && b.indices.len() == 1));
    }

    #[test]
    fn plan_batches_read_only_but_exclusive_never_batches() {
        // `question`-style tools: read-only metadata, exclusive contract.
        let reg = ToolRegistry::new();
        reg.register(Box::new(QuestionLike));
        let batches = reg.plan_batches(&[
            ScheduleEntry::Execute("question_like".into()),
            ScheduleEntry::Execute("question_like".into()),
        ]);
        assert_eq!(batches.len(), 2);
        assert!(batches.iter().all(|b| !b.parallel));
    }

    #[test]
    fn execute_batch_preserves_order_and_ids() {
        let reg = ToolRegistry::new();
        reg.register(Box::new(ReadTool));
        reg.register(Box::new(WriteTool));

        let calls = vec![
            BatchCall {
                name: "reader".into(),
                id: "a".into(),
                args: serde_json::json!({}),
            },
            BatchCall {
                name: "writer".into(),
                id: "b".into(),
                args: serde_json::json!({}),
            },
            BatchCall {
                name: "reader".into(),
                id: "c".into(),
                args: serde_json::json!({}),
            },
        ];
        let out = reg.execute_batch(&calls, 25_000);
        assert_eq!(out.len(), 3);
        assert_eq!(
            out.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert!(out.iter().all(|o| o.success));
    }

    #[test]
    fn mixed_schedule_runs_mutating_alone_in_original_order() {
        // P0-2 verification: a mixed [read, read, write, read] sequence
        // schedules as three batches; the mutating write runs alone and
        // slot order stays aligned with the original call order.
        let reg = ToolRegistry::new();
        reg.register(Box::new(ReadTool));
        reg.register(Box::new(WriteTool));

        let entries = vec!["reader", "reader", "writer", "reader"]
            .into_iter()
            .map(|n| ScheduleEntry::Execute(n.into()))
            .collect::<Vec<_>>();
        let batches = reg.plan_batches(&entries);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].indices, vec![0, 1]);
        assert!(batches[0].parallel);
        assert_eq!(batches[1].indices, vec![2]);
        assert!(!batches[1].parallel);
        assert_eq!(batches[2].indices, vec![3]);
        assert!(batches[2].parallel);

        // Executing the parallel slice preserves call order + ids.
        let calls: Vec<BatchCall> = batches[0]
            .indices
            .iter()
            .map(|&i| BatchCall {
                name: "reader".into(),
                id: format!("slot-{}", i),
                args: serde_json::json!({}),
            })
            .collect();
        let out = reg.execute_batch(&calls, 25_000);
        assert_eq!(
            out.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
            vec!["slot-0", "slot-1"]
        );
        assert!(out.iter().all(|o| o.success));
    }

    #[test]
    fn execute_batch_truncates_long_output() {
        struct LoudTool;
        impl Tool for LoudTool {
            fn name(&self) -> &str {
                "loud"
            }
            fn description(&self) -> &str {
                ""
            }
            fn schema(&self) -> ToolSchema {
                ToolSchema::new("loud", "")
            }
            fn execute(&self, _: serde_json::Value) -> Result<serde_json::Value, ToolError> {
                Ok(serde_json::json!({ "data": "x".repeat(10_000) }))
            }
        }

        let reg = ToolRegistry::new();
        reg.register(Box::new(LoudTool));
        let out = reg.execute_batch(
            &[BatchCall {
                name: "loud".into(),
                id: "a".into(),
                args: serde_json::json!({}),
            }],
            100,
        );
        assert!(out[0].truncated);
        assert!(out[0].output.len() < 10_000);
    }

    #[test]
    fn execute_batch_panicking_tool_gets_synthetic_error() {
        struct PanicTool;
        impl Tool for PanicTool {
            fn name(&self) -> &str {
                "boom"
            }
            fn description(&self) -> &str {
                ""
            }
            fn schema(&self) -> ToolSchema {
                ToolSchema::new("boom", "")
            }
            fn execute(&self, _: serde_json::Value) -> Result<serde_json::Value, ToolError> {
                panic!("kaboom");
            }
        }

        let reg = ToolRegistry::new();
        reg.register(Box::new(PanicTool));
        reg.register(Box::new(ReadTool));

        let calls = vec![
            BatchCall {
                name: "boom".into(),
                id: "a".into(),
                args: serde_json::json!({}),
            },
            BatchCall {
                name: "reader".into(),
                id: "b".into(),
                args: serde_json::json!({}),
            },
        ];
        let out = reg.execute_batch(&calls, 25_000);
        // The panicking slot gets a synthetic error with its OWN id…
        assert_eq!(out[0].id, "a");
        assert!(!out[0].success);
        assert!(out[0].output.contains("task panicked"));
        // …and its neighbor still succeeds.
        assert_eq!(out[1].id, "b");
        assert!(out[1].success);
    }
}
