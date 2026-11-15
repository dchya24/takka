use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use agent_client_protocol_schema::v1 as acp;
use serde_json::Value;
use takka_core::attachments::{Attachment, AttachmentKind, AttachmentSource, ALLOWED_IMAGE_MIME};
use takka_core::runtime::protocol::{ProtocolRequest, Request};

use crate::acp_permission::{self as acp_permission, PermissionAnswer};

use super::*;

/// Body of one prompt turn (owns the response for `reply_id`).
///
/// Takes the parked event receiver, runs `Request::Run`, translates every
/// engine event into a `session/update` notification, and resolves the
/// response at the terminal: `Done` → `stopReason: end_turn`; `Error` →
/// JSON-RPC error with the engine's message; engine death → error + session
/// teardown. The receiver is parked back before the response is written so
/// the session survives failed turns (no poisoned sessions).
pub(crate) fn run_prompt_turn(
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    session: Arc<Session>,
    sink: Arc<dyn AcpSink>,
    permissions: Arc<PermissionWaiters>,
    reply_id: Value,
    task: String,
    attachments: Vec<Attachment>,
) {
    let respond = |payload: &Result<acp::PromptResponse, acp::Error>| {
        let frame = match payload {
            Ok(response) => serde_json::json!({
                "jsonrpc": "2.0", "id": reply_id,
                "result": serde_json::to_value(response).expect("prompt response serializes")
            }),
            Err(error) => serde_json::json!({
                "jsonrpc": "2.0", "id": reply_id, "error": error
            }),
        };
        sink.send_line(&frame);
    };

    // Take the parked stream — the busy flag guarantees we are the only
    // taker. `None` means the stream is already gone (engine died): the
    // session is beyond repair and is torn down.
    let receiver = match session.events.lock().expect("session events lock").take() {
        Some(receiver) => receiver,
        None => {
            sessions
                .lock()
                .expect("session registry lock")
                .remove(&session.id);
            session.busy.store(false, Ordering::SeqCst);
            respond(&Err(internal_error(
                "session engine stream is not available",
            )));
            return;
        }
    };

    let run_id = session.next_request_id("run");
    let run = ProtocolRequest::new(run_id, Request::Run { task, attachments });
    if session.req_tx.send(run).is_err() {
        // Engine thread is gone — no session without an engine.
        sessions
            .lock()
            .expect("session registry lock")
            .remove(&session.id);
        session.busy.store(false, Ordering::SeqCst);
        respond(&Err(internal_error(
            "runtime engine died before the turn started",
        )));
        return;
    }

    let mut translator =
        TranslatorState::with_context_size(session.context_size.load(Ordering::SeqCst));
    let mut engine_died = false;
    // D12: a turn that ends because a limit was hit must report the real
    // reason instead of degrading to `end_turn`. The engine emits the limit
    // event right before its terminator, so remember it here.
    let mut turn_limit: Option<acp::StopReason> = None;
    let outcome: Result<acp::PromptResponse, acp::Error> = loop {
        // No overall deadline here (a legitimate coding turn can run for
        // minutes; the daemon drain has the same shape). A dead engine is
        // detected immediately via the disconnected channel.
        match receiver.recv() {
            Ok(event) => match &event.event {
                takka_core::Event::TurnLimitExceeded { kind } => {
                    turn_limit = Some(match kind {
                        takka_core::events::TurnLimitKind::MaxTokens => acp::StopReason::MaxTokens,
                        takka_core::events::TurnLimitKind::MaxTurnRequests => {
                            acp::StopReason::MaxTurnRequests
                        }
                    });
                }
                takka_core::Event::Done { .. } => {
                    break Ok(acp::PromptResponse::new(
                        turn_limit.unwrap_or(acp::StopReason::EndTurn),
                    ));
                }
                takka_core::Event::Error { message } => {
                    if session.cancel_requested.load(Ordering::SeqCst) {
                        // T-05: the engine surfaced the cooperative cancel
                        // as its terminal (`AgenticError::Cancelled` maps
                        // to `Event::Error`) — the client gets the ACP
                        // `cancelled` stop reason, never an error, and
                        // the session stays reusable for the next prompt.
                        break Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
                    }
                    break Err(internal_error(message.clone()));
                }
                takka_core::Event::ConfirmationRequest {
                    action,
                    description,
                    target,
                    ..
                } => {
                    // T-05: the permission bridge (D8). Ask the client,
                    // park until its answer frame (routed by the framing
                    // loop) or a cancel/teardown deny frees the waiter,
                    // then unblock the engine with `ConfirmResponse` so
                    // the turn continues either way. An "always allow"
                    // additionally persists the target command on the
                    // engine's dynamic auto-approve list (in-session).
                    let answer =
                        request_permission(&session, &permissions, &sink, action, description);
                    let approved = !matches!(answer, PermissionAnswer::Deny);
                    let confirm_id = session.next_request_id("confirm");
                    let confirm = ProtocolRequest::new(
                        confirm_id,
                        acp_permission::confirm_response(None, approved),
                    );
                    if session.req_tx.send(confirm).is_err() {
                        engine_died = true;
                        break Err(internal_error("runtime engine died mid-turn"));
                    }
                    if let (PermissionAnswer::AllowAlways, Some(command)) = (answer, target) {
                        let allow_id = session.next_request_id("allow");
                        let allow = ProtocolRequest::new(
                            allow_id,
                            Request::AllowCommand {
                                command: command.clone(),
                            },
                        );
                        // FIFO behind the ConfirmResponse above; fire-and-
                        // forget (the engine applies it without an ack).
                        let _ = session.req_tx.send(allow);
                    }
                }
                event => {
                    if let Some(update) = translator.translate(event) {
                        let notification = acp::SessionNotification::new(
                            acp::SessionId::new(session.id.as_str()),
                            update,
                        );
                        sink.send_line(&serde_json::json!({
                            "jsonrpc": "2.0", "method": SESSION_UPDATE,
                            "params": serde_json::to_value(notification)
                                .expect("session notification serializes")
                        }));
                    }
                }
            },
            // The engine's event senders live inside the engine thread
            // (its `MemoryTransport`); an error here means the engine is
            // gone for good.
            Err(_) => {
                engine_died = true;
                break Err(internal_error("runtime engine died mid-turn"));
            }
        }
    };

    // Reopen the event channel and release the turn slot BEFORE the
    // response hits the wire — a client that saw the response must be
    // able to prompt again right away (T-03: no poisoned sessions).
    if engine_died {
        sessions
            .lock()
            .expect("session registry lock")
            .remove(&session.id);
    } else {
        *session.events.lock().expect("session events lock") = Some(receiver);
    }
    session.busy.store(false, Ordering::SeqCst);

    match outcome {
        Ok(response) => respond(&Ok(response)),
        Err(error) => respond(&Err(error)),
    }
}

