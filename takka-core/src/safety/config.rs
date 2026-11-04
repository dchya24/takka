//! Configuration for the safety engine: thresholds, blocklists,
//! risk patterns. Pure data, deserialized from user config.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::audit::RateLimit;

// ---------------------------------------------------------------------------
// Safety Config (serializable)
// ---------------------------------------------------------------------------

/// Fully configurable safety configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyConfig {
    /// Whether safety checks are enabled at all.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Whether to require confirmation for medium+ risk actions.
    #[serde(default = "default_true")]
    pub require_confirmation: bool,

    /// Auto-approve actions with risk score <= this threshold.
    #[serde(default = "default_auto_approve_threshold")]
    pub auto_approve_threshold: f64,

    /// Risk score threshold for "Medium" classification.
    #[serde(default = "default_medium_threshold")]
    pub medium_threshold: f64,

    /// Risk score threshold for "High" classification.
    #[serde(default = "default_high_threshold")]
    pub high_threshold: f64,

    /// Risk score threshold for "Critical" classification.
    #[serde(default = "default_critical_threshold")]
    pub critical_threshold: f64,

    /// Hard-blocked command patterns (exact + substring match).
    #[serde(default = "default_blocked_commands")]
    pub blocked_commands: Vec<String>,

    /// Regex patterns that trigger automatic risk scoring.
    #[serde(default = "default_risk_patterns")]
    pub risk_patterns: Vec<RiskPattern>,

    /// Allowed command prefixes — if non-empty, only these are permitted.
    #[serde(default)]
    pub allowed_commands: Vec<String>,

    /// Sandbox directory roots. File operations outside these are blocked.
    #[serde(default)]
    pub sandbox_paths: Vec<String>,

    /// Domain allowlist for URL-taking tools (`fetch`, `web_search`).
    ///
    /// Empty = no restriction (current behavior).
    /// Non-empty = only requests to these hosts (and their subdomains)
    /// are permitted. Match is case-insensitive on the registered
    /// domain, with dot-boundary suffix matching (so `github.com`
    /// allows `api.github.com` but not `evilgithub.com`).
    #[serde(default)]
    pub allowed_domains: Vec<String>,

    /// When the URL allowlist is in effect, also reject URLs that
    /// resolve to an IP literal (`http://192.168.1.1/...`,
    /// `http://[::1]/...`). Defaults to `false` so local dev setups
    /// keep working.
    #[serde(default)]
    pub block_ip_urls: bool,

    /// Per-tool rate limiting configuration.
    #[serde(default)]
    pub rate_limits: HashMap<String, RateLimit>,

    /// Whether to emit audit log entries.
    #[serde(default = "default_true")]
    pub audit_logging: bool,

    /// Max number of audit entries to keep in memory (ring buffer).
    #[serde(default = "default_audit_capacity")]
    pub audit_capacity: usize,
}

fn default_true() -> bool {
    true
}
fn default_auto_approve_threshold() -> f64 {
    0.3
}
fn default_medium_threshold() -> f64 {
    0.3
}
fn default_high_threshold() -> f64 {
    0.6
}
fn default_critical_threshold() -> f64 {
    0.8
}
fn default_audit_capacity() -> usize {
    1000
}

fn default_blocked_commands() -> Vec<String> {
    vec![
        "rm -rf /".into(),
        "rm -rf /*".into(),
        "rm -rf ~".into(),
        "del /f".into(),
        "format:".into(),
        "mkfs".into(),
        "dd if=/dev/zero".into(),
        "dd if=/dev/random".into(),
        ":(){ :|:& };:".into(), // fork bomb
        "chmod -R 777 /".into(),
        "chown -R".into(),
        "> /dev/sda".into(),
    ]
}

