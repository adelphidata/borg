use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use anyhow::Result;

use crate::constants;
use crate::pairing::DmPolicy;

use super::llm::{LlmFallback, ThinkingLevel};

/// Configuration for the webhook gateway server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    /// Bind address for the gateway HTTP server.
    pub host: String,
    /// Port for the gateway HTTP server.
    pub port: u16,
    /// Maximum concurrent webhook handlers.
    pub max_concurrent: usize,
    /// Hard wall-clock ceiling in milliseconds for an entire agent turn.
    /// In practice the inactivity timer (`inactivity_timeout_secs`) is the
    /// effective limit; this only fires if the inactivity guard is disabled
    /// or the agent loop misbehaves.
    pub request_timeout_ms: u64,
    /// Inactivity timeout in seconds. The agent turn is cancelled if no
    /// progress event (stream token, tool call, tool result, etc.) arrives
    /// within this window. `0` disables. Default: 1800 (30 min). Ported
    /// from hermes-agent's `HERMES_AGENT_TIMEOUT`.
    #[serde(default = "default_inactivity_timeout_secs")]
    pub inactivity_timeout_secs: u64,
    /// Send a one-shot warning message after this many idle seconds, before
    /// the final timeout fires. `0` disables. Default: 900 (15 min).
    #[serde(default = "default_inactivity_warning_secs")]
    pub inactivity_warning_secs: u64,
    /// Send a "still working…" status message every N seconds while the
    /// agent is busy (resets on activity). `0` disables. Default: 600 (10 min).
    #[serde(default = "default_inactivity_notify_secs")]
    pub inactivity_notify_secs: u64,
    /// Maximum inbound requests per minute per sender.
    #[serde(default = "default_rate_limit")]
    pub rate_limit_per_minute: u32,
    /// Public URL for automatic webhook registration (e.g. Telegram).
    #[serde(default)]
    pub public_url: Option<String>,
    /// Maximum webhook request body size in bytes.
    pub max_body_size: usize,
    /// Telegram long-polling timeout in seconds.
    pub telegram_poll_timeout_secs: u64,
    /// Consecutive failures before Telegram circuit breaker opens.
    pub telegram_circuit_failure_threshold: u32,
    /// Seconds the Telegram circuit breaker stays open after tripping.
    pub telegram_circuit_suspension_secs: u64,
    /// Capacity of the Telegram update deduplicator.
    pub telegram_dedup_capacity: usize,
    /// Per-channel agent routing bindings (first match wins).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<GatewayBinding>,
    /// Optional allowlist of Slack channel IDs. Empty = allow all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slack_channel_allowlist: Option<Vec<String>>,
    /// Optional allowlist of Discord guild IDs. Empty = allow all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_guild_allowlist: Option<Vec<String>>,
    /// Default access policy for direct messages: pairing, open, disabled.
    #[serde(default)]
    pub dm_policy: DmPolicy,
    /// Per-channel DM policy overrides. Key = channel name.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub channel_policies: HashMap<String, DmPolicy>,
    /// Pairing code TTL in seconds (default 3600 = 60 minutes).
    #[serde(default = "default_pairing_ttl")]
    pub pairing_ttl_secs: i64,
    /// signal-cli daemon host (default "localhost").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_cli_host: Option<String>,
    /// signal-cli daemon port (default 8080).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_cli_port: Option<u16>,
    /// Default activation mode for group chats (default: Mention).
    /// DMs always activate regardless of this setting.
    #[serde(default)]
    pub group_activation: ActivationMode,
    /// Auto-reply configuration for when the agent is away.
    #[serde(default)]
    pub auto_reply: AutoReplyConfig,
    /// Link understanding: auto-extract and fetch URLs from inbound messages.
    #[serde(default)]
    pub link_understanding: LinkUnderstandingConfig,
    /// Error policy for external channels: always, once, silent.
    #[serde(default)]
    pub error_policy: ErrorPolicy,
    /// Error dedup cooldown in milliseconds (default 4 hours).
    /// When error_policy is "once", duplicate errors within this window are suppressed.
    #[serde(default = "default_error_cooldown_ms")]
    pub error_cooldown_ms: u64,
    /// Per-channel error policy overrides. Key = channel name.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub channel_error_policies: HashMap<String, ErrorPolicy>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 7842,
            max_concurrent: 10,
            request_timeout_ms: 1_800_000,
            inactivity_timeout_secs: default_inactivity_timeout_secs(),
            inactivity_warning_secs: default_inactivity_warning_secs(),
            inactivity_notify_secs: default_inactivity_notify_secs(),
            rate_limit_per_minute: default_rate_limit(),
            public_url: None,
            max_body_size: constants::GATEWAY_MAX_BODY_SIZE,
            telegram_poll_timeout_secs: constants::TELEGRAM_POLL_TIMEOUT_SECS,
            telegram_circuit_failure_threshold: constants::TELEGRAM_CIRCUIT_FAILURE_THRESHOLD,
            telegram_circuit_suspension_secs: constants::TELEGRAM_CIRCUIT_SUSPENSION_SECS,
            telegram_dedup_capacity: constants::TELEGRAM_DEDUP_CAPACITY,
            bindings: Vec::new(),
            slack_channel_allowlist: None,
            discord_guild_allowlist: None,
            dm_policy: DmPolicy::default(),
            channel_policies: HashMap::new(),
            pairing_ttl_secs: default_pairing_ttl(),
            signal_cli_host: None,
            signal_cli_port: None,
            group_activation: ActivationMode::default(),
            auto_reply: AutoReplyConfig::default(),
            link_understanding: LinkUnderstandingConfig::default(),
            error_policy: ErrorPolicy::default(),
            error_cooldown_ms: default_error_cooldown_ms(),
            channel_error_policies: HashMap::new(),
        }
    }
}