// ---------------------------------------------------------------------------
// Prompt content mapping (spec §4, client → agent).
// ---------------------------------------------------------------------------

/// Map ACP content blocks to one task string + engine attachments.
///
/// `Text` blocks join with newlines into the task; `Image` blocks become
/// engine `Attachment`s (data-URL source, whitelisted vision MIME types
/// only); `ResourceLink` blocks are the editor's @-file mentions and are
/// inlined (`file://` URIs only: text files become fenced blocks, image
/// files become engine attachments, directories become a path note);
/// everything else (`resource`, `audio`, future
/// variants) is rejected — the adapter advertises `image: true` and nothing
/// else (D7). An empty prompt is a parameter error.
pub(crate) fn prompt_content(
    prompt: &[acp::ContentBlock],
) -> Result<(String, Vec<Attachment>), acp::Error> {
    let mut task = String::new();
    let mut attachments = Vec::new();
    for block in prompt {
        match block {
            acp::ContentBlock::Text(text) => {
                if !task.is_empty() {
                    task.push('\n');
                }
                task.push_str(&text.text);
            }
            acp::ContentBlock::Image(image) => {
                let mime_type = image.mime_type.to_ascii_lowercase();
                if !ALLOWED_IMAGE_MIME.contains(&mime_type.as_str()) {
                    return Err(unsupported_content_type(
                        &format!("image ({})", image.mime_type),
                        "supported image MIME types: image/png, image/jpeg, image/gif, image/webp",
                    ));
                }
                attachments.push(Attachment {
                    kind: AttachmentKind::Image,
                    mime_type,
                    data_base64: image.data.clone(),
                    source: AttachmentSource::DataUrl,
                    size_bytes: base64_decoded_len(&image.data),
                });
            }
            acp::ContentBlock::ResourceLink(link) => match resource_link_content(link)? {
                ResourceLinkContent::Text(text) => {
                    if !task.is_empty() {
                        task.push('\n');
                    }
                    task.push_str(&text);
                }
                ResourceLinkContent::Image(attachment) => attachments.push(attachment),
            },
            other => {
                return Err(unsupported_content_type(
                    content_block_kind(other),
                    "inline the text, or attach the file as an image (PNG/JPEG/GIF/WebP) — \
                     the agent advertises text + image prompts only",
                ));
            }
        }
    }
    if task.trim().is_empty() {
        return Err(acp::Error::invalid_params().data(serde_json::json!(
            { "method": SESSION_PROMPT, "reason": "prompt must contain at least one non-empty text block" }
        )));
    }
    Ok((task, attachments))
}

