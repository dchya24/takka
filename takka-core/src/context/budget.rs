//! Token budget allocation for a model request (P0-1 of the hardening
//! plan).
//!
//! The budget answers one question: of the model's total context window,
//! how much may the conversation occupy? The remainder is deliberately
//! left unallocated here — it is headroom for the system prompt, tool
//! definitions, and the response itself. Keeping this split in the
//! context engine (rather than on `Memory`) means storage stays pure and
//! every consumer of [`crate::context::ContextEngine`] gets the same
//! allocation rules.

/// Allocation of the model context window across request sinks.
///
/// ```text
/// max_tokens ──┬── conversation()   ratio share (default 70%)
///             └── (reserved)       system prompt + tool defs + response
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextBudget {
    max_tokens: u32,
    ratio: f64,
}

/// Lower clamp for the conversation ratio. Protects against a config
/// typo starving the conversation to nothing.
const MIN_RATIO: f64 = 0.1;
/// Upper clamp. Leaves at least 5% of the window for the response.
const MAX_RATIO: f64 = 0.95;

impl ContextBudget {
    /// Create a budget for a `max_tokens` window with the given
    /// conversation `ratio`. Out-of-range ratios are clamped to
    /// `[0.1, 0.95]`.
    pub fn new(max_tokens: u32, ratio: f64) -> Self {
        Self {
            max_tokens,
            ratio: ratio.clamp(MIN_RATIO, MAX_RATIO),
        }
    }

    /// Total context window this budget was built from.
    pub fn total(&self) -> u32 {
        self.max_tokens
    }

    /// Token budget for the conversation slice. This is the value the
    /// window selection may fill; the rest of the window is reserved for
    /// the system prompt, tool definitions, and the response.
    pub fn conversation(&self) -> u32 {
        (self.max_tokens as f64 * self.ratio) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_applies_ratio() {
        assert_eq!(ContextBudget::new(100_000, 0.5).conversation(), 50_000);
    }

    #[test]
    fn clamps_extreme_ratios() {
        // Out-of-range ratios get clamped to a sensible band so a user
        // typo doesn't accidentally send 0 or 99% of the window.
        assert_eq!(ContextBudget::new(100_000, 0.0).conversation(), 10_000); // 0.1
        assert_eq!(ContextBudget::new(100_000, 5.0).conversation(), 95_000); // 0.95
    }

    #[test]
    fn conversation_never_exceeds_total() {
        let b = ContextBudget::new(1_000, MAX_RATIO);
        assert!(b.conversation() <= b.total());
    }
}
