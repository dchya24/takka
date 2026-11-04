//! Pure request-shaping helpers — the rules for turning stored messages
//! into the slice that goes to a provider.
//!
//! This is the "builder" layer of the context engine: it knows about
//! [`Message`] and [`ChatMessageRequest`] but nothing about the
//! orchestrator's lock state, safety engine, or LLM provider — keeping
//! the rules here makes them easy to unit-test in isolation and gives
//! every frontend (CLI, TUI, desktop, server) the same context shape.

use crate::memory::{Message, MessageRole};
use crate::providers::{ChatMessageRequest, ToolCallFunction, ToolCallResponse};

/// Placeholder substituted for stale tool results when Layer 2 fires.
pub const CLEARED_TOOL_RESULT_PLACEHOLDER: &str = "[Cleared: older tool result removed to save context. Re-run the tool if you need this output.]";

/// Build the per-request message list, applying Layer 2 compression
/// (replace older tool-result contents with a placeholder).
///
/// `keep_recent_tool_results == 0` disables the placeholder substitution
/// entirely (everything passes through verbatim).
pub fn build_request_messages(
    context: &[Message],
    keep_recent_tool_results: usize,
) -> Vec<ChatMessageRequest> {
    let keep = keep_recent_tool_results;
    let keep_indices: std::collections::HashSet<usize> = if keep == 0 {
        (0..context.len()).collect()
    } else {
        context
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, m)| matches!(m.role, MessageRole::Tool { .. }))
            .take(keep)
            .map(|(i, _)| i)
            .collect()
    };

    let raw: Vec<ChatMessageRequest> = context
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let (role, tool_call_id) = match &m.role {
                MessageRole::User => ("user", None),
                MessageRole::Assistant => ("assistant", None),
                MessageRole::System => ("system", None),
                MessageRole::Tool { tool_call_id, .. } => ("tool", Some(tool_call_id.clone())),
            };

            let content = if matches!(m.role, MessageRole::Tool { .. })
                && keep > 0
                && !keep_indices.contains(&i)
            {
                CLEARED_TOOL_RESULT_PLACEHOLDER.to_string()
            } else {
                m.content.clone()
            };

            // Reattach tool_calls on assistant messages so tool results
            // that follow can be matched by id (per OpenAI spec).
            let tool_calls = if matches!(m.role, MessageRole::Assistant) {
                m.metadata.tool_calls.clone()
            } else {
                vec![]
            };

            ChatMessageRequest {
                role: role.to_string(),
                content,
                tool_call_id,
                tool_calls,
                attachments: m.metadata.attachments.clone(),
            }
        })
        .collect();

    sanitize_for_provider(raw)
}

/// Convert a list of `(id, name, arguments_json_string)` triples into
/// `ToolCallResponse`s suitable for storing on an assistant message.
pub fn build_tool_call_responses(tool_calls: &[(String, String, String)]) -> Vec<ToolCallResponse> {
    tool_calls
        .iter()
        .map(|(id, name, args)| ToolCallResponse {
            id: id.clone(),
            call_type: "function".to_string(),
            function: ToolCallFunction {
                name: name.clone(),
                arguments: args.clone(),
            },
        })
        .collect()
}

/// Truncate a tool result string to `max_chars`, appending a marker note.
/// Layer 1 of context compression: prevents large tool outputs from blowing
/// up the context window.
pub fn truncate_tool_result(raw: &str, max_chars: usize) -> String {
    if max_chars == 0 || raw.len() <= max_chars {
        return raw.to_string();
    }
    // Truncate on a UTF-8 char boundary.
    let mut end = max_chars;
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    let omitted = raw.len() - end;
    format!(
        "{}\n\n[truncated: {} chars omitted of {} total — re-run with narrower scope if you need more]",
        &raw[..end],
        omitted,
        raw.len()
    )
}

