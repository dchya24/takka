//! Audit logging and rate-limiting state.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Instant;

use super::risk::RiskScore;

// ---------------------------------------------------------------------------
// Rate Limiting
// ---------------------------------------------------------------------------

/// Rate limit configuration per tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimit {
    /// Max invocations allowed within the window.
    pub max_calls: u32,
    /// Window duration in seconds.
    pub window_secs: u64,
}

/// Tracks rate-limit state for a single tool. `pub(super)` so the
/// engine module can construct and update it.
#[derive(Debug)]
pub(super) struct RateLimitState {
    calls: Vec<Instant>,
}

impl RateLimitState {
    pub(super) fn new() -> Self {
        Self { calls: Vec::new() }
    }

    pub(super) fn check_and_record(&mut self, limit: &RateLimit) -> bool {
        let now = Instant::now();
        let cutoff = now - std::time::Duration::from_secs(limit.window_secs);
        self.calls.retain(|t| *t > cutoff);

        if (self.calls.len() as u32) >= limit.max_calls {
            return false; // rate limited
        }
        self.calls.push(now);
        true
    }
}

// ---------------------------------------------------------------------------
// Audit Log Entry
// ---------------------------------------------------------------------------

/// A single audit log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub timestamp: DateTime<Utc>,
    pub action: String,
    pub target: Option<String>,
    pub risk_score: RiskScore,
    pub decision: AuditDecision,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditDecision {
    Approved,
    Blocked,
    RateLimited,
    DeniedByConfirmation,
}
