use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::skills::discovery::DiscoveryConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub safety: SafetyConfig,
    #[serde(default)]
    pub output: OutputConfig,
    #[serde(default)]
    pub mcp_servers: std::collections::HashMap<String, crate::mcp::types::McpServerConfig>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Agent loop knobs (auto-compact, summarizer model). Optional; when
    /// absent the orchestrator's compiled-in defaults apply.
    #[serde(default)]
    pub agent: AgentLoopConfig,
    /// Skill system configuration.
    #[serde(default)]
    pub skills: SkillsConfig,
}

/// Cache configuration for prompt caching support.
///
/// Provider-level settings for prompt caching. Supported providers:
/// - Anthropic: manual `cache_control` breakpoints
/// - OpenAI: automatic server-side caching (no-op)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Enable prompt caching for this provider.
    #[serde(default = "default_cache_enabled")]
    pub enabled: bool,
    /// Breakpoint strategy for cache control markers.
    #[serde(default)]
    pub breakpoint_strategy: BreakpointStrategy,
}

fn default_cache_enabled() -> bool {
    false
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            breakpoint_strategy: BreakpointStrategy::SystemOnly,
        }
    }
}

/// Where cache-control breakpoints are inserted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BreakpointStrategy {
    /// Only cache the system prompt.
    #[default]
    SystemOnly,
    /// Cache system prompt + conversation prefix (up to last pinned/memory message).
    Prefix,
    /// Cache everything including the latest turn (aggressive — use with caution).
    Full,
}

/// Skill system configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillsConfig {
    /// Skill names to exclude from the index.
    #[serde(default)]
    pub blocklist: Vec<String>,
    /// Extra directories to scan for skills (e.g. `~/.claude/skills`).
    /// Paths support `~` and `$ENV_VAR` expansion.
    #[serde(default)]
    pub compat_dirs: Vec<String>,
}

impl From<&SkillsConfig> for DiscoveryConfig {
    fn from(cfg: &SkillsConfig) -> Self {
        DiscoveryConfig {
            blocklist: cfg.blocklist.clone(),
            compat_dirs: cfg.compat_dirs.clone(),
        }
    }
}

/// Agent-loop configuration: compaction strategy + summarizer model.
///
/// All fields are optional. When `auto_compact_with_llm` is true the
/// orchestrator asks the provider to summarize older messages instead of
/// using the heuristic string-truncation path. `summarizer_model`
/// overrides the model used for that call (recommended: a cheaper/faster
/// model than the main one).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentLoopConfig {
    #[serde(default)]
    pub auto_compact_with_llm: bool,
    #[serde(default)]
    pub summarizer_model: Option<String>,
    /// Hard cap on agent loop iterations. Prevents runaway tool-call loops.
    /// Default is 30 (the orchestrator's compiled-in default).
    #[serde(default)]
    pub max_iterations: Option<u32>,
    /// Planner agent configuration.
    #[serde(default)]
    pub planner: PlannerLoopConfig,
}

/// Planner agent configuration.
///
/// Controls plan generation and execution limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannerLoopConfig {
    /// Maximum number of steps allowed in a single plan.
    #[serde(default = "default_planner_max_steps")]
    pub max_steps: usize,
    /// Maximum number of re-plan attempts on failure.
    #[serde(default = "default_planner_max_replan")]
    pub max_replan_attempts: usize,
    /// Whether plans require user approval before execution.
    #[serde(default = "default_true")]
    pub require_approval: bool,
    /// Model override for planning LLM calls (cheaper/faster model).
    #[serde(default)]
    pub model: Option<String>,
    /// Provider name override for planning LLM calls.
    #[serde(default)]
    pub provider: Option<String>,
}

fn default_planner_max_steps() -> usize {
    20
}
fn default_planner_max_replan() -> usize {
    3
}

