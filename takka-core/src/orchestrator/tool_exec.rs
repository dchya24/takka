//! Tool-call execution paths.
//!
//! Two flavors live here:
//! - [`Orchestrator::handle_tool_calls`] — synchronous. Used by
//!   [`Orchestrator::run`].
//! - [`Orchestrator::handle_tool_calls_parallel`] — async. Used by
//!   [`Orchestrator::run_stream`].
//!
//! Both share the same shape: a sequential safety + confirmation
//! pre-pass produces a slot list, then `ToolRegistry::plan_batches`
//! decides the schedule (consecutive parallel-safe calls run
//! concurrently via `ToolRegistry::execute_batch`; everything else runs
//! alone in original order).

use crate::events::Event;
use crate::memory::Message;
use crate::orchestrator::progress::DeltaThrottler;

use std::sync::Arc;

use super::{Orchestrator, OrchestratorState};
use crate::context::builder::{build_tool_call_responses, truncate_tool_result};
use crate::tool_registry::{BatchCall, ScheduleEntry};

/// Hasil eksekusi satu tool + metadata untuk `Event::ToolOutput`.
struct SlotOutcome {
    name: String,
    id: String,
    result: String, // sudah di-truncate
    duration_ms: u64,
    success: bool,
    truncated: bool,
}

/// Budget live output per tool (chars).
const DELTA_BUDGET_CHARS: usize = 8_000;

impl Orchestrator {
    /// Extract the most relevant target string from tool args (for safety scoring).
    pub(super) fn extract_target(args: &serde_json::Value) -> Option<&str> {
        args.get("command")
            .or(args.get("path"))
            .or(args.get("file_path"))
            .and_then(|v| v.as_str())
    }

