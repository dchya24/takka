//! Safety engine: scoring, sandboxing, rate-limiting, audit logging,
//! and the `evaluate` decision pipeline used by the orchestrator.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use tracing::{debug, info, warn};

use super::audit::{AuditDecision, AuditEntry, RateLimitState};
use super::config::SafetyConfig;
use super::risk::{
    is_state_changing_action, ConfirmationRequest, PermissionMode, RiskLevel, RiskScore,
};

// ---------------------------------------------------------------------------
// Safety Engine
// ---------------------------------------------------------------------------

/// Main safety engine. Thread-safe via interior mutability.
pub struct Safety {
    config: SafetyConfig,
    rate_limit_states: Mutex<HashMap<String, RateLimitState>>,
    audit_log: Mutex<Vec<AuditEntry>>,
    /// Active permission mode. Defaults to `Default` (ask).
    mode: Mutex<PermissionMode>,
}

// ---------------------------------------------------------------------------
// Policy pipeline types (P2-1)
// ---------------------------------------------------------------------------

/// Input to the unified policy pipeline ([`Safety::evaluate_tool`]).
#[derive(Debug, Clone)]
pub struct PolicyRequest {
    /// Tool name (e.g. `run_command`, an MCP tool id).
    pub tool: String,
    /// Requested arguments (JSON as produced by the model).
    pub args: serde_json::Value,
    /// Static tool-level risk floor (0-100) from `ToolMetadata::risk`.
    /// `0` for unknown tools — never assume safety.
    pub risk_floor: u8,
}

impl PolicyRequest {
    pub fn new(tool: impl Into<String>, args: serde_json::Value, risk_floor: u8) -> Self {
        Self {
            tool: tool.into(),
            args,
            risk_floor,
        }
    }

    /// Extract the audit/sandbox target from the arguments — the same
    /// convention the orchestrator uses (`command` > `path` >
    /// `file_path`, first string wins).
    pub fn target(&self) -> Option<&str> {
        self.args
            .get("command")
            .or_else(|| self.args.get("path"))
            .or_else(|| self.args.get("file_path"))
            .and_then(|v| v.as_str())
    }
}

/// Output of the unified policy pipeline.
#[derive(Debug, Clone)]
pub struct PolicyDecision {
    /// Whether the call may proceed at all.
    pub allowed: bool,
    /// Why the call was denied (set iff `!allowed`).
    pub denial_reason: Option<String>,
    /// Whether a user confirmation is required before execution.
    pub confirmation_required: bool,
    /// Effective risk: max(per-argument score, tool risk floor).
    pub score: RiskScore,
    /// Post-decision notes (floor adjustments, sanitization hints) for
    /// audit trails and UIs.
    pub notes: Vec<String>,
}

impl Default for Safety {
    fn default() -> Self {
        Self::new()
    }
}

impl Safety {
    // -----------------------------------------------------------------------
    // Constructors
    // -----------------------------------------------------------------------

    pub fn new() -> Self {
        Self {
            config: SafetyConfig::default(),
            rate_limit_states: Mutex::new(HashMap::new()),
            audit_log: Mutex::new(Vec::new()),
            mode: Mutex::new(PermissionMode::default()),
        }
    }

    pub fn with_config(config: SafetyConfig) -> Self {
        Self {
            config,
            rate_limit_states: Mutex::new(HashMap::new()),
            audit_log: Mutex::new(Vec::new()),
            mode: Mutex::new(PermissionMode::default()),
        }
    }

    pub fn with_blocked_commands(mut self, commands: Vec<String>) -> Self {
        self.config.blocked_commands = commands;
        self
    }

    pub fn with_allowed_commands(mut self, commands: Vec<String>) -> Self {
        self.config.allowed_commands = commands;
        self
    }

    pub fn with_sandbox_paths(mut self, paths: Vec<String>) -> Self {
        self.config.sandbox_paths = paths;
        self
    }

    pub fn set_require_confirmation(&mut self, require: bool) {
        self.config.require_confirmation = require;
    }

    /// Switch the active permission mode at runtime.
    pub fn set_mode(&self, mode: PermissionMode) {
        *self.mode.lock().unwrap() = mode;
    }

    /// Get the active permission mode.
    pub fn mode(&self) -> PermissionMode {
        *self.mode.lock().unwrap()
    }

    // -----------------------------------------------------------------------
    // Core Risk Scoring
    // -----------------------------------------------------------------------