impl Default for PlannerLoopConfig {
    fn default() -> Self {
        Self {
            max_steps: 20,
            max_replan_attempts: 3,
            require_approval: true,
            model: None,
            provider: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    pub api_base: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
    /// Prompt cache configuration for this provider.
    #[serde(default)]
    pub cache: CacheConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub model: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Override the built-in capability lookup for this model. Useful
    /// for self-hosted endpoints (e.g. an OpenAI-compatible server
    /// proxying a vision-enabled local model). When `None`, the
    /// runtime falls back to `capabilities::resolve(&model)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<crate::capabilities::ModelCapabilities>,
}

impl ModelConfig {
    /// Effective capabilities: the explicit `capabilities` override
    /// when set, otherwise the built-in lookup.
    pub fn effective_capabilities(&self) -> crate::capabilities::ModelCapabilities {
        self.capabilities
            .unwrap_or_else(|| crate::capabilities::resolve(&self.model))
    }
}

fn default_temperature() -> f32 {
    0.7
}

fn default_max_tokens() -> u32 {
    8192
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyConfig {
    #[serde(default = "default_auto_approve")]
    pub auto_approve_low_risk: bool,
    #[serde(default)]
    pub blocked_commands: Vec<String>,
    /// Domain allowlist for URL-taking tools (`fetch`, `web_search`).
    /// Empty = no restriction. See `takka_core::safety::UrlPolicy`.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// When the URL allowlist is in effect, also reject URLs that
    /// resolve to an IP literal. Defaults to `false`.
    #[serde(default)]
    pub block_ip_urls: bool,
}

fn default_auto_approve() -> bool {
    true
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            auto_approve_low_risk: true,
            blocked_commands: vec![
                "rm -rf /".to_string(),
                "mkfs".to_string(),
                "dd if=".to_string(),
            ],
            allowed_domains: vec![],
            block_ip_urls: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfig {
    #[serde(default = "default_true")]
    pub color: bool,
    #[serde(default = "default_true")]
    pub stream: bool,
    #[serde(default = "default_true")]
    pub show_thoughts: bool,
    #[serde(default = "default_true")]
    pub show_tool_calls: bool,
}

fn default_true() -> bool {
    true
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            color: true,
            stream: true,
            show_thoughts: true,
            show_tool_calls: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyCliConfig {
    provider: LegacyProviderConfig,
    model: LegacyModelConfig,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: String,
    pub base_url: String,
    pub api_key: String,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyModelConfig {
    pub id: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacySingleProvider {
    provider_type: String,
    api_base: String,
    api_key: String,
    model: String,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    #[allow(dead_code)]
    tools_enabled: Option<Vec<String>>,
}

impl Config {
    /// Build a `takka_core::safety::UrlPolicy` from the user-facing
    /// safety config. Returns the unrestricted default when neither
    /// `allowed_domains` nor `block_ip_urls` is set.
    pub fn url_policy(&self) -> super::safety::UrlPolicy {
        super::safety::UrlPolicy::new(
            self.safety.allowed_domains.clone(),
            self.safety.block_ip_urls,
        )
    }

    pub fn config_path() -> PathBuf {
        let home = if cfg!(windows) {
            std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
        } else {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
        };

        home.join(".config").join("agentic").join("config.json")
    }

    /// Check if config file exists
    pub fn config_exists() -> bool {
        Self::config_path().exists()
    }

    /// Load config from default path
    pub fn load() -> Option<Self> {
        Self::load_from(Self::config_path())
    }

    /// Load config from custom path
    pub fn load_from_path(path: &str) -> Option<Self> {
        Self::load_from(PathBuf::from(path))
    }

    /// Load config from PathBuf
    pub fn load_from(path: PathBuf) -> Option<Self> {
        log::info!("Loading config from: {}", path.display());

        if !path.exists() {
            log::info!("Config file not found at: {}", path.display());
            return None;
        }

        let content = std::fs::read_to_string(&path).ok()?;

        let mut config = Self::try_parse_unified(&content)
            .or_else(|| Self::try_parse_legacy_cli(&content))
            .or_else(|| Self::try_parse_legacy_single(&content))?;

        config.apply_env_substitution();

        log::info!("Loaded config with {} provider(s)", config.providers.len());
        Some(config)
    }

    fn try_parse_unified(content: &str) -> Option<Self> {
        serde_json::from_str::<Config>(content).ok()
    }

    fn try_parse_legacy_cli(content: &str) -> Option<Self> {
        let legacy: LegacyCliConfig = serde_json::from_str(content).ok()?;

        Some(Config {
            providers: vec![ProviderConfig {
                name: "default".to_string(),
                provider_type: legacy.provider.provider_type,
                api_base: legacy.provider.base_url,
                api_key: legacy.provider.api_key,
                models: vec![ModelConfig {
                    model: legacy.model.id,
                    display_name: None,
                    temperature: legacy.model.temperature.unwrap_or(0.7),
                    max_tokens: legacy.model.max_tokens.unwrap_or(8192),
                    capabilities: None,
                }],
                cache: CacheConfig::default(),
            }],
            safety: SafetyConfig::default(),
            output: OutputConfig::default(),
            mcp_servers: std::collections::HashMap::new(),
            system_prompt: None,
            agent: AgentLoopConfig::default(),
            skills: SkillsConfig::default(),
        })
    }

    fn try_parse_legacy_single(content: &str) -> Option<Self> {
        let legacy: LegacySingleProvider = serde_json::from_str(content).ok()?;

        Some(Config {
            providers: vec![ProviderConfig {
                name: "default".to_string(),
                provider_type: legacy.provider_type,
                api_base: legacy.api_base,
                api_key: legacy.api_key,
                models: vec![ModelConfig {
                    model: legacy.model,
                    display_name: None,
                    temperature: legacy.temperature.unwrap_or(0.7),
                    max_tokens: legacy.max_tokens.unwrap_or(8192),
                    capabilities: None,
                }],
                cache: CacheConfig::default(),
            }],
            safety: SafetyConfig::default(),
            output: OutputConfig::default(),
            mcp_servers: std::collections::HashMap::new(),
            system_prompt: None,
            agent: AgentLoopConfig::default(),
            skills: SkillsConfig::default(),
        })
    }

    fn apply_env_substitution(&mut self) {
        for provider in &mut self.providers {
            if provider.api_key.starts_with('$') {
                let var = &provider.api_key[1..];
                if let Ok(value) = std::env::var(var) {
                    provider.api_key = value;
                }
            }
            if provider.api_base.starts_with('$') {
                let var = &provider.api_base[1..];
                if let Ok(value) = std::env::var(var) {
                    provider.api_base = value;
                }
            }
        }
    }

    /// Save config to default path
    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config directory: {}", e))?;
        }
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config: {}", e))?;
        std::fs::write(&path, content).map_err(|e| format!("Failed to write config file: {}", e))
    }

    /// Create default config file (overwrites if exists)
    pub fn create_default() -> Result<Self, String> {
        let config = Self::fallback();
        config.save()?;
        Ok(config)
    }

    /// Get fallback/default config
    pub fn fallback() -> Self {
        Self {
            providers: vec![ProviderConfig {
                name: "openai".to_string(),
                provider_type: "openai-compatible".to_string(),
                api_base: std::env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
                api_key: std::env::var("OPENAI_API_KEY").unwrap_or_default(),
                models: vec![ModelConfig {
                    model: "gpt-4o".to_string(),
                    display_name: Some("GPT-4o".to_string()),
                    temperature: 0.7,
                    max_tokens: 8192,
                    capabilities: None,
                }],
                cache: CacheConfig::default(),
            }],
            safety: SafetyConfig::default(),
            output: OutputConfig::default(),
            mcp_servers: std::collections::HashMap::new(),
            system_prompt: None,
            agent: AgentLoopConfig::default(),
            skills: SkillsConfig::default(),
        }
    }

    /// Check if config is valid (has required fields)
    pub fn is_valid(&self) -> (bool, Vec<String>) {
        let mut errors = Vec::new();

        if self.providers.is_empty() {
            errors.push("No providers configured".to_string());
        }

        for (i, provider) in self.providers.iter().enumerate() {
            if provider.name.is_empty() {
                errors.push(format!("Provider #{}: name is empty", i + 1));
            }

            if provider.provider_type.is_empty() {
                errors.push(format!("Provider #{}: type is empty", i + 1));
            }

            if provider.api_base.is_empty() {
                errors.push(format!("Provider #{}: API base URL is empty", i + 1));
            }

            if provider.models.is_empty() {
                errors.push(format!("Provider #{}: No models configured", i + 1));
            }
        }

        (errors.is_empty(), errors)
    }

    pub fn active_provider(&self) -> Option<&ProviderConfig> {
        self.providers.first()
    }

    pub fn active_model(&self) -> Option<&ModelConfig> {
        self.providers.first().and_then(|p| p.models.first())
    }

    pub fn to_output_mapping(&self) -> Vec<ModelOutput> {
        self.providers
            .iter()
            .flat_map(|provider| {
                provider.models.iter().map(|model| {
                    let display = model
                        .display_name
                        .clone()
                        .unwrap_or_else(|| normalize_model_name(&model.model));
                    ModelOutput {
                        name: format!("{} ({})", display, provider.name),
                        model: model.model.clone(),
                    }
                })
            })
            .collect()
    }

    pub fn to_provider_config(&self) -> Option<super::providers::OpenAIProviderConfig> {
        let provider = self.providers.first()?;
        let model = provider.models.first()?;

        Some(super::providers::OpenAIProviderConfig::new(
            &provider.name,
            &provider.api_base,
            &provider.api_key,
            &model.model,
        ))
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::load().unwrap_or_else(Self::fallback)
    }
}

fn normalize_model_name(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOutput {
    pub name: String,
    pub model: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_defaults_when_omitted() {
        // Older configs without the `agent` block must still parse.
        let json = r#"{
            "providers": [{
                "name": "p",
                "type": "openai-compatible",
                "api_base": "https://x",
                "api_key": "k",
                "models": [{"model": "m"}]
            }]
        }"#;
        let cfg: Config = serde_json::from_str(json).expect("parse");
        assert!(!cfg.agent.auto_compact_with_llm);
        assert!(cfg.agent.summarizer_model.is_none());

        // Planner config should have defaults when omitted.
        assert_eq!(cfg.agent.planner.max_steps, 20);
        assert_eq!(cfg.agent.planner.max_replan_attempts, 3);
        assert!(cfg.agent.planner.require_approval);
        assert!(cfg.agent.planner.model.is_none());
        assert!(cfg.agent.planner.provider.is_none());
    }

    #[test]
    fn agent_config_round_trip() {
        let json = r#"{
            "providers": [{
                "name": "p",
                "type": "openai-compatible",
                "api_base": "https://x",
                "api_key": "k",
                "models": [{"model": "m"}]
            }],
            "agent": {
                "auto_compact_with_llm": true,
                "summarizer_model": "gpt-4o-mini"
            }
        }"#;
        let cfg: Config = serde_json::from_str(json).expect("parse");
        assert!(cfg.agent.auto_compact_with_llm);
        assert_eq!(cfg.agent.summarizer_model.as_deref(), Some("gpt-4o-mini"));

        // Re-serialize and re-parse to confirm the field round-trips.
        let out = serde_json::to_string(&cfg).expect("serialize");
        let cfg2: Config = serde_json::from_str(&out).expect("reparse");
        assert!(cfg2.agent.auto_compact_with_llm);
        assert_eq!(cfg2.agent.summarizer_model.as_deref(), Some("gpt-4o-mini"));
    }

    #[test]
    fn planner_config_round_trip() {
        let json = r#"{
            "providers": [{
                "name": "p",
                "type": "openai-compatible",
                "api_base": "https://x",
                "api_key": "k",
                "models": [{"model": "m"}]
            }],
            "agent": {
                "planner": {
                    "max_steps": 10,
                    "max_replan_attempts": 5,
                    "require_approval": false,
                    "model": "gpt-4o-mini",
                    "provider": "openai"
                }
            }
        }"#;
        let cfg: Config = serde_json::from_str(json).expect("parse");
        assert_eq!(cfg.agent.planner.max_steps, 10);
        assert_eq!(cfg.agent.planner.max_replan_attempts, 5);
        assert!(!cfg.agent.planner.require_approval);
        assert_eq!(cfg.agent.planner.model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(cfg.agent.planner.provider.as_deref(), Some("openai"));

        // Re-serialize and re-parse.
        let out = serde_json::to_string(&cfg).expect("serialize");
        let cfg2: Config = serde_json::from_str(&out).expect("reparse");
        assert_eq!(cfg2.agent.planner.max_steps, 10);
        assert_eq!(cfg2.agent.planner.max_replan_attempts, 5);
        assert!(!cfg2.agent.planner.require_approval);
    }

    #[test]
    fn url_policy_defaults_to_unrestricted() {
        let cfg = Config::fallback();
        let policy = cfg.url_policy();
        assert!(policy.is_unrestricted());
    }

    #[test]
    fn url_policy_reads_from_safety_config() {
        let json = r#"{
            "providers": [{
                "name": "p",
                "type": "openai-compatible",
                "api_base": "https://x",
                "api_key": "k",
                "models": [{"model": "m"}]
            }],
            "safety": {
                "allowed_domains": ["docs.rs", "github.com"],
                "block_ip_urls": true
            }
        }"#;
        let cfg: Config = serde_json::from_str(json).expect("parse");
        let policy = cfg.url_policy();
        assert!(!policy.is_unrestricted());
        assert!(policy.is_allowed("https://docs.rs/x"));
        assert!(policy.is_allowed("https://api.github.com/x"));
        assert!(!policy.is_allowed("https://example.com/x"));
        assert!(!policy.is_allowed("http://192.168.1.1/x"));
    }
}
