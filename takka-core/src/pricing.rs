//! Model price registry — what does a given model cost per token?
//!
//! Same shape as [`crate::capabilities`]: a built-in table of well-known
//! models, exact (case-insensitive) match first, then the longest
//! `contains` fallback so dated / vendor-suffixed names
//! (`claude-3-5-sonnet-20241022`, `gpt-4o-2024-08-06`) resolve correctly.
//!
//! Why a table and not a provider call? The spend ceiling
//! (`agent.max_cost_usd`) has to price every usage report *synchronously*
//! at the agent-loop boundary. A network round-trip there would both stall
//! the loop and add a second way for a run to fail.
//!
//! Prices are list prices in USD per **million** tokens (what vendors
//! publish). Unknown models return `None`, and the ceiling then simply
//! cannot be evaluated — the loop keeps running (fail-open), never
//! inventing a price.

/// List price of one model, in USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    /// Uncached prompt (input) tokens.
    pub input_per_mtok: f64,
    /// Completion (output) tokens.
    pub output_per_mtok: f64,
    /// Prompt-cache reads. `None` falls back to [`Self::input_per_mtok`].
    pub cache_read_per_mtok: Option<f64>,
    /// Prompt-cache writes. `None` falls back to [`Self::input_per_mtok`].
    pub cache_write_per_mtok: Option<f64>,
}

impl ModelPrice {
    const fn new(input_per_mtok: f64, output_per_mtok: f64) -> Self {
        Self {
            input_per_mtok,
            output_per_mtok,
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
        }
    }

    const fn with_cache(
        input_per_mtok: f64,
        output_per_mtok: f64,
        cache_read_per_mtok: f64,
        cache_write_per_mtok: f64,
    ) -> Self {
        Self {
            input_per_mtok,
            output_per_mtok,
            cache_read_per_mtok: Some(cache_read_per_mtok),
            cache_write_per_mtok: Some(cache_write_per_mtok),
        }
    }

    /// Cost of one usage report, in USD.
    ///
    /// Providers that report cache tokens already exclude them from
    /// `prompt_tokens`; when one reports a prompt count that still includes
    /// them the estimate is slightly high, which errs on the safe side of a
    /// ceiling.
    pub fn cost_usd(
        &self,
        prompt_tokens: u32,
        completion_tokens: u32,
        cache_read_tokens: u32,
        cache_creation_tokens: u32,
    ) -> f64 {
        let per_mtok = |tokens: u32, price: f64| f64::from(tokens) * price / 1_000_000.0;
        per_mtok(prompt_tokens, self.input_per_mtok)
            + per_mtok(completion_tokens, self.output_per_mtok)
            + per_mtok(
                cache_read_tokens,
                self.cache_read_per_mtok.unwrap_or(self.input_per_mtok),
            )
            + per_mtok(
                cache_creation_tokens,
                self.cache_write_per_mtok.unwrap_or(self.input_per_mtok),
            )
    }
}

/// Built-in price table. Keys match the runtime model name with
/// case-insensitive exact match first, then longest-`contains` fallback.
fn price_table() -> &'static [(&'static str, ModelPrice)] {
    static TABLE: &[(&str, ModelPrice)] = &[
        // ── OpenAI ─────────────────────────────────────────────
        ("gpt-4o", ModelPrice::new(2.50, 10.00)),
        ("gpt-4o-mini", ModelPrice::new(0.15, 0.60)),
        ("gpt-4-turbo", ModelPrice::new(10.00, 30.00)),
        ("gpt-4", ModelPrice::new(30.00, 60.00)),
        ("gpt-3.5-turbo", ModelPrice::new(0.50, 1.50)),
        ("o1", ModelPrice::new(15.00, 60.00)),
        ("o1-mini", ModelPrice::new(3.00, 12.00)),
        // ── Anthropic (cache read 0.1x, cache write 1.25x input) ──
        (
            "claude-3-5-sonnet",
            ModelPrice::with_cache(3.00, 15.00, 0.30, 3.75),
        ),
        (
            "claude-3-5-haiku",
            ModelPrice::with_cache(0.80, 4.00, 0.08, 1.00),
        ),
        (
            "claude-3-opus",
            ModelPrice::with_cache(15.00, 75.00, 1.50, 18.75),
        ),
        (
            "claude-3-sonnet",
            ModelPrice::with_cache(3.00, 15.00, 0.30, 3.75),
        ),
        (
            "claude-3-haiku",
            ModelPrice::with_cache(0.25, 1.25, 0.03, 0.30),
        ),
        // ── DeepSeek / GLM ─────────────────────────────────────
        ("deepseek-chat", ModelPrice::new(0.27, 1.10)),
        ("deepseek-coder", ModelPrice::new(0.27, 1.10)),
        ("glm-4.7", ModelPrice::new(0.60, 2.20)),
        ("glm-4", ModelPrice::new(0.60, 2.20)),
    ];
    TABLE
}

/// Look up the list price for a model name. Same fallback strategy as
/// `capabilities::lookup`: exact match (case-insensitive) first, then the
/// longest substring match against the canonical names.
///
/// Returns `None` for unknown models — callers treat that as "cannot be
/// priced", never as "free".
pub fn lookup(model: &str) -> Option<ModelPrice> {
    let model_lower = model.to_lowercase();
    let table = price_table();

    for (name, price) in table {
        if name.eq_ignore_ascii_case(model) {
            return Some(*price);
        }
    }

    let mut best: Option<(&str, ModelPrice)> = None;
    for (name, price) in table {
        if model_lower.contains(&name.to_lowercase()) {
            match best {
                Some((current, _)) if current.len() >= name.len() => {}
                _ => best = Some((name, *price)),
            }
        }
    }
    best.map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_wins_over_substring() {
        // `gpt-4o-mini` must not resolve as `gpt-4o`.
        assert_eq!(lookup("gpt-4o-mini"), Some(ModelPrice::new(0.15, 0.60)));
        assert_eq!(lookup("GPT-4o-Mini"), Some(ModelPrice::new(0.15, 0.60)));
    }

    #[test]
    fn dated_model_names_fall_back_to_their_family() {
        let sonnet = lookup("claude-3-5-sonnet-20241022").expect("known family");
        assert_eq!(sonnet.input_per_mtok, 3.00);
        let gpt4 = lookup("gpt-4o-2024-08-06").expect("known family");
        assert_eq!(gpt4.output_per_mtok, 10.00);
    }

    #[test]
    fn unknown_model_has_no_price() {
        assert!(lookup("totally-made-up-model").is_none());
    }

    #[test]
    fn cost_is_price_times_tokens() {
        let price = lookup("gpt-4o-mini").unwrap();
        // 1M prompt + 1M completion = $0.15 + $0.60.
        let cost = price.cost_usd(1_000_000, 1_000_000, 0, 0);
        assert!((cost - 0.75).abs() < 1e-9, "cost = {cost}");
    }

    #[test]
    fn cache_tokens_use_their_own_price() {
        let price = lookup("claude-3-5-sonnet").unwrap();
        // 1M cache reads at $0.30, 1M cache writes at $3.75.
        let cost = price.cost_usd(0, 0, 1_000_000, 1_000_000);
        assert!((cost - 4.05).abs() < 1e-9, "cost = {cost}");
    }

    #[test]
    fn models_without_cache_prices_fall_back_to_input_price() {
        let price = lookup("gpt-4o-mini").unwrap();
        let cost = price.cost_usd(0, 0, 1_000_000, 0);
        assert!((cost - 0.15).abs() < 1e-9, "cost = {cost}");
    }
}