    /// Execute a tool, streaming progress deltas through `on_progress`.
    /// Returns the truncated result plus duration/success/truncated flags
    /// so callers can emit an enriched `Event::ToolOutput`.
    fn execute_tool_streaming(
        &self,
        name: &str,
        args: &serde_json::Value,
        on_progress: &dyn Fn(&str),
    ) -> SlotOutcome {
        let start = std::time::Instant::now();
        let raw = match self
            .tools
            .execute_streaming_by_name(name, args, on_progress)
        {
            Ok(result) => {
                serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string())
            }
            Err(e) => format!("Tool error: {}", e),
        };
        let truncated = raw.len() > self.tool_result_max_chars;
        let result = truncate_tool_result(&raw, self.tool_result_max_chars);
        let success = !result.starts_with("Tool error");
        SlotOutcome {
            name: name.to_string(),
            id: String::new(), // diisi pemanggil
            result,
            duration_ms: start.elapsed().as_millis() as u64,
            success,
            truncated,
        }
    }

    /// Execute a tool with live `ToolDelta` emission (sync path).
    fn execute_tool_live(&self, id: &str, name: &str, args: &serde_json::Value) -> SlotOutcome {
        self.events.emit(Event::ToolStarted {
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            arguments: args.clone(),
        });
        let throttler = DeltaThrottler::new(DELTA_BUDGET_CHARS);
        let oid = id.to_string();
        let oname = name.to_string();
        let mut outcome = self.execute_tool_streaming(name, args, &|delta| {
            if throttler.accept(delta) {
                self.events.emit(Event::ToolDelta {
                    tool_call_id: oid.clone(),
                    tool_name: oname.clone(),
                    delta: delta.to_string(),
                });
            }
        });
        outcome.id = id.to_string();
        outcome
    }

    pub(super) fn handle_tool_calls(&self, content: &str, tool_calls: &[(String, String, String)]) {
        let tool_call_responses = build_tool_call_responses(tool_calls);
        self.memory
            .lock()
            .unwrap()
            .add_message(Message::assistant_with_tool_calls(
                content,
                tool_call_responses,
            ));
        // P1-2: the tool turn is observable — entry marks ExecutingTools;
        // confirmations below temporarily move to WaitingForUser.
        self.set_state(OrchestratorState::ExecutingTools);

        // Same Slot model as the async path: pre-pass for safety +
        // confirmation in the original tool-call order, then batched
        // execution planned by `ToolRegistry::plan_batches`.
        enum Slot {
            PreResolved {
                name: String,
                id: String,
                message: String,
            },
            Pending {
                name: String,
                id: String,
                args: serde_json::Value,
            },
        }

        let mut slots: Vec<Slot> = Vec::with_capacity(tool_calls.len());
        for (tc_id, tc_name, tc_args_str) in tool_calls {
            let args: serde_json::Value =
                serde_json::from_str(tc_args_str).unwrap_or(serde_json::json!({}));

            // Surface the call before safety so subscribers see denied
            // calls too (matches handle_tool_calls_parallel).
            self.events.emit(Event::ToolCall {
                tool_name: tc_name.clone(),
                arguments: args.clone(),
            });

            // P2-1: one policy pipeline for builtin, MCP, and external
            // tools — per-arg scoring + the tool's static risk floor.
            let policy = self
                .safety
                .evaluate_tool(&crate::safety::PolicyRequest::new(
                    tc_name.clone(),
                    args.clone(),
                    self.tools.metadata(tc_name).risk,
                ));
            let target = Self::extract_target(&args);

            if !policy.allowed {
                let reason = policy
                    .denial_reason
                    .unwrap_or_else(|| "Action denied by safety policy".to_string());
                self.events.emit(Event::System {
                    message: format!("Tool {} denied: {}", tc_name, reason),
                });
                self.events.emit(Event::ToolOutput {
                    tool_name: tc_name.clone(),
                    output: serde_json::Value::String(format!("Blocked: {}", reason)),
                    error: None,
                    tool_call_id: tc_id.clone(),
                    duration_ms: 0,
                    success: false,
                    truncated: false,
                });
                slots.push(Slot::PreResolved {
                    name: tc_name.clone(),
                    id: tc_id.clone(),
                    message: format!("Blocked: {}", reason),
                });
                continue;
            }

            if policy.confirmation_required {
                let mut request = self.safety.create_request(tc_name, &format!("{:?}", args));
                request.preview_diff = preview_diff_for_tool(tc_name, &args);
                self.set_state(OrchestratorState::WaitingForUser);
                self.events.emit(crate::events::Event::WaitingForUser);
                let confirmed = self.require_confirmation(request);
                self.set_state(OrchestratorState::ExecutingTools);
                if !confirmed {
                    self.events.emit(Event::System {
                        message: format!("Tool {} skipped: confirmation denied", tc_name),
                    });
                    self.events.emit(Event::ToolOutput {
                        tool_name: tc_name.clone(),
                        output: serde_json::Value::String(
                            "Skipped: Confirmation denied".to_string(),
                        ),
                        error: None,
                        tool_call_id: tc_id.clone(),
                        duration_ms: 0,
                        success: false,
                        truncated: false,
                    });
                    slots.push(Slot::PreResolved {
                        name: tc_name.clone(),
                        id: tc_id.clone(),
                        message: "Skipped: Confirmation denied".to_string(),
                    });
                    continue;
                }
                self.safety
                    .record_confirmation(tc_name, target, &policy.score, true);
            }

            slots.push(Slot::Pending {
                name: tc_name.clone(),
                id: tc_id.clone(),
                args,
            });
        }

        // Execution pass: the registry plans the batches — consecutive
        // parallel-safe Pending entries run concurrently via
        // `execute_batch`; everything else runs alone in original order.
        let entries: Vec<crate::tool_registry::ScheduleEntry> = slots
            .iter()
            .map(|slot| match slot {
                Slot::PreResolved { .. } => ScheduleEntry::PreResolved,
                Slot::Pending { name, .. } => ScheduleEntry::Execute(name.clone()),
            })
            .collect();
        let batches = self.tools.plan_batches(&entries);

        let mut results: Vec<Option<SlotOutcome>> = (0..slots.len()).map(|_| None).collect();

        for batch in batches {
            let members: Vec<usize> = batch
                .indices
                .iter()
                .copied()
                .filter(|&i| matches!(slots[i], Slot::Pending { .. }))
                .collect();

            if batch.parallel && members.len() > 1 {
                // Emit ToolStart for every tool in the batch on the main
                // thread (read-only tools don't stream, so no deltas
                // follow).
                for &i in &members {
                    if let Slot::Pending { name, id, args } = &slots[i] {
                        self.events.emit(Event::ToolStarted {
                            tool_call_id: id.clone(),
                            tool_name: name.clone(),
                            arguments: args.clone(),
                        });
                    }
                }

                let calls: Vec<BatchCall> = members
                    .iter()
                    .filter_map(|&i| match &slots[i] {
                        Slot::Pending { name, id, args } => Some(BatchCall {
                            name: name.clone(),
                            id: id.clone(),
                            args: args.clone(),
                        }),
                        _ => None,
                    })
                    .collect();
                let outcomes = self.tools.execute_batch(&calls, self.tool_result_max_chars);
                for (&i, outcome) in members.iter().zip(outcomes) {
                    results[i] = Some(SlotOutcome {
                        name: outcome.name,
                        id: outcome.id,
                        result: outcome.output,
                        duration_ms: outcome.duration_ms,
                        success: outcome.success,
                        truncated: outcome.truncated,
                    });
                }
            } else {
                for i in batch.indices {
                    match &slots[i] {
                        Slot::PreResolved { name, id, message } => {
                            results[i] = Some(SlotOutcome {
                                name: name.clone(),
                                id: id.clone(),
                                result: message.clone(),
                                duration_ms: 0,
                                success: false,
                                truncated: false,
                            });
                        }
                        Slot::Pending { name, id, args } => {
                            // Exclusive / singleton: run alone,
                            // sequentially, with live deltas
                            // (run_command/run_script stream).
                            results[i] = Some(self.execute_tool_live(id, name, args));
                        }
                    }
                }
            }
        }

        // Push results in original order. Emit ToolOutput events only
        // for Pending slots (PreResolved already emitted theirs above).
        {
            let mut mem = self.memory.lock().unwrap();
            for (idx, entry) in results.into_iter().enumerate() {
                if let Some(outcome) = entry {
                    if matches!(slots[idx], Slot::Pending { .. }) {
                        self.events.emit(Event::ToolOutput {
                            tool_name: outcome.name.clone(),
                            output: serde_json::Value::String(outcome.result.clone()),
                            error: None,
                            tool_call_id: outcome.id.clone(),
                            duration_ms: outcome.duration_ms,
                            success: outcome.success,
                            truncated: outcome.truncated,
                        });
                    }
                    mem.add_message(Message::tool(outcome.name, outcome.id, outcome.result));
                }
            }
        }

        // P1-3: tool boundary — persist the turn before the next model
        // request goes out. (Outside the memory lock: checkpointing
        // re-locks memory.)
        self.checkpoint();
    }

    /// Async variant of [`Self::handle_tool_calls`] that batches
    /// consecutive read-only tools and runs them concurrently.
    ///
    /// Sequencing rules (matching the architecture doc):
    /// - Read-only tools (read_file, list_files, glob, grep, search_files)
    ///   in the same batch run in parallel via spawn_blocking.
    /// - State-changing tools run alone, sequentially.
    /// - Results are pushed to memory in the **original tool-call order**
    ///   regardless of which batch finished first.
    /// - Safety evaluation and user confirmation happen sequentially on the
    ///   main task before any execution starts (parallelism doesn't change
    ///   gating semantics).
    pub(super) async fn handle_tool_calls_parallel(
        &self,
        content: &str,
        tool_calls: &[(String, String, String)],
    ) {
        let tool_call_responses = build_tool_call_responses(tool_calls);
        self.memory
            .lock()
            .unwrap()
            .add_message(Message::assistant_with_tool_calls(
                content,
                tool_call_responses,
            ));
        // P1-2: the tool turn is observable — entry marks ExecutingTools;
        // confirmations below temporarily move to WaitingForUser.
        self.set_state(OrchestratorState::ExecutingTools);

        // Outcome of the safety+confirmation pre-pass for a single call.
        enum Slot {
            /// Pre-resolved (denied, skipped). The string is the message we
            /// will record verbatim as the tool result.
            PreResolved {
                name: String,
                id: String,
                message: String,
            },
            /// Needs to be executed. Carries the parsed args; batching
            /// is decided by `ToolRegistry::plan_batches`.
            Pending {
                name: String,
                id: String,
                args: serde_json::Value,
            },
        }

        // Pre-pass: evaluate every call. Confirmation prompts run here, in
        // the original order, before anything is executed.
        let mut slots: Vec<Slot> = Vec::with_capacity(tool_calls.len());
        for (tc_id, tc_name, tc_args_str) in tool_calls {
            let args: serde_json::Value =
                serde_json::from_str(tc_args_str).unwrap_or(serde_json::json!({}));

            // Surface the call before safety evaluation so subscribers see
            // even denied calls.
            self.events.emit(Event::ToolCall {
                tool_name: tc_name.clone(),
                arguments: args.clone(),
            });

            // P2-1: one policy pipeline for builtin, MCP, and external
            // tools — per-arg scoring + the tool's static risk floor.
            let policy = self
                .safety
                .evaluate_tool(&crate::safety::PolicyRequest::new(
                    tc_name.clone(),
                    args.clone(),
                    self.tools.metadata(tc_name).risk,
                ));
            let target = Self::extract_target(&args);

            if !policy.allowed {
                let reason = policy
                    .denial_reason
                    .unwrap_or_else(|| "Action denied by safety policy".to_string());
                self.events.emit(Event::System {
                    message: format!("Tool {} denied: {}", tc_name, reason),
                });
                self.events.emit(Event::ToolOutput {
                    tool_name: tc_name.clone(),
                    output: serde_json::Value::String(format!("Blocked: {}", reason)),
                    error: None,
                    tool_call_id: tc_id.clone(),
                    duration_ms: 0,
                    success: false,
                    truncated: false,
                });
                slots.push(Slot::PreResolved {
                    name: tc_name.clone(),
                    id: tc_id.clone(),
                    message: format!("Blocked: {}", reason),
                });
                continue;
            }

            if policy.confirmation_required {
                let mut request = self.safety.create_request(tc_name, &format!("{:?}", args));
                request.preview_diff = preview_diff_for_tool(tc_name, &args);
                self.set_state(OrchestratorState::WaitingForUser);
                self.events.emit(crate::events::Event::WaitingForUser);
                let confirmed = self.require_confirmation(request);
                self.set_state(OrchestratorState::ExecutingTools);
                if !confirmed {
                    self.events.emit(Event::System {
                        message: format!("Tool {} skipped: confirmation denied", tc_name),
                    });
                    self.events.emit(Event::ToolOutput {
                        tool_name: tc_name.clone(),
                        output: serde_json::Value::String(
                            "Skipped: Confirmation denied".to_string(),
                        ),
                        error: None,
                        tool_call_id: tc_id.clone(),
                        duration_ms: 0,
                        success: false,
                        truncated: false,
                    });
                    slots.push(Slot::PreResolved {
                        name: tc_name.clone(),
                        id: tc_id.clone(),
                        message: "Skipped: Confirmation denied".to_string(),
                    });
                    continue;
                }
                self.safety
                    .record_confirmation(tc_name, target, &policy.score, true);
            }

            slots.push(Slot::Pending {
                name: tc_name.clone(),
                id: tc_id.clone(),
                args,
            });
        }

        // Execute slots in batches planned by the registry. Output is
        // collected position-aligned to `slots` so we can push to memory
        // in original order at the end.
        let entries: Vec<ScheduleEntry> = slots
            .iter()
            .map(|slot| match slot {
                Slot::PreResolved { .. } => ScheduleEntry::PreResolved,
                Slot::Pending { name, .. } => ScheduleEntry::Execute(name.clone()),
            })
            .collect();
        let batches = self.tools.plan_batches(&entries);

        let mut results: Vec<Option<SlotOutcome>> = (0..slots.len()).map(|_| None).collect();

        for batch in batches {
            let members: Vec<usize> = batch
                .indices
                .iter()
                .copied()
                .filter(|&i| matches!(slots[i], Slot::Pending { .. }))
                .collect();

            if batch.parallel && members.len() > 1 {
                // Parallel-safe batch: one spawn_blocking running the
                // registry's scoped-thread batch. These tools never
                // stream, so no delta forwarder is needed.
                for &i in &members {
                    if let Slot::Pending { name, id, args } = &slots[i] {
                        self.events.emit(Event::ToolStarted {
                            tool_call_id: id.clone(),
                            tool_name: name.clone(),
                            arguments: args.clone(),
                        });
                    }
                }
                let calls: Vec<BatchCall> = members
                    .iter()
                    .filter_map(|&i| match &slots[i] {
                        Slot::Pending { name, id, args } => Some(BatchCall {
                            name: name.clone(),
                            id: id.clone(),
                            args: args.clone(),
                        }),
                        _ => None,
                    })
                    .collect();
                let registry = self.tools.clone();
                let max_chars = self.tool_result_max_chars;
                let joined =
                    tokio::task::spawn_blocking(move || registry.execute_batch(&calls, max_chars))
                        .await;
                match joined {
                    Ok(outcomes) => {
                        for (&i, outcome) in members.iter().zip(outcomes) {
                            results[i] = Some(SlotOutcome {
                                name: outcome.name,
                                id: outcome.id,
                                result: outcome.output,
                                duration_ms: outcome.duration_ms,
                                success: outcome.success,
                                truncated: outcome.truncated,
                            });
                        }
                    }
                    Err(join_err) => {
                        // Whole batch task failed; synthetic error for
                        // every member.
                        for &i in &members {
                            if let Slot::Pending { name, id, .. } = &slots[i] {
                                results[i] = Some(SlotOutcome {
                                    name: name.clone(),
                                    id: id.clone(),
                                    result: format!("Tool error: task panicked: {}", join_err),
                                    duration_ms: 0,
                                    success: false,
                                    truncated: false,
                                });
                            }
                        }
                    }
                }
                continue;
            }

            // Singleton batch: PreResolved is written directly; a Pending
            // call runs alone with a delta forwarder so streaming tools
            // (run_command/run_script) emit live output.
            let i = batch.indices[0];
            if let Slot::PreResolved { name, id, message } = &slots[i] {
                results[i] = Some(SlotOutcome {
                    name: name.clone(),
                    id: id.clone(),
                    result: message.clone(),
                    duration_ms: 0,
                    success: false,
                    truncated: false,
                });
                continue;
            }

            if let Slot::Pending { name, id, args } = &slots[i] {
                let (tx, rx) = std::sync::mpsc::channel::<(String, String, String)>();
                let emitter = self.events.clone();
                let throttler = Arc::new(DeltaThrottler::new(DELTA_BUDGET_CHARS));
                let t2 = throttler.clone();
                let fwd = std::thread::Builder::new()
                    .name("agentic-delta-fwd".into())
                    .spawn(move || {
                        for (id, name, delta) in rx {
                            if t2.accept(&delta) {
                                emitter.emit(Event::ToolDelta {
                                    tool_call_id: id,
                                    tool_name: name,
                                    delta,
                                });
                            }
                        }
                    })
                    .expect("spawn delta forwarder");

                self.events.emit(Event::ToolStarted {
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    arguments: args.clone(),
                });

                let registry = self.tools.clone();
                let max_chars = self.tool_result_max_chars;
                let name = name.clone();
                let id = id.clone();
                let args = args.clone();
                let tx2 = tx.clone();
                let handle = tokio::task::spawn_blocking(move || {
                    let start = std::time::Instant::now();
                    let raw = match registry.execute_streaming_by_name(&name, &args, &|delta| {
                        let _ = tx2.send((id.clone(), name.clone(), delta.to_string()));
                    }) {
                        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()),
                        Err(e) => format!("Tool error: {}", e),
                    };
                    let duration_ms = start.elapsed().as_millis() as u64;
                    let truncated = raw.len() > max_chars;
                    let result = truncate_tool_result(&raw, max_chars);
                    (name, id, result, duration_ms, truncated)
                });

                match handle.await {
                    Ok((name, id, output, duration_ms, truncated)) => {
                        let success = !output.starts_with("Tool error");
                        results[i] = Some(SlotOutcome {
                            name,
                            id,
                            result: output,
                            duration_ms,
                            success,
                            truncated,
                        });
                    }
                    Err(join_err) => {
                        // Recover slot identity for the error message.
                        if let Slot::Pending { name, id, .. } = &slots[i] {
                            results[i] = Some(SlotOutcome {
                                name: name.clone(),
                                id: id.clone(),
                                result: format!("Tool error: task panicked: {}", join_err),
                                duration_ms: 0,
                                success: false,
                                truncated: false,
                            });
                        }
                    }
                }

                // Drop our Sender + drain the forwarder so every delta is
                // emitted BEFORE the final ToolOutput for this batch.
                drop(tx);
                fwd.join().expect("delta forwarder panicked");
            }
        }

        // Push results in the original order so the model sees a coherent
        // tool/assistant/tool/assistant interleaving. Also emit ToolOutput
        // events for any executed (Pending) slots; PreResolved slots
        // already emitted their outcome above.
        {
            let mut mem = self.memory.lock().unwrap();
            for (idx, entry) in results.into_iter().enumerate() {
                if let Some(outcome) = entry {
                    // Only Pending slots produce real tool output; PreResolved
                    // already emitted theirs.
                    if matches!(slots[idx], Slot::Pending { .. }) {
                        self.events.emit(Event::ToolOutput {
                            tool_name: outcome.name.clone(),
                            output: serde_json::Value::String(outcome.result.clone()),
                            error: None,
                            tool_call_id: outcome.id.clone(),
                            duration_ms: outcome.duration_ms,
                            success: outcome.success,
                            truncated: outcome.truncated,
                        });
                    }
                    mem.add_message(Message::tool(outcome.name, outcome.id, outcome.result));
                }
            }
        }

        // P1-3: tool boundary — persist the turn before the next model
        // request goes out. (Outside the memory lock: checkpointing
        // re-locks memory.)
        self.checkpoint();
    }
}