/// Drop messages that would violate the OpenAI/Anthropic tool-call spec
/// before they hit the wire.
///
/// `Memory::get_context(N)` returns the last N raw messages without
/// awareness of pairing rules, so a slice can be malformed in several
/// ways. This function fixes them all in one pass:
///
/// 1. **Orphan tool message**: a `tool` message whose announced parent
///    assistant was trimmed off (or had its tool_calls cleared). Drop it.
/// 2. **Dangling tool_calls**: an assistant advertises tool calls in
///    `tool_calls` but the matching `tool` results are not present.
///    Trim the unmatched IDs from the assistant's tool_calls list.
/// 3. **Empty assistant after trimming**: an assistant whose `content` is
///    empty AND whose tool_calls became empty after step 2. Z.AI rejects
///    `{role: "assistant", content: ""}` with no tool_calls. Drop it.
/// 4. **Bad first non-system message**: providers require the first
///    non-system message to be `user`. Drop leading assistant/tool
///    messages that would otherwise lead the slice.
///
/// All decisions are local: we never invent messages, only drop or trim.
pub fn sanitize_for_provider(messages: Vec<ChatMessageRequest>) -> Vec<ChatMessageRequest> {
    use std::collections::HashSet;

    if messages.is_empty() {
        return messages;
    }

    // Pass 1: trim assistant tool_calls to those whose result actually
    // appears later in the slice (before the next user/assistant turn),
    // and drop tool messages whose IDs are not announced by an earlier
    // assistant.
    let mut sanitized: Vec<ChatMessageRequest> = Vec::with_capacity(messages.len());
    let mut announced_ids: HashSet<String> = HashSet::new();

    for (i, msg) in messages.iter().enumerate() {
        match msg.role.as_str() {
            "assistant" if !msg.tool_calls.is_empty() => {
                // Look ahead until the next assistant/user turn and collect
                // tool_call_ids whose result lands in this group.
                let mut seen_after: HashSet<String> = HashSet::new();
                for next in &messages[i + 1..] {
                    match next.role.as_str() {
                        "tool" => {
                            if let Some(id) = &next.tool_call_id {
                                seen_after.insert(id.clone());
                            }
                        }
                        "assistant" | "user" => break,
                        _ => {}
                    }
                }

                let mut kept = msg.clone();
                kept.tool_calls.retain(|tc| seen_after.contains(&tc.id));

                // Failure mode #3: assistant ended up with neither content
                // nor tool_calls. Drop it rather than emit an empty turn.
                if kept.tool_calls.is_empty() && kept.content.trim().is_empty() {
                    tracing::debug!(
                        "sanitize_for_provider: dropping empty assistant after tool_call trim"
                    );
                    continue;
                }

                for tc in &kept.tool_calls {
                    announced_ids.insert(tc.id.clone());
                }
                sanitized.push(kept);
            }
            "tool" => {
                // Failure mode #1: orphan tool message. Drop unless its
                // parent's announcement is in scope.
                match &msg.tool_call_id {
                    Some(id) if announced_ids.contains(id) => {
                        sanitized.push(msg.clone());
                    }
                    Some(id) => {
                        tracing::debug!(
                            tool_call_id = %id,
                            "sanitize_for_provider: dropping orphan tool message"
                        );
                    }
                    None => {
                        tracing::warn!(
                            "sanitize_for_provider: dropping tool message with no tool_call_id"
                        );
                    }
                }
            }
            "assistant" => {
                // Plain assistant (no tool_calls). Drop if also empty
                // content — same provider rejection as in failure mode #3.
                if msg.content.trim().is_empty() {
                    tracing::debug!("sanitize_for_provider: dropping empty assistant message");
                    continue;
                }
                sanitized.push(msg.clone());
            }
            _ => {
                sanitized.push(msg.clone());
            }
        }
    }

    // Pass 2: drop leading assistant/tool messages so the slice starts
    // with a user (or a system-then-user) sequence. Failure mode #4.
    //
    // We only drop *leading* non-user/system messages — once we hit a
    // user, the rest of the slice is well-formed (per pass 1).
    while let Some(first) = sanitized.first() {
        match first.role.as_str() {
            "system" | "user" => break,
            _ => {
                tracing::debug!(
                    role = %first.role,
                    "sanitize_for_provider: dropping leading non-user/system message"
                );
                sanitized.remove(0);
            }
        }
    }

    // Pass 3: collapse consecutive system messages so the slice has at
    // most one leading system (provider-friendly). Defensive — the
    // current orchestrator only emits one, but this guards against
    // future regressions.
    let mut leading_system_seen = false;
    sanitized.retain(|m| {
        if m.role == "system" {
            if leading_system_seen {
                tracing::debug!("sanitize_for_provider: dropping duplicate system message");
                return false;
            }
            leading_system_seen = true;
        }
        true
    });

    sanitized
}

#[cfg(test)]
mod tests {
    use super::{
        build_request_messages, sanitize_for_provider, truncate_tool_result,
        CLEARED_TOOL_RESULT_PLACEHOLDER,
    };
    use crate::memory::Message;
    use crate::providers::{ChatMessageRequest, ToolCallFunction, ToolCallResponse};

    #[test]
    fn passthrough_when_under_limit() {
        let s = "hello world";
        assert_eq!(truncate_tool_result(s, 100), s);
    }

