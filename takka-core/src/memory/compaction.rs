//! Compaction and summarization logic for `Memory`.
//!
//! Two paths:
//! - `compact()` — heuristic: builds a summary by truncating old
//!   message content. Cheap, deterministic, no LLM call.
//! - `build_summarization_prompt()` + `compact_with_summary()` — the
//!   caller drives an LLM to produce a real summary, then hands the
//!   text back. Higher quality, async, optional.
//!
//! Both produce the same `SummarizedContext` shape so callers can
//! switch between them without other changes.

use super::store::Memory;
use super::types::{estimate_tokens, Message, SummarizedContext};

impl Memory {
    /// Check if compaction should be triggered.
    pub fn needs_compaction(&self) -> bool {
        let threshold = (self.config.max_tokens as f64 * self.config.compaction_threshold) as u32;
        self.total_tokens >= threshold
    }

    /// Compact memory: summarize older messages, keep recent + pinned.
    pub fn compact(&mut self) -> SummarizedContext {
        let tokens_before = self.total_tokens;
        let total_count = self.messages.len();

        if total_count <= self.config.keep_recent {
            return SummarizedContext {
                summary: String::new(),
                summarized_count: 0,
                tokens_before,
                tokens_after: tokens_before,
                kept_ids: self.messages.iter().map(|m| m.id.clone()).collect(),
            };
        }

        // Build summary from messages that will be removed
        let keep_from = total_count.saturating_sub(self.config.keep_recent);

        // Clone messages first to avoid borrow conflict
        let all_messages: Vec<Message> = self.messages.clone();
        let mut to_compact_content: Vec<String> = Vec::new();
        let mut to_keep: Vec<Message> = Vec::new();
        let mut kept_ids: Vec<String> = Vec::new();

        for (i, msg) in all_messages.iter().enumerate() {
            if i >= keep_from || self.pinned_ids.contains(&msg.id) {
                to_keep.push(msg.clone());
                kept_ids.push(msg.id.clone());
            } else {
                to_compact_content.push(format!(
                    "[{}]: {}",
                    msg.role.as_str(),
                    if msg.content.len() > 50 {
                        format!("{}...", &msg.content[..47])
                    } else {
                        msg.content.clone()
                    }
                ));
            }
        }

        let compact_count = to_compact_content.len();

        // Truncate summary if too long to keep tokens manageable
        let raw_summary = to_compact_content.join("\n");
        let max_summary_chars = ((self.config.max_tokens as usize) / 4).max(200);
        let truncated_summary = if raw_summary.len() > max_summary_chars {
            format!(
                "{}\n... ({} more messages truncated)",
                &raw_summary[..max_summary_chars.saturating_sub(40)],
                compact_count
            )
        } else {
            raw_summary
        };

        // Generate summary
        let new_summary = match &self.summary {
            Some(existing) => format!(
                "{}\n--- ({} messages): {}",
                existing, compact_count, truncated_summary
            ),
            None => format!(
                "[Summary of {} messages, {} tokens]: {}",
                compact_count, tokens_before, truncated_summary
            ),
        };

        let summary_tokens = estimate_tokens(&new_summary);
        let kept_tokens: u32 = to_keep.iter().map(|m| estimate_tokens(&m.content)).sum();
        let tokens_after = summary_tokens + kept_tokens;

        // Apply
        self.summary = Some(new_summary);
        self.messages = to_keep;
        self.total_tokens = tokens_after;

        SummarizedContext {
            summary: self.summary.clone().unwrap_or_default(),
            summarized_count: compact_count,
            tokens_before,
            tokens_after,
            kept_ids,
        }
    }

    /// Legacy compatibility: alias for compact().
    pub fn summarize(&mut self) {
        self.compact();
    }

