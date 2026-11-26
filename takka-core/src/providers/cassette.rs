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
//! no ambient config. It serves both `chat()` and `chat_stream()`:
//!
//! * an exchange with a `response` is replayed as-is for `chat()` and
//!   synthesised into a one-chunk stream for `chat_stream()`;
//! * an exchange with recorded `chunks` is replayed chunk by chunk for
//!   `chat_stream()` and folded into a `ChatResponse` for `chat()`.
//!
//! Record mode is a separate step of
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
    ChatChunk, ChatMessageResponse, ChatRequest, ChatResponse, LLMProvider, ProviderError,
    ProviderResult, StreamResult, ToolCallDelta, ToolCallFunction, ToolCallResponse,
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
    /// Full response (non-streaming recording or hand-authored).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ChatResponse>,
    /// Recorded stream chunks (streaming recording).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<ChatChunk>,
}

impl Exchange {
    fn to_response(&self) -> ProviderResult<ChatResponse> {
        match (&self.response, self.chunks.is_empty()) {
            (Some(r), _) => Ok(r.clone()),
            (None, false) => Ok(response_from_chunks(&self.chunks)),
            (None, true) => Err(ProviderError::new(format!(
                "cassette seq {}: exchange has neither response nor chunks",
                self.seq
            ))),
        }
    }

    fn to_chunks(&self) -> ProviderResult<Vec<ChatChunk>> {
        if !self.chunks.is_empty() {
            return Ok(self.chunks.clone());
        }
        match &self.response {
            Some(r) => Ok(chunks_from_response(r)),
            None => Err(ProviderError::new(format!(
                "cassette seq {}: exchange has neither response nor chunks",
                self.seq
            ))),
        }
    }
}

/// Encode a full response as a single stream chunk (text delta plus every
/// tool call with complete arguments), the shape the orchestrator's stream
/// accumulator already handles.
pub fn chunks_from_response(resp: &ChatResponse) -> Vec<ChatChunk> {
    let tool_calls = resp
        .message
        .tool_calls
        .iter()
        .enumerate()
        .map(|(i, tc)| ToolCallDelta {
            index: i as u32,
            id: Some(tc.id.clone()),
            function_name: Some(tc.function.name.clone()),
            function_arguments: Some(tc.function.arguments.clone()),
        })
        .collect();
    vec![ChatChunk {
        id: resp.id.clone(),
        delta: resp.message.content.clone().unwrap_or_default(),
        finish_reason: resp.finish_reason.clone(),
        tool_calls,
        usage: resp.usage.clone(),
    }]
}

/// Fold recorded stream chunks back into one response: text deltas are
/// concatenated, tool-call deltas are merged by `index` (arguments appended),
/// and the last `finish_reason` / `usage` seen win.
pub fn response_from_chunks(chunks: &[ChatChunk]) -> ChatResponse {
    let mut content = String::new();
    let mut calls: Vec<(String, String, String)> = Vec::new(); // id, name, args
    let mut finish_reason = None;
    let mut usage = None;
    for c in chunks {
        content.push_str(&c.delta);
        for d in &c.tool_calls {
            let i = d.index as usize;
            while calls.len() <= i {
                calls.push(Default::default());
            }
            if let Some(id) = &d.id {
                calls[i].0 = id.clone();
            }
            if let Some(n) = &d.function_name {
                calls[i].1.push_str(n);
            }
            if let Some(a) = &d.function_arguments {
                calls[i].2.push_str(a);
            }
        }
        if c.finish_reason.is_some() {
            finish_reason = c.finish_reason.clone();
        }
        if c.usage.is_some() {
            usage = c.usage.clone();
        }
    }
    ChatResponse {
        id: chunks.first().map(|c| c.id.clone()).unwrap_or_default(),
        model: String::new(),
        message: ChatMessageResponse {
            role: "assistant".into(),
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
            tool_calls: calls
                .into_iter()
                .map(|(id, name, arguments)| ToolCallResponse {
                    id,
                    call_type: "function".into(),
                    function: ToolCallFunction { name, arguments },
                })
                .collect(),
        },
        finish_reason,
        usage,
    }
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
        self.take_next(&request)?.to_response()
    }

    fn chat_stream(&self, request: ChatRequest) -> StreamResult<ChatChunk, ProviderError> {
        let chunks = self.take_next(&request)?.to_chunks()?;
        Ok(Box::pin(futures::stream::iter(
            chunks.into_iter().map(Ok::<_, ProviderError>),
        )))
    }
}