/// Stable wire-ish tag for a (non-exhaustively matched) content block.
pub(crate) fn content_block_kind(block: &acp::ContentBlock) -> &'static str {
    match block {
        acp::ContentBlock::Text(_) => "text",
        acp::ContentBlock::Image(_) => "image",
        acp::ContentBlock::Audio(_) => "audio",
        acp::ContentBlock::ResourceLink(_) => "resource_link",
        acp::ContentBlock::Resource(_) => "resource",
        _ => "unknown content block",
    }
}

/// What a `file://` resource link contributes to the prompt.
pub(crate) enum ResourceLinkContent {
    Text(String),
    Image(Attachment),
}

/// Upper bound on inlined resource-link text (128 KiB of characters) —
/// beyond it the block is truncated with a note, mirroring the tool-result
/// budget.
pub(crate) const MAX_RESOURCE_LINK_CHARS: usize = 128 * 1024;

/// Map one `file://` resource link (an editor @-mention) onto prompt
/// content: text files inline as fenced blocks (bounded), image files
/// become engine image attachments, directories become a path note.
/// Non-`file://` URIs stay an honest rejection (D7: no embedded context).
pub(crate) fn resource_link_content(
    link: &acp::ResourceLink,
) -> Result<ResourceLinkContent, acp::Error> {
    let rest = link.uri.strip_prefix("file://").ok_or_else(|| {
        unsupported_content_type(
            "resource_link (non-file URI)",
            "only file:// resource links are supported; inline the text instead",
        )
    })?;
    // Strip the query (`?symbol=...` on symbol mentions) and the fragment
    // (`#L12:20` line ranges), then percent-decode the path.
    let raw_path = rest.split(['?', '#']).next().unwrap_or(rest);
    let path = std::path::PathBuf::from(percent_decode(raw_path));
    let metadata = std::fs::metadata(&path).map_err(|error| {
        acp::Error::invalid_params().data(serde_json::json!({
            "method": SESSION_PROMPT,
            "reason": "resource not found",
            "uri": link.uri,
            "detail": error.to_string(),
        }))
    })?;
    if metadata.is_dir() {
        return Ok(ResourceLinkContent::Text(format!(
            "[directory: {}] — explore it with the glob tool",
            path.display()
        )));
    }
    let bytes = std::fs::read(&path).map_err(|error| {
        acp::Error::invalid_params().data(serde_json::json!({
            "method": SESSION_PROMPT,
            "reason": "resource unreadable",
            "uri": link.uri,
            "detail": error.to_string(),
        }))
    })?;
    let declared_mime = link
        .mime_type
        .as_ref()
        .map(|mime| mime.to_ascii_lowercase())
        .unwrap_or_default();
    if declared_mime.starts_with("image/") {
        if !ALLOWED_IMAGE_MIME.contains(&declared_mime.as_str()) {
            return Err(unsupported_content_type(
                &format!("resource_link ({declared_mime})"),
                "supported image MIME types: image/png, image/jpeg, image/gif, image/webp",
            ));
        }
        return Ok(ResourceLinkContent::Image(Attachment {
            kind: AttachmentKind::Image,
            mime_type: declared_mime,
            data_base64: {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD.encode(&bytes)
            },
            source: AttachmentSource::FilePath {
                path: path.to_string_lossy().into_owned(),
            },
            size_bytes: bytes.len(),
        }));
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Err(unsupported_content_type(
            "resource_link (binary file)",
            "binary files cannot be inlined; attach them as images (PNG/JPEG/GIF/WebP) instead",
        ));
    };
    let mut body = text;
    if body.chars().count() > MAX_RESOURCE_LINK_CHARS {
        body = body.chars().take(MAX_RESOURCE_LINK_CHARS - 1).collect();
        body.push_str("\n… (truncated at the resource-link budget)");
    }
    Ok(ResourceLinkContent::Text(format!(
        "[file: {}]\n```\n{}\n```",
        path.display(),
        body
    )))
}

