//! Anthropic Claude provider implementation

use futures::stream::StreamExt;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use super::{ChatRequest, ChatResponse, ChatUsage, LLMProvider, ProviderError, ProviderResult};
use crate::config::BreakpointStrategy;

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
    pub fn delay_for_attempt(&self, attempt: u32) -> std::time::Duration {
        let exp = 2u64.saturating_pow(attempt);
        let delay = self.base_delay_ms.saturating_mul(exp);
        std::time::Duration::from_millis(delay.min(self.max_delay_ms))
    }
}

pub struct AnthropicProvider {
    config: AnthropicProviderConfig,
    client: Client,
    async_client: reqwest::Client,
    retry: RetryConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicProviderConfig {
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
    #[serde(default)]
    pub version: String,
    /// Cache configuration for prompt caching.
    #[serde(default)]
    pub cache: super::super::config::CacheConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub context_window: Option<u32>,
}

impl AnthropicProviderConfig {
    pub fn new(
        id: impl Into<String>,
        api_key: impl Into<String>,
        default_model: impl Into<String>,
    ) -> Self {
        let id_str = id.into();
        Self {
            id: id_str.clone(),
            name: id_str,
            provider_type: "anthropic".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            api_key: api_key.into(),
            models: vec![],
            default_model: default_model.into(),
            retry: RetryConfig::default(),
            version: "2023-06-01".into(),
            cache: super::super::config::CacheConfig::default(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }
}

// Anthropic API request types
//
// Note: structs for the legacy non-untagged content shape (AnthropicMessage,
// AnthropicToolUse, AnthropicToolResult) were removed when we switched to
// the untagged `AnthropicContentBlock` enum. Restore from git history if a
// future change needs the named-struct form.

/// Cache control marker for Anthropic prompt caching.
///
/// See: https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching
#[derive(Debug, Clone, Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    marker_type: String,
}

impl CacheControl {
    fn ephemeral() -> Self {
        Self {
            marker_type: "ephemeral".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum AnthropicContentBlock {
    Text {
        #[serde(rename = "type")]
        text_type: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        #[serde(rename = "type")]
        tool_use_type: String,
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        #[serde(rename = "type")]
        tool_result_type: String,
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    /// Image attachment. Anthropic accepts two `source.type` shapes:
    ///   - `"base64"` with `media_type` + `data` for inline payloads.
    ///   - `"url"` with `url` for remote URLs.
    Image {
        #[serde(rename = "type")]
        image_type: String,
        source: AnthropicImageSource,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicMessageRequest {
    role: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    content: Vec<AnthropicContentBlock>,
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<AnthropicContentBlock>>,
    messages: Vec<AnthropicMessageRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    max_tokens: u32,
    stream: bool,
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicToolDefinition {
    #[serde(rename = "type")]
    tool_type: String,
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

// Anthropic API response types

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicResponse {
    pub id: String,
    #[allow(dead_code)]
    pub r#type: String,
    pub role: String,
    pub content: Vec<AnthropicContentBlockResponse>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    pub usage: Option<AnthropicUsage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AnthropicContentBlockResponse {
    Text {
        text: String,
    },
    ToolUse {
        #[serde(rename = "type")]
        tool_use_type: String,
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Tokens read from a prompt cache (optional, present when cache hit).
    #[serde(default)]
    pub cache_read_input_tokens: Option<u32>,
    /// Tokens used to create a prompt cache entry (optional, present when cache written).
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicError {
    #[allow(dead_code)]
    pub r#type: String,
    pub error: AnthropicErrorDetail,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicErrorDetail {
    #[allow(dead_code)]
    pub r#type: String,
    pub message: String,
}

// Streaming response types

#[derive(Debug, Deserialize)]
pub struct AnthropicStreamEvent {
    #[allow(dead_code)]
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<AnthropicStreamDelta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<AnthropicStreamMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<AnthropicUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AnthropicErrorDetail>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicStreamDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicStreamMessage {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    pub model: String,
    pub role: String,
}

impl AnthropicProvider {
    pub fn new(config: AnthropicProviderConfig) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("Failed to build HTTP client");

        let async_client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .read_timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("Failed to build async HTTP client");

        let retry = config.retry.clone();
        Self {
            config,
            client,
            async_client,
            retry,
        }
    }

    fn convert_request(&self, request: ChatRequest) -> Result<AnthropicRequest, ProviderError> {
        let cache_enabled = self.config.cache.enabled;
        let breakpoint_strategy = &self.config.cache.breakpoint_strategy;

        let mut system_message: Option<String> = None;
        let mut anthropic_messages = Vec::new();

        for msg in &request.messages {
            match msg.role.as_str() {
                "system" => {
                    system_message = Some(msg.content.clone());
                }
                "user" | "assistant" => {
                    let content_blocks = if !msg.tool_calls.is_empty() {
                        // Convert tool calls to Anthropic format
                        let mut blocks = Vec::new();
                        for tc in &msg.tool_calls {
                            blocks.push(AnthropicContentBlock::ToolUse {
                                tool_use_type: "tool_use".into(),
                                id: tc.id.clone(),
                                name: tc.function.name.clone(),
                                input: serde_json::from_str(&tc.function.arguments)
                                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
                            });
                        }
                        if !msg.content.is_empty() {
                            blocks.push(AnthropicContentBlock::Text {
                                text_type: "text".into(),
                                text: msg.content.clone(),
                                cache_control: None,
                            });
                        }
                        blocks
                    } else if msg.tool_call_id.is_some() {
                        // Tool result
                        vec![AnthropicContentBlock::ToolResult {
                            tool_result_type: "tool_result".into(),
                            tool_use_id: msg.tool_call_id.clone().unwrap_or_default(),
                            content: msg.content.clone(),
                            is_error: None,
                        }]
                    } else {
                        // Plain user/assistant message. Image attachments
                        // (when present) ride alongside the text. Anthropic
                        // is documented to handle the image block before
                        // the text block, but accepts both orderings; we
                        // emit images first so the model sees them as
                        // input context for the prompt that follows.
                        let mut blocks = Vec::new();
                        for att in &msg.attachments {
                            if !matches!(att.kind, crate::attachments::AttachmentKind::Image) {
                                continue;
                            }
                            let source = match &att.source {
                                crate::attachments::AttachmentSource::RemoteUrl { url } => {
                                    AnthropicImageSource::Url { url: url.clone() }
                                }
                                _ => AnthropicImageSource::Base64 {
                                    media_type: att.mime_type.clone(),
                                    data: att.data_base64.clone(),
                                },
                            };
                            blocks.push(AnthropicContentBlock::Image {
                                image_type: "image".into(),
                                source,
                            });
                        }
                        if !msg.content.is_empty() || blocks.is_empty() {
                            blocks.push(AnthropicContentBlock::Text {
                                text_type: "text".into(),
                                text: msg.content.clone(),
                                cache_control: None,
                            });
                        }
                        blocks
                    };

                    anthropic_messages.push(AnthropicMessageRequest {
                        role: msg.role.clone(),
                        content: content_blocks,
                    });
                }
                _ => {
                    // Skip unknown roles
                }
            }
        }

        // ── Inject cache_control breakpoints ──────────────────────────

        // Build system prompt as content blocks when caching is enabled,
        // so we can attach cache_control to the first block.
        let system_blocks: Option<Vec<AnthropicContentBlock>> = system_message.map(|text| {
            let mut blocks = Vec::new();
            let cache_system = cache_enabled
                && (breakpoint_strategy == &BreakpointStrategy::SystemOnly
                    || breakpoint_strategy == &BreakpointStrategy::Prefix
                    || breakpoint_strategy == &BreakpointStrategy::Full);
            blocks.push(AnthropicContentBlock::Text {
                text_type: "text".into(),
                text,
                cache_control: if cache_system {
                    Some(CacheControl::ephemeral())
                } else {
                    None
                },
            });
            blocks
        });

        // For Prefix strategy, add cache_control to the second-to-last message
        // (everything before the current turn is cached as a prefix).
        if cache_enabled
            && breakpoint_strategy == &BreakpointStrategy::Prefix
            && anthropic_messages.len() >= 2
        {
            // The prefix ends at the message before the current user turn.
            let prefix_idx = anthropic_messages.len() - 2;
            if let Some(last_text_block) = anthropic_messages[prefix_idx]
                .content
                .iter_mut()
                .rev()
                .find_map(|block| match block {
                    AnthropicContentBlock::Text {
                        ref mut cache_control,
                        ..
                    } => Some(cache_control),
                    _ => None,
                })
            {
                *last_text_block = Some(CacheControl::ephemeral());
            }
        }

        // For Full strategy, add cache_control to every message up to the last.
        if cache_enabled && breakpoint_strategy == &BreakpointStrategy::Full {
            if let Some(last_msg) = anthropic_messages.last_mut() {
                if let Some(last_text_block) =
                    last_msg
                        .content
                        .iter_mut()
                        .rev()
                        .find_map(|block| match block {
                            AnthropicContentBlock::Text {
                                ref mut cache_control,
                                ..
                            } => Some(cache_control),
                            _ => None,
                        })
                {
                    *last_text_block = Some(CacheControl::ephemeral());
                }
            }
        }

        let tools = if request.tools.is_empty() {
            None
        } else {
            Some(
                request
                    .tools
                    .into_iter()
                    .map(|t| AnthropicToolDefinition {
                        tool_type: t.tool_type,
                        name: t.function.name,
                        description: t.function.description,
                        input_schema: t.function.parameters,
                    })
                    .collect(),
            )
        };

        Ok(AnthropicRequest {
            model: request.model,
            system: system_blocks,
            messages: anthropic_messages,
            tools,
            temperature: request.temperature,
            top_p: None,
            top_k: None,
            max_tokens: request.max_tokens.unwrap_or(4096),
            stream: false,
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
            || msg.contains("rate limit")
    }

    pub fn chat(&self, request: ChatRequest) -> ProviderResult<ChatResponse> {
        tracing::trace!(
            provider = %self.config.id,
            model = %request.model,
            messages = request.messages.len(),
            tools = request.tools.len(),
            stream = false,
            "anthropic chat request"
        );
        let anthropic_request = self.convert_request(request)?;

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

            match self.chat_once(&anthropic_request) {
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

    fn chat_once(&self, request: &AnthropicRequest) -> ProviderResult<ChatResponse> {
        let url = format!("{}/messages", self.config.base_url.trim_end_matches('/'));

        let response = self
            .client
            .post(&url)
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", &self.config.version)
            .header("Content-Type", "application/json")
            .json(request)
            .send()
            .map_err(|e| ProviderError::new(format!("Request failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().unwrap_or_default();

            // Try to parse as Anthropic error
            if let Ok(anthropic_err) = serde_json::from_str::<AnthropicError>(&text) {
                return Err(ProviderError::new(format!(
                    "API error ({}): {}",
                    status, anthropic_err.error.message
                )));
            }

            return Err(ProviderError::new(format!(
                "API error ({}): {}",
                status, text
            )));
        }

        let anthropic_response: AnthropicResponse = response
            .json()
            .map_err(|e| ProviderError::new(format!("Failed to parse response: {}", e)))?;

        let (content, tool_calls) =
            self.extract_content_and_tool_calls(&anthropic_response.content);

        let usage = anthropic_response.usage.map(|u| ChatUsage {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.input_tokens + u.output_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
            cache_creation_input_tokens: u.cache_creation_input_tokens,
        });

        tracing::trace!(
            provider = %self.config.id,
            model = %anthropic_response.model,
            finish_reason = ?anthropic_response.stop_reason,
            prompt_tokens = usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0),
            completion_tokens = usage.as_ref().map(|u| u.completion_tokens).unwrap_or(0),
            tool_calls = tool_calls.len(),
            "anthropic chat response"
        );

        Ok(ChatResponse {
            id: anthropic_response.id,
            model: anthropic_response.model,
            message: super::ChatMessageResponse {
                role: anthropic_response.role,
                content: Some(content),
                tool_calls,
            },
            finish_reason: anthropic_response.stop_reason,
            usage,
        })
    }

    fn extract_content_and_tool_calls(
        &self,
        content_blocks: &[AnthropicContentBlockResponse],
    ) -> (String, Vec<super::ToolCallResponse>) {
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();

        for block in content_blocks {
            match block {
                AnthropicContentBlockResponse::Text { text } => {
                    text_parts.push(text.clone());
                }
                AnthropicContentBlockResponse::ToolUse {
                    tool_use_type,
                    id,
                    name,
                    input,
                } => {
                    tool_calls.push(super::ToolCallResponse {
                        id: id.clone(),
                        call_type: tool_use_type.clone(),
                        function: super::ToolCallFunction {
                            name: name.clone(),
                            arguments: serde_json::to_string(input).unwrap_or_default(),
                        },
                    });
                }
            }
        }

        (text_parts.join(""), tool_calls)
    }

    fn parse_sse_line(buffer: &mut String) -> Option<String> {
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
}

impl LLMProvider for AnthropicProvider {
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
        tracing::trace!(
            provider = %self.config.id,
            model = %request.model,
            messages = request.messages.len(),
            tools = request.tools.len(),
            stream = true,
            "anthropic chat_stream request"
        );

        let url = format!("{}/messages", self.config.base_url.trim_end_matches('/'));

        let mut anthropic_request = self.convert_request(request)?;
        anthropic_request.stream = true;

        let api_key = self.config.api_key.clone();
        let version = self.config.version.clone();
        let async_client = self.async_client.clone();
        let retry = self.retry.clone();

        let mut last_error = None;

        for attempt in 0..=retry.max_retries {
            if attempt > 0 {
                let delay = retry.delay_for_attempt(attempt - 1);
                log::warn!(
                    "Stream retry attempt {}/{} after {:?}",
                    attempt,
                    retry.max_retries,
                    delay
                );
                std::thread::sleep(delay);
            }

            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    async_client
                        .post(&url)
                        .header("x-api-key", &api_key)
                        .header("anthropic-version", &version)
                        .header("Content-Type", "application/json")
                        .json(&anthropic_request)
                        .send()
                        .await
                        .map_err(|e| ProviderError::new(format!("Stream request failed: {}", e)))?
                        .error_for_status()
                        .map_err(|e| ProviderError::new(format!("Stream API error: {}", e)))
                })
            });

            match result {
                Ok(response) => {
                    let stream = async_stream::stream! {
                        let mut buffer = String::new();
                        let mut stream = response.bytes_stream();
                        let mut response_id = String::new();

                        while let Some(chunk_result) = stream.next().await {
                            match chunk_result {
                                Ok(bytes) => {
                                    buffer.push_str(&String::from_utf8_lossy(&bytes));

                                    while let Some(line) = Self::parse_sse_line(&mut buffer) {
                                        match serde_json::from_str::<AnthropicStreamEvent>(&line) {
                                            Ok(event) => {
                                                if let Some(error) = event.error {
                                                    yield Err(ProviderError::new(format!(
                                                        "Stream error: {}", error.message
                                                    )));
                                                    return;
                                                }

                                                if let Some(msg) = event.message {
                                                    response_id = msg.id;
                                                }

                                                if let Some(delta) = event.delta {
                                                    yield Ok(super::ChatChunk {
                                                        id: response_id.clone(),
                                                        delta: delta.text.unwrap_or_default(),
                                                        finish_reason: None,
                                                        tool_calls: vec![],
                                                        usage: None,
                                                    });
                                                }

                                                if event.r#type == "content_block_stop" {
                                                    yield Ok(super::ChatChunk {
                                                        id: response_id.clone(),
                                                        delta: String::new(),
                                                        finish_reason: None,
                                                        tool_calls: vec![],
                                                        usage: None,
                                                    });
                                                }

                                                if event.r#type == "message_stop" {
                                                    // Final usage often arrives in `message_delta`
                                                    // events before this; the orchestrator will
                                                    // have already received it via the dedicated
                                                    // event below if so.
                                                    return;
                                                }

                                                // `message_delta` carries the cumulative usage
                                                // for the whole response. Emit it as a final
                                                // empty chunk so the orchestrator can record
                                                // the cost.
                                                if event.r#type == "message_delta" {
                                                    if let Some(u) = event.usage {
                                                        yield Ok(super::ChatChunk {
                                                            id: response_id.clone(),
                                                            delta: String::new(),
                                                            finish_reason: Some("stop".to_string()),
                                                            tool_calls: vec![],
                                                            usage: Some(super::ChatUsage {
                                                                prompt_tokens: u.input_tokens,
                                                                completion_tokens: u.output_tokens,
                                                                total_tokens: u.input_tokens + u.output_tokens,
                                                                cache_read_input_tokens: u.cache_read_input_tokens,
                                                                cache_creation_input_tokens: u.cache_creation_input_tokens,
                                                            }),
                                                        });
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                // Ignore parse errors for non-event lines
                                                log::trace!("Failed to parse SSE line: {}", e);
                                            }
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

                    return Ok(Box::pin(stream));
                }
                Err(e) => {
                    let retryable = Self::is_retryable_error(&e);
                    last_error = Some(e);
                    if !retryable || attempt == retry.max_retries {
                        break;
                    }
                }
            }
        }

        Err(last_error.unwrap())
    }

    fn health_check(&self) -> super::ProviderResult<bool> {
        // Anthropic doesn't have a dedicated health endpoint.
        // We just check that we can reach the API base URL.
        let url = self.config.base_url.trim_end_matches('/').to_string();
        let result = self
            .client
            .get(&url)
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", &self.config.version)
            .send();
        match result {
            Ok(_) => Ok(true), // Any response means the API is reachable
            Err(e) => Err(super::ProviderError::new(format!(
                "Health check connection failed: {}",
                e
            ))),
        }
    }

    fn list_models(&self) -> super::ProviderResult<Vec<super::ModelInfo>> {
        // Return the statically configured models from config
        Ok(self
            .config
            .models
            .iter()
            .map(|m| super::ModelInfo {
                id: m.id.clone(),
                name: m.name.clone(),
                context_window: m.context_window,
                capabilities: vec![
                    super::ModelCapability::Chat,
                    super::ModelCapability::Streaming,
                    super::ModelCapability::ToolCalling,
                ],
            })
            .collect())
    }

    fn count_tokens(&self, text: &str) -> usize {
        // Claude uses a different tokenizer, but ~3.5 chars per token is reasonable
        (text.len() as f32 / 3.5) as usize
    }
}

#[cfg(test)]
mod wire_format_tests {
    use super::{
        AnthropicContentBlock, AnthropicImageSource, AnthropicProvider, AnthropicProviderConfig,
    };
    use crate::attachments::{Attachment, AttachmentKind, AttachmentSource};
    use crate::providers::{ChatMessageRequest, ChatRequest};

    fn provider() -> AnthropicProvider {
        AnthropicProvider::new(AnthropicProviderConfig::new(
            "test-id",
            "test-key",
            "claude-3-5-sonnet-20241022",
        ))
    }

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

    /// Helper: convert through the public path and JSON-serialize the
    /// resulting Anthropic request body so tests can assert on the wire
    /// shape without poking at private structs.
    fn convert_and_serialize(req: ChatRequest) -> serde_json::Value {
        let p = provider();
        let anthropic_req = p.convert_request(req).expect("convert");
        serde_json::to_value(&anthropic_req).expect("serialize")
    }

    #[test]
    fn no_attachment_renders_single_text_block() {
        let req = ChatRequest::new(
            "claude-3-5-sonnet-20241022",
            vec![ChatMessageRequest::user("hello")],
        );
        let body = convert_and_serialize(req);
        let msgs = body["messages"].as_array().expect("messages");
        assert_eq!(msgs.len(), 1);
        let blocks = msgs[0]["content"].as_array().expect("content array");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "hello");
    }

    #[test]
    fn single_image_renders_image_then_text() {
        let msg =
            ChatMessageRequest::user("What's in this?").with_attachments(vec![png_attachment()]);
        let req = ChatRequest::new("claude-3-5-sonnet-20241022", vec![msg]);
        let body = convert_and_serialize(req);
        let blocks = body["messages"][0]["content"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 2);
        // Image first so the model sees it as input context.
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["type"], "base64");
        assert_eq!(blocks[0]["source"]["media_type"], "image/png");
        assert_eq!(blocks[0]["source"]["data"], "iVBORw0KGgo=");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "What's in this?");
    }

    #[test]
    fn empty_text_with_image_omits_text_block() {
        let msg = ChatMessageRequest::user("").with_attachments(vec![png_attachment()]);
        let req = ChatRequest::new("claude-3-5-sonnet-20241022", vec![msg]);
        let body = convert_and_serialize(req);
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        // Only the image block — no leading empty text.
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "image");
    }

    #[test]
    fn remote_url_attachment_uses_url_source() {
        let msg = ChatMessageRequest::user("caption").with_attachments(vec![remote_attachment()]);
        let req = ChatRequest::new("claude-3-5-sonnet-20241022", vec![msg]);
        let body = convert_and_serialize(req);
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["type"], "url");
        assert_eq!(blocks[0]["source"]["url"], "https://example.com/cat.png");
        // No `data` field on URL-source images.
        assert!(blocks[0]["source"]["data"].is_null());
    }

    #[test]
    fn multiple_attachments_render_in_order() {
        let msg = ChatMessageRequest::user("two pics")
            .with_attachments(vec![png_attachment(), remote_attachment()]);
        let req = ChatRequest::new("claude-3-5-sonnet-20241022", vec![msg]);
        let body = convert_and_serialize(req);
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        // 2 images + 1 text = 3 blocks; images come first.
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["type"], "base64");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "url");
        assert_eq!(blocks[2]["type"], "text");
    }

    #[test]
    fn system_messages_are_pulled_out_of_messages_array() {
        let req = ChatRequest::new(
            "claude-3-5-sonnet-20241022",
            vec![
                ChatMessageRequest::system("sys-prompt"),
                ChatMessageRequest::user("hi").with_attachments(vec![png_attachment()]),
            ],
        );
        let body = convert_and_serialize(req);
        // Anthropic puts system prompt at the top level as a list of content blocks.
        let sys = body["system"].as_array().expect("system is an array");
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["type"], "text");
        assert_eq!(sys[0]["text"], "sys-prompt");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        // The user turn still carries its image.
        let blocks = msgs[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "image");
    }

    #[test]
    fn image_source_serializes_with_internal_type_tag() {
        // Sanity check on the AnthropicImageSource enum: serde tag = type,
        // snake_case, so `Base64 { media_type, data }` becomes
        // `{"type": "base64", "media_type": ..., "data": ...}`.
        let s = AnthropicImageSource::Base64 {
            media_type: "image/jpeg".into(),
            data: "abc".into(),
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["type"], "base64");
        assert_eq!(v["media_type"], "image/jpeg");
        assert_eq!(v["data"], "abc");

        let u = AnthropicImageSource::Url {
            url: "https://x/y".into(),
        };
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(v["type"], "url");
        assert_eq!(v["url"], "https://x/y");
    }

    #[test]
    fn text_block_emits_explicit_type_field() {
        // Regression: untagged enum without a type field used to omit
        // the discriminator, which Anthropic's API requires.
        let block = AnthropicContentBlock::Text {
            text_type: "text".into(),
            text: "hi".into(),
            cache_control: None,
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(v["type"], "text");
        assert_eq!(v["text"], "hi");
    }

    // ── Prompt caching tests ────────────────────────────────────────

    #[test]
    fn cache_control_is_absent_by_default() {
        // When caching is not enabled, no cache_control field should appear.
        let req = ChatRequest::new(
            "claude-3-5-sonnet-20241022",
            vec![
                ChatMessageRequest::system("You are a helpful assistant."),
                ChatMessageRequest::user("hello"),
            ],
        );
        let body = convert_and_serialize(req);
        // System block should not have cache_control.
        let sys = body["system"].as_array().unwrap();
        assert!(!sys[0].as_object().unwrap().contains_key("cache_control"));
        // Messages should not have cache_control.
        let msgs = body["messages"].as_array().unwrap();
        for msg in msgs {
            if let Some(content) = msg["content"].as_array() {
                for block in content {
                    assert!(!block.as_object().unwrap().contains_key("cache_control"));
                }
            }
        }
    }

    /// Helper: provider with cache enabled.
    fn cached_provider() -> AnthropicProvider {
        let mut cfg =
            AnthropicProviderConfig::new("test-id", "test-key", "claude-3-5-sonnet-20241022");
        cfg.cache = crate::config::CacheConfig {
            enabled: true,
            breakpoint_strategy: crate::config::BreakpointStrategy::SystemOnly,
        };
        AnthropicProvider::new(cfg)
    }

    fn convert_cached(req: ChatRequest) -> serde_json::Value {
        let p = cached_provider();
        let anthropic_req = p.convert_request(req).expect("convert");
        serde_json::to_value(&anthropic_req).expect("serialize")
    }

    #[test]
    fn cache_control_system_only_strategy() {
        // With SystemOnly strategy, only the system prompt gets cache_control.
        let req = ChatRequest::new(
            "claude-3-5-sonnet-20241022",
            vec![
                ChatMessageRequest::system("sys-prompt"),
                ChatMessageRequest::user("hello"),
            ],
        );
        let body = convert_cached(req);
        // System has cache_control.
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
        // Messages do not have cache_control.
        let msgs = body["messages"].as_array().unwrap();
        for msg in msgs {
            if let Some(content) = msg["content"].as_array() {
                for block in content {
                    assert!(!block.as_object().unwrap().contains_key("cache_control"));
                }
            }
        }
    }

    #[test]
    fn cache_control_prefix_strategy() {
        // With Prefix strategy, system prompt + the last prefix message
        // (second-to-last overall) get cache_control. The final user turn
        // should NOT have cache_control.
        //
        // Anthropic's API only needs one cache_control breakpoint at the
        // boundary between cached and uncached content — the API caches
        // everything from the start up to that breakpoint.
        let mut cfg =
            AnthropicProviderConfig::new("test-id", "test-key", "claude-3-5-sonnet-20241022");
        cfg.cache = crate::config::CacheConfig {
            enabled: true,
            breakpoint_strategy: crate::config::BreakpointStrategy::Prefix,
        };
        let p = AnthropicProvider::new(cfg);

        let req = ChatRequest::new(
            "claude-3-5-sonnet-20241022",
            vec![
                ChatMessageRequest::system("sys-prompt"),
                ChatMessageRequest::user("first message"),
                ChatMessageRequest::assistant("first reply"),
                ChatMessageRequest::user("second message"),
                ChatMessageRequest::assistant("second reply"),
                ChatMessageRequest::user("final turn"),
            ],
        );
        let body =
            serde_json::to_value(p.convert_request(req).expect("convert")).expect("serialize");

        // System has cache_control.
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");

        let msgs = body["messages"].as_array().unwrap();
        // Exactly 5 messages (system is separate in Anthropic API).
        assert_eq!(msgs.len(), 5);

        // The second-to-last message (index 3 = assistant "second reply")
        // should have cache_control on its last text block.
        let prefix_msg = &msgs[3];
        let prefix_blocks = prefix_msg["content"].as_array().unwrap();
        let prefix_last_text = prefix_blocks
            .iter()
            .rev()
            .find(|b| b["type"] == "text")
            .unwrap();
        assert_eq!(prefix_last_text["cache_control"]["type"], "ephemeral");

        // The final message (index 4 = user "final turn") should NOT have cache_control.
        let last_msg = &msgs[4];
        if let Some(blocks) = last_msg["content"].as_array() {
            for block in blocks {
                assert!(
                    !block.as_object().unwrap().contains_key("cache_control"),
                    "final message should not have cache_control"
                );
            }
        }

        // Earlier messages (indices 0-2) should not have cache_control either
        // — only one breakpoint is needed at the prefix boundary.
        for (i, msg) in msgs.iter().take(3).enumerate() {
            if let Some(blocks) = msg["content"].as_array() {
                for block in blocks {
                    // Images/tool results won't have cache_control; only text blocks.
                    if block["type"] == "text" {
                        assert!(
                            !block.as_object().unwrap().contains_key("cache_control"),
                            "msg[{}] should NOT have cache_control (only the prefix boundary)",
                            i
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn cache_control_not_applied_with_single_turn() {
        // With Prefix strategy, a single-turn conversation has no prefix
        // to cache, so only system gets cache_control.
        let mut cfg =
            AnthropicProviderConfig::new("test-id", "test-key", "claude-3-5-sonnet-20241022");
        cfg.cache = crate::config::CacheConfig {
            enabled: true,
            breakpoint_strategy: crate::config::BreakpointStrategy::Prefix,
        };
        let p = AnthropicProvider::new(cfg);

        let req = ChatRequest::new(
            "claude-3-5-sonnet-20241022",
            vec![
                ChatMessageRequest::system("sys-prompt"),
                ChatMessageRequest::user("hello"),
            ],
        );
        let body =
            serde_json::to_value(p.convert_request(req).expect("convert")).expect("serialize");

        // System has cache_control.
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");

        // The single user message should NOT have cache_control
        // (there's no prefix before it).
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        if let Some(blocks) = msgs[0]["content"].as_array() {
            for block in blocks {
                assert!(!block.as_object().unwrap().contains_key("cache_control"));
            }
        }
    }
}
