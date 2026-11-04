//! Risk scoring primitives: permission modes, risk levels, scores,
//! and confirmation requests.
//!
//! Pure data + small helpers. No engine state, no I/O.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Permission Mode
// ---------------------------------------------------------------------------

/// High-level permission mode that gates the safety engine.
///
/// Maps directly to the architecture doc's three modes:
/// - `Default` — ask for writes & commands, allow reads (current behavior).
/// - `Plan`    — read-only mode. All writes/commands are denied.
/// - `Yolo`    — allow everything (dangerous; for trusted automation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    #[default]
    Default,
    Plan,
    Yolo,
}

impl PermissionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PermissionMode::Default => "default",
            PermissionMode::Plan => "plan",
            PermissionMode::Yolo => "yolo",
        }
    }

    /// Parse from a free-form string (case-insensitive). Accepts a few
    /// common synonyms so CLI users don't have to memorize exact spelling.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "default" | "normal" | "ask" => Some(PermissionMode::Default),
            "plan" | "readonly" | "read-only" | "dry-run" => Some(PermissionMode::Plan),
            "yolo" | "auto" | "trust" | "unsafe" => Some(PermissionMode::Yolo),
            _ => None,
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Tool actions that mutate state. Used by Plan mode to deny writes
/// without having to enumerate every shell command.
pub(super) fn is_state_changing_action(action: &str) -> bool {
    matches!(
        action,
        "write_file" | "edit_file" | "delete_file" | "run_command" | "run_script"
    )
}

// ---------------------------------------------------------------------------
// Risk Score
// ---------------------------------------------------------------------------

/// Numeric risk score with categorized level.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RiskScore {
    /// 0.0 (safe) to 1.0 (extremely dangerous)
    pub value: f64,
    pub level: RiskLevel,
}

impl RiskScore {
    pub fn new(value: f64, level: RiskLevel) -> Self {
        Self {
            value: value.clamp(0.0, 1.0),
            level,
        }
    }

    pub fn low() -> Self {
        Self {
            value: 0.1,
            level: RiskLevel::Low,
        }
    }

    pub fn medium() -> Self {
        Self {
            value: 0.5,
            level: RiskLevel::Medium,
        }
    }

    pub fn high() -> Self {
        Self {
            value: 0.8,
            level: RiskLevel::High,
        }
    }

    pub fn critical() -> Self {
        Self {
            value: 1.0,
            level: RiskLevel::Critical,
        }
    }
}

// ---------------------------------------------------------------------------
// Risk Level
// ---------------------------------------------------------------------------

/// Categorized risk level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

impl RiskLevel {
    pub fn requires_confirmation(&self) -> bool {
        matches!(
            self,
            RiskLevel::Medium | RiskLevel::High | RiskLevel::Critical
        )
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            RiskLevel::Low => "low",
            RiskLevel::Medium => "medium",
            RiskLevel::High => "high",
            RiskLevel::Critical => "critical",
        }
    }

    pub fn from_score(value: f64) -> Self {
        if value >= 0.8 {
            RiskLevel::Critical
        } else if value >= 0.6 {
            RiskLevel::High
        } else if value >= 0.3 {
            RiskLevel::Medium
        } else {
            RiskLevel::Low
        }
    }
}

// ---------------------------------------------------------------------------
// Confirmation Request
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmationRequest {
    pub action: String,
    pub description: String,
    pub risk_level: RiskLevel,
    pub risk_score: f64,
    pub reason: String,
    pub timestamp: DateTime<Utc>,
    /// Optional unified-diff preview of the change about to be made.
    /// Populated for state-changing tools (`write_file`, `edit_file`,
    /// `apply_patch`) so the user sees the exact change before they
    /// approve it. `None` for non-file actions or when the preview
    /// could not be computed (e.g. file unreadable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_diff: Option<String>,
}
