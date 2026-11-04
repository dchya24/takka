//! Auto-compaction (Layer 3 of context compression) and per-request
//! message building.
//!
//! - [`Orchestrator::maybe_autocompact`] — checks the budget and either
//!   runs the heuristic compactor or asks the LLM for a real summary.
//! - [`Orchestrator::summarize_via_provider`] — one-shot LLM call that
//!   produces the summary text for `compact_with_summary`.
//! - [`Orchestrator::build_messages`] — pulls the active context window
//!   from memory and runs it through `build_request_messages` (which
//!   applies Layer 2 compression and the sanitization passes).

use crate::providers::{ChatMessageRequest, ChatRequest};
use crate::AgenticError;

use super::{Orchestrator, OrchestratorState};

impl Orchestrator {
    /// Run autocompact if configured and memory is over threshold.
    /// This is Layer 3 of context compression.
    ///
    /// When `auto_compact_with_llm` is set, asks the LLM provider for a
    /// real summary; on provider error falls back to the heuristic
    /// `Memory::compact()` so the loop never blocks on summarization.
    pub(super) fn maybe_autocompact(&self) {
        if !self.auto_compact {
            return;
        }
        // Cheap check first to avoid prompt construction work.
        {
            let mem = self.memory.lock().unwrap();
            if !mem.needs_compaction() {
                return;
            }
        }

        if self.auto_compact_with_llm {
            // Build the prompt before calling the LLM (no lock held during
            // the network call).
            let prompt = {
                let mem = self.memory.lock().unwrap();
                mem.build_summarization_prompt()
            };
            if let Some(prompt) = prompt {
                self.set_state(OrchestratorState::Compacting);
                self.events.emit(crate::events::Event::CompactionStarted);
                match self.summarize_via_provider(&prompt) {
                    Ok(summary) => {
                        let mut mem = self.memory.lock().unwrap();
                        let r = mem.compact_with_summary(&summary);
                        tracing::info!(
                            summarized = r.summarized_count,
                            tokens_before = r.tokens_before,
                            tokens_after = r.tokens_after,
                            "Memory autocompacted via LLM"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "LLM summarization failed; falling back to heuristic");
                    }
                }
            }
        }

        // Heuristic path (default and fallback).
        let mut mem = self.memory.lock().unwrap();
        if mem.needs_compaction() {
            self.set_state(OrchestratorState::Compacting);
            self.events.emit(crate::events::Event::CompactionStarted);
            let result = mem.compact();
            tracing::info!(
                summarized = result.summarized_count,
                tokens_before = result.tokens_before,
                tokens_after = result.tokens_after,
                "Memory autocompacted (heuristic)"
            );
        }
    }

    /// Issue a one-shot LLM call to summarize older context. Uses the
    /// summarizer model if set, otherwise the main model.
    fn summarize_via_provider(&self, prompt: &str) -> Result<String, AgenticError> {
        let model = self
            .summarizer_model
            .clone()
            .unwrap_or_else(|| self.model.clone());
        let messages = vec![ChatMessageRequest {
            role: "user".to_string(),
            content: prompt.to_string(),
            tool_call_id: None,
            tool_calls: vec![],
            attachments: Vec::new(),
        }];
        let request = ChatRequest::new(&model, messages);
        let response = self
            .provider
            .chat(request)
            .map_err(|e| AgenticError::Provider(e.to_string()))?;
        Ok(response.message.content.unwrap_or_default())
    }

    pub(super) fn build_messages(&self) -> Vec<ChatMessageRequest> {
        // Token-budget context builder. ContextBudget applies the
        // configured context_budget_ratio (default 70%) so we leave
        // headroom for the system prompt, tool definitions, and the
        // response itself.
        //
        // The builder walks complete user-turns (user + assistant + tool
        // group) so a tool_call/result pair is never split, eliminating
        // the orphan-tool / dangling-tool_calls / no-user-anchor cases
        // that previously produced HTTP 400 from the provider.
        let memory = self.memory.lock().unwrap();
        crate::context::ContextEngine::build(
            &memory,
            crate::context::BuildOptions::new(&memory, self.keep_recent_tool_results),
        )
    }
}
