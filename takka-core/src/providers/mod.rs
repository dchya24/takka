//! LLM Provider trait and implementations

pub mod anthropic;
pub mod openai;

pub use anthropic::{
    AnthropicContentBlockResponse, AnthropicError, AnthropicErrorDetail, AnthropicProvider,
    AnthropicProviderConfig, AnthropicResponse, AnthropicStreamDelta, AnthropicStreamEvent,
    AnthropicStreamMessage, AnthropicUsage, RetryConfig,
};
pub use openai::{OpenAIProvider, OpenAIProviderConfig};

use serde::{Deserialize, Serialize};

/// Baseline system prompt, re-exported from the canonical definition in
/// [`crate::prompts::DEFAULT_SYSTEM_PROMPT`].
pub use crate::prompts::DEFAULT_SYSTEM_PROMPT;

pub type ProviderResult<T> = std::result::Result<T, ProviderError>;

#[derive(Debug, thiserror::Error)]
#[error("Provider error: {0}")]
pub struct ProviderError(pub String);

impl ProviderError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatMessageRequest {
    pub role: String,
    /// Message content. We serialize manually so that an assistant
    /// message with `tool_calls` can emit `content: null` when the model
    /// returned no text alongside its tool requests — several
    /// OpenAI-compatible providers (notably Z.AI) reject
    /// `{role: "assistant", content: "", tool_calls: [...]}` with
    /// HTTP 400 "messages parameter is illegal".
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallResponse>,
    /// Image (and future binary) attachments riding along with this
    /// message. Each provider serializes these into its own multipart
    /// shape — the field stays in core so memory + safety + cost can
    /// reason about them in one place.
    ///
    /// Skipped from on-the-wire serialization here; providers consume
    /// it directly when building their request bodies (see
    /// `providers::openai::serialize_request_with_attachments` etc.).
    #[serde(default, skip)]
    pub attachments: Vec<crate::attachments::Attachment>,
}

impl Serialize for ChatMessageRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        // Decide field count up front so we don't emit empty optionals.
        let emit_content_as_null =
            self.role == "assistant" && !self.tool_calls.is_empty() && self.content.is_empty();
        let mut field_count = 2; // role + content
        if self.tool_call_id.is_some() {
            field_count += 1;
        }
        if !self.tool_calls.is_empty() {
            field_count += 1;
        }

        let mut state = serializer.serialize_struct("ChatMessageRequest", field_count)?;
        state.serialize_field("role", &self.role)?;

        if emit_content_as_null {
            // None makes serde emit `null` for the content field. This
            // is what OpenAI/Z.AI expect for an assistant turn that
            // consisted purely of tool_calls.
            state.serialize_field("content", &Option::<String>::None)?;
        } else {
            state.serialize_field("content", &self.content)?;
        }

        if let Some(id) = &self.tool_call_id {
            state.serialize_field("tool_call_id", id)?;
        }
        if !self.tool_calls.is_empty() {
            state.serialize_field("tool_calls", &self.tool_calls)?;
        }

        state.end()
    }
}