fn default_risk_patterns() -> Vec<RiskPattern> {
    vec![
        // --- Critical (≥0.8) ---
        RiskPattern::new(
            "rm_recursive_root",
            r"(?i)\brm\s+(-[a-zA-Z]*f[a-zA-Z]*\s+)?(/[^\s]*|\*|~)",
            0.95,
        )
        .with_reason("Recursive deletion of root/home/wildcard"),
        RiskPattern::new("format_disk", r"(?i)\b(mkfs|format)\b", 0.9)
            .with_reason("Disk formatting"),
        RiskPattern::new("dd_disk", r"(?i)\bdd\s+if=", 0.9).with_reason("Raw disk write with dd"),
        RiskPattern::new("fork_bomb", r":\(\)\{\s*:\|:&\s*\};:", 0.95)
            .with_reason("Fork bomb detected"),
        RiskPattern::new(
            "chmod_777_recursive",
            r"(?i)\bchmod\s+(-R\s+)?777\s+/",
            0.85,
        )
        .with_reason("Recursive 777 permission change on root"),
        // --- High (≥0.6) ---
        RiskPattern::new(
            "force_delete",
            r"(?i)\brm\s+(-[a-zA-Z]*f[a-zA-Z]*|-r[a-zA-Z]*\s)",
            0.7,
        )
        .with_reason("Force/recursive delete"),
        RiskPattern::new(
            "overwrite_device",
            r"(?i)>\s*/dev/(sd|hd|nvme|vd|loop)",
            0.85,
        )
        .with_reason("Writing directly to block device"),
        RiskPattern::new("kill_all", r"(?i)\bkillall?\s+(-9\s+)?(\*|\d+)", 0.65)
            .with_reason("Kill all processes"),
        RiskPattern::new("iptables_flush", r"(?i)\biptables\s+-F", 0.7)
            .with_reason("Flushing all firewall rules"),
        RiskPattern::new("git_reset_hard", r"(?i)\bgit\s+reset\s+--hard", 0.65)
            .with_reason("Hard git reset (uncommitted changes lost)"),
        // --- Medium (≥0.3) ---
        RiskPattern::new("sudo", r"(?i)\bsudo\b", 0.5).with_reason("Elevated privileges via sudo"),
        RiskPattern::new("su_switch", r"(?i)\bsu\s+(-|\w)", 0.55).with_reason("Switching user"),
        RiskPattern::new("network_download", r"(?i)\b(curl|wget)\s+", 0.35)
            .with_reason("Network download"),
        RiskPattern::new(
            "network_upload",
            r"(?i)\b(curl|wget|scp|rsync)\b.*(-T|--upload-file)",
            0.5,
        )
        .with_reason("Network upload"),
        RiskPattern::new("pip_install", r"(?i)\bpip\s+install\b", 0.35)
            .with_reason("Installing Python packages"),
        RiskPattern::new("npm_global", r"(?i)\bnpm\s+install\s+-g\b", 0.35)
            .with_reason("Global npm install"),
        RiskPattern::new("move_rename", r"(?i)\bmv\s+", 0.3).with_reason("Moving/renaming files"),
        RiskPattern::new("delete_file", r"(?i)\b(rm\s+|del\s+)", 0.4).with_reason("Deleting files"),
        RiskPattern::new("git_clean", r"(?i)\bgit\s+clean\s+", 0.45)
            .with_reason("Git clean removes untracked files"),
        RiskPattern::new("docker_rm", r"(?i)\bdocker\s+(rm|rmi)\b", 0.35)
            .with_reason("Removing docker containers/images"),
        // --- Low (<0.3) ---
        RiskPattern::new(
            "read_only",
            r"(?i)\b(ls|cat|head|tail|less|more|find|grep|wc|file|stat)\b",
            0.05,
        )
        .with_reason("Read-only command"),
        RiskPattern::new(
            "git_read",
            r"(?i)\bgit\s+(log|status|diff|show|branch|tag)\b",
            0.05,
        )
        .with_reason("Read-only git command"),
    ]
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            require_confirmation: true,
            auto_approve_threshold: 0.3,
            medium_threshold: 0.3,
            high_threshold: 0.6,
            critical_threshold: 0.8,
            blocked_commands: default_blocked_commands(),
            risk_patterns: default_risk_patterns(),
            allowed_commands: vec![],
            sandbox_paths: vec![],
            allowed_domains: vec![],
            block_ip_urls: false,
            rate_limits: HashMap::new(),
            audit_logging: true,
            audit_capacity: 1000,
        }
    }
}

// ---------------------------------------------------------------------------
// Risk Pattern (regex-based)
// ---------------------------------------------------------------------------

/// A regex pattern that maps to a risk score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskPattern {
    pub name: String,
    pub pattern: String,
    pub score: f64,
    #[serde(default)]
    pub reason: String,
    #[serde(skip)]
    compiled: Option<Regex>,
}

impl RiskPattern {
    pub fn new(name: impl Into<String>, pattern: &str, score: f64) -> Self {
        Self {
            name: name.into(),
            pattern: pattern.to_string(),
            score,
            reason: String::new(),
            compiled: None,
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = reason.into();
        self
    }

    /// Lazy-compile and match. Returns true if the pattern matches.
    pub fn matches(&mut self, text: &str) -> bool {
        if self.compiled.is_none() {
            self.compiled = Regex::new(&self.pattern).ok();
        }
        self.compiled
            .as_ref()
            .map(|re| re.is_match(text))
            .unwrap_or(false)
    }
}
