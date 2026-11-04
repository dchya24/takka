//! Window selection over conversation sources (P0-1 of the hardening
//! plan).
//!
//! Pure functions: they decide **which stored messages** are sent to the
//! model this turn. `Memory` owns the storage; these selectors own the
//! windowing policy. Everything here is side-effect free so any frontend
//! can reuse the exact policy the orchestrator uses.

use chrono::Utc;

use crate::memory::{estimate_tokens, ContextWindow, Message, MessageMetadata, MessageRole};

/// Production-grade context builder for the LLM main loop.
///
/// Strategy (closer to what Claude Code, Aider, and Continue.dev do):
///
/// 1. **Walk turns, not messages.** A "turn" is a self-contained group:
///    `user` followed by zero or more `assistant`/`tool` pairs until the
///    next `user`. We never cut a turn in half, which means we never
///    split an assistant's `tool_calls` from its `tool` results.
///
/// 2. **Token budget, not message count.** Walk turns from newest to
///    oldest, accumulating estimated tokens, and stop just before
///    exceeding `token_budget`. The most recent turn is always included
///    even if it alone exceeds budget — the model will then truncate,
///    but at least we send a valid request rather than nothing.
///
/// 3. **Anchored to user.** Because we walk turn boundaries, the result
///    always starts with a `user` message (or system + user if a summary
///    is present), satisfying provider requirements.
///
/// 4. **Summary prepended.** If a compaction summary exists, it is
///    prepended as a system message so the model has high-level context
///    even after older turns are evicted.
pub fn turn_aware_window(
    messages: &[Message],
    summary: Option<&str>,
    token_budget: u32,
) -> Vec<Message> {
    if messages.is_empty() {
        return Vec::new();
    }

    // Walk backwards finding turn boundaries (each `user` message starts
    // a new turn). Collect (start_idx, end_idx_exclusive) ranges.
    let mut turn_starts: Vec<usize> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        if matches!(msg.role, MessageRole::User) {
            turn_starts.push(i);
        }
    }

    if turn_starts.is_empty() {
        // No user messages exist (shouldn't happen in practice; means
        // the orchestrator was misused). Fall back to anchor.
        return user_anchored_tail(messages, 20, Some(200));
    }

    let total = messages.len();
    let mut turns: Vec<(usize, usize)> = Vec::with_capacity(turn_starts.len());
    for (idx, &start) in turn_starts.iter().enumerate() {
        let end = turn_starts.get(idx + 1).copied().unwrap_or(total);
        turns.push((start, end));
    }

    // Walk turns newest-first, accumulating tokens.
    let mut earliest_kept: usize = turns.last().map(|(s, _)| *s).unwrap_or(0);
    let mut used: u32 = 0;
    // Reserve space for an optional summary at the head.
    let summary_tokens: u32 = summary.map(estimate_tokens).unwrap_or(0);
    let effective_budget = token_budget.saturating_sub(summary_tokens);

    for (start, end) in turns.iter().rev() {
        let turn_tokens: u32 = messages[*start..*end]
            .iter()
            .map(|m| {
                if m.metadata.token_count > 0 {
                    m.metadata.token_count
                } else {
                    estimate_tokens(&m.content)
                }
            })
            .sum();

        // Always include the most recent turn, even if it alone blows
        // the budget. Otherwise we'd send an empty request.
        let is_most_recent = *start == turns.last().unwrap().0;

        if !is_most_recent && used + turn_tokens > effective_budget {
            // This turn would overflow; stop and use the previous
            // (newer) earliest_kept.
            break;
        }

        used += turn_tokens;
        earliest_kept = *start;
    }

    // Build the output: optional summary + kept turns in chronological
    // order.
    let mut out: Vec<Message> = Vec::with_capacity(total - earliest_kept + 1);
    if let Some(summary) = summary {
        out.push(summary_message(summary, summary_tokens));
    }
    out.extend(messages[earliest_kept..].iter().cloned());
    out
}

