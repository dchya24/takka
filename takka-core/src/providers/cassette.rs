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
//! # Recording
//!
//! [`CassetteProvider::record`] wraps a real provider and appends every
//! exchange to a cassette file (the file is truncated when recording starts).
//! `chat()` calls are stored as a `response`, `chat_stream()` calls as the
//! observed `chunks`, written when the stream ends cleanly (a stream that
//! errors midway is not recorded). Each line is passed through [`scrub`]
//! before it reaches disk: only request/response bodies are ever stored,
//! never headers or config, and API-key-shaped strings and the values of
//! secret-looking environment variables are replaced with `[REDACTED]`.
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
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};

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
    /// `Some` in record mode: the wrapped provider and the cassette writer.
    recording: Option<(Arc<dyn LLMProvider>, Arc<Recorder>)>,
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
            recording: None,
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
            recording: None,
        }
    }

    /// Record mode: forward every call to `inner` and append the exchange to
    /// the cassette at `path`, truncating any existing file first.
    pub fn record(inner: Arc<dyn LLMProvider>, path: impl Into<PathBuf>) -> ProviderResult<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| ProviderError::new(format!("cassette {}: {e}", path.display())))?;
            }
        }
        std::fs::File::create(&path)
            .map_err(|e| ProviderError::new(format!("cassette {}: {e}", path.display())))?;
        let recorder = Arc::new(Recorder {
            path,
            next_seq: Mutex::new(0),
            extra_secrets: secrets_from_env(),
        });
        Ok(Self {
            queue: Mutex::new(VecDeque::new()),
            strict: true,
            recording: Some((inner, recorder)),
        })
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
        if let Some((inner, recorder)) = &self.recording {
            let fingerprint = request_fingerprint(&request);
            let response = inner.chat(request)?;
            recorder.append(fingerprint, Some(response.clone()), Vec::new())?;
            return Ok(response);
        }
        self.take_next(&request)?.to_response()
    }

    fn chat_stream(&self, request: ChatRequest) -> StreamResult<ChatChunk, ProviderError> {
        if let Some((inner, recorder)) = &self.recording {
            let fingerprint = request_fingerprint(&request);
            let stream = inner.chat_stream(request)?;
            return Ok(Box::pin(RecordingStream {
                inner: stream,
                chunks: Vec::new(),
                recorder: recorder.clone(),
                fingerprint,
                finished: false,
            }));
        }
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

/// Appends scrubbed exchanges to the cassette file.
struct Recorder {
    path: PathBuf,
    next_seq: Mutex<usize>,
    extra_secrets: Vec<String>,
}

impl Recorder {
    fn append(
        &self,
        fingerprint: String,
        response: Option<ChatResponse>,
        chunks: Vec<ChatChunk>,
    ) -> ProviderResult<()> {
        use std::io::Write;
        // Hold the lock across the write so seq order matches file order.
        let mut seq = sync::lock(&self.next_seq);
        let exchange = Exchange {
            seq: *seq,
            fingerprint: Some(fingerprint),
            response,
            chunks,
        };
        let line = serde_json::to_string(&exchange)
            .map_err(|e| ProviderError::new(format!("cassette encode: {e}")))?;
        let line = scrub(&line, &self.extra_secrets);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|e| ProviderError::new(format!("cassette {}: {e}", self.path.display())))?;
        writeln!(file, "{line}")
            .map_err(|e| ProviderError::new(format!("cassette {}: {e}", self.path.display())))?;
        *seq += 1;
        Ok(())
    }
}

/// Passes chunks through unchanged and records them once the stream ends.
struct RecordingStream {
    inner: Pin<Box<dyn futures::Stream<Item = Result<ChatChunk, ProviderError>> + Send + Sync>>,
    chunks: Vec<ChatChunk>,
    recorder: Arc<Recorder>,
    fingerprint: String,
    /// Set after the exchange is written, or after an error (not recorded).
    finished: bool,
}

impl futures::Stream for RecordingStream {
    type Item = Result<ChatChunk, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.chunks.push(chunk.clone());
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.finished = true;
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                if !this.finished {
                    this.finished = true;
                    let chunks = std::mem::take(&mut this.chunks);
                    if let Err(e) = this.recorder.append(this.fingerprint.clone(), None, chunks) {
                        log::warn!("cassette: failed to record stream: {e}");
                    }
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

const REDACTED: &str = "[REDACTED]";
/// Environment values shorter than this are not treated as secrets (avoids
/// redacting things like `KEY=1`).
const MIN_SECRET_LEN: usize = 12;

fn secret_patterns() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(concat!(
            r"sk-[A-Za-z0-9_-]{16,}",
            r"|gh[pousr]_[A-Za-z0-9]{20,}",
            r"|AKIA[0-9A-Z]{16}",
            r"|xox[abprs]-[A-Za-z0-9-]{10,}",
            r"|(?i:bearer)\s+[A-Za-z0-9._~+/=-]{16,}",
        ))
        .expect("static secret regex is valid")
    })
}

/// Values of environment variables whose names look secret.
fn secrets_from_env() -> Vec<String> {
    std::env::vars()
        .filter(|(k, v)| {
            let k = k.to_ascii_uppercase();
            v.len() >= MIN_SECRET_LEN
                && ["KEY", "TOKEN", "SECRET", "PASSWORD"]
                    .iter()
                    .any(|w| k.contains(w))
        })
        .map(|(_, v)| v)
        .collect()
}

