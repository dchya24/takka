//! Record/replay provider ("cassette").
//!
//! A cassette is a JSONL file with one exchange per line:
//!
//! ```json
//! {"seq":0,"fingerprint":"<16 hex>","response":{...ChatResponse...}}
//! ```
//!
//! [`CassetteProvider`] replays those exchanges in order, one per `chat()`
//! call. It is deterministic, free and hermetic: no network, no credentials,
//! no ambient config. This step (A1) is replay-only and non-streaming;
//! streaming replay and record mode are separate steps of
//! `docs/superpowers/plans/2026-11-26-eval-harness-and-record-replay.md`.
//!
//! # Strictness
//!
//! * `fingerprint` present and the provider is strict (default): the incoming
//!   request must hash to the same fingerprint, otherwise replay fails with an
//!   error naming the sequence number. This is what makes a changed prompt or
//!   tool list visible instead of silently replaying stale answers.
//! * `fingerprint` absent (`null` / missing): hand-authored exchange, matches
//!   any request.
//! * [`CassetteProvider::loose`]: ignore fingerprints entirely, replay by
//!   sequence only.

use super::{
    ChatChunk, ChatRequest, ChatResponse, LLMProvider, ProviderError, ProviderResult, StreamResult,
};
use crate::runtime::pipe_id::fnv1a64;
use crate::sync;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

/// How many trailing messages feed the fingerprint. Earlier messages are
/// covered transitively (they produced the later ones) and excluding them
/// keeps the fingerprint independent of long-history compaction.
const FINGERPRINT_TAIL: usize = 3;

/// One recorded request/response pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Exchange {
    pub seq: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    pub response: ChatResponse,
}

/// Stable fingerprint of the parts of a request that should force a
/// re-record when they change: model, tool names, and the role + content of
/// the last [`FINGERPRINT_TAIL`] messages.
///
/// Uses FNV-1a (not `DefaultHasher`) so the value is stable across Rust
/// versions and can be stored in checked-in cassettes.
pub fn request_fingerprint(req: &ChatRequest) -> String {
    let mut buf = String::new();
    buf.push_str(&req.model);
    buf.push('\u{1f}');
    for t in &req.tools {
        buf.push_str(&t.function.name);
        buf.push('\u{1e}');
    }
    buf.push('\u{1f}');
    let start = req.messages.len().saturating_sub(FINGERPRINT_TAIL);
    for m in &req.messages[start..] {
        buf.push_str(&m.role);
        buf.push('\u{1e}');
        buf.push_str(&m.content);
        buf.push('\u{1e}');
    }
    format!("{:016x}", fnv1a64(buf.as_bytes()))
}

/// Replays a cassette as an [`LLMProvider`].
pub struct CassetteProvider {
    queue: Mutex<VecDeque<Exchange>>,
    strict: bool,
}

impl CassetteProvider {
    /// Parse a cassette from JSONL text. Blank lines are ignored.
    pub fn from_jsonl(text: &str) -> ProviderResult<Self> {
        let mut exchanges = VecDeque::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let ex: Exchange = serde_json::from_str(line)
                .map_err(|e| ProviderError::new(format!("cassette line {}: {e}", i + 1)))?;
            exchanges.push_back(ex);
        }
        Ok(Self {
            queue: Mutex::new(exchanges),
            strict: true,
        })
    }

    /// Load a cassette from a JSONL file.
    pub fn from_path(path: impl AsRef<Path>) -> ProviderResult<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| ProviderError::new(format!("cassette {}: {e}", path.display())))?;
        Self::from_jsonl(&text)
    }

    /// Build directly from exchanges (tests, programmatic fixtures).
    pub fn from_exchanges(exchanges: Vec<Exchange>) -> Self {
        Self {
            queue: Mutex::new(exchanges.into()),
            strict: true,
        }
    }

    /// Ignore fingerprints and replay purely by sequence.
    pub fn loose(mut self) -> Self {
        self.strict = false;
        self
    }

    /// Exchanges not yet consumed. A finished test should assert this is 0.
    pub fn remaining(&self) -> usize {
        sync::lock(&self.queue).len()
    }
}

impl LLMProvider for CassetteProvider {
    fn provider_type(&self) -> &str {
        "cassette"
    }

    fn provider_id(&self) -> &str {
        "cassette"
    }

    fn chat(&self, request: ChatRequest) -> ProviderResult<ChatResponse> {
        let mut q = sync::lock(&self.queue);
        let Some(next) = q.front() else {
            return Err(ProviderError::new(
                "cassette exhausted: the agent made more provider calls than were recorded",
            ));
        };
        if self.strict {
            if let Some(expected) = &next.fingerprint {
                let actual = request_fingerprint(&request);
                if *expected != actual {
                    return Err(ProviderError::new(format!(
                        "cassette mismatch at seq {}: request fingerprint {actual} != recorded \
                         {expected} (prompt, tools or model changed; re-record the cassette)",
                        next.seq
                    )));
                }
            }
        }
        // Checked non-empty above.
        Ok(q.pop_front().expect("front() was Some").response)
    }