/// Route a gateway channel to specific agent configuration overrides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayBinding {
    /// Channel name or glob pattern.
    pub channel: String,
    /// Optional sender ID filter (glob pattern).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    /// Optional peer kind filter: "direct" or "group".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_kind: Option<String>,
    /// Override LLM provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Override LLM model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Override API key env var.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Override temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Override max tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Custom identity file (relative to ~/.borg/).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Scoped memory directory name (~/.borg/memory/scopes/{name}/).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_scope: Option<String>,
    /// Per-binding fallback providers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback: Vec<LlmFallback>,
    /// Activation mode override for this binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activation: Option<ActivationMode>,
    /// Extended thinking level override for this binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingLevel>,
    /// Override LLM request timeout in milliseconds.
    /// Useful for slow thinking models that need more than the default 120s.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_timeout_ms: Option<u64>,
    /// Override gateway agent-processing timeout in milliseconds.
    /// Should be >= request_timeout_ms to avoid premature cancellation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_timeout_ms: Option<u64>,
    /// Override per-SSE-chunk timeout in seconds.
    /// Increase for models with long thinking phases before first token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_chunk_timeout_secs: Option<u64>,
    /// Override gateway inactivity timeout in seconds for this binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inactivity_timeout_secs: Option<u64>,
    /// Override inactivity warning threshold in seconds for this binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inactivity_warning_secs: Option<u64>,
    /// Override "still working" notify interval in seconds for this binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inactivity_notify_secs: Option<u64>,
}

/// Activation mode for group chats.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ActivationMode {
    /// Borg responds to every message in groups.
    Always,
    /// Borg only responds when @mentioned in groups.
    #[default]
    Mention,
}

/// Error delivery policy for external channels.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ErrorPolicy {
    /// Send every error to the user (no suppression).
    Always,
    /// Send the first error, then suppress duplicates within the cooldown window.
    #[default]
    Once,
    /// Never send errors to the user.
    Silent,
}