    /// Get the current compaction summary, if any.
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    /// Build a prompt that asks an LLM to summarize the messages that
    /// would be compacted away on the next call to [`Self::compact`].
    ///
    /// Returns `None` if there's nothing to summarize (memory is below
    /// `keep_recent`). The caller is responsible for calling its own
    /// LLM provider with this text and then passing the summary into
    /// [`Self::compact_with_summary`].
    ///
    /// Layered design: keeping the LLM call out of `core::memory` means
    /// this module stays free of any provider/runtime dependency and is
    /// trivially unit-testable.
    pub fn build_summarization_prompt(&self) -> Option<String> {
        let total = self.messages.len();
        if total <= self.config.keep_recent {
            return None;
        }
        let keep_from = total.saturating_sub(self.config.keep_recent);

        let mut transcript = String::new();
        for (i, msg) in self.messages.iter().enumerate() {
            if i >= keep_from || self.pinned_ids.contains(&msg.id) {
                continue;
            }
            transcript.push_str(&format!("[{}]: {}\n", msg.role.as_str(), msg.content));
        }

        if transcript.is_empty() {
            return None;
        }

        let prior_summary = self.summary.as_deref().unwrap_or("(none)");
        Some(format!(
            "You are summarizing an in-progress conversation between a user, an AI \
             coding assistant, and the assistant's tool calls. Produce a concise \
             summary that preserves:\n\
             - the user's intent / goals,\n\
             - decisions already made,\n\
             - files read or modified (with paths) and key findings,\n\
             - open questions or pending steps.\n\
             \n\
             Keep the summary under ~400 words. Use plain prose, no markdown \
             headers. Do not include verbatim tool output — paraphrase.\n\
             \n\
             Prior summary (to extend, do not repeat verbatim):\n{}\n\
             \n\
             Conversation excerpt to summarize:\n{}",
            prior_summary, transcript
        ))
    }

    /// Compact memory using a caller-provided summary string (typically
    /// generated by an LLM via [`Self::build_summarization_prompt`]).
    ///
    /// Behavior matches [`Self::compact`] except the summary text comes
    /// from outside instead of the heuristic truncation. Returns the
    /// same [`SummarizedContext`] shape so callers can switch between
    /// the two with no other changes.
    pub fn compact_with_summary(&mut self, llm_summary: &str) -> SummarizedContext {
        let tokens_before = self.total_tokens;
        let total_count = self.messages.len();

        if total_count <= self.config.keep_recent {
            return SummarizedContext {
                summary: String::new(),
                summarized_count: 0,
                tokens_before,
                tokens_after: tokens_before,
                kept_ids: self.messages.iter().map(|m| m.id.clone()).collect(),
            };
        }

        let keep_from = total_count.saturating_sub(self.config.keep_recent);
        let all_messages: Vec<Message> = self.messages.clone();
        let mut to_keep: Vec<Message> = Vec::new();
        let mut kept_ids: Vec<String> = Vec::new();
        let mut compact_count: usize = 0;

        for (i, msg) in all_messages.iter().enumerate() {
            if i >= keep_from || self.pinned_ids.contains(&msg.id) {
                to_keep.push(msg.clone());
                kept_ids.push(msg.id.clone());
            } else {
                compact_count += 1;
            }
        }

        let new_summary = match &self.summary {
            Some(existing) => format!(
                "{}\n---\n[LLM summary of {} more messages]:\n{}",
                existing,
                compact_count,
                llm_summary.trim()
            ),
            None => format!(
                "[LLM summary of {} messages, {} tokens]:\n{}",
                compact_count,
                tokens_before,
                llm_summary.trim()
            ),
        };

        let summary_tokens = estimate_tokens(&new_summary);
        let kept_tokens: u32 = to_keep.iter().map(|m| estimate_tokens(&m.content)).sum();
        let tokens_after = summary_tokens + kept_tokens;

        self.summary = Some(new_summary);
        self.messages = to_keep;
        self.total_tokens = tokens_after;

        SummarizedContext {
            summary: self.summary.clone().unwrap_or_default(),
            summarized_count: compact_count,
            tokens_before,
            tokens_after,
            kept_ids,
        }
    }
}