impl CassetteProvider {
    /// Pop the next exchange, enforcing exhaustion and strict fingerprints.
    /// A failed match leaves the queue untouched.
    fn take_next(&self, request: &ChatRequest) -> ProviderResult<Exchange> {
        let mut q = sync::lock(&self.queue);
        let Some(next) = q.front() else {
            return Err(ProviderError::new(
                "cassette exhausted: the agent made more provider calls than were recorded",
            ));
        };
        if self.strict {
            if let Some(expected) = &next.fingerprint {
                let actual = request_fingerprint(request);
                if *expected != actual {
                    return Err(ProviderError::new(format!(
                        "cassette mismatch at seq {}: request fingerprint {actual} != recorded \
                         {expected} (prompt, tools or model changed; re-record the cassette)",
                        next.seq
                    )));
                }
            }
        }
        Ok(q.pop_front().expect("front() was Some"))
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
            response: Some(resp(text)),
            chunks: vec![],
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

    async fn collect(p: &CassetteProvider, r: ChatRequest) -> Vec<ChatChunk> {
        use futures::StreamExt;
        let stream = p.chat_stream(r).expect("stream");
        stream.map(|c| c.expect("chunk")).collect().await
    }

    #[tokio::test]
    async fn response_exchange_streams_as_one_chunk() {
        let p = CassetteProvider::from_exchanges(vec![ex(0, None, "hello")]);
        let chunks = collect(&p, req("x")).await;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].delta, "hello");
        assert_eq!(chunks[0].finish_reason.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn tool_calls_stream_with_distinct_indices() {
        let mut r = resp("");
        r.message.tool_calls = vec![
            ToolCallResponse {
                id: "a".into(),
                call_type: "function".into(),
                function: ToolCallFunction {
                    name: "read_file".into(),
                    arguments: "{}".into(),
                },
            },
            ToolCallResponse {
                id: "b".into(),
                call_type: "function".into(),
                function: ToolCallFunction {
                    name: "glob".into(),
                    arguments: "{}".into(),
                },
            },
        ];
        let p = CassetteProvider::from_exchanges(vec![Exchange {
            seq: 0,
            fingerprint: None,
            response: Some(r),
            chunks: vec![],
        }]);
        let chunks = collect(&p, req("x")).await;
        let idx: Vec<u32> = chunks[0].tool_calls.iter().map(|t| t.index).collect();
        assert_eq!(idx, vec![0, 1]);
    }

    fn chunk(delta: &str, calls: Vec<ToolCallDelta>, fin: Option<&str>) -> ChatChunk {
        ChatChunk {
            id: "c".into(),
            delta: delta.into(),
            finish_reason: fin.map(Into::into),
            tool_calls: calls,
            usage: None,
        }
    }

    #[tokio::test]
    async fn recorded_chunks_replay_in_order_and_fold_for_chat() {
        let chunks = vec![
            chunk("Hel", vec![], None),
            chunk(
                "lo",
                vec![ToolCallDelta {
                    index: 0,
                    id: Some("t1".into()),
                    function_name: Some("read_file".into()),
                    function_arguments: Some("{\"pa".into()),
                }],
                None,
            ),
            chunk(
                "",
                vec![ToolCallDelta {
                    index: 0,
                    id: None,
                    function_name: None,
                    function_arguments: Some("th\":\"x\"}".into()),
                }],
                Some("tool_calls"),
            ),
        ];
        let mk = || {
            CassetteProvider::from_exchanges(vec![Exchange {
                seq: 0,
                fingerprint: None,
                response: None,
                chunks: chunks.clone(),
            }])
        };
        let streamed = collect(&mk(), req("x")).await;
        let deltas: Vec<&str> = streamed.iter().map(|c| c.delta.as_str()).collect();
        assert_eq!(deltas, vec!["Hel", "lo", ""]);

        let folded = mk().chat(req("x")).unwrap();
        assert_eq!(folded.message.content.as_deref(), Some("Hello"));
        assert_eq!(folded.message.tool_calls[0].id, "t1");
        assert_eq!(folded.message.tool_calls[0].function.name, "read_file");
        assert_eq!(
            folded.message.tool_calls[0].function.arguments,
            "{\"path\":\"x\"}"
        );
        assert_eq!(folded.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn empty_exchange_is_an_error() {
        let p = CassetteProvider::from_exchanges(vec![Exchange {
            seq: 3,
            fingerprint: None,
            response: None,
            chunks: vec![],
        }]);
        let err = p.chat(req("x")).unwrap_err();
        assert!(err.0.contains("seq 3"), "{}", err.0);
    }
}