/// Get the conversation tail anchored to a user message.
///
/// `max_messages` is a soft floor: if the last `max_messages` don't
/// include any user message (because the agent ran a long chain of tool
/// calls between user turns), the window is extended backwards to
/// include the most recent user message. This avoids producing a slice
/// that's purely assistant/tool turns, which providers reject with HTTP
/// 400 "messages parameter is illegal".
///
/// `hard_cap` bounds how far we'll look back. When set to `None` we
/// search all the way to the start of memory; when set we never return
/// more than `hard_cap` messages even if the latest user turn is older
/// than that.
pub fn user_anchored_tail(
    messages: &[Message],
    max_messages: usize,
    hard_cap: Option<usize>,
) -> Vec<Message> {
    let total = messages.len();
    if total == 0 {
        return Vec::new();
    }

    let mut start = total.saturating_sub(max_messages);

    // Does the candidate slice already contain a user message?
    let has_user = messages[start..]
        .iter()
        .any(|m| matches!(m.role, MessageRole::User));

    if !has_user {
        // Walk backwards to find the most recent user message.
        if let Some(user_idx) = messages[..start]
            .iter()
            .rposition(|m| matches!(m.role, MessageRole::User))
        {
            start = user_idx;
        }
    }

    // Apply hard_cap if set.
    if let Some(cap) = hard_cap {
        let earliest = total.saturating_sub(cap);
        if start < earliest {
            start = earliest;
        }
    }

    messages[start..].to_vec()
}

/// Token-based sliding window with metadata about what was included and
/// excluded. Always includes the summary (if any) as a system message.
pub fn sliding_window(
    messages: &[Message],
    summary: Option<&str>,
    budget: u32,
    message_limit: usize,
) -> ContextWindow {
    // Always include summary (if exists)
    let mut selected: Vec<Message> = Vec::new();
    let mut used_tokens: u32 = 0;

    if let Some(summary) = summary {
        let summary_tokens = estimate_tokens(summary);
        used_tokens += summary_tokens;
        selected.push(summary_message(summary, summary_tokens));
    }

    // Walk messages from newest to oldest
    let recent_slice: Vec<&Message> = messages.iter().rev().take(message_limit).collect();

    for msg in &recent_slice {
        let msg_tokens = if msg.metadata.token_count > 0 {
            msg.metadata.token_count
        } else {
            estimate_tokens(&msg.content)
        };

        if used_tokens + msg_tokens > budget {
            break;
        }

        used_tokens += msg_tokens;
        selected.push((*msg).clone());
    }

    // Reverse to restore chronological order
    selected.reverse();

    let used_pct = if budget > 0 {
        (used_tokens as f64 / budget as f64) * 100.0
    } else {
        0.0
    };

    let selected_count = selected.len();

    ContextWindow {
        messages: selected,
        total_tokens: used_tokens,
        budget,
        used_percentage: used_pct,
        was_compacted: summary.is_some(),
        removed_count: messages.len().saturating_sub(selected_count),
    }
}

