//! OpenAI-compatible provider implementation

use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};

use super::{ChatRequest, ChatResponse, ChatUsage, LLMProvider, ProviderError, ProviderResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_base_delay_ms")]
    pub base_delay_ms: u64,
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,
}

fn default_max_retries() -> u32 {
    3
}
fn default_base_delay_ms() -> u64 {
    1000
}
fn default_max_delay_ms() -> u64 {
    30000
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            base_delay_ms: default_base_delay_ms(),
            max_delay_ms: default_max_delay_ms(),
        }
    }
}

impl RetryConfig {
    fn delay_for_attempt(&self, attempt: u32) -> std::time::Duration {
        let exp = 2u64.saturating_pow(attempt);
        let delay = self.base_delay_ms.saturating_mul(exp);
        std::time::Duration::from_millis(delay.min(self.max_delay_ms))
    }
}

pub struct OpenAIProvider {
    config: OpenAIProviderConfig,
    async_client: reqwest::Client,
    retry: RetryConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIProviderConfig {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    pub base_url: String,
    pub api_key: String,
    pub models: Vec<ModelConfig>,
    pub default_model: String,
    #[serde(default)]
    pub retry: RetryConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub context_window: Option<u32>,
}

impl OpenAIProviderConfig {
    pub fn new(
        id: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        default_model: impl Into<String>,
    ) -> Self {
        let id_str = id.into();
        Self {
            id: id_str.clone(),
            name: id_str,
            provider_type: "openai-compatible".into(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            models: vec![],
            default_model: default_model.into(),
            retry: RetryConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct OpenAIChatResponse {
    id: String,
    #[allow(dead_code)]
    object: String,
    #[allow(dead_code)]
    created: u64,
    model: String,
    choices: Vec<OpenAIChoice>,
    usage: Option<OpenAIUsageResp>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChoice {
    #[allow(dead_code)]
    index: u32,
    message: OpenAIMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIMessage {
    role: String,
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAIToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OpenAIToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OpenAIFunction,
}

#[derive(Debug, Deserialize)]
struct OpenAIFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIUsageResp {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    total_tokens: Option<u32>,
}

impl OpenAIProvider {
    pub fn new(config: OpenAIProviderConfig) -> Self {
        let async_client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("Failed to build async HTTP client");

        let retry = config.retry.clone();
        Self {
            config,
            async_client,
            retry,
        }
    }

    fn extract_sse_line(buffer: &mut String) -> Option<String> {
        while let Some(pos) = buffer.find('\n') {
            let line = buffer[..pos].trim().to_string();
            buffer.drain(..=pos);
            if let Some(data) = line.strip_prefix("data: ") {
                let trimmed = data.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }
        None
    }

    fn parse_sse_chunk(data: &str) -> Option<super::ChatChunk> {
        #[derive(Deserialize)]
        struct StreamFunctionDelta {
            name: Option<String>,
            arguments: Option<String>,
        }
        #[derive(Deserialize)]
        struct StreamToolCallDelta {
            index: u32,
            id: Option<String>,
            function: Option<StreamFunctionDelta>,
        }
        #[derive(Deserialize)]
        struct StreamDelta {
            content: Option<String>,
            #[serde(default)]
            tool_calls: Option<Vec<StreamToolCallDelta>>,
        }
        #[derive(Deserialize)]
        struct StreamChoice {
            delta: StreamDelta,
            finish_reason: Option<String>,
        }
        #[derive(Deserialize)]
        struct StreamResponse {
            id: String,
            choices: Vec<StreamChoice>,
            #[serde(default)]
            usage: Option<OpenAIUsageResp>,
        }

        let resp: StreamResponse = serde_json::from_str(data).ok()?;
        let choice = resp.choices.first()?;
        let tool_calls: Vec<super::ToolCallDelta> = choice
            .delta
            .tool_calls
            .as_ref()
            .map(|tcs| {
                tcs.iter()
                    .map(|tc| super::ToolCallDelta {
                        index: tc.index,
                        id: tc.id.clone(),
                        function_name: tc.function.as_ref().and_then(|f| f.name.clone()),
                        function_arguments: tc.function.as_ref().and_then(|f| f.arguments.clone()),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let usage = resp.usage.map(|u| super::ChatUsage {
            prompt_tokens: u.prompt_tokens.unwrap_or(0),
            completion_tokens: u.completion_tokens.unwrap_or(0),
            total_tokens: u.total_tokens.unwrap_or(0),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        });
        Some(super::ChatChunk {
            id: resp.id,
            delta: choice.delta.content.clone().unwrap_or_default(),
            finish_reason: choice.finish_reason.clone(),
            tool_calls,
            usage,
        })
    }

    fn is_retryable_error(error: &ProviderError) -> bool {
        let msg = error.0.to_lowercase();
        msg.contains("timeout")
            || msg.contains("connection")
            || msg.contains("429")
            || msg.contains("500")
            || msg.contains("502")
            || msg.contains("503")
            || msg.contains("504")
    }

    pub fn chat(&self, request: ChatRequest) -> ProviderResult<ChatResponse> {
        let system_content = request.effective_system_prompt().to_string();

        let mut messages: Vec<super::ChatMessageRequest> = vec![super::ChatMessageRequest {
            role: "system".into(),
            content: system_content,
            tool_call_id: None,
            tool_calls: vec![],
            attachments: Vec::new(),
        }];

        messages.extend(request.messages.iter().cloned());

        let wire_messages = serialize_messages_for_wire(&messages);
        let mut body = serde_json::json!({
            "model": request.model,
            "messages": wire_messages,
        });

        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(max) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }
        if !request.tools.is_empty() {
            body["tools"] = serde_json::json!(request.tools);
        }

        tracing::trace!(
            provider = %self.config.id,
            model = %request.model,
            messages = request.messages.len(),
            tools = request.tools.len(),
            stream = false,
            "openai chat request"
        );

        let mut last_error = None;

        for attempt in 0..=self.retry.max_retries {
            if attempt > 0 {
                let delay = self.retry.delay_for_attempt(attempt - 1);
                log::warn!(
                    "Retry attempt {}/{} after {:?}",
                    attempt,
                    self.retry.max_retries,
                    delay
                );
                std::thread::sleep(delay);
            }

            match self.chat_once(&body) {
                Ok(response) => return Ok(response),
                Err(e) => {
                    let retryable = Self::is_retryable_error(&e);
                    last_error = Some(e);
                    if !retryable || attempt == self.retry.max_retries {
                        break;
                    }
                }
            }
        }

        Err(last_error.unwrap())
    }

    fn chat_once(&self, body: &serde_json::Value) -> ProviderResult<ChatResponse> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );

        let response = futures::executor::block_on(async {
            self.async_client
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.config.api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
        })
        .map_err(|e| ProviderError::new(format!("Request failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = futures::executor::block_on(response.text()).unwrap_or_default();
            return Err(ProviderError::new(format!(
                "API error ({}): {}",
                status, text
            )));
        }

        let oai_response: OpenAIChatResponse = futures::executor::block_on(response.json())
            .map_err(|e| ProviderError::new(format!("Failed to parse response: {}", e)))?;

        let choice = oai_response
            .choices
            .first()
            .ok_or_else(|| ProviderError::new("No choices in response".to_string()))?;

        let usage = oai_response.usage.map(|u| ChatUsage {
            prompt_tokens: u.prompt_tokens.unwrap_or(0),
            completion_tokens: u.completion_tokens.unwrap_or(0),
            total_tokens: u.total_tokens.unwrap_or(0),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        });

        let tool_calls: Vec<super::ToolCallResponse> = choice
            .message
            .tool_calls
            .as_ref()
            .map(|tc| {
                tc.iter()
                    .map(|t| super::ToolCallResponse {
                        id: t.id.clone(),
                        call_type: t.call_type.clone(),
                        function: super::ToolCallFunction {
                            name: t.function.name.clone(),
                            arguments: t.function.arguments.clone(),
                        },
                    })
                    .collect()
            })
            .unwrap_or_default();

        tracing::trace!(
            provider = %self.config.id,
            model = %oai_response.model,
            finish_reason = ?choice.finish_reason,
            prompt_tokens = usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0),
            completion_tokens = usage.as_ref().map(|u| u.completion_tokens).unwrap_or(0),
            tool_calls = tool_calls.len(),
            "openai chat response"
        );

        Ok(ChatResponse {
            id: oai_response.id,
            model: oai_response.model,
            message: super::ChatMessageResponse {
                role: choice.message.role.clone(),
                content: choice.message.content.clone(),
                tool_calls,
            },
            finish_reason: choice.finish_reason.clone(),
            usage,
        })
    }
}

impl LLMProvider for OpenAIProvider {
    fn provider_type(&self) -> &str {
        &self.config.provider_type
    }

    fn provider_id(&self) -> &str {
        &self.config.id
    }

    fn chat(&self, request: ChatRequest) -> ProviderResult<ChatResponse> {
        self.chat(request)
    }

    fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> super::StreamResult<super::ChatChunk, ProviderError> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );

        let system_content = request.effective_system_prompt().to_string();

        let mut messages: Vec<super::ChatMessageRequest> = vec![super::ChatMessageRequest {
            role: "system".into(),
            content: system_content,
            tool_call_id: None,
            tool_calls: vec![],
            attachments: Vec::new(),
        }];

        messages.extend(request.messages.iter().cloned());

        let wire_messages = serialize_messages_for_wire(&messages);
        let mut body = serde_json::json!({
            "model": request.model,
            "messages": wire_messages,
            "stream": true,
        });

        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(max) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }
        if !request.tools.is_empty() {
            body["tools"] = serde_json::json!(request.tools);
        }

        tracing::trace!(
            provider = %self.config.id,
            model = %request.model,
            messages = request.messages.len(),
            tools = request.tools.len(),
            stream = true,
            "openai chat_stream request"
        );

        let api_key = self.config.api_key.clone();
        let async_client = self.async_client.clone();

        let stream = async_stream::stream! {
            let response = match async_client
                .post(&url)
                .header("Authorization", format!("Bearer {}", api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if !status.is_success() {
                        let text = resp.text().await.unwrap_or_default();
                        log::error!("Stream request failed: {} - Body: {}", status, text);
                        yield Err(ProviderError::new(format!(
                            "Stream API error: HTTP status {} error ({}) for url ({})",
                            status, text, url
                        )));
                        return;
                    }
                    resp
                },
                Err(e) => {
                    yield Err(ProviderError::new(format!("Stream request failed: {}", e)));
                    return;
                }
            };

            let mut buffer = String::new();
            let mut byte_stream = response.bytes_stream();

            while let Some(chunk_result) = byte_stream.next().await {
                match chunk_result {
                    Ok(bytes) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(line) = Self::extract_sse_line(&mut buffer) {
                            if line == "[DONE]" {
                                return;
                            }
                            if let Some(chat_chunk) = Self::parse_sse_chunk(&line) {
                                yield Ok(chat_chunk);
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(ProviderError::new(format!("Stream read error: {}", e)));
                        return;
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }

    fn health_check(&self) -> super::ProviderResult<bool> {
        let url = format!("{}/models", self.config.base_url.trim_end_matches('/'));
        let result = futures::executor::block_on(async {
            self.async_client
                .get(&url)
                .header("Authorization", format!("Bearer {}", self.config.api_key))
                .send()
                .await
        });
        match result {
            Ok(resp) if resp.status().is_success() => Ok(true),
            Ok(resp) => Err(super::ProviderError::new(format!(
                "Health check failed: HTTP {}",
                resp.status()
            ))),
            Err(e) => Err(super::ProviderError::new(format!(
                "Health check connection failed: {}",
                e
            ))),
        }
    }

    fn list_models(&self) -> super::ProviderResult<Vec<super::ModelInfo>> {
        let url = format!("{}/models", self.config.base_url.trim_end_matches('/'));
        let response = futures::executor::block_on(async {
            self.async_client
                .get(&url)
                .header("Authorization", format!("Bearer {}", self.config.api_key))
                .send()
                .await
        })
        .map_err(|e| super::ProviderError::new(format!("List models request failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(super::ProviderError::new(format!(
                "List models failed: HTTP {}",
                response.status()
            )));
        }

        #[derive(Deserialize)]
        struct ModelsResponse {
            data: Vec<ModelEntry>,
        }
        #[derive(Deserialize)]
        struct ModelEntry {
            id: String,
        }

        let models: ModelsResponse = futures::executor::block_on(response.json()).map_err(|e| {
            super::ProviderError::new(format!("Failed to parse models response: {}", e))
        })?;

        Ok(models
            .data
            .into_iter()
            .map(|m| super::ModelInfo {
                id: m.id.clone(),
                name: m.id,
                context_window: None,
                capabilities: vec![
                    super::ModelCapability::Chat,
                    super::ModelCapability::Streaming,
                    super::ModelCapability::ToolCalling,
                ],
            })
            .collect())
    }

    fn count_tokens(&self, text: &str) -> usize {
        // OpenAI uses BPE, ~4 chars per token is a decent approximation
        text.len() / 4
    }
}

/// Serialize a list of `ChatMessageRequest`s into the OpenAI wire
/// format, expanding any image attachments into the multimodal content
/// shape that vision-capable models expect:
///
/// ```json
/// {
///   "role": "user",
///   "content": [
///     {"type": "text", "text": "What's in this image?"},
///     {"type": "image_url", "image_url": {"url": "data:image/png;base64,..."}}
///   ]
/// }
/// ```
///
/// Messages without attachments serialize through the existing
/// `Serialize` impl (string content) so the wire format stays unchanged
/// for the common case. Tool calls + tool_call_id are forwarded
/// verbatim.
fn serialize_messages_for_wire(messages: &[super::ChatMessageRequest]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|m| {
            if m.attachments.is_empty() {
                // Common path: delegate to the existing string-content
                // Serialize impl.
                serde_json::to_value(m).unwrap_or(serde_json::Value::Null)
            } else {
                let mut parts: Vec<serde_json::Value> = Vec::new();
                if !m.content.is_empty() {
                    parts.push(serde_json::json!({
                        "type": "text",
                        "text": m.content,
                    }));
                }
                for att in &m.attachments {
                    let url = match &att.source {
                        crate::attachments::AttachmentSource::RemoteUrl { url } => url.clone(),
                        _ => att.as_data_url(),
                    };
                    parts.push(serde_json::json!({
                        "type": "image_url",
                        "image_url": { "url": url },
                    }));
                }
                let mut obj = serde_json::Map::new();
                obj.insert("role".into(), serde_json::json!(m.role));
                obj.insert("content".into(), serde_json::json!(parts));
                if let Some(ref id) = m.tool_call_id {
                    obj.insert("tool_call_id".into(), serde_json::json!(id));
                }
                if !m.tool_calls.is_empty() {
                    obj.insert("tool_calls".into(), serde_json::json!(m.tool_calls));
                }
                serde_json::Value::Object(obj)
            }
        })
        .collect()
}

#[cfg(test)]
mod wire_format_tests {
    use super::super::ChatMessageRequest;
    use super::serialize_messages_for_wire;
    use crate::attachments::{Attachment, AttachmentKind, AttachmentSource};

    fn png_attachment() -> Attachment {
        Attachment {
            kind: AttachmentKind::Image,
            mime_type: "image/png".into(),
            data_base64: "iVBORw0KGgo=".into(),
            source: AttachmentSource::FilePath {
                path: "/tmp/x.png".into(),
            },
            size_bytes: 8,
        }
    }

    fn remote_attachment() -> Attachment {
        Attachment {
            kind: AttachmentKind::Image,
            mime_type: String::new(),
            data_base64: String::new(),
            source: AttachmentSource::RemoteUrl {
                url: "https://example.com/cat.png".into(),
            },
            size_bytes: 0,
        }
    }

    #[test]
    fn no_attachment_falls_back_to_string_content() {
        let msg = ChatMessageRequest::user("hello");
        let wire = serialize_messages_for_wire(&[msg]);
        assert_eq!(wire.len(), 1);
        // String content for the common path.
        assert_eq!(wire[0]["content"], "hello");
    }

    #[test]
    fn single_image_produces_text_plus_image_url_parts() {
        let msg =
            ChatMessageRequest::user("What's in this?").with_attachments(vec![png_attachment()]);
        let wire = serialize_messages_for_wire(&[msg]);
        let parts = wire[0]["content"].as_array().expect("array content");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "What's in this?");
        assert_eq!(parts[1]["type"], "image_url");
        let url = parts[1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn empty_text_with_image_omits_text_part() {
        let msg = ChatMessageRequest::user("").with_attachments(vec![png_attachment()]);
        let wire = serialize_messages_for_wire(&[msg]);
        let parts = wire[0]["content"].as_array().expect("array content");
        // Only the image part — no leading empty text part.
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "image_url");
    }

    #[test]
    fn remote_url_attachment_passes_url_through() {
        let msg = ChatMessageRequest::user("caption").with_attachments(vec![remote_attachment()]);
        let wire = serialize_messages_for_wire(&[msg]);
        let parts = wire[0]["content"].as_array().unwrap();
        assert_eq!(parts[1]["image_url"]["url"], "https://example.com/cat.png");
    }

    #[test]
    fn multiple_attachments_render_in_order() {
        let msg = ChatMessageRequest::user("two pics")
            .with_attachments(vec![png_attachment(), remote_attachment()]);
        let wire = serialize_messages_for_wire(&[msg]);
        let parts = wire[0]["content"].as_array().unwrap();
        // text + 2 images = 3 parts
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[2]["type"], "image_url");
        assert!(parts[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:"));
        assert_eq!(parts[2]["image_url"]["url"], "https://example.com/cat.png");
    }

    #[test]
    fn role_passes_through() {
        let msg = ChatMessageRequest::user("x").with_attachments(vec![png_attachment()]);
        let wire = serialize_messages_for_wire(&[msg]);
        assert_eq!(wire[0]["role"], "user");
    }
}