    /// Score a command/action and return a numeric risk assessment.
    pub fn score_command(&self, action: &str, target: Option<&str>) -> RiskScore {
        if !self.config.enabled {
            return RiskScore::low();
        }

        let target_str = target.unwrap_or("");
        let combined = format!("{} {}", action, target_str).to_lowercase();

        // 1) Hard blocklist check → automatic critical
        for blocked in &self.config.blocked_commands {
            if combined.contains(&blocked.to_lowercase()) {
                return RiskScore::new(1.0, RiskLevel::Critical);
            }
        }

        // 2) Allowed commands allowlist check (if configured)
        if !self.config.allowed_commands.is_empty() {
            let cmd_part = target_str.split_whitespace().next().unwrap_or(target_str);
            let is_allowed = self
                .config
                .allowed_commands
                .iter()
                .any(|a| cmd_part.eq_ignore_ascii_case(a.trim()));

            if !is_allowed && action == "run_command" {
                return RiskScore::new(0.7, RiskLevel::High);
            }
        }

        // 3) Pattern-based scoring — take the highest match
        let mut best_score = 0.0_f64;

        // We need &mut for lazy regex compilation, so we clone patterns
        // to avoid borrowing issues. The patterns are compiled once.
        let mut patterns = self.config.risk_patterns.clone();
        for pattern in &mut patterns {
            if pattern.matches(&combined) && pattern.score > best_score {
                best_score = pattern.score;
            }
        }

        // Also score based on action type
        let action_base = match action {
            "run_command" => 0.0, // scored by patterns above
            "write_file" | "edit_file" => 0.2,
            "delete_file" => 0.35,
            _ => 0.0,
        };

        if action_base > best_score {
            best_score = action_base;
        }

        // Classify using configurable thresholds
        let level = if best_score >= self.config.critical_threshold {
            RiskLevel::Critical
        } else if best_score >= self.config.high_threshold {
            RiskLevel::High
        } else if best_score >= self.config.medium_threshold {
            RiskLevel::Medium
        } else {
            RiskLevel::Low
        };

        RiskScore::new(best_score, level)
    }

    /// Legacy compatibility: return RiskLevel for an action.
    pub fn check_risk(&self, action: &str, target: Option<&str>) -> RiskLevel {
        self.score_command(action, target).level
    }

    // -----------------------------------------------------------------------
    // Path Sandboxing
    // -----------------------------------------------------------------------

    /// Check if a file path is within allowed sandbox boundaries.
    /// Returns `true` if the path is allowed (or if no sandbox is configured).
    pub fn is_path_allowed(&self, path: &str) -> bool {
        if self.config.sandbox_paths.is_empty() {
            return true; // no sandbox configured → everything allowed
        }

        let resolved = Path::new(path);

        // Block traversal attempts
        if path.contains("..") {
            debug!(path = path, "Path traversal attempt blocked");
            return false;
        }

        self.config
            .sandbox_paths
            .iter()
            .any(|sandbox| resolved.starts_with(sandbox))
    }

    /// Check if a URL's host is permitted by the domain allowlist.
    ///
    /// Empty allowlist = everything allowed (matches `sandbox_paths`
    /// semantics). Non-empty allowlist = only listed domains and their
    /// subdomains are reachable.
    ///
    /// Matching rules:
    ///   - Case-insensitive on the host.
    ///   - Exact match: `"github.com"` permits `github.com`.
    ///   - Subdomain match: `"github.com"` permits `api.github.com`
    ///     (dot-boundary). It does NOT permit `evilgithub.com`.
    ///   - Leading `.` on an allowlist entry is tolerated (`".github.com"`).
    ///   - URLs that don't parse as `<scheme>://<host>...` are rejected
    ///     when an allowlist is active.
    ///   - When `block_ip_urls = true`, hosts that look like an IPv4
    ///     octet sequence or a bracketed IPv6 literal are rejected
    ///     regardless of the allowlist.
    pub fn is_url_allowed(&self, url: &str) -> bool {
        if self.config.allowed_domains.is_empty() && !self.config.block_ip_urls {
            return true;
        }
        let host = match parse_host(url) {
            Some(h) => h.to_lowercase(),
            None => {
                debug!(url = url, "URL rejected: unparseable host");
                return false;
            }
        };

        if self.config.block_ip_urls && is_ip_literal(&host) {
            debug!(url = url, host = %host, "URL rejected: IP literal blocked");
            return false;
        }

        if self.config.allowed_domains.is_empty() {
            // block_ip_urls was set but no allowlist; the IP check above
            // is the only filter. Anything else passes.
            return true;
        }

        let allowed = self.config.allowed_domains.iter().any(|entry| {
            let entry = entry.trim_start_matches('.').to_lowercase();
            if entry.is_empty() {
                return false;
            }
            host == entry || host.ends_with(&format!(".{}", entry))
        });

        if !allowed {
            debug!(
                url = url,
                host = %host,
                "URL rejected: host not in allowed_domains"
            );
        }
        allowed
    }