/// The compaction summary rendered as a pinned system message.
fn summary_message(summary: &str, summary_tokens: u32) -> Message {
    Message {
        id: "__summary__".into(),
        role: MessageRole::System,
        content: summary.to_string(),
        timestamp: Utc::now(),
        pinned: true,
        metadata: MessageMetadata {
            token_count: summary_tokens,
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Memory;

    // ---- user_anchored_tail ----

    #[test]
    fn anchor_extends_to_include_user_when_window_too_small() {
        // Simulate an agent run with one user message followed by a long
        // chain of assistant + tool turns. A naive last-N slice would
        // contain no user message, which providers reject.
        let mut m = Memory::new(10000);
        m.add_message(Message::user("original prompt"));
        for i in 0..30 {
            m.add_message(Message::assistant(format!("thinking {}", i)));
            m.add_message(Message::tool("read_file", format!("call-{}", i), "result"));
        }

        let ctx = user_anchored_tail(m.get_messages(), 20, None);
        assert!(
            ctx.iter().any(|msg| matches!(msg.role, MessageRole::User)),
            "slice must contain at least one user message"
        );
        // First message of the slice should be the user prompt.
        assert!(matches!(ctx[0].role, MessageRole::User));
        assert_eq!(ctx[0].content, "original prompt");
    }

    #[test]
    fn anchor_keeps_short_slice_when_window_already_has_user() {
        // The anchor only extends; if the window already contains a
        // user, leave it alone.
        let mut m = Memory::new(10000);
        for i in 0..5 {
            m.add_message(Message::user(format!("q{}", i)));
            m.add_message(Message::assistant(format!("a{}", i)));
        }
        let ctx = user_anchored_tail(m.get_messages(), 4, None);
        assert_eq!(ctx.len(), 4);
        // Last 4 of 10: q3, a3, q4, a4.
        assert_eq!(ctx[0].content, "q3");
    }

    #[test]
    fn anchor_respects_hard_cap() {
        // Even if the user message is far back, hard_cap caps the slice.
        let mut m = Memory::new(10000);
        m.add_message(Message::user("way back"));
        for _ in 0..50 {
            m.add_message(Message::assistant("more"));
        }
        let ctx = user_anchored_tail(m.get_messages(), 5, Some(10));
        assert!(ctx.len() <= 10);
        // The user message is older than the hard cap, so it WON'T be
        // included — sanitize_for_provider in the context engine will
        // then strip leading non-user messages and drop everything.
        // That's acceptable: the alternative is an unbounded payload.
    }

    #[test]
    fn anchor_handles_empty_memory() {
        let ctx = user_anchored_tail(&[], 20, None);
        assert!(ctx.is_empty());
    }

    #[test]
    fn anchor_picks_most_recent_user_when_multiple_exist() {
        // Multi-turn conversation. Latest user turn becomes the anchor,
        // not the first one ever.
        let mut m = Memory::new(10000);
        m.add_message(Message::user("first prompt"));
        m.add_message(Message::assistant("first answer"));
        m.add_message(Message::user("second prompt"));
        for i in 0..30 {
            m.add_message(Message::assistant(format!("thinking {}", i)));
        }
        let ctx = user_anchored_tail(m.get_messages(), 5, None);
        // The slice must start at the second user prompt, not the first.
        assert!(matches!(ctx[0].role, MessageRole::User));
        assert_eq!(ctx[0].content, "second prompt");
    }

    // ---- turn_aware_window (production builder) ----

    #[test]
    fn turn_builder_starts_with_user_message() {
        let mut m = Memory::new(10000);
        m.add_message(Message::user("first"));
        m.add_message(Message::assistant("a"));
        m.add_message(Message::user("second"));
        for i in 0..50 {
            m.add_message(Message::assistant(format!("t{}", i)));
            m.add_message(Message::tool("read_file", format!("c-{}", i), "r"));
        }

        let ctx = turn_aware_window(m.get_messages(), None, 100_000);
        assert!(matches!(ctx[0].role, MessageRole::User));
    }

    #[test]
    fn turn_builder_keeps_complete_turns_under_budget() {
        // Three small turns. With a generous budget all should fit.
        let mut m = Memory::new(10000);
        m.add_message(Message::user("q1"));
        m.add_message(Message::assistant("a1"));
        m.add_message(Message::user("q2"));
        m.add_message(Message::assistant("a2"));
        m.add_message(Message::user("q3"));
        m.add_message(Message::assistant("a3"));

        let ctx = turn_aware_window(m.get_messages(), None, 100_000);
        assert_eq!(ctx.len(), 6);
        assert_eq!(ctx[0].content, "q1");
        assert_eq!(ctx[5].content, "a3");
    }

    #[test]
    fn turn_builder_drops_older_turns_when_budget_tight() {
        // Three turns with ~heavy content. Budget admits only the newest
        // few. The oldest must be dropped at the turn boundary, not
        // mid-turn. We use natural-language repetition, not a single
        // repeated character (BPE would compress "x".repeat(N) much
        // harder than the heuristic does).
        let mut m = Memory::new(10000);
        let big: String = (0..400)
            .map(|i| format!("sentence number {} with some words. ", i))
            .collect();
        m.add_message(Message::user(format!("q1 {}", big)));
        m.add_message(Message::assistant(format!("a1 {}", big)));
        m.add_message(Message::user(format!("q2 {}", big)));
        m.add_message(Message::assistant(format!("a2 {}", big)));
        m.add_message(Message::user(format!("q3 {}", big)));
        m.add_message(Message::assistant(format!("a3 {}", big)));

        // Tight budget: each turn is ~3.5k tokens (heuristic) or ~2.5k
        // (tiktoken). Either way, 3000 tokens fits at most one full
        // turn.
        let ctx = turn_aware_window(m.get_messages(), None, 3_000);
        assert!(
            ctx.len() <= 4,
            "should drop at least one full turn; got {}",
            ctx.len()
        );
        // Whatever survives must start with a user message.
        assert!(matches!(ctx[0].role, MessageRole::User));
    }

    #[test]
    fn turn_builder_never_splits_tool_pair() {
        // Build a turn that has assistant+tool_call+tool_result. Even if
        // the budget is tight, the splitter must keep them together (or
        // drop them together), never one without the other.
        use crate::providers::{ToolCallFunction, ToolCallResponse};

        let mut m = Memory::new(10000);
        m.add_message(Message::user("please look"));
        m.add_message(Message::assistant_with_tool_calls(
            "",
            vec![ToolCallResponse {
                id: "call-1".into(),
                call_type: "function".into(),
                function: ToolCallFunction {
                    name: "read_file".into(),
                    arguments: "{}".into(),
                },
            }],
        ));
        m.add_message(Message::tool("read_file", "call-1", "result"));
        m.add_message(Message::assistant("final answer"));

        let ctx = turn_aware_window(m.get_messages(), None, 100_000);
        // Find the assistant_with_tool_calls and verify the matching
        // tool message is also present.
        let assistant_idx = ctx
            .iter()
            .position(|m| !m.metadata.tool_calls.is_empty())
            .expect("assistant tool_call should be in context");
        let tool_id = &ctx[assistant_idx].metadata.tool_calls[0].id;
        let tool_present = ctx.iter().any(|m| {
            matches!(
                &m.role,
                MessageRole::Tool { tool_call_id, .. } if tool_call_id == tool_id
            )
        });
        assert!(tool_present, "tool result must accompany tool_call");
    }

    #[test]
    fn turn_builder_always_includes_most_recent_turn() {
        // Even if the most recent turn alone exceeds budget, include it.
        // The model will surface the truncation; sending nothing would
        // be worse.
        let mut m = Memory::new(10000);
        let huge = "x".repeat(40_000); // ~10k tokens
        m.add_message(Message::user(huge.clone()));

        let ctx = turn_aware_window(m.get_messages(), None, 1_000);
        assert_eq!(ctx.len(), 1);
        assert!(matches!(ctx[0].role, MessageRole::User));
    }

    #[test]
    fn turn_builder_prepends_summary_when_present() {
        let mut m = Memory::new(10000);
        // Need enough messages for compact_with_summary to actually run
        // (default keep_recent=4 — anything beyond that gets summarized).
        for i in 0..6 {
            m.add_message(Message::user(format!("q{}", i)));
            m.add_message(Message::assistant(format!("a{}", i)));
        }
        m.compact_with_summary("Earlier we discussed X and Y.");

        let ctx = turn_aware_window(m.get_messages(), m.summary(), 100_000);
        assert!(matches!(ctx[0].role, MessageRole::System));
        assert!(ctx[0].content.contains("discussed X and Y"));
        // A user/assistant follows.
        assert!(ctx.len() > 1);
        assert!(matches!(ctx[1].role, MessageRole::User));
    }

    // ---- sliding_window ----

    #[test]
    fn sliding_window_within_budget() {
        let mut m = Memory::new(10000);
        for i in 0..5 {
            m.add_message(Message::user(format!("short {}", i)));
        }
        let ctx = sliding_window(m.get_messages(), None, 10_000, 100);
        assert_eq!(ctx.messages.len(), 5);
        assert!(!ctx.was_compacted);
    }

    #[test]
    fn sliding_window_respects_limit() {
        let mut m = Memory::new(10000);
        for i in 0..10 {
            m.add_message(Message::user(format!("msg {}", i)));
        }
        let ctx = sliding_window(m.get_messages(), None, 10_000, 3);
        assert!(ctx.messages.len() <= 3);
    }

    #[test]
    fn sliding_window_with_summary() {
        let mut m = Memory::new(200);
        for i in 0..20 {
            m.add_message(Message::user(format!(
                "padding message {} with enough text to fill budget",
                i
            )));
        }
        m.compact();

        let ctx = sliding_window(m.get_messages(), m.summary(), 200, 100);
        assert!(ctx.was_compacted);
        assert!(ctx.removed_count > 0 || ctx.messages.len() < 20);
    }
}
