use serde::{Deserialize, Serialize};

use crate::secrets_resolve::SecretRef;

/// Extended thinking level for supported LLM providers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    /// 1024 budget tokens (Anthropic) / low effort (OpenAI)
    Low,
    /// 4096 budget tokens (Anthropic) / medium effort (OpenAI)
    Medium,
    /// 16384 budget tokens (Anthropic) / high effort (OpenAI)
    High,
    /// 32768 budget tokens (Anthropic) / high effort (OpenAI)
    Xhigh,
}

impl ThinkingLevel {
    /// Budget tokens for Anthropic's thinking parameter. Returns `None` when thinking is off.
    pub fn budget_tokens(&self) -> Option<u32> {
        match self {
            Self::Off => None,
            Self::Low => Some(1024),
            Self::Medium => Some(4096),
            Self::High => Some(16384),
            Self::Xhigh => Some(32768),
        }
    }

    /// Reasoning effort string for OpenAI o-series models. Returns `None` when thinking is off.
    pub fn openai_reasoning_effort(&self) -> Option<&str> {
        match self {
            Self::Off => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High | Self::Xhigh => Some("high"),
        }
    }

    /// Returns true if extended thinking is active (any level other than Off).
    pub fn is_enabled(&self) -> bool {
        *self != Self::Off
    }
}

/// Primary LLM provider and model configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    /// LLM provider name (auto-detected from API keys if omitted).
    pub provider: Option<String>,
    /// Environment variable name holding the API key (legacy).
    pub api_key_env: String,
    /// Single SecretRef for API key (takes priority over api_key_env).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SecretRef>,
    /// Multiple API keys for fallback/rotation (tried in order on auth failure).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub api_keys: Vec<SecretRef>,
    /// Model identifier (e.g. "anthropic/claude-sonnet-4").
    pub model: String,
    /// Sampling temperature (0.0 = deterministic, higher = more creative).
    pub temperature: f32,
    /// Maximum tokens in the LLM response.
    pub max_tokens: u32,
    /// Maximum retry attempts on transient LLM errors.
    pub max_retries: u32,
    /// Initial delay in milliseconds before the first retry.
    pub initial_retry_delay_ms: u64,
    /// Total request timeout in milliseconds.
    pub request_timeout_ms: u64,
    /// Timeout in seconds for receiving each SSE chunk during streaming. 0 = no timeout.
    pub stream_chunk_timeout_secs: u64,
    /// Override the provider's default API URL (e.g., for Ollama on a remote host, Azure OpenAI, or proxies).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Provider-level failover chain (tried in order when primary provider fails).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback: Vec<LlmFallback>,
    /// Extended thinking level: off, low, medium, high, xhigh.
    /// Enables native thinking for supported providers (Anthropic, OpenAI o-series).
    #[serde(default)]
    pub thinking: ThinkingLevel,
    /// Path to the `claude` CLI binary (auto-detected if omitted).
    /// Only used when provider is `claude-cli`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_cli_path: Option<String>,
    /// Prompt caching configuration (currently only used by the native Anthropic provider).
    #[serde(default)]
    pub cache: PromptCacheConfig,
}

/// Prompt caching configuration.
///
/// When enabled and the provider supports it (currently: Anthropic), the LLM client
/// attaches `cache_control` markers to the system prompt and the last two non-system
/// messages, allowing the provider to reuse cached prefixes across turns.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptCacheConfig {
    /// Whether prompt caching is enabled.
    pub enabled: bool,
}

impl Default for PromptCacheConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: None,
            api_key_env: "OPENROUTER_API_KEY".into(),
            api_key: None,
            api_keys: Vec::new(),
            model: "anthropic/claude-sonnet-4".into(),
            temperature: 0.7,
            max_tokens: 4096,
            max_retries: 3,
            initial_retry_delay_ms: 200,
            request_timeout_ms: 60000,
            stream_chunk_timeout_secs: 30,
            base_url: None,
            fallback: Vec::new(),
            thinking: ThinkingLevel::Off,
            claude_cli_path: None,
            cache: PromptCacheConfig::default(),
        }
    }
}

/// A fallback provider configuration for provider-level failover.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmFallback {
    /// Fallback provider name.
    pub provider: String,
    /// Fallback model identifier.
    pub model: String,
    /// Optional API key secret for this fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SecretRef>,
    /// Optional API key env var name for this fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Multiple API keys for rotation on this fallback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api_keys: Vec<SecretRef>,
    /// Override temperature for this fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Override max tokens for this fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Override base URL for this fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// Configuration for the compaction model — use a different (cheaper/faster)
/// model for context compaction than the primary conversation model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    /// Override provider for compaction (e.g., "openrouter", "anthropic").
    /// If omitted, uses the primary LLM provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Override model for compaction (e.g., "anthropic/claude-haiku-4-5").
    /// If omitted, uses the primary LLM model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Override API key env var for compaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Override temperature for compaction (default: primary LLM temperature).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Override max tokens for compaction (default: primary LLM max_tokens).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Timeout for compaction requests in ms (default: primary LLM timeout).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl CompactionConfig {
    /// Returns true if any compaction override is configured.
    pub fn has_overrides(&self) -> bool {
        self.provider.is_some()
            || self.model.is_some()
            || self.api_key_env.is_some()
            || self.temperature.is_some()
            || self.max_tokens.is_some()
            || self.timeout_ms.is_some()
    }
}

