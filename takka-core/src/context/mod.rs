//! Context Engine — the first-class subsystem that decides **what gets
//! sent to the model** each turn.
//!
//! This is the single source of truth for context assembly. The
//! orchestrator (and any frontend that drives the agent) builds its
//! request through [`ContextEngine::build`] instead of reaching into
//! memory internals.
//!
//! ```text
//! ContextEngine
//!      │
//!      ▼
//! Token Budget        (budget::ContextBudget — ratio of context window)
//!      │
//!      ▼
//! Window Selection    (sources::turn_aware_window — turn-aware slice)
//!      │
//!      ▼
//! Request Shaping     (Layer 2 clear + sanitize_for_provider)
//!      │
//!      ▼
//! ChatMessageRequest[]
//! ```
//!
//! The split follows the core principle:
//!
//! > **Memory menyimpan informasi. Context menentukan informasi apa yang
//! > dikirim ke model sekarang.**
//!
//! `Memory` (in [`crate::memory`]) is pure storage: CRUD, pinning,
//! persistence. All windowing and budget policy lives here — in
//! [`mod@budget`] and [`mod@sources`] — composed with the pure builder
//! rules in [`mod@builder`] into a single, UI-agnostic assembly point.

use crate::memory::Memory;
use crate::providers::ChatMessageRequest;

pub mod budget;
pub mod builder;
pub mod sources;

pub use budget::ContextBudget;
pub use builder::{
    build_request_messages, build_tool_call_responses, truncate_tool_result,
    CLEARED_TOOL_RESULT_PLACEHOLDER,
};
pub use sources::{sliding_window, turn_aware_window, user_anchored_tail};

/// Assembly options for a single request slice.
#[derive(Debug, Clone, Copy)]
pub struct BuildOptions {
    /// Token budget for the conversation portion of the window, derived
    /// from [`ContextBudget`] — the remainder of the model window stays
    /// reserved for the system prompt, tool definitions, and the
    /// response.
    pub token_budget: u32,
    /// How many most-recent tool results to keep verbatim (Layer 2).
    /// `0` disables clearing entirely.
    pub keep_recent_tool_results: usize,
}

impl BuildOptions {
    /// Compute the default options for a memory: the conversation slice
    /// gets the configured budget ratio (see [`ContextBudget`]), and
    /// tool results stay verbatim up to `keep_recent_tool_results`.
    pub fn new(memory: &Memory, keep_recent_tool_results: usize) -> Self {
        let budget = ContextBudget::new(memory.budget(), memory.config.context_budget_ratio);
        Self {
            token_budget: budget.conversation(),
            keep_recent_tool_results,
        }
    }
}

/// The context assembly point. See the [module docs](self).
pub struct ContextEngine;

impl ContextEngine {
    /// Build the provider-ready message slice for the current state of
    /// `memory`, applying token-budget window selection, Layer 2 tool-
    /// result clearing, and the provider-sanitization passes.
    ///
    /// This is the replacement for `Orchestrator::build_messages` — a
    /// single, reusable entry point every frontend can drive.
    pub fn build(memory: &Memory, options: BuildOptions) -> Vec<ChatMessageRequest> {
        let context = sources::turn_aware_window(
            memory.get_messages(),
            memory.summary(),
            options.token_budget,
        );
        build_request_messages(&context, options.keep_recent_tool_results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_produces_well_formed_slice() {
        let mut mem = Memory::new(10000);
        mem.add_message(crate::memory::Message::user("hello"));

        let out = ContextEngine::build(&mem, BuildOptions::new(&mem, 6));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "user");
        assert_eq!(out[0].content, "hello");
    }

    #[test]
    fn build_options_expose_keep_count() {
        let mem = Memory::new(10000);
        let opts = BuildOptions::new(&mem, 4);
        assert_eq!(opts.keep_recent_tool_results, 4);
        assert!(opts.token_budget > 0);
    }

    #[test]
    fn build_options_default_ratio_is_seventy_percent() {
        // Tracks the documented contract: default context_budget_ratio
        // = 0.7.
        let mem = Memory::new(100_000);
        let opts = BuildOptions::new(&mem, 6);
        assert_eq!(opts.token_budget, 70_000);
    }

    #[test]
    fn build_handles_empty_memory() {
        let mem = Memory::new(10000);
        let out = ContextEngine::build(&mem, BuildOptions::new(&mem, 6));
        assert!(out.is_empty());
    }
}