    fn chat_stream(&self, _request: ChatRequest) -> StreamResult<ChatChunk, ProviderError> {
        Err(ProviderError::new(
            "cassette: streaming replay is not implemented yet",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ChatMessageRequest, ChatMessageResponse};

    fn resp(text: &str) -> ChatResponse {
        ChatResponse {
            id: "r".into(),
            model: "m".into(),
            message: ChatMessageResponse {
                role: "assistant".into(),
                content: Some(text.into()),
                tool_calls: vec![],
            },
            finish_reason: Some("stop".into()),
            usage: None,
        }
    }

    fn req(prompt: &str) -> ChatRequest {
        ChatRequest::new("m", vec![ChatMessageRequest::user(prompt)])
    }

    fn ex(seq: usize, fp: Option<String>, text: &str) -> Exchange {
        Exchange {
            seq,
            fingerprint: fp,
            response: resp(text),
        }
    }

    fn text_of(r: &ChatResponse) -> &str {
        r.message.content.as_deref().unwrap()
    }

    #[test]
    fn replays_in_order_and_drains() {
        let p = CassetteProvider::from_exchanges(vec![ex(0, None, "one"), ex(1, None, "two")]);
        assert_eq!(text_of(&p.chat(req("a")).unwrap()), "one");
        assert_eq!(p.remaining(), 1);
        assert_eq!(text_of(&p.chat(req("b")).unwrap()), "two");
        assert_eq!(p.remaining(), 0);
    }

    #[test]
    fn exhausted_cassette_errors_loudly() {
        let p = CassetteProvider::from_exchanges(vec![ex(0, None, "one")]);
        p.chat(req("a")).unwrap();
        let err = p.chat(req("b")).unwrap_err();
        assert!(err.0.contains("exhausted"), "{}", err.0);
    }

    #[test]
    fn strict_mismatch_errors_with_seq() {
        let fp = request_fingerprint(&req("original prompt"));
        let p = CassetteProvider::from_exchanges(vec![ex(0, Some(fp), "one")]);
        let err = p.chat(req("changed prompt")).unwrap_err();
        assert!(err.0.contains("mismatch at seq 0"), "{}", err.0);
        // A failed match must not consume the exchange.
        assert_eq!(p.remaining(), 1);
    }

    #[test]
    fn strict_match_replays() {
        let fp = request_fingerprint(&req("same"));
        let p = CassetteProvider::from_exchanges(vec![ex(0, Some(fp), "one")]);
        assert_eq!(text_of(&p.chat(req("same")).unwrap()), "one");
    }

    #[test]
    fn loose_ignores_fingerprint() {
        let fp = request_fingerprint(&req("original"));
        let p = CassetteProvider::from_exchanges(vec![ex(0, Some(fp), "one")]).loose();
        assert_eq!(text_of(&p.chat(req("different")).unwrap()), "one");
    }

    #[test]
    fn missing_fingerprint_matches_any_request() {
        let p = CassetteProvider::from_exchanges(vec![ex(0, None, "one")]);
        assert_eq!(text_of(&p.chat(req("whatever")).unwrap()), "one");
    }

    #[test]
    fn fingerprint_depends_on_model_tools_and_tail() {
        let base = req("hi");
        let mut other_model = req("hi");
        other_model.model = "other".into();
        assert_ne!(
            request_fingerprint(&base),
            request_fingerprint(&other_model)
        );

        // Only the last FINGERPRINT_TAIL messages count.
        let long = |first: &str| {
            let mut msgs = vec![ChatMessageRequest::user(first)];
            for i in 0..FINGERPRINT_TAIL {
                msgs.push(ChatMessageRequest::user(format!("m{i}")));
            }
            ChatRequest::new("m", msgs)
        };
        assert_eq!(
            request_fingerprint(&long("a")),
            request_fingerprint(&long("b"))
        );
    }

    #[test]
    fn jsonl_round_trip_skips_blank_lines() {
        let lines = [
            serde_json::to_string(&ex(0, None, "one")).unwrap(),
            String::new(),
            serde_json::to_string(&ex(1, Some("abc".into()), "two")).unwrap(),
        ]
        .join("\n");
        let p = CassetteProvider::from_jsonl(&lines).unwrap().loose();
        assert_eq!(p.remaining(), 2);
        assert_eq!(text_of(&p.chat(req("x")).unwrap()), "one");
    }

    #[test]
    fn malformed_line_reports_line_number() {
        let err = CassetteProvider::from_jsonl("{\"seq\":0}\nnot json")
            .err()
            .expect("must fail");
        assert!(err.0.contains("line 1"), "{}", err.0);
    }

    #[test]
    fn streaming_is_explicitly_unsupported() {
        let p = CassetteProvider::from_exchanges(vec![]);
        assert!(p.chat_stream(req("x")).is_err());
    }
}
