//! Domain types for memory: messages, sessions, context windows, config.
//!
//! Pure data + small constructors. No persistence, no compaction logic,
//! no LLM-aware prompt building. Anything that mutates a `Memory` lives
//! in `super::store`, `super::compaction`, or `super::persist`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::providers::ToolCallResponse;

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

/// A single message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: MessageRole,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub metadata: MessageMetadata,
}

/// Optional metadata attached to a message.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageMetadata {
    /// Token count for this message (estimated).
    #[serde(default)]
    pub token_count: u32,
    /// Model that generated this message (for assistant messages).
    #[serde(default)]
    pub model: Option<String>,
    /// Duration to generate this message (for assistant messages).
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// Tool call ID if this is a tool result message.
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// Tool calls emitted by this assistant message.
    /// Required for the next request so tool results can be matched
    /// to their parent assistant call (per OpenAI spec).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallResponse>,
    /// Image attachments riding along with this message. Survives
    /// memory persistence (saved as base64) so a `/load`-ed session can
    /// resume mid-conversation with image context intact.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::attachments::Attachment>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            role: MessageRole::User,
            content: content.into(),
            timestamp: Utc::now(),
            pinned: false,
            metadata: MessageMetadata::default(),
        }
    }

    /// User message carrying image attachments. The attachments survive
    /// memory persistence and flow back into the next provider request
    /// via `MessageMetadata::attachments`.
    pub fn user_with_attachments(
        content: impl Into<String>,
        attachments: Vec<crate::attachments::Attachment>,
    ) -> Self {
        let mut msg = Self::user(content);
        msg.metadata.attachments = attachments;
        msg
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            role: MessageRole::Assistant,
            content: content.into(),
            timestamp: Utc::now(),
            pinned: false,
            metadata: MessageMetadata::default(),
        }
    }

    /// Assistant message that emitted tool calls.
    /// The tool calls are stored in metadata so the next request can
    /// reattach them on the assistant `ChatMessageRequest` — required
    /// for tool results to be valid per OpenAI spec.
    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCallResponse>,
    ) -> Self {
        Self {
            id: new_id(),
            role: MessageRole::Assistant,
            content: content.into(),
            timestamp: Utc::now(),
            pinned: false,
            metadata: MessageMetadata {
                tool_calls,
                ..MessageMetadata::default()
            },
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            role: MessageRole::System,
            content: content.into(),
            timestamp: Utc::now(),
            pinned: false,
            metadata: MessageMetadata::default(),
        }
    }

    pub fn tool(
        tool_name: impl Into<String>,
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        let tc_id = tool_call_id.into();
        Self {
            id: new_id(),
            role: MessageRole::Tool {
                tool_name: tool_name.into(),
                tool_call_id: tc_id.clone(),
            },
            content: content.into(),
            timestamp: Utc::now(),
            pinned: false,
            metadata: MessageMetadata {
                tool_call_id: Some(tc_id),
                ..Default::default()
            },
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.metadata.model = Some(model.into());
        self
    }

    pub fn with_duration(mut self, ms: u64) -> Self {
        self.metadata.duration_ms = Some(ms);
        self
    }

    /// Estimate and store token count.
    pub fn with_estimated_tokens(mut self) -> Self {
        self.metadata.token_count = estimate_tokens(&self.content);
        self
    }

    /// Pin this message (won't be removed during compaction).
    pub fn pinned(mut self) -> Self {
        self.pinned = true;
        self
    }
}

// ---------------------------------------------------------------------------
// Message Role
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MessageRole {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "system")]
    System,
    #[serde(rename = "tool")]
    Tool {
        tool_name: String,
        tool_call_id: String,
    },
}

impl MessageRole {
    pub fn as_str(&self) -> &str {
        match self {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
            MessageRole::Tool { .. } => "tool",
        }
    }
}

// ---------------------------------------------------------------------------
// Session Info
// ---------------------------------------------------------------------------

/// Session metadata for isolation and persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