    #[test]
    fn truncates_when_over_limit() {
        let s = "a".repeat(1000);
        let out = truncate_tool_result(&s, 100);
        assert!(out.starts_with(&"a".repeat(100)));
        assert!(out.contains("truncated"));
        assert!(out.contains("900 chars omitted"));
    }

    #[test]
    fn zero_disables_truncation() {
        let s = "a".repeat(1000);
        assert_eq!(truncate_tool_result(&s, 0), s);
    }

    #[test]
    fn handles_utf8_boundary() {
        // Multi-byte chars; ensure we don't slice mid-codepoint.
        let s = "日本語".repeat(50); // 9 bytes per repeat = 450 bytes
        let out = truncate_tool_result(&s, 10);
        // Must not panic and prefix must be valid UTF-8.
        assert!(out.contains("truncated"));
    }

    fn make_context() -> Vec<Message> {
        // Three rounds of: assistant(with tool_calls) -> tool(matching id).
        // This mirrors the real history shape now that assistant messages
        // carry their tool_calls in metadata.
        let tc = |id: &str| ToolCallResponse {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: ToolCallFunction {
                name: "read_file".to_string(),
                arguments: "{}".to_string(),
            },
        };
        vec![
            Message::user("q"),
            Message::assistant_with_tool_calls("thinking", vec![tc("call-1")]),
            Message::tool("read_file", "call-1", "OLD result 1"),
            Message::assistant_with_tool_calls("more thinking", vec![tc("call-2")]),
            Message::tool("read_file", "call-2", "OLD result 2"),
            Message::assistant_with_tool_calls("still thinking", vec![tc("call-3")]),
            Message::tool("read_file", "call-3", "FRESH result 3"),
        ]
    }

    #[test]
    fn clears_older_tool_results_keeping_recent() {
        let ctx = make_context();
        let out = build_request_messages(&ctx, 1);

        // Find each tool message in the output and check content.
        let tool_msgs: Vec<&ChatMessageRequest> = out.iter().filter(|m| m.role == "tool").collect();
        assert_eq!(tool_msgs.len(), 3);

        // The two oldest tool results should be cleared
        assert_eq!(tool_msgs[0].content, CLEARED_TOOL_RESULT_PLACEHOLDER);
        assert_eq!(tool_msgs[1].content, CLEARED_TOOL_RESULT_PLACEHOLDER);

        // The most recent should be intact
        assert_eq!(tool_msgs[2].content, "FRESH result 3");
    }

    #[test]
    fn keeps_all_tool_results_when_under_limit() {
        let ctx = make_context();
        let out = build_request_messages(&ctx, 10);

        let tool_msgs: Vec<&ChatMessageRequest> = out.iter().filter(|m| m.role == "tool").collect();
        assert_eq!(tool_msgs.len(), 3);

        for msg in tool_msgs {
            assert!(!msg.content.starts_with("[Cleared"));
        }
    }

    #[test]
    fn keep_zero_disables_clearing() {
        let ctx = make_context();
        let out = build_request_messages(&ctx, 0);

        for msg in out.iter().filter(|m| m.role == "tool") {
            assert!(!msg.content.starts_with("[Cleared"));
        }
    }

    #[test]
    fn non_tool_messages_unaffected() {
        let ctx = make_context();
        let out = build_request_messages(&ctx, 1);

        let user_msgs: Vec<&ChatMessageRequest> = out.iter().filter(|m| m.role == "user").collect();
        let assistant_msgs: Vec<&ChatMessageRequest> =
            out.iter().filter(|m| m.role == "assistant").collect();

        assert_eq!(user_msgs.len(), 1);
        assert_eq!(assistant_msgs.len(), 3);
        assert_eq!(user_msgs[0].content, "q");
    }

    fn assistant_with(content: &str, ids: &[&str]) -> ChatMessageRequest {
        ChatMessageRequest {
            role: "assistant".into(),
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: ids
                .iter()
                .map(|id| ToolCallResponse {
                    id: id.to_string(),
                    call_type: "function".into(),
                    function: ToolCallFunction {
                        name: "read_file".into(),
                        arguments: "{}".into(),
                    },
                })
                .collect(),
            attachments: vec![],
        }
    }

    fn tool_msg(id: &str, content: &str) -> ChatMessageRequest {
        ChatMessageRequest {
            role: "tool".into(),
            content: content.to_string(),
            tool_call_id: Some(id.to_string()),
            tool_calls: vec![],
            attachments: vec![],
        }
    }