/// Replace API-key-shaped strings and every occurrence of `extra_secrets`
/// with `[REDACTED]`. Applied to the serialized JSON line; replacements
/// contain no quote or backslash, so the line stays valid JSON.
pub fn scrub(text: &str, extra_secrets: &[String]) -> String {
    let mut out = secret_patterns().replace_all(text, REDACTED).into_owned();
    for s in extra_secrets {
        if s.len() >= MIN_SECRET_LEN {
            out = out.replace(s.as_str(), REDACTED);
        }
    }
    out
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

    // ---- record mode -------------------------------------------------

    /// Provider that returns fixed responses/chunks, standing in for a real one.
    struct Fake {
        text: String,
        stream_chunks: Vec<ChatChunk>,
        fail_stream: bool,
    }
    impl LLMProvider for Fake {
        fn provider_type(&self) -> &str {
            "fake"
        }
        fn provider_id(&self) -> &str {
            "fake"
        }
        fn chat(&self, _r: ChatRequest) -> ProviderResult<ChatResponse> {
            Ok(resp(&self.text))
        }
        fn chat_stream(&self, _r: ChatRequest) -> StreamResult<ChatChunk, ProviderError> {
            let mut items: Vec<Result<ChatChunk, ProviderError>> =
                self.stream_chunks.iter().cloned().map(Ok).collect();
            if self.fail_stream {
                items.push(Err(ProviderError::new("boom")));
            }
            Ok(Box::pin(futures::stream::iter(items)))
        }
    }

    fn fake(text: &str) -> Arc<dyn LLMProvider> {
        Arc::new(Fake {
            text: text.into(),
            stream_chunks: vec![chunk("a", vec![], None), chunk("b", vec![], Some("stop"))],
            fail_stream: false,
        })
    }

    fn tmp_cassette() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("nested/dir/c.jsonl");
        (d, p)
    }

    #[test]
    fn record_then_replay_round_trip_is_strict() {
        let (_d, path) = tmp_cassette();
        let rec = CassetteProvider::record(fake("recorded answer"), &path).unwrap();
        let live = rec.chat(req("question")).unwrap();
        assert_eq!(text_of(&live), "recorded answer");

        let replay = CassetteProvider::from_path(&path).unwrap();
        assert_eq!(replay.remaining(), 1);
        // Same request replays...
        assert_eq!(
            text_of(&replay.chat(req("question")).unwrap()),
            "recorded answer"
        );
        // ...a changed prompt is rejected by the recorded fingerprint.
        let replay = CassetteProvider::from_path(&path).unwrap();
        let err = replay.chat(req("different question")).unwrap_err();
        assert!(err.0.contains("mismatch at seq 0"), "{}", err.0);
    }

    #[test]
    fn record_assigns_increasing_seq_and_truncates_old_file() {
        let (_d, path) = tmp_cassette();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "stale garbage\n").unwrap();
        let rec = CassetteProvider::record(fake("x"), &path).unwrap();
        rec.chat(req("1")).unwrap();
        rec.chat(req("2")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let seqs: Vec<usize> = text
            .lines()
            .map(|l| serde_json::from_str::<Exchange>(l).unwrap().seq)
            .collect();
        assert_eq!(seqs, vec![0, 1]);
    }

    #[tokio::test]
    async fn record_stream_then_replay_chunks() {
        let (_d, path) = tmp_cassette();
        let rec = CassetteProvider::record(fake("unused"), &path).unwrap();
        let live = collect(&rec, req("q")).await;
        assert_eq!(live.len(), 2);

        let replay = CassetteProvider::from_path(&path).unwrap();
        let replayed = collect(&replay, req("q")).await;
        let deltas: Vec<&str> = replayed.iter().map(|c| c.delta.as_str()).collect();
        assert_eq!(deltas, vec!["a", "b"]);
        assert_eq!(replayed[1].finish_reason.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn failed_stream_is_not_recorded() {
        use futures::StreamExt;
        let (_d, path) = tmp_cassette();
        let inner: Arc<dyn LLMProvider> = Arc::new(Fake {
            text: String::new(),
            stream_chunks: vec![chunk("a", vec![], None)],
            fail_stream: true,
        });
        let rec = CassetteProvider::record(inner, &path).unwrap();
        let items: Vec<_> = rec.chat_stream(req("q")).unwrap().collect().await;
        assert!(items.last().unwrap().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    #[test]
    fn recorded_secrets_are_scrubbed_but_line_stays_valid() {
        let (_d, path) = tmp_cassette();
        let rec = CassetteProvider::record(
            fake("here is the key sk-abcdefghijklmnopqrstuvwxyz0123 and Bearer abcdefghijklmnop12345"),
            &path,
        )
        .unwrap();
        rec.chat(req("q")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("sk-abcdefghijkl"), "{text}");
        assert!(!text.contains("abcdefghijklmnop12345"), "{text}");
        assert!(text.contains(REDACTED));
        // Still parses.
        CassetteProvider::from_jsonl(&text).unwrap();
    }

    #[test]
    fn scrub_handles_patterns_and_extra_secrets() {
        let out = scrub(
            "ghp_abcdefghijklmnopqrstuv AKIAABCDEFGHIJKLMNOP my-env-secret-value-1234 ok",
            &["my-env-secret-value-1234".to_string(), "short".to_string()],
        );
        assert_eq!(out, format!("{r} {r} {r} ok", r = REDACTED));
        // Short extras are ignored, ordinary text is untouched.
        assert_eq!(
            scrub("short and plain", &["short".to_string()]),
            "short and plain"
        );
    }
}
