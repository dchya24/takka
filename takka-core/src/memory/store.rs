//! Core `Memory` struct and its primary operations.
//!
//! Pure storage: holds messages, tracks tokens, manages pinning, and
//! owns the session lifecycle. Compaction logic lives in
//! `super::compaction`; persistence lives in `super::persist`; window
//! selection and token-budget policy live in `crate::context`.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::tool::{ToolCall, ToolResultValue};

use super::types::{
    config_default_max_tokens, estimate_tokens, MemoryConfig, Message, MessageRole, SessionInfo,
};

// ---------------------------------------------------------------------------
// Memory (main struct)
// ---------------------------------------------------------------------------

/// Conversation memory with context window management, pinning,
/// session isolation, and optional persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub messages: Vec<Message>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResultValue>,

    #[serde(default)]
    pub total_tokens: u32,

    #[serde(default = "config_default_max_tokens")]
    pub max_tokens: u32,

    #[serde(default)]
    pub session: SessionInfo,

    #[serde(default)]
    pub config: MemoryConfig,

    /// IDs of pinned messages (never compacted away).
    /// `pub(super)` so sibling modules in `memory/` (compaction,
    /// persist) can mutate it without exposing it crate-wide.
    #[serde(default)]
    pub(super) pinned_ids: HashSet<String>,

    /// Compaction summary, if any.
    #[serde(default)]
    pub(super) summary: Option<String>,
}

impl Memory {
    // -----------------------------------------------------------------------
    // Constructors
    // -----------------------------------------------------------------------

    pub fn new(max_tokens: u32) -> Self {
        Self {
            messages: Vec::new(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            total_tokens: 0,
            max_tokens,
            session: SessionInfo::new(),
            config: MemoryConfig {
                max_tokens,
                ..Default::default()
            },
            pinned_ids: HashSet::new(),
            summary: None,
        }
    }

    pub fn with_config(config: MemoryConfig) -> Self {
        let max = config.max_tokens;
        Self {
            messages: Vec::new(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            total_tokens: 0,
            max_tokens: max,
            session: SessionInfo::new(),
            config,
            pinned_ids: HashSet::new(),
            summary: None,
        }
    }

    pub fn with_session(mut self, session: SessionInfo) -> Self {
        self.session = session;
        self
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.session.label = label.into();
        self
    }

    // -----------------------------------------------------------------------
    // Add messages
    // -----------------------------------------------------------------------

    /// Add a message and update token tracking.
    pub fn add_message(&mut self, message: Message) {
        let tokens = estimate_tokens(&message.content);
        self.total_tokens += tokens;
        self.session.touch();

        // Track pinned state
        if message.pinned {
            self.pinned_ids.insert(message.id.clone());
        }

        self.messages.push(message);

        if self.config.auto_persist {
            let _ = self.persist();
        }
    }

    pub fn add_tool_call(&mut self, call: ToolCall) {
        self.tool_calls.push(call);
        self.session.touch();
    }

    pub fn add_tool_result(&mut self, result: ToolResultValue) {
        self.tool_results.push(result);
        self.session.touch();
    }

    // -----------------------------------------------------------------------
    // Pinning
    // -----------------------------------------------------------------------

    /// Pin a message by ID so it survives compaction.
    pub fn pin(&mut self, message_id: &str) -> bool {
        if self.messages.iter().any(|m| m.id == message_id) {
            self.pinned_ids.insert(message_id.to_string());
            if let Some(msg) = self.messages.iter_mut().find(|m| m.id == message_id) {
                msg.pinned = true;
            }
            true
        } else {
            false
        }
    }

    /// Unpin a message.
    pub fn unpin(&mut self, message_id: &str) {
        self.pinned_ids.remove(message_id);
        if let Some(msg) = self.messages.iter_mut().find(|m| m.id == message_id) {
            msg.pinned = false;
        }
    }

    /// Get all pinned message IDs.
    pub fn pinned_ids(&self) -> &HashSet<String> {
        &self.pinned_ids
    }

    /// Total token budget for this memory's context window.
    pub fn budget(&self) -> u32 {
        self.max_tokens
    }

    /// Get all messages.
    pub fn get_messages(&self) -> &[Message] {
        &self.messages
    }

    // -----------------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------------

    /// Simple keyword search across messages.
    pub fn search(&self, query: &str) -> Vec<&Message> {
        let query_lower = query.to_lowercase();
        self.messages
            .iter()
            .filter(|m| m.content.to_lowercase().contains(&query_lower))
            .collect()
    }

    /// Search messages by role.
    pub fn search_by_role(&self, role: &MessageRole) -> Vec<&Message> {
        self.messages
            .iter()
            .filter(|m| std::mem::discriminant(&m.role) == std::mem::discriminant(role))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Token tracking
    // -----------------------------------------------------------------------

    pub fn token_count(&self) -> u32 {
        self.total_tokens
    }

    /// Recalculate total tokens from scratch (fix drift).
    pub fn recalculate_tokens(&mut self) {
        self.total_tokens = self
            .messages
            .iter()
            .map(|m| estimate_tokens(&m.content))
            .sum();
    }

    pub fn role_type(&self) -> &str {
        self.messages
            .last()
            .map(|m| m.role.as_str())
            .unwrap_or("user")
    }

    /// Token budget remaining.
    pub fn remaining_budget(&self) -> u32 {
        self.config.max_tokens.saturating_sub(self.total_tokens)
    }

    /// Usage as a percentage of budget.
    pub fn usage_percentage(&self) -> f64 {
        if self.config.max_tokens == 0 {
            return 0.0;
        }
        (self.total_tokens as f64 / self.config.max_tokens as f64) * 100.0
    }

    // -----------------------------------------------------------------------
    // Session Info
    // -----------------------------------------------------------------------

    pub fn session(&self) -> &SessionInfo {
        &self.session
    }

    /// Start a new session (clears messages, keeps config).
    pub fn new_session(&mut self) -> SessionInfo {
        self.clear();
        self.session = SessionInfo::new();
        self.session.clone()
    }

    /// Start a new session with a label.
    pub fn new_session_with_label(&mut self, label: impl Into<String>) -> SessionInfo {
        self.clear();
        self.session = SessionInfo::new().with_label(label);
        self.session.clone()
    }

    // -----------------------------------------------------------------------
    // Clear
    // -----------------------------------------------------------------------

    /// Clear all messages, tool data, and summary.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.tool_calls.clear();
        self.tool_results.clear();
        self.total_tokens = 0;
        self.pinned_ids.clear();
        self.summary = None;
    }
}