impl ChatMessageRequest {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            tool_call_id: None,
            tool_calls: vec![],
            attachments: Vec::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            tool_call_id: None,
            tool_calls: vec![],
            attachments: Vec::new(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
            tool_call_id: None,
            tool_calls: vec![],
            attachments: Vec::new(),
        }
    }

    /// Builder helper: attach one or more images to this message.
    /// Replaces any previously-set attachments.
    pub fn with_attachments(mut self, attachments: Vec<crate::attachments::Attachment>) -> Self {
        self.attachments = attachments;
        self
    }

    /// Convenience: this message carries at least one image.
    pub fn has_images(&self) -> bool {
        self.attachments
            .iter()
            .any(|a| matches!(a.kind, crate::attachments::AttachmentKind::Image))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessageRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    /// Optional system prompt override. If `None`, the provider's default is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessageRequest>) -> Self {
        Self {
            model: model.into(),
            messages,
            temperature: None,
            max_tokens: None,
            stream: false,
            tools: vec![],
            system_prompt: None,
        }
    }

    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_temperature(mut self, temp: f32) -> Self {
        self.temperature = Some(temp);
        self
    }

    pub fn with_max_tokens(mut self, max: u32) -> Self {
        self.max_tokens = Some(max);
        self
    }

    pub fn stream(mut self) -> Self {
        self.stream = true;
        self
    }

    /// Set a custom system prompt for this request.
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Resolve the effective system prompt: custom override or default.
    pub fn effective_system_prompt(&self) -> &str {
        self.system_prompt
            .as_deref()
            .unwrap_or(DEFAULT_SYSTEM_PROMPT)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessageResponse {
    pub role: String,
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// Tokens read from a prompt cache (Anthropic-specific).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u32>,
    /// Tokens used to create a prompt cache entry (Anthropic-specific).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub id: String,
    pub model: String,
    pub message: ChatMessageResponse,
    pub finish_reason: Option<String>,
    pub usage: Option<ChatUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDelta {
    pub index: u32,
    pub id: Option<String>,
    pub function_name: Option<String>,
    pub function_arguments: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatChunk {
    pub id: String,
    pub delta: String,
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
    /// Provider-reported token usage. Typically only set on the final
    /// chunk of a stream (when `finish_reason` is also set). `None` on
    /// intermediate chunks.
    #[serde(default)]
    pub usage: Option<ChatUsage>,
}

pub type StreamResult<T, E> = std::result::Result<
    std::pin::Pin<Box<dyn futures::Stream<Item = std::result::Result<T, E>> + Send + Sync>>,
    E,
>;

pub trait LLMProvider: Send + Sync {
    fn provider_type(&self) -> &str;
    fn provider_id(&self) -> &str;

    fn chat(&self, request: ChatRequest) -> ProviderResult<ChatResponse>;

    fn chat_stream(&self, request: ChatRequest) -> StreamResult<ChatChunk, ProviderError>;

    /// Check if the provider API is reachable.
    /// Default: send a minimal request and check for a non-connection error.
    fn health_check(&self) -> ProviderResult<bool> {
        Ok(true)
    }

    /// List models available from this provider.
    /// Default: return empty list (provider doesn't support listing).
    fn list_models(&self) -> ProviderResult<Vec<ModelInfo>> {
        Ok(vec![])
    }

    /// Estimate the token count for the given text.
    /// Default: rough estimate of ~4 characters per token.
    fn count_tokens(&self, text: &str) -> usize {
        // Rough BPE approximation: ~4 chars per token for English
        text.len() / 4
    }
}

/// Metadata about a model available from a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub context_window: Option<u32>,
    /// What modalities the model supports.
    #[serde(default)]
    pub capabilities: Vec<ModelCapability>,
}

/// Capabilities a model may support.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    Chat,
    Streaming,
    ToolCalling,
    Vision,
    Embeddings,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call(id: &str) -> ToolCallResponse {
        ToolCallResponse {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: ToolCallFunction {
                name: "read_file".to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    #[test]
    fn assistant_with_tool_calls_serializes_empty_content_as_null() {
        // OpenAI/Z.AI reject {role:"assistant", content:"", tool_calls:[...]}
        // with HTTP 400. The fix: when content is empty AND tool_calls is
        // non-empty, emit `content: null`.
        let msg = ChatMessageRequest {
            role: "assistant".into(),
            content: "".into(),
            tool_call_id: None,
            tool_calls: vec![tool_call("call-1")],
            attachments: vec![],
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "assistant");
        assert!(
            json["content"].is_null(),
            "content should serialize as null"
        );
        assert!(json["tool_calls"].is_array());
    }

    #[test]
    fn assistant_with_tool_calls_keeps_content_when_present() {
        // The model can return both text content and tool_calls in the
        // same turn. Don't null out the content in that case.
        let msg = ChatMessageRequest {
            role: "assistant".into(),
            content: "thinking out loud".into(),
            tool_call_id: None,
            tool_calls: vec![tool_call("call-1")],
            attachments: vec![],
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["content"], "thinking out loud");
    }

    #[test]
    fn user_message_keeps_empty_string_content() {
        // The null-content rule is specific to assistant + tool_calls.
        // A plain user/tool message with empty content should still
        // serialize as an empty string (caller's choice).
        let msg = ChatMessageRequest {
            role: "user".into(),
            content: "".into(),
            tool_call_id: None,
            tool_calls: vec![],
            attachments: vec![],
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["content"], "");
    }

    #[test]
    fn tool_message_includes_tool_call_id() {
        let msg = ChatMessageRequest {
            role: "tool".into(),
            content: "result".into(),
            tool_call_id: Some("call-1".into()),
            tool_calls: vec![],
            attachments: vec![],
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["tool_call_id"], "call-1");
        assert_eq!(json["content"], "result");
    }

    #[test]
    fn empty_optional_fields_omitted() {
        let msg = ChatMessageRequest {
            role: "user".into(),
            content: "hi".into(),
            tool_call_id: None,
            tool_calls: vec![],
            attachments: vec![],
        };
        let json = serde_json::to_value(&msg).unwrap();
        let obj = json.as_object().unwrap();
        assert!(!obj.contains_key("tool_call_id"));
        assert!(!obj.contains_key("tool_calls"));
    }
}