    /// Return a RiskScore for a file operation, incorporating sandbox check.
    pub fn score_file_operation(&self, path: &str, operation: &str) -> RiskScore {
        if !self.is_path_allowed(path) {
            return RiskScore::new(0.9, RiskLevel::Critical);
        }

        let mut score = self.score_command(operation, Some(path));

        // Extra risk for sensitive system paths
        let sensitive = [
            "/etc/",
            "/usr/",
            "/boot/",
            "/sys/",
            "/proc/",
            "/dev/",
            "C:\\Windows\\",
            "C:\\Program Files\\",
        ];
        for prefix in &sensitive {
            if path.starts_with(prefix) {
                score.value = (score.value + 0.3).min(1.0);
                score.level = RiskLevel::from_score(score.value);
                break;
            }
        }

        score
    }

    // -----------------------------------------------------------------------
    // Rate Limiting
    // -----------------------------------------------------------------------

    /// Check rate limit for a tool. Returns `true` if the call is allowed.
    pub fn check_rate_limit(&self, tool_name: &str) -> bool {
        let limit = match self.config.rate_limits.get(tool_name) {
            Some(l) => l,
            None => return true, // no limit configured → allowed
        };

        let mut states = self.rate_limit_states.lock().unwrap();
        let state = states
            .entry(tool_name.to_string())
            .or_insert_with(RateLimitState::new);

        state.check_and_record(limit)
    }

    // -----------------------------------------------------------------------
    // Confirmation & Decision
    // -----------------------------------------------------------------------

    /// Should the user be prompted for confirmation?
    pub fn needs_confirmation(&self, action: &str, target: Option<&str>) -> bool {
        let mode = *self.mode.lock().unwrap();
        match mode {
            // Yolo never asks.
            PermissionMode::Yolo => false,
            // Plan denies state-changing actions outright (no confirmation).
            PermissionMode::Plan if is_state_changing_action(action) => false,
            _ => {
                if !self.config.enabled || !self.config.require_confirmation {
                    return false;
                }
                let score = self.score_command(action, target);
                score.value > self.config.auto_approve_threshold
            }
        }
    }

    // -----------------------------------------------------------------------
    // Unified tool-request policy pipeline (P2-1)
    // -----------------------------------------------------------------------

    /// Evaluate a tool request through the ONE policy pipeline used for
    /// builtin, MCP, and external tools alike.
    ///
    /// Combines the safety engine's per-argument scoring (blocklist,
    /// sandbox, rate limit, plan mode — see [`Self::evaluate`]) with the
    /// tool's static capability metadata: the tool-level risk floor
    /// (`ToolMetadata::risk`, 0-100) raises the *effective* risk used
    /// for the confirmation gate, so a low-argument call to an
    /// inherently risky tool class still surfaces for review.
    ///
    /// Denial and sandbox decisions always come from the per-argument
    /// evaluation — a capability floor never *grants* permission.
    pub fn evaluate_tool(&self, request: &PolicyRequest) -> PolicyDecision {
        let decision = self.evaluate(&request.tool, request.target());

        let floor = f64::from(request.risk_floor) / 100.0;
        let mut notes: Vec<String> = Vec::new();

        if !decision.allowed {
            return PolicyDecision {
                allowed: false,
                denial_reason: Some(if decision.reason.is_empty() {
                    "Action denied by safety policy".to_string()
                } else {
                    decision.reason.clone()
                }),
                confirmation_required: false,
                score: decision.score,
                notes,
            };
        }

        let effective = decision.score.value.max(floor);
        if floor > decision.score.value {
            notes.push(format!(
                "tool risk floor {} raised effective risk {:.2} → {:.2}",
                request.risk_floor, decision.score.value, effective
            ));
        }

        // The floor may only *raise* the confirmation bar in modes where
        // confirmations exist at all. Yolo auto-approves (except hard
        // blocks) and Plan decides by allow/deny — never prompt.
        let confirmation_required = match *self.mode.lock().unwrap() {
            PermissionMode::Yolo | PermissionMode::Plan => decision.needs_confirmation,
            PermissionMode::Default => {
                decision.needs_confirmation
                    || (self.config.require_confirmation
                        && effective > self.config.auto_approve_threshold)
            }
        };

        PolicyDecision {
            allowed: true,
            denial_reason: None,
            confirmation_required,
            score: crate::safety::risk::RiskScore::new(
                effective,
                crate::safety::risk::RiskLevel::from_score(effective),
            ),
            notes,
        }
    }