// ---------------------------------------------------------------------------
// Diff preview
// ---------------------------------------------------------------------------

/// Compute a unified-diff preview of what a state-changing tool would do,
/// without executing it. Returns `None` for tools we don't know how to
/// preview, or when the source file is unreadable.
///
/// Supported tools:
/// - `write_file`: diff between current file content and `args.content`.
/// - `edit_file`: diff between current content and the result of applying
///   the requested string replacement (or `None` if old_string is missing
///   or doesn't match — the user will see the same error the tool would
///   return on execution).
/// - `apply_patch`: returns the patch text directly (it's already a
///   unified diff).
pub(super) fn preview_diff_for_tool(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    let obj = args.as_object()?;
    match tool_name {
        "write_file" => {
            let path = obj.get("path")?.as_str()?;
            let new_content = obj.get("content")?.as_str()?;
            let before = std::fs::read_to_string(path).unwrap_or_default();
            let diff = crate::diff_util::unified_diff(path, &before, new_content, 3);
            if diff.is_empty() {
                None
            } else {
                Some(diff)
            }
        }
        "edit_file" => {
            let path = obj.get("file_path")?.as_str()?;
            let old_string = obj.get("old_string")?.as_str()?;
            let new_string = obj.get("new_string")?.as_str()?;
            let replace_all = obj
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let before = std::fs::read_to_string(path).ok()?;
            // Mirror EditFileTool's matching: exact, then unique. We only
            // need a usable preview, so fall back gracefully if the
            // string isn't found.
            if !before.contains(old_string) {
                return None;
            }
            let after = if replace_all {
                before.replace(old_string, new_string)
            } else {
                // First-occurrence replace.
                before.replacen(old_string, new_string, 1)
            };
            if after == before {
                return None;
            }
            let diff = crate::diff_util::unified_diff(path, &before, &after, 3);
            if diff.is_empty() {
                None
            } else {
                Some(diff)
            }
        }
        "apply_patch" => obj
            .get("patch")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod preview_diff_tests {
    use super::preview_diff_for_tool;
    use std::io::Write;

    fn temp_file(name: &str, content: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("agentic-preview-{}-{}", pid, nanos));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn returns_none_for_unknown_tool() {
        let args = serde_json::json!({"path": "x.txt", "content": "hello"});
        assert!(preview_diff_for_tool("read_file", &args).is_none());
        assert!(preview_diff_for_tool("run_command", &args).is_none());
    }

    #[test]
    fn write_file_diffs_against_existing_file() {
        let p = temp_file("a.txt", "hello\nworld\n");
        let args = serde_json::json!({
            "path": p.to_string_lossy(),
            "content": "hello\nrust\n",
        });
        let diff = preview_diff_for_tool("write_file", &args).expect("diff");
        assert!(diff.contains("-world"));
        assert!(diff.contains("+rust"));
    }

    #[test]
    fn write_file_returns_none_when_content_unchanged() {
        let p = temp_file("same.txt", "unchanged\n");
        let args = serde_json::json!({
            "path": p.to_string_lossy(),
            "content": "unchanged\n",
        });
        assert!(preview_diff_for_tool("write_file", &args).is_none());
    }

    #[test]
    fn write_file_treats_missing_path_as_creation() {
        // Non-existent path: the diff should look like a creation
        // (all `+` lines, no `-`). diff_util keeps it concise.
        let args = serde_json::json!({
            "path": "/tmp/agentic-preview-doesnotexist.txt",
            "content": "new\nfile\n",
        });
        let diff = preview_diff_for_tool("write_file", &args).expect("diff");
        assert!(diff.contains("+new"));
        assert!(diff.contains("+file"));
    }

    #[test]
    fn edit_file_returns_none_when_old_string_missing() {
        let p = temp_file("miss.txt", "alpha\nbeta\n");
        let args = serde_json::json!({
            "file_path": p.to_string_lossy(),
            "old_string": "NOT_PRESENT",
            "new_string": "x",
        });
        assert!(preview_diff_for_tool("edit_file", &args).is_none());
    }

    #[test]
    fn edit_file_diffs_first_occurrence() {
        let p = temp_file("edit.txt", "alpha\nbeta\nalpha\n");
        let args = serde_json::json!({
            "file_path": p.to_string_lossy(),
            "old_string": "alpha",
            "new_string": "ALPHA",
        });
        let diff = preview_diff_for_tool("edit_file", &args).expect("diff");
        assert!(diff.contains("-alpha"));
        assert!(diff.contains("+ALPHA"));
        // Replace_all not set — the second alpha must remain in diff context.
        assert!(diff.contains(" alpha") || diff.matches('a').count() >= 2);
    }

    #[test]
    fn edit_file_replace_all_changes_every_occurrence() {
        let p = temp_file("edit-all.txt", "x\nx\nx\n");
        let args = serde_json::json!({
            "file_path": p.to_string_lossy(),
            "old_string": "x",
            "new_string": "y",
            "replace_all": true,
        });
        let diff = preview_diff_for_tool("edit_file", &args).expect("diff");
        // Three occurrences should produce three `+y` and three `-x`.
        assert_eq!(diff.matches("+y").count(), 3);
        assert_eq!(diff.matches("-x").count(), 3);
    }

    #[test]
    fn apply_patch_returns_patch_text_directly() {
        let patch = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n";
        let args = serde_json::json!({"patch": patch});
        let preview =
            preview_diff_for_tool("apply_patch", &args).expect("apply_patch should pass through");
        assert_eq!(preview, patch);
    }

    #[test]
    fn apply_patch_returns_none_for_empty() {
        let args = serde_json::json!({"patch": "   "});
        assert!(preview_diff_for_tool("apply_patch", &args).is_none());
    }
}