impl FromStr for ErrorPolicy {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "always" => Ok(Self::Always),
            "once" => Ok(Self::Once),
            "silent" => Ok(Self::Silent),
            other => anyhow::bail!("Unknown error policy '{other}'. Valid: always, once, silent"),
        }
    }
}

impl fmt::Display for ErrorPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Always => write!(f, "always"),
            Self::Once => write!(f, "once"),
            Self::Silent => write!(f, "silent"),
        }
    }
}

/// Auto-reply configuration for when the agent is unavailable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoReplyConfig {
    /// Enable the auto-reply subsystem.
    pub enabled: bool,
    /// Default message sent when the agent is in "away" mode.
    pub away_message: String,
    /// Whether to queue inbound messages during away mode for later processing.
    pub queue_messages: bool,
}

impl Default for AutoReplyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            away_message: "I'm currently away and will respond when I'm back.".into(),
            queue_messages: true,
        }
    }
}

/// Link understanding: auto-extract URLs from inbound messages and inject content.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkUnderstandingConfig {
    /// Enable automatic link content extraction.
    pub enabled: bool,
    /// Max number of links to fetch per message.
    pub max_links: usize,
    /// Max characters to extract per link.
    pub max_chars_per_link: usize,
    /// HTTP timeout in milliseconds for link fetching.
    pub timeout_ms: u64,
}

impl Default for LinkUnderstandingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_links: 3,
            max_chars_per_link: constants::LINK_UNDERSTANDING_MAX_CHARS,
            timeout_ms: 10_000,
        }
    }
}

fn default_inactivity_timeout_secs() -> u64 {
    1_800
}

fn default_inactivity_warning_secs() -> u64 {
    900
}

fn default_inactivity_notify_secs() -> u64 {
    600
}

fn default_error_cooldown_ms() -> u64 {
    constants::ERROR_POLICY_COOLDOWN_MS
}

fn default_rate_limit() -> u32 {
    constants::GATEWAY_RATE_LIMIT_PER_MINUTE_DEFAULT
}