    pub fn evaluate(&self, action: &str, target: Option<&str>) -> SafetyDecision {
        let mode = *self.mode.lock().unwrap();

        // Plan mode: hard-deny any state-changing tool. Reads still allowed.
        if mode == PermissionMode::Plan && is_state_changing_action(action) {
            let score = RiskScore::new(0.5, RiskLevel::Medium);
            self.audit(action, target, &score, AuditDecision::Blocked);
            return SafetyDecision {
                score,
                allowed: false,
                needs_confirmation: false,
                reason: format!("Blocked by plan mode: {} is a state-changing tool", action),
            };
        }

        // Yolo mode: allow everything except the hard blocklist (which is
        // checked below as part of normal scoring). We bypass confirmation
        // and rate limiting but keep critical-pattern blocking as a final
        // safety net.
        if mode == PermissionMode::Yolo {
            let score = self.score_command(action, target);
            if score.level == RiskLevel::Critical {
                self.audit(action, target, &score, AuditDecision::Blocked);
                return SafetyDecision {
                    score,
                    allowed: false,
                    needs_confirmation: false,
                    reason:
                        "Action blocked: critical risk level (yolo mode still blocks blocklist)"
                            .into(),
                };
            }
            return SafetyDecision {
                score,
                allowed: true,
                needs_confirmation: false,
                reason: "yolo mode: auto-approved".into(),
            };
        }

        if !self.config.enabled {
            return SafetyDecision {
                score: RiskScore::low(),
                allowed: true,
                needs_confirmation: false,
                reason: "Safety system disabled".into(),
            };
        }

        // 1) Score the action
        let score = self.score_command(action, target);

        // 2) Critical actions are always blocked
        if score.level == RiskLevel::Critical {
            self.audit(action, target, &score, AuditDecision::Blocked);
            return SafetyDecision {
                score,
                allowed: false,
                needs_confirmation: false,
                reason: "Action blocked: critical risk level".into(),
            };
        }

        // 3) Path sandboxing for file operations
        if matches!(
            action,
            "write_file" | "edit_file" | "delete_file" | "read_file"
        ) {
            if let Some(path) = target {
                if !self.is_path_allowed(path) {
                    let file_score = RiskScore::new(0.9, RiskLevel::Critical);
                    self.audit(action, target, &file_score, AuditDecision::Blocked);
                    return SafetyDecision {
                        score: file_score,
                        allowed: false,
                        needs_confirmation: false,
                        reason: "Path outside sandbox boundaries".into(),
                    };
                }
            }
        }

        // 4) Rate limiting
        if !self.check_rate_limit(action) {
            self.audit(action, target, &score, AuditDecision::RateLimited);
            return SafetyDecision {
                score,
                allowed: false,
                needs_confirmation: false,
                reason: "Rate limit exceeded".into(),
            };
        }

        // 5) Medium/High needs confirmation
        let needs_confirm =
            self.config.require_confirmation && score.value > self.config.auto_approve_threshold;

        SafetyDecision {
            score,
            allowed: true,
            needs_confirmation: needs_confirm,
            reason: String::new(),
        }
    }

    /// Create a confirmation request (for the UI / CLI to prompt the user).
    pub fn create_request(&self, action: &str, description: &str) -> ConfirmationRequest {
        let score = self.score_command(action, None);
        ConfirmationRequest {
            action: action.to_string(),
            description: description.to_string(),
            risk_level: score.level,
            risk_score: score.value,
            reason: format!("Risk score: {:.2} ({})", score.value, score.level.as_str()),
            timestamp: Utc::now(),
            preview_diff: None,
        }
    }