    fn user_msg(content: &str) -> ChatMessageRequest {
        ChatMessageRequest {
            role: "user".into(),
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: vec![],
            attachments: vec![],
        }
    }

    #[test]
    fn sanitize_drops_orphan_tool_at_start_of_slice() {
        // Real failure case: get_context(N) sliced mid-pair so the
        // slice starts with a tool message whose announcing assistant
        // is gone.
        let input = vec![
            tool_msg("call-orphan", "result without parent"),
            user_msg("next question"),
        ];
        let out = sanitize_for_provider(input);

        // Tool should be dropped.
        assert!(out.iter().all(|m| m.role != "tool"));
        // User survives.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "user");
    }

    #[test]
    fn sanitize_keeps_well_formed_pair() {
        let input = vec![
            user_msg("q"),
            assistant_with("calling", &["call-1"]),
            tool_msg("call-1", "result"),
        ];
        let out = sanitize_for_provider(input);
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].role, "tool");
    }

    #[test]
    fn sanitize_drops_dangling_tool_calls_from_assistant() {
        // Assistant announces 2 calls but only one tool result follows.
        // The unmatched tool_call entry would cause provider error.
        let input = vec![
            user_msg("q"),
            assistant_with("calling", &["call-1", "call-missing"]),
            tool_msg("call-1", "result"),
        ];
        let out = sanitize_for_provider(input);

        let assistant = out.iter().find(|m| m.role == "assistant").unwrap();

        // The assistant's tool_calls should be reduced to just call-1.
        assert_eq!(assistant.tool_calls.len(), 1);
        assert_eq!(assistant.tool_calls[0].id, "call-1");
    }

    #[test]
    fn sanitize_handles_empty() {
        assert!(sanitize_for_provider(vec![]).is_empty());
    }

    #[test]
    fn sanitize_does_not_pull_results_across_user_turn() {
        // A tool result appearing AFTER a new user turn must not be
        // counted toward an earlier assistant's tool_calls.
        let input = vec![
            assistant_with("first call", &["call-1"]),
            user_msg("interrupt"),
            tool_msg("call-1", "should-not-rescue"),
        ];
        let out = sanitize_for_provider(input);

        // Net effect: only the user message survives — a clean slice
        // starting from the user turn.
        let kinds: Vec<&str> = out.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(kinds, vec!["user"]);
    }

    #[test]
    fn sanitize_drops_empty_assistant_with_no_tool_calls() {
        // Provider rejects {role: assistant, content: ""} with no
        // tool_calls.
        let input = vec![
            user_msg("q"),
            ChatMessageRequest {
                role: "assistant".into(),
                content: "".into(),
                tool_call_id: None,
                tool_calls: vec![],
                attachments: vec![],
            },
        ];
        let out = sanitize_for_provider(input);
        let kinds: Vec<&str> = out.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(kinds, vec!["user"]);
    }

    #[test]
    fn sanitize_drops_leading_assistant() {
        // Slice starts with an assistant message (no tool_calls). The
        // first non-system message must be `user`.
        let input = vec![
            ChatMessageRequest {
                role: "assistant".into(),
                content: "hi from a previous turn".into(),
                tool_call_id: None,
                tool_calls: vec![],
                attachments: vec![],
            },
            user_msg("hello"),
        ];
        let out = sanitize_for_provider(input);
        let kinds: Vec<&str> = out.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(kinds, vec!["user"]);
    }

    #[test]
    fn sanitize_keeps_system_then_user_lead() {
        // Standard well-formed lead: system + user. Both should survive.
        let input = vec![
            ChatMessageRequest {
                role: "system".into(),
                content: "you are helpful".into(),
                tool_call_id: None,
                tool_calls: vec![],
                attachments: vec![],
            },
            user_msg("hi"),
        ];
        let out = sanitize_for_provider(input);
        let kinds: Vec<&str> = out.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(kinds, vec!["system", "user"]);
    }

    #[test]
    fn sanitize_collapses_duplicate_system_messages() {
        // Defensive: only one system message should survive.
        let input = vec![
            ChatMessageRequest {
                role: "system".into(),
                content: "rule 1".into(),
                tool_call_id: None,
                tool_calls: vec![],
                attachments: vec![],
            },
            ChatMessageRequest {
                role: "system".into(),
                content: "rule 2".into(),
                tool_call_id: None,
                tool_calls: vec![],
                attachments: vec![],
            },
            user_msg("hi"),
        ];
        let out = sanitize_for_provider(input);
        let kinds: Vec<&str> = out.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(kinds, vec!["system", "user"]);
    }
}