/// Minimal percent-decoding for `file://` URI paths (`%20` → space, …).
pub(crate) fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &input[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decoded byte-length estimate for a standard-base64 payload — feeds the
/// diagnostic `size_bytes` on an `Attachment` without a decode pass.
pub(crate) fn base64_decoded_len(encoded: &str) -> usize {
    encoded.trim_end_matches('=').len() * 3 / 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::test_support::*;

    /// `file://` resource links (the editor's @-file mentions) are inlined:
    /// text files become fenced blocks with a path header. This prompt is
    /// accepted by the adapter — the turn then fails against the pinned
    /// discard provider, proving the CONTENT passed validation (any
    /// invalid-params error would fail this test).
    #[test]
    fn prompt_with_a_file_resource_link_is_inlined() {
        let file = std::env::temp_dir().join(format!(
            "takka-mention-{}-{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::write(&file, "the mentioned note\n").expect("write mention fixture");
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        let session = new_session(&acp, 1);
        send_prompt(
            &acp,
            2,
            &session,
            serde_json::json!([
                {
                    "type": "resource_link",
                    "name": "notes",
                    "mime_type": "text/markdown",
                    "uri": format!("file://{}", file.display())
                }
            ]),
        );
        let response = acp.response(2);
        assert!(
            response["error"]["code"] != serde_json::json!(-32602),
            "file mentions must pass content validation: {response}"
        );
        acp.close_and_join();
        let _ = std::fs::remove_file(&file);
    }

    /// `file://` paths arrive percent-encoded (`%20` for spaces) and
    /// symbol mentions carry a query/fragment — both are stripped and
    /// decoded before the file is read.
    #[test]
    fn resource_link_paths_are_decoded_and_fragments_stripped() {
        assert_eq!(percent_decode("/a%20b/c.rs"), "/a b/c.rs");
        assert_eq!(percent_decode("/plain/path.rs"), "/plain/path.rs");
        assert_eq!(percent_decode("bad%2z"), "bad%2z");
    }

    /// A `file://` resource link pointing at a missing file answers
    /// invalid params with the reason (never a silent drop).
    #[test]
    fn prompt_with_a_missing_file_resource_link_is_invalid_params() {
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        let session = new_session(&acp, 1);
        send_prompt(
            &acp,
            2,
            &session,
            serde_json::json!([
                {
                    "type": "resource_link",
                    "name": "gone",
                    "uri": "file:///definitely/not/a/real/path.md"
                }
            ]),
        );
        let response = acp.response(2);
        assert_eq!(response["error"]["code"], serde_json::json!(-32602));
        let text = serde_json::to_string(&response["error"]).expect("error serializes");
        assert!(text.contains("resource not found"), "{text}");
        acp.close_and_join();
    }

    /// Non-`file://` resource links stay an honest rejection (D7).
    #[test]
    fn prompt_with_a_non_file_resource_link_is_rejected() {
        let acp = TestAcp::spawn(DEFAULT_MAX_SESSIONS);
        let session = new_session(&acp, 1);
        send_prompt(
            &acp,
            2,
            &session,
            serde_json::json!([
                { "type": "resource_link", "name": "notes", "uri": "https://example.com/notes.md" }
            ]),
        );
        let response = acp.response(2);
        assert_eq!(response["error"]["code"], serde_json::json!(-32602));
        let text = serde_json::to_string(&response["error"]).expect("error serializes");
        assert!(text.contains("non-file URI"), "{text}");
        acp.close_and_join();
    }
}
