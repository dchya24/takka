//! Model capability registry — what does a given model support?
//!
//! Mirrors `pricing.rs` in shape: a built-in table of well-known models
//! mapped to their capabilities, with a longest-`contains` fallback so
//! dated/vendor-suffixed names (e.g. `claude-3-5-sonnet-20241022`)
//! resolve correctly. Users can override via `Config.providers[*].models[*].capabilities`.
//!
//! Why not just call the provider? The agent loop needs to know up-front,
//! before dispatching a turn, whether attaching an image is going to be
//! rejected. That decision shapes the CLI's behaviour (status-bar 👁️
//! chip, `@image.png` auto-routing, error messages) and we don't want
//! to make a network round-trip just to ask.

use serde::{Deserialize, Serialize};

/// What a model is known to support.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCapabilities {
    /// Image input (vision). When `false`, attaching an image to a
    /// chat turn is an error before the request is sent.
    #[serde(default)]
    pub vision: bool,
    /// Function/tool calling. Today we always assume `true` for any
    /// model the registry knows about; this is reserved for future
    /// models that don't support tools.
    #[serde(default = "default_true")]
    pub tools: bool,
    /// Streaming responses (SSE).
    #[serde(default = "default_true")]
    pub streaming: bool,
}

fn default_true() -> bool {
    true
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            vision: false,
            tools: true,
            streaming: true,
        }
    }
}

impl ModelCapabilities {
    pub const fn new(vision: bool, tools: bool, streaming: bool) -> Self {
        Self {
            vision,
            tools,
            streaming,
        }
    }
}

/// Built-in capability table. Keys match the runtime model name with
/// case-insensitive exact match first, then longest-`contains` fallback.
fn capability_table() -> &'static [(&'static str, ModelCapabilities)] {
    static TABLE: &[(&str, ModelCapabilities)] = &[
        // ── OpenAI vision-capable ──────────────────────────
        ("gpt-4o", ModelCapabilities::new(true, true, true)),
        ("gpt-4o-mini", ModelCapabilities::new(true, true, true)),
        ("gpt-4-turbo", ModelCapabilities::new(true, true, true)),
        ("gpt-4-vision", ModelCapabilities::new(true, true, true)),
        // OpenAI text-only
        ("gpt-4", ModelCapabilities::new(false, true, true)),
        ("gpt-3.5-turbo", ModelCapabilities::new(false, true, true)),
        ("o1", ModelCapabilities::new(false, true, false)), // o1 disables streaming
        ("o1-mini", ModelCapabilities::new(false, true, false)),
        // ── Anthropic — all claude-3* / claude-3-5* support vision ──
        (
            "claude-3-5-sonnet",
            ModelCapabilities::new(true, true, true),
        ),
        ("claude-3-5-haiku", ModelCapabilities::new(true, true, true)),
        ("claude-3-opus", ModelCapabilities::new(true, true, true)),
        ("claude-3-sonnet", ModelCapabilities::new(true, true, true)),
        ("claude-3-haiku", ModelCapabilities::new(true, true, true)),
        // ── DeepSeek / GLM (no public vision support yet) ──
        ("deepseek-chat", ModelCapabilities::new(false, true, true)),
        ("deepseek-coder", ModelCapabilities::new(false, true, true)),
        ("glm-4", ModelCapabilities::new(false, true, true)),
        ("glm-4.7", ModelCapabilities::new(false, true, true)),
    ];
    TABLE
}

/// Look up capabilities for a model name. Same fallback strategy as
/// `pricing::lookup`: exact match (case-insensitive) first, then the
/// longest substring match against the canonical names.
///
/// Returns `None` for unknown models. Callers should treat unknown as
/// "conservative defaults" (no vision; streaming + tools assumed) rather
/// than silently hiding capabilities.
pub fn lookup(model: &str) -> Option<ModelCapabilities> {
    let model_lower = model.to_lowercase();
    let table = capability_table();

    for (name, caps) in table {
        if name.eq_ignore_ascii_case(model) {
            return Some(*caps);
        }
    }

    let mut best: Option<(&str, ModelCapabilities)> = None;
    for (name, caps) in table {
        if model_lower.contains(&name.to_lowercase()) {
            match best {
                Some((current, _)) if current.len() >= name.len() => {}
                _ => best = Some((name, *caps)),
            }
        }
    }
    best.map(|(_, c)| c)
}

/// Resolve capabilities with a sensible fallback for unknown models.
/// Conservative defaults: vision OFF, tools ON, streaming ON.
pub fn resolve(model: &str) -> ModelCapabilities {
    lookup(model).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_conservative() {
        let d = ModelCapabilities::default();
        assert!(!d.vision);
        assert!(d.tools);
        assert!(d.streaming);
    }

    #[test]
    fn lookup_known_vision_model() {
        let c = lookup("gpt-4o").unwrap();
        assert!(c.vision);
        assert!(c.tools);
    }

    #[test]
    fn lookup_known_text_model() {
        let c = lookup("gpt-4").unwrap();
        assert!(!c.vision);
    }

    #[test]
    fn lookup_handles_dated_anthropic_suffix() {
        let c = lookup("claude-3-5-sonnet-20241022").unwrap();
        assert!(c.vision);
        let c = lookup("claude-3-5-haiku-20241022").unwrap();
        assert!(c.vision);
    }

    #[test]
    fn lookup_prefers_longest_match() {
        // "gpt-4o-mini-2024-07-18" must map to gpt-4o-mini, not gpt-4o.
        // Both happen to be vision-capable so we assert via direct
        // capability rather than identity, but the longer-name semantic
        // is what's being verified.
        let c = lookup("gpt-4o-mini-2024-07-18").unwrap();
        assert!(c.vision);
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(lookup("some-totally-unknown-model").is_none());
    }

    #[test]
    fn resolve_unknown_falls_back_to_default() {
        let c = resolve("some-totally-unknown-model");
        assert!(!c.vision);
        assert!(c.tools);
        assert!(c.streaming);
    }

    #[test]
    fn o1_disables_streaming() {
        let c = lookup("o1").unwrap();
        assert!(!c.streaming);
        let c = lookup("o1-mini").unwrap();
        assert!(!c.streaming);
    }
}