    // -----------------------------------------------------------------------
    // Audit Logging
    // -----------------------------------------------------------------------

    fn audit(
        &self,
        action: &str,
        target: Option<&str>,
        score: &RiskScore,
        decision: AuditDecision,
    ) {
        if !self.config.audit_logging {
            return;
        }

        let entry = AuditEntry {
            timestamp: Utc::now(),
            action: action.to_string(),
            target: target.map(|s| s.to_string()),
            risk_score: *score,
            decision,
            reason: String::new(),
        };

        let mut log = self.audit_log.lock().unwrap();
        if log.len() >= self.config.audit_capacity {
            log.remove(0); // ring buffer
        }
        log.push(entry);

        debug!(
            action = action,
            target = target.unwrap_or(""),
            score = score.value,
            level = score.level.as_str(),
            decision = ?decision,
            "Safety audit"
        );
    }

    /// Record that a user confirmed or denied an action.
    pub fn record_confirmation(
        &self,
        action: &str,
        target: Option<&str>,
        score: &RiskScore,
        approved: bool,
    ) {
        let decision = if approved {
            AuditDecision::Approved
        } else {
            AuditDecision::DeniedByConfirmation
        };
        self.audit(action, target, score, decision);

        if approved {
            info!(action = action, "User approved action");
        } else {
            warn!(action = action, "User denied action");
        }
    }

    /// Get recent audit entries (newest first).
    pub fn audit_log(&self) -> Vec<AuditEntry> {
        let log = self.audit_log.lock().unwrap();
        let mut entries: Vec<AuditEntry> = log.clone();
        entries.reverse();
        entries
    }

    /// Clear audit log.
    pub fn clear_audit_log(&self) {
        self.audit_log.lock().unwrap().clear();
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    pub fn config(&self) -> &SafetyConfig {
        &self.config
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Snapshot of the URL allowlist policy. Cheap to clone; intended
    /// to be handed to URL-taking tools (`fetch`, `web_search`) so they
    /// can self-gate without holding a `Safety` reference.
    pub fn url_policy(&self) -> super::UrlPolicy {
        super::UrlPolicy::new(
            self.config.allowed_domains.clone(),
            self.config.block_ip_urls,
        )
    }
}

// ---------------------------------------------------------------------------
// Safety Decision (result of evaluate())
// ---------------------------------------------------------------------------

/// Result of evaluating an action through the safety system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyDecision {
    pub score: RiskScore,
    pub allowed: bool,
    pub needs_confirmation: bool,
    pub reason: String,
}

// ---------------------------------------------------------------------------
// URL helpers
// ---------------------------------------------------------------------------

/// Extract the host (case as-given) from a URL string.
///
/// Supports the common shapes we see in tools: absolute URLs with a
/// scheme + authority, optional userinfo, optional port, optional path/
/// query/fragment. IPv6 literals in brackets are returned with the
/// brackets stripped. Returns `None` for inputs that don't begin with a
/// `<scheme>://` or whose authority section is empty.
///
/// We avoid pulling in the `url` crate so the safety layer stays
/// dependency-free; the parsing here is intentionally permissive and
/// is paired with allowlist matching, not used for canonicalization.
pub(crate) fn parse_host(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let scheme_end = trimmed.find("://")?;
    if scheme_end == 0 {
        // "://example.com" — empty scheme.
        return None;
    }
    let after_scheme = &trimmed[scheme_end + 3..];
    if after_scheme.is_empty() {
        return None;
    }
    // Authority ends at the first '/', '?' or '#'.
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];

    // Strip optional userinfo (`user:pass@`).
    let host_and_port = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);

    if host_and_port.is_empty() {
        return None;
    }

    // IPv6 literal: `[::1]` or `[::1]:8080`.
    if let Some(stripped) = host_and_port.strip_prefix('[') {
        let close = stripped.find(']')?;
        let host = &stripped[..close];
        if host.is_empty() {
            return None;
        }
        return Some(host.to_string());
    }

    // IPv4 / DNS host: drop the optional `:<port>` suffix.
    let host = host_and_port.split(':').next().unwrap_or(host_and_port);
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// True if `host` looks like an IPv4 dotted quad or a parsed IPv6
/// address (after bracket stripping by `parse_host`).
pub(crate) fn is_ip_literal(host: &str) -> bool {
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    if host.parse::<std::net::Ipv6Addr>().is_ok() {
        return true;
    }
    false
}