impl SessionInfo {
    pub fn new() -> Self {
        let now = Utc::now();
        Self {
            id: new_id(),
            created_at: now,
            updated_at: now,
            label: String::new(),
            provider: None,
            model: None,
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Bump `updated_at`. `pub(super)` so sibling modules in `memory/`
    /// can call it from `Memory::add_message` etc. without exposing it
    /// to the rest of the crate.
    pub(super) fn touch(&mut self) {
        self.updated_at = Utc::now();
    }
}

impl Default for SessionInfo {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Context Window
// ---------------------------------------------------------------------------

/// Result of context window calculation.
#[derive(Debug, Clone)]
pub struct ContextWindow {
    pub messages: Vec<Message>,
    pub total_tokens: u32,
    pub budget: u32,
    pub used_percentage: f64,
    pub was_compacted: bool,
    pub removed_count: usize,
}

// ---------------------------------------------------------------------------
// Summarized Context
// ---------------------------------------------------------------------------

/// Result of memory compaction.
#[derive(Debug, Clone)]
pub struct SummarizedContext {
    pub summary: String,
    pub summarized_count: usize,
    pub tokens_before: u32,
    pub tokens_after: u32,
    pub kept_ids: Vec<String>,
}

// ---------------------------------------------------------------------------
// Memory Config
// ---------------------------------------------------------------------------

/// Configuration for memory behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Maximum token budget for context window.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,

    /// Number of recent messages to always keep (even if over budget).
    #[serde(default = "default_keep_recent")]
    pub keep_recent: usize,

    /// Trigger compaction when usage exceeds this percentage (0.0-1.0).
    #[serde(default = "default_compaction_threshold")]
    pub compaction_threshold: f64,

    /// Max messages to include in context when building for LLM.
    #[serde(default = "default_context_message_limit")]
    pub context_message_limit: usize,

    /// Fraction of `max_tokens` allocated to message history when
    /// building a request. The remainder is reserved for the system
    /// prompt, tool definitions, and the model's response.
    ///
    /// Defaults to 0.7. Anthropic-style providers may benefit from a
    /// lower value (0.5–0.6) since their tool definitions tend to be
    /// verbose; OpenAI/Z.AI-style providers usually do fine at 0.7.
    #[serde(default = "default_context_budget_ratio")]
    pub context_budget_ratio: f64,

    /// Directory for persisting sessions to disk.
    #[serde(default)]
    pub persist_dir: Option<String>,

    /// Whether to auto-persist on every add_message.
    #[serde(default)]
    pub auto_persist: bool,
}

fn default_max_tokens() -> u32 {
    128_000
}
fn default_keep_recent() -> usize {
    4
}
fn default_compaction_threshold() -> f64 {
    0.85
}
fn default_context_message_limit() -> usize {
    50
}
fn default_context_budget_ratio() -> f64 {
    0.7
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            max_tokens: default_max_tokens(),
            keep_recent: default_keep_recent(),
            compaction_threshold: default_compaction_threshold(),
            context_message_limit: default_context_message_limit(),
            context_budget_ratio: default_context_budget_ratio(),
            persist_dir: None,
            auto_persist: false,
        }
    }
}

// Used by `Memory` defaults during deserialization.
pub(super) fn config_default_max_tokens() -> u32 {
    default_max_tokens()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate a short unique ID.
pub(super) fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = Utc::now().timestamp_millis() as u64;
    // Mix timestamp + counter for uniqueness
    format!("{:x}-{:04x}", ts, (count % 0xFFFF) as u16)
}

/// Estimate token count from text.
///
/// Two backends:
/// - Default: heuristic ~4 chars per token. Fast, dependency-free, but
///   off by up to 30% for code-heavy content.
/// - With the `tiktoken` cargo feature: real BPE encoder (`cl100k_base`,
///   the OpenAI/Anthropic/Z.AI tokenizer family). Within ~2% of actual
///   usage, at the cost of a one-time encoder load (~50ms, ~10MB binary).
///
/// The encoder is lazy-initialized on first call and reused thereafter.
pub fn estimate_tokens(text: &str) -> u32 {
    #[cfg(feature = "tiktoken")]
    {
        use std::sync::OnceLock;
        static ENCODER: OnceLock<Result<tiktoken_rs::CoreBPE, String>> = OnceLock::new();
        let encoder = ENCODER.get_or_init(|| tiktoken_rs::cl100k_base().map_err(|e| e.to_string()));
        if let Ok(bpe) = encoder.as_ref() {
            return bpe.encode_with_special_tokens(text).len() as u32;
        }
        // Encoder failed to load; fall through to heuristic.
        tracing::warn!("tiktoken cl100k_base failed to load; using heuristic");
    }

    // Heuristic fallback (also the default when the feature is off).
    (text.len() as u32 / 4).max(1)
}