/// Configuration for the proactive heartbeat check-in system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HeartbeatConfig {
    /// Check-in interval (e.g. "30m", "1h").
    pub interval: String,
    /// Start of quiet hours during which heartbeats are suppressed (HH:MM).
    pub quiet_hours_start: Option<String>,
    /// End of quiet hours (HH:MM).
    pub quiet_hours_end: Option<String>,
    /// Optional cron expression that overrides the interval.
    pub cron: Option<String>,
    /// Channel names to deliver heartbeat messages to.
    pub channels: Vec<String>,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval: "30m".into(),
            quiet_hours_start: Some("00:00".into()),
            quiet_hours_end: Some("06:00".into()),
            cron: None,
            channels: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ThinkingLevel ──

    #[test]
    fn thinking_level_default_is_off() {
        assert_eq!(ThinkingLevel::default(), ThinkingLevel::Off);
    }

    #[test]
    fn thinking_level_budget_tokens() {
        assert_eq!(ThinkingLevel::Off.budget_tokens(), None);
        assert_eq!(ThinkingLevel::Low.budget_tokens(), Some(1024));
        assert_eq!(ThinkingLevel::Medium.budget_tokens(), Some(4096));
        assert_eq!(ThinkingLevel::High.budget_tokens(), Some(16384));
        assert_eq!(ThinkingLevel::Xhigh.budget_tokens(), Some(32768));
    }

    #[test]
    fn thinking_level_openai_reasoning_effort() {
        assert_eq!(ThinkingLevel::Off.openai_reasoning_effort(), None);
        assert_eq!(ThinkingLevel::Low.openai_reasoning_effort(), Some("low"));
        assert_eq!(
            ThinkingLevel::Medium.openai_reasoning_effort(),
            Some("medium")
        );
        assert_eq!(ThinkingLevel::High.openai_reasoning_effort(), Some("high"));
        assert_eq!(ThinkingLevel::Xhigh.openai_reasoning_effort(), Some("high"));
    }

    #[test]
    fn thinking_level_is_enabled() {
        assert!(!ThinkingLevel::Off.is_enabled());
        assert!(ThinkingLevel::Low.is_enabled());
        assert!(ThinkingLevel::Medium.is_enabled());
        assert!(ThinkingLevel::High.is_enabled());
        assert!(ThinkingLevel::Xhigh.is_enabled());
    }

    // ── LlmConfig defaults ──

    #[test]
    fn llm_config_defaults() {
        let cfg = LlmConfig::default();
        assert_eq!(cfg.provider, None);
        assert_eq!(cfg.api_key_env, "OPENROUTER_API_KEY");
        assert_eq!(cfg.model, "anthropic/claude-sonnet-4");
        assert!((cfg.temperature - 0.7).abs() < f32::EPSILON);
        assert_eq!(cfg.max_tokens, 4096);
        assert_eq!(cfg.max_retries, 3);
        assert_eq!(cfg.initial_retry_delay_ms, 200);
        assert_eq!(cfg.request_timeout_ms, 60000);
        assert_eq!(cfg.stream_chunk_timeout_secs, 30);
        assert!(cfg.base_url.is_none());
        assert!(cfg.fallback.is_empty());
        assert_eq!(cfg.thinking, ThinkingLevel::Off);
    }

    // ── LlmConfig deserialization ──

    #[test]
    fn llm_config_from_toml_minimal() {
        let toml_str = r#"
            model = "gpt-4"
            api_key_env = "OPENAI_API_KEY"
        "#;
        let cfg: LlmConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.model, "gpt-4");
        assert_eq!(cfg.api_key_env, "OPENAI_API_KEY");
        // defaults apply for unset fields
        assert_eq!(cfg.max_tokens, 4096);
    }

    #[test]
    fn llm_config_from_toml_with_thinking() {
        let toml_str = r#"
            model = "claude-sonnet-4"
            api_key_env = "ANTHROPIC_API_KEY"
            thinking = "high"
        "#;
        let cfg: LlmConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.thinking, ThinkingLevel::High);
    }

    // ── CompactionConfig ──

    #[test]
    fn compaction_config_no_overrides_by_default() {
        let cfg = CompactionConfig::default();
        assert!(!cfg.has_overrides());
    }

    #[test]
    fn compaction_config_detects_overrides() {
        let mut cfg = CompactionConfig::default();
        cfg.model = Some("anthropic/claude-haiku-4-5".into());
        assert!(cfg.has_overrides());
    }

    // ── HeartbeatConfig defaults ──

    #[test]
    fn heartbeat_config_defaults() {
        let cfg = HeartbeatConfig::default();
        assert_eq!(cfg.interval, "30m");
        assert_eq!(cfg.quiet_hours_start.as_deref(), Some("00:00"));
        assert_eq!(cfg.quiet_hours_end.as_deref(), Some("06:00"));
        assert!(cfg.cron.is_none());
        assert!(cfg.channels.is_empty());
    }

    #[test]
    fn heartbeat_config_from_toml() {
        let toml_str = r#"
            interval = "1h"
            channels = ["telegram", "slack"]
        "#;
        let cfg: HeartbeatConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.interval, "1h");
        assert_eq!(cfg.channels, vec!["telegram", "slack"]);
    }
}
