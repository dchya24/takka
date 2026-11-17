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
use crate::sync;
use crate::AgenticError;

use super::{Orchestrator, OrchestratorState};


impl Orchestrator {
    /// Run autocompact if configured and memory is over threshold.
    /// This is Layer 3 of context compression.
    ///
    /// When `auto_compact_with_llm` is set, asks the LLM provider for a
    /// real summary; on provider error falls back to the heuristic
    /// `Memory::compact()` so the loop never blocks on summarization.
    ///
    /// Takes `iteration` so a successful full compact can be recorded,
    /// letting the cadenced pass ([`Self::maybe_cadence_compact`]) no-op
    /// right after one instead of fighting over the same messages.
    pub(super) fn maybe_autocompact(&self, iteration: u32) {
        if !self.auto_compact {
            return;
        }
        // Cheap check first to avoid prompt construction work.
        {
            let mem = sync::lock(&self.memory);
            if !mem.needs_compaction() {
                return;
            }
        }

        if self.auto_compact_with_llm {
            // Build the prompt before calling the LLM (no lock held during
            // the network call).
            let prompt = {
                let mem = sync::lock(&self.memory);
                mem.build_summarization_prompt()
            };
            if let Some(prompt) = prompt {
                self.set_state(OrchestratorState::Compacting);
                self.events.emit(crate::events::Event::CompactionStarted);
                match self.summarize_via_provider(&prompt) {
                    Ok(summary) => {
                        let mut mem = sync::lock(&self.memory);
                        let r = mem.compact_with_summary(&summary);
                        *sync::lock(&self.last_full_compact) = Some(iteration);
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
        let mut mem = sync::lock(&self.memory);
        if mem.needs_compaction() {
            self.set_state(OrchestratorState::Compacting);
            self.events.emit(crate::events::Event::CompactionStarted);
            let result = mem.compact();
            if result.summarized_count > 0 {
                *sync::lock(&self.last_full_compact) = Some(iteration);
            }
            tracing::info!(
                summarized = result.summarized_count,
                tokens_before = result.tokens_before,
                tokens_after = result.tokens_after,
                "Memory autocompacted (heuristic)"
            );
        }
    }

    /// Iteration-cadenced compaction (companion to the token-budget
    /// autocompact): every `compaction_cadence` iterations, collapse old
    /// tool results to one-line summaries, keeping the newest
    /// `keep_recent_tool_results` verbatim. Skipped right after a full
    /// compact so the two passes never fight over the same messages.
    pub(super) fn maybe_cadence_compact(&self, iteration: u32) {
        let last = *sync::lock(&self.last_full_compact);
        if !Self::cadence_due(iteration, self.compaction_cadence, last) {
            return;
        }
        let collapsed = {
            let mut mem = sync::lock(&self.memory);
            mem.collapse_old_tool_results(self.keep_recent_tool_results)
        };
        if collapsed > 0 {
            tracing::info!(
                iteration,
                collapsed,
                keep = self.keep_recent_tool_results,
                "Cadenced pass collapsed old tool results"
            );
        }
    }

    /// Pure scheduling rule for the cadenced pass, unit-tested:
    /// disabled at cadence 0, due every N iterations, and never due
    /// right after (or during the same iteration as) a full compact.
    pub(super) fn cadence_due(
        iteration: u32,
        cadence: usize,
        last_full_compact: Option<u32>,
    ) -> bool {
        if cadence == 0 {
            return false;
        }
        if let Some(last) = last_full_compact {
            if iteration.saturating_sub(last) <= 1 {
                return false;
            }
        }
        iteration.is_multiple_of(cadence as u32)
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
            .map_err(|e| AgenticError::Provider(e.0.clone()))?;
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
        let memory = sync::lock(&self.memory);
        crate::context::ContextEngine::build(
            &memory,
            crate::context::BuildOptions::new(&memory, self.keep_recent_tool_results),
        )
    }
}

#[cfg(test)]
mod cadence_tests {
    use super::Orchestrator;

    #[test]
    fn cadence_zero_disables() {
        assert!(!Orchestrator::cadence_due(10, 0, None));
    }

    #[test]
    fn cadence_fires_every_n_iterations() {
        assert!(Orchestrator::cadence_due(10, 10, None));
        assert!(!Orchestrator::cadence_due(9, 10, None));
        assert!(Orchestrator::cadence_due(20, 10, None));
    }

    #[test]
    fn cadence_skips_right_after_full_compact() {
        // Due at the multiple...
        assert!(Orchestrator::cadence_due(20, 10, None));
        // ...unless a full compact just ran: same iteration or the one
        // right after it, the pass must no-op.
        assert!(!Orchestrator::cadence_due(20, 10, Some(20)));
        assert!(!Orchestrator::cadence_due(20, 10, Some(19)));
        // An older full compact no longer blocks the pass.
        assert!(Orchestrator::cadence_due(20, 10, Some(10)));
    }
}