fn default_pairing_ttl() -> i64 {
    constants::PAIRING_CODE_TTL_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gateway_config_default() {
        let cfg = GatewayConfig::default();
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 7842);
        assert_eq!(cfg.max_concurrent, 10);
        assert_eq!(cfg.request_timeout_ms, 1_800_000);
        assert_eq!(cfg.inactivity_timeout_secs, 1_800);
        assert_eq!(cfg.inactivity_warning_secs, 900);
        assert_eq!(cfg.inactivity_notify_secs, 600);
        assert_eq!(cfg.rate_limit_per_minute, 60);
        assert!(cfg.public_url.is_none());
        assert!(cfg.bindings.is_empty());
        assert!(cfg.slack_channel_allowlist.is_none());
        assert!(cfg.discord_guild_allowlist.is_none());
        assert_eq!(cfg.pairing_ttl_secs, 3600);
        assert!(cfg.signal_cli_host.is_none());
        assert!(cfg.signal_cli_port.is_none());
        assert_eq!(cfg.group_activation, ActivationMode::Mention);
        assert_eq!(cfg.error_policy, ErrorPolicy::Once);
        assert_eq!(cfg.error_cooldown_ms, 14_400_000);
        assert!(cfg.channel_error_policies.is_empty());
        assert!(cfg.channel_policies.is_empty());
    }

    #[test]
    fn test_error_policy_from_str() {
        assert_eq!(
            "always".parse::<ErrorPolicy>().unwrap(),
            ErrorPolicy::Always
        );
        assert_eq!("once".parse::<ErrorPolicy>().unwrap(), ErrorPolicy::Once);
        assert_eq!(
            "silent".parse::<ErrorPolicy>().unwrap(),
            ErrorPolicy::Silent
        );
        assert_eq!(
            "ALWAYS".parse::<ErrorPolicy>().unwrap(),
            ErrorPolicy::Always
        );
        assert_eq!("Once".parse::<ErrorPolicy>().unwrap(), ErrorPolicy::Once);
        assert!("unknown".parse::<ErrorPolicy>().is_err());
    }

    #[test]
    fn test_error_policy_display() {
        assert_eq!(ErrorPolicy::Always.to_string(), "always");
        assert_eq!(ErrorPolicy::Once.to_string(), "once");
        assert_eq!(ErrorPolicy::Silent.to_string(), "silent");
    }

    #[test]
    fn test_error_policy_roundtrip() {
        for policy in [ErrorPolicy::Always, ErrorPolicy::Once, ErrorPolicy::Silent] {
            let s = policy.to_string();
            let parsed: ErrorPolicy = s.parse().unwrap();
            assert_eq!(parsed, policy);
        }
    }

    #[test]
    fn test_activation_mode_serde_roundtrip() {
        for mode in [ActivationMode::Always, ActivationMode::Mention] {
            let json = serde_json::to_string(&mode).unwrap();
            let parsed: ActivationMode = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, mode);
        }
    }

    #[test]
    fn test_gateway_binding_serde() {
        let binding = GatewayBinding {
            channel: "telegram".into(),
            sender: Some("user123".into()),
            peer_kind: Some("direct".into()),
            provider: Some("openai".into()),
            model: Some("gpt-4".into()),
            api_key_env: None,
            temperature: Some(0.7),
            max_tokens: Some(4096),
            identity: None,
            memory_scope: Some("private".into()),
            fallback: Vec::new(),
            activation: Some(ActivationMode::Always),
            thinking: None,
            request_timeout_ms: None,
            gateway_timeout_ms: None,
            stream_chunk_timeout_secs: None,
            inactivity_timeout_secs: None,
            inactivity_warning_secs: None,
            inactivity_notify_secs: None,
        };
        let json = serde_json::to_string(&binding).unwrap();
        let parsed: GatewayBinding = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.channel, "telegram");
        assert_eq!(parsed.sender.as_deref(), Some("user123"));
        assert_eq!(parsed.provider.as_deref(), Some("openai"));
        assert_eq!(parsed.temperature, Some(0.7));
        assert_eq!(parsed.max_tokens, Some(4096));
        assert_eq!(parsed.memory_scope.as_deref(), Some("private"));
        assert_eq!(parsed.activation, Some(ActivationMode::Always));
        // Fields set to None with skip_serializing_if should be absent
        assert!(!json.contains("\"api_key_env\""));
        assert!(!json.contains("\"identity\""));
        assert!(!json.contains("\"thinking\""));
    }

    #[test]
    fn test_auto_reply_config_default() {
        let cfg = AutoReplyConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.away_message.contains("away"));
        assert!(cfg.queue_messages);
    }

    #[test]
    fn test_link_understanding_config_default() {
        let cfg = LinkUnderstandingConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.max_links, 3);
        assert_eq!(cfg.max_chars_per_link, 5000);
        assert_eq!(cfg.timeout_ms, 10_000);
    }

    #[test]
    fn test_gateway_config_serde_roundtrip() {
        let original = GatewayConfig::default();
        let json = serde_json::to_string(&original).unwrap();
        let parsed: GatewayConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.host, original.host);
        assert_eq!(parsed.port, original.port);
        assert_eq!(parsed.max_concurrent, original.max_concurrent);
        assert_eq!(parsed.request_timeout_ms, original.request_timeout_ms);
        assert_eq!(parsed.rate_limit_per_minute, original.rate_limit_per_minute);
        assert_eq!(parsed.pairing_ttl_secs, original.pairing_ttl_secs);
        assert_eq!(parsed.error_policy, original.error_policy);
        assert_eq!(parsed.error_cooldown_ms, original.error_cooldown_ms);
    }
}
