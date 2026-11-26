//! Guards for the live lane, which spends real money.

use anyhow::{bail, Result};

/// Refuse a model that has no entry in the price table: the spend ceilings
/// price usage with that table, so an unpriced model would run unmetered.
pub fn check_priced(model: &str, allow_unpriced: bool) -> Result<()> {
    if takka_core::pricing::lookup(model).is_some() || allow_unpriced {
        return Ok(());
    }
    bail!(
        "model {model:?} has no entry in the price table, so cost ceilings cannot be enforced; \
         pass --allow-unpriced to run it anyway"
    )
}

/// Total spend tracker for one live invocation.
#[derive(Debug)]
pub struct Budget {
    limit: f64,
    spent: f64,
}

impl Budget {
    pub fn new(limit_usd: f64) -> Self {
        Self {
            limit: limit_usd,
            spent: 0.0,
        }
    }

    pub fn record(&mut self, usd: f64) {
        self.spent += usd;
    }

    pub fn spent(&self) -> f64 {
        self.spent
    }

    pub fn remaining(&self) -> f64 {
        (self.limit - self.spent).max(0.0)
    }

    pub fn exhausted(&self) -> bool {
        self.spent >= self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_model_is_refused_unless_allowed() {
        let err = check_priced("totally-unknown-model-xyz", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--allow-unpriced"), "{err}");
        assert!(check_priced("totally-unknown-model-xyz", true).is_ok());
    }

    #[test]
    fn known_model_passes() {
        assert!(check_priced("gpt-4o-mini", false).is_ok());
    }

    #[test]
    fn budget_tracks_spend_and_exhaustion() {
        let mut b = Budget::new(1.0);
        assert!(!b.exhausted());
        b.record(0.4);
        assert!((b.remaining() - 0.6).abs() < 1e-9);
        b.record(0.7);
        assert!(b.exhausted());
        assert_eq!(b.remaining(), 0.0);
        assert!((b.spent() - 1.1).abs() < 1e-9);
    }
}
