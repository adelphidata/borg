//! Settings resolution layer: DB → compiled defaults.

use anyhow::{Context, Result};

use crate::config::Config;
use crate::db::Database;

/// Where a setting's effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingSource {
    Default,
    Database,
}

impl std::fmt::Display for SettingSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingSource::Default => write!(f, "default"),
            SettingSource::Database => write!(f, "db"),
        }
    }
}

/// One entry in the full settings listing.
#[derive(Debug, Clone)]
pub struct SettingInfo {
    pub key: String,
    pub value: String,
    pub source: SettingSource,
}

pub type SettingExtractor = fn(&Config) -> String;

/// Single source of truth for setting keys and their config extractors.
///
/// Each entry is `(key, extractor_fn)`. `ALL_SETTING_KEYS` and `config_value_for_key()`
/// are both derived from this table.
pub const SETTING_REGISTRY: &[(&str, SettingExtractor)] = &[
    ("model", |c| c.llm.model.clone()),
    ("temperature", |c| format!("{}", c.llm.temperature)),
    ("max_tokens", |c| format!("{}", c.llm.max_tokens)),
    ("provider", |c| {
        c.llm
            .provider
            .as_deref()
            .unwrap_or("(auto-detect)")
            .to_string()
    }),
    ("sandbox.mode", |c| c.sandbox.mode.clone()),
    ("sandbox.enabled", |c| format!("{}", c.sandbox.enabled)),
    ("memory.max_context_tokens", |c| {
        format!("{}", c.memory.max_context_tokens)
    }),
    ("memory.flush_before_compaction", |c| {
        format!("{}", c.memory.flush_before_compaction)
    }),
    ("memory.flush_min_messages", |c| {
        format!("{}", c.memory.flush_min_messages)
    }),
    ("memory.extra_paths", |c| c.memory.extra_paths.join(", ")),
    ("memory.embeddings.mmr_enabled", |c| {
        format!("{}", c.memory.embeddings.mmr_enabled)
    }),
    ("memory.embeddings.mmr_lambda", |c| {
        format!("{}", c.memory.embeddings.mmr_lambda)
    }),
    ("skills.enabled", |c| format!("{}", c.skills.enabled)),
    ("skills.max_context_tokens", |c| {
        format!("{}", c.skills.max_context_tokens)
    }),
    ("conversation.max_iterations", |c| {
        format!("{}", c.conversation.max_iterations)
    }),
    ("conversation.show_thinking", |c| {
        format!("{}", c.conversation.show_thinking)
    }),
    ("conversation.tool_output_max_tokens", |c| {
        format!("{}", c.conversation.tool_output_max_tokens)
    }),
    ("conversation.compaction_marker_tokens", |c| {
        format!("{}", c.conversation.compaction_marker_tokens)
    }),
    ("conversation.max_transcript_chars", |c| {
        format!("{}", c.conversation.max_transcript_chars)
    }),
    ("security.secret_detection", |c| {
        format!("{}", c.security.secret_detection)
    }),
    ("security.host_audit", |c| {
        format!("{}", c.security.host_audit)
    }),
    ("budget.monthly_token_limit", |c| {
        format!("{}", c.budget.monthly_token_limit)
    }),
    ("budget.warning_threshold", |c| {
        format!("{}", c.budget.warning_threshold)
    }),
    ("gateway.max_body_size", |c| {
        format!("{}", c.gateway.max_body_size)
    }),
    ("gateway.telegram_poll_timeout_secs", |c| {
        format!("{}", c.gateway.telegram_poll_timeout_secs)
    }),
    ("gateway.telegram_circuit_failure_threshold", |c| {
        format!("{}", c.gateway.telegram_circuit_failure_threshold)
    }),
    ("gateway.telegram_circuit_suspension_secs", |c| {
        format!("{}", c.gateway.telegram_circuit_suspension_secs)
    }),
    ("gateway.telegram_dedup_capacity", |c| {
        format!("{}", c.gateway.telegram_dedup_capacity)
    }),
    ("tts.enabled", |c| format!("{}", c.tts.enabled)),
    ("tts.auto_mode", |c| format!("{}", c.tts.auto_mode)),
    ("tts.default_voice", |c| c.tts.default_voice.clone()),
    ("tts.default_format", |c| c.tts.default_format.clone()),
    ("conversation.collaboration_mode", |c| {
        format!("{}", c.conversation.collaboration_mode)
    }),
    ("evolution.enabled", |c| format!("{}", c.evolution.enabled)),
    ("llm.claude_cli_path", |c| {
        c.llm
            .claude_cli_path
            .as_deref()
            .unwrap_or("(auto-detect)")
            .to_string()
    }),
    ("workflow.enabled", |c| c.workflow.enabled.clone()),
    // ── LLM extended ──
    ("llm.api_key_env", |c| c.llm.api_key_env.clone()),
    ("llm.api_key", |c| {
        c.llm
            .api_key
            .as_ref()
            .map(|sr| serde_json::to_string(sr).unwrap_or_default())
            .unwrap_or_default()
    }),
    ("llm.api_keys", |c| {
        serde_json::to_string(&c.llm.api_keys).unwrap_or_default()
    }),
    ("llm.max_retries", |c| format!("{}", c.llm.max_retries)),
    ("llm.initial_retry_delay_ms", |c| {
        format!("{}", c.llm.initial_retry_delay_ms)
    }),
    ("llm.request_timeout_ms", |c| {
        format!("{}", c.llm.request_timeout_ms)
    }),
    ("llm.stream_chunk_timeout_secs", |c| {
        format!("{}", c.llm.stream_chunk_timeout_secs)
    }),
    ("llm.base_url", |c| {
        c.llm.base_url.clone().unwrap_or_default()
    }),
    ("llm.thinking", |c| {
        serde_json::to_string(&c.llm.thinking)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string()
    }),
    ("llm.fallback", |c| {
        serde_json::to_string(&c.llm.fallback).unwrap_or_default()
    }),
    ("llm.cache.enabled", |c| format!("{}", c.llm.cache.enabled)),
    ("llm.cache.ttl", |c| format!("{}", c.llm.cache.ttl)),
    ("llm.cache.cache_tools", |c| {
        format!("{}", c.llm.cache.cache_tools)
    }),
    ("llm.cache.cache_system", |c| {
        format!("{}", c.llm.cache.cache_system)
    }),
    ("llm.cache.rolling_messages", |c| {
        format!("{}", c.llm.cache.rolling_messages)
    }),
    // ── Tools extended ──
    ("tools.default_timeout_ms", |c| {
        format!("{}", c.tools.default_timeout_ms)
    }),
    ("tools.conditional_loading", |c| {
        format!("{}", c.tools.conditional_loading)
    }),
    ("tools.compact_schemas", |c| {
        format!("{}", c.tools.compact_schemas)
    }),
    ("tools.policy.profile", |c| c.tools.policy.profile.clone()),
    ("tools.policy.allow", |c| {
        serde_json::to_string(&c.tools.policy.allow).unwrap_or_default()
    }),
    ("tools.policy.deny", |c| {
        serde_json::to_string(&c.tools.policy.deny).unwrap_or_default()
    }),
    ("tools.policy.subagent_deny", |c| {
        serde_json::to_string(&c.tools.policy.subagent_deny).unwrap_or_default()
    }),
    // ── Heartbeat extended ──
    ("heartbeat.interval", |c| c.heartbeat.interval.clone()),
    ("heartbeat.quiet_hours_start", |c| {
        c.heartbeat.quiet_hours_start.clone().unwrap_or_default()
    }),
    ("heartbeat.quiet_hours_end", |c| {
        c.heartbeat.quiet_hours_end.clone().unwrap_or_default()
    }),
    ("heartbeat.cron", |c| {
        c.heartbeat.cron.clone().unwrap_or_default()
    }),
    ("heartbeat.channels", |c| {
        serde_json::to_string(&c.heartbeat.channels).unwrap_or_default()
    }),
    ("heartbeat.recipients", |c| {
        serde_json::to_string(&c.heartbeat.recipients).unwrap_or_default()
    }),
    // ── Conversation extended ──
    ("conversation.max_history_tokens", |c| {
        format!("{}", c.conversation.max_history_tokens)
    }),
    ("conversation.age_based_degradation", |c| {
        format!("{}", c.conversation.age_based_degradation)
    }),
    // ── User ──
    ("user.name", |c| c.user.name.clone().unwrap_or_default()),
    ("user.agent_name", |c| {
        c.user.agent_name.clone().unwrap_or_default()
    }),
    ("user.timezone", |c| {
        c.user.timezone.clone().unwrap_or_default()
    }),
    // ── Web ──
    ("web.enabled", |c| format!("{}", c.web.enabled)),
    ("web.search_provider", |c| c.web.search_provider.clone()),
    // ── Tasks ──
    ("tasks.max_concurrent", |c| {
        format!("{}", c.tasks.max_concurrent)
    }),
    // ── Gateway extended ──
    ("gateway.host", |c| c.gateway.host.clone()),
    ("gateway.port", |c| format!("{}", c.gateway.port)),
    ("gateway.max_concurrent", |c| {
        format!("{}", c.gateway.max_concurrent)
    }),
    ("gateway.request_timeout_ms", |c| {
        format!("{}", c.gateway.request_timeout_ms)
    }),
    ("gateway.rate_limit_per_minute", |c| {
        format!("{}", c.gateway.rate_limit_per_minute)
    }),
    ("gateway.public_url", |c| {
        c.gateway.public_url.clone().unwrap_or_default()
    }),
    ("gateway.dm_policy", |c| {
        serde_json::to_string(&c.gateway.dm_policy)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string()
    }),
    ("gateway.pairing_ttl_secs", |c| {
        format!("{}", c.gateway.pairing_ttl_secs)
    }),
    ("gateway.group_activation", |c| {
        serde_json::to_string(&c.gateway.group_activation)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string()
    }),
    ("gateway.error_policy", |c| {
        format!("{}", c.gateway.error_policy)
    }),
    ("gateway.error_cooldown_ms", |c| {
        format!("{}", c.gateway.error_cooldown_ms)
    }),
    ("gateway.bindings", |c| {
        serde_json::to_string(&c.gateway.bindings).unwrap_or_default()
    }),
    ("gateway.channel_policies", |c| {
        serde_json::to_string(&c.gateway.channel_policies).unwrap_or_default()
    }),
    ("gateway.auto_reply", |c| {
        serde_json::to_string(&c.gateway.auto_reply).unwrap_or_default()
    }),
    ("gateway.link_understanding", |c| {
        serde_json::to_string(&c.gateway.link_understanding).unwrap_or_default()
    }),
    ("gateway.channel_error_policies", |c| {
        serde_json::to_string(&c.gateway.channel_error_policies).unwrap_or_default()
    }),
    // ── Memory extended ──
    ("memory.flush_soft_threshold_tokens", |c| {
        format!("{}", c.memory.flush_soft_threshold_tokens)
    }),
    ("memory.chunk_level_selection", |c| {
        format!("{}", c.memory.chunk_level_selection)
    }),
    ("memory.embeddings.enabled", |c| {
        format!("{}", c.memory.embeddings.enabled)
    }),
    ("memory.embeddings.recency_weight", |c| {
        format!("{}", c.memory.embeddings.recency_weight)
    }),
    ("memory.embeddings.chunk_size_tokens", |c| {
        format!("{}", c.memory.embeddings.chunk_size_tokens)
    }),
    ("memory.embeddings.chunk_overlap_tokens", |c| {
        format!("{}", c.memory.embeddings.chunk_overlap_tokens)
    }),
    ("memory.embeddings.bm25_weight", |c| {
        format!("{}", c.memory.embeddings.bm25_weight)
    }),
    ("memory.embeddings.vector_weight", |c| {
        format!("{}", c.memory.embeddings.vector_weight)
    }),
    // ── Security extended ──
    ("security.blocked_paths", |c| {
        serde_json::to_string(&c.security.blocked_paths).unwrap_or_default()
    }),
    ("security.allowed_paths", |c| {
        serde_json::to_string(&c.security.allowed_paths).unwrap_or_default()
    }),
    ("security.action_limits", |c| {
        serde_json::to_string(&c.security.action_limits).unwrap_or_default()
    }),
    ("security.gateway_action_limits", |c| {
        serde_json::to_string(&c.security.gateway_action_limits).unwrap_or_default()
    }),
    // ── Agents ──
    ("agents.enabled", |c| format!("{}", c.agents.enabled)),
    ("agents.max_spawn_depth", |c| {
        format!("{}", c.agents.max_spawn_depth)
    }),
    ("agents.max_children_per_agent", |c| {
        format!("{}", c.agents.max_children_per_agent)
    }),
    ("agents.max_concurrent", |c| {
        format!("{}", c.agents.max_concurrent)
    }),
    // ── Debug ──
    ("debug.llm_logging", |c| format!("{}", c.debug.llm_logging)),
    // ── Audio ──
    ("audio.enabled", |c| format!("{}", c.audio.enabled)),
    ("audio.models", |c| {
        serde_json::to_string(&c.audio.models).unwrap_or_default()
    }),
    // ── TTS extended ──
    ("tts.models", |c| {
        serde_json::to_string(&c.tts.models).unwrap_or_default()
    }),
    ("tts.max_text_length", |c| {
        format!("{}", c.tts.max_text_length)
    }),
    ("tts.timeout_ms", |c| format!("{}", c.tts.timeout_ms)),
    // ── Media ──
    ("media.max_image_bytes", |c| {
        format!("{}", c.media.max_image_bytes)
    }),
    ("media.compression_enabled", |c| {
        format!("{}", c.media.compression_enabled)
    }),
    ("media.max_dimension_px", |c| {
        format!("{}", c.media.max_dimension_px)
    }),
    // ── Image Gen ──
    ("image_gen.enabled", |c| format!("{}", c.image_gen.enabled)),
    ("image_gen.default_size", |c| {
        c.image_gen.default_size.clone()
    }),
    // ── Scripts ──
    ("scripts.enabled", |c| format!("{}", c.scripts.enabled)),
    ("scripts.default_timeout_ms", |c| {
        format!("{}", c.scripts.default_timeout_ms)
    }),
    // ── Compaction ──
    ("compaction.provider", |c| {
        c.compaction.provider.clone().unwrap_or_default()
    }),
    ("compaction.model", |c| {
        c.compaction.model.clone().unwrap_or_default()
    }),
    // ── Plugins ──
    ("plugins.enabled", |c| format!("{}", c.plugins.enabled)),
    ("plugins.auto_verify", |c| {
        format!("{}", c.plugins.auto_verify)
    }),
    // ── Credentials (JSON) ──
    ("credentials", |c| {
        serde_json::to_string(&c.credentials).unwrap_or_default()
    }),
];

/// All known setting keys, derived from `SETTING_REGISTRY`.
pub static ALL_SETTING_KEYS: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| SETTING_REGISTRY.iter().map(|(k, _)| *k).collect());

/// Merges settings from two layers: DB overrides → compiled defaults.
pub struct SettingsResolver {
    db: Database,
}

impl SettingsResolver {
    /// Open the database for settings resolution.
    pub fn load() -> Result<Self> {
        let db = Database::open().with_context(|| "Failed to open database for settings")?;
        Ok(Self { db })
    }

    /// Build from a pre-existing Database.
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Resolve a full Config from defaults + DB overrides.
    pub fn resolve(&self) -> Result<Config> {
        let mut config = Config::default();
        let db_settings = self.db.list_settings()?;
        for (key, value, _) in &db_settings {
            if let Err(e) = config.apply_setting(key, value) {
                tracing::warn!("Ignoring invalid setting {key}: {e}");
            }
        }
        Ok(config)
    }

    /// Validate and write a setting to the database.
    /// Returns the confirmation string from `apply_setting`.
    pub fn set(&self, key: &str, value: &str) -> Result<String> {
        // Validate by applying to a throwaway config
        let mut scratch = Config::default();
        let confirmation = scratch.apply_setting(key, value)?;
        self.db.set_setting(key, value)?;
        Ok(confirmation)
    }

    /// Revert a setting to its compiled default value.
    /// Writes the default back to the DB rather than deleting the row,
    /// keeping the settings table as the complete source of truth.
    pub fn unset(&self, key: &str) -> Result<()> {
        let defaults = Config::default();
        let default_value = config_value_for_key(&defaults, key)
            .ok_or_else(|| anyhow::anyhow!("Unknown setting key: {key}"))?;
        self.db.set_setting(key, &default_value)?;
        Ok(())
    }

    /// Get the effective value and its source for a single key.
    /// Source is determined by comparing the DB value to the compiled default:
    /// matching values report `Default`, differing values report `Database`.
    pub fn get_with_source(&self, key: &str) -> Result<(String, SettingSource)> {
        let defaults = Config::default();
        let default_value = config_value_for_key(&defaults, key)
            .ok_or_else(|| anyhow::anyhow!("Unknown setting key: {key}"))?;

        if let Some(db_value) = self.db.get_setting(key)? {
            let source = if db_value == default_value {
                SettingSource::Default
            } else {
                SettingSource::Database
            };
            return Ok((db_value, source));
        }

        // Fallback for keys not yet seeded (e.g. new setting added between startups)
        Ok((default_value, SettingSource::Default))
    }

    /// List all settings with their effective values and sources.
    pub fn list_all(&self) -> Result<Vec<SettingInfo>> {
        let mut result = Vec::new();
        for &key in ALL_SETTING_KEYS.iter() {
            match self.get_with_source(key) {
                Ok((value, source)) => result.push(SettingInfo {
                    key: key.to_string(),
                    value,
                    source,
                }),
                Err(e) => {
                    tracing::warn!("Failed to resolve setting {key}: {e}");
                    continue;
                }
            }
        }
        Ok(result)
    }

    /// Access the underlying database.
    pub fn database(&self) -> &Database {
        &self.db
    }
}

/// Extract the current string value of a config key.
fn config_value_for_key(config: &Config, key: &str) -> Option<String> {
    SETTING_REGISTRY
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, extract)| extract(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn test_resolver() -> SettingsResolver {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let db = Database::from_connection(conn).expect("db setup");
        SettingsResolver::new(db)
    }

    #[test]
    fn defaults_resolve_without_db() {
        let resolver = test_resolver();
        let config = resolver.resolve().unwrap();
        assert!((config.llm.temperature - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn db_overrides_default() {
        let resolver = test_resolver();
        resolver.set("temperature", "1.2").unwrap();
        let config = resolver.resolve().unwrap();
        assert!((config.llm.temperature - 1.2).abs() < f32::EPSILON);
    }

    #[test]
    fn unset_reverts_to_default() {
        let resolver = test_resolver();
        resolver.set("temperature", "1.2").unwrap();
        resolver.unset("temperature").unwrap();
        let config = resolver.resolve().unwrap();
        assert!((config.llm.temperature - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn get_with_source_default() {
        let resolver = test_resolver();
        let (val, source) = resolver.get_with_source("temperature").unwrap();
        assert_eq!(val, "0.7");
        assert_eq!(source, SettingSource::Default);
    }

    #[test]
    fn get_with_source_db() {
        let resolver = test_resolver();
        resolver.set("temperature", "0.9").unwrap();
        let (val, source) = resolver.get_with_source("temperature").unwrap();
        assert_eq!(val, "0.9");
        assert_eq!(source, SettingSource::Database);
    }

    #[test]
    fn evolution_enabled_round_trip() {
        let resolver = test_resolver();
        let (val, source) = resolver.get_with_source("evolution.enabled").unwrap();
        assert_eq!(val, "true");
        assert_eq!(source, SettingSource::Default);

        resolver.set("evolution.enabled", "false").unwrap();
        let (val, source) = resolver.get_with_source("evolution.enabled").unwrap();
        assert_eq!(val, "false");
        assert_eq!(source, SettingSource::Database);
    }

    #[test]
    fn evolution_enabled_in_list_all() {
        let resolver = test_resolver();
        let all = resolver.list_all().unwrap();
        assert!(all.iter().any(|s| s.key == "evolution.enabled"));
    }

    #[test]
    fn invalid_key_errors() {
        let resolver = test_resolver();
        assert!(resolver.set("nonexistent", "value").is_err());
    }

    #[test]
    fn invalid_value_errors() {
        let resolver = test_resolver();
        assert!(resolver.set("temperature", "5.0").is_err());
    }

    #[test]
    fn list_all_returns_all_keys() {
        let resolver = test_resolver();
        let all = resolver.list_all().unwrap();
        assert!(all.len() >= 14); // at least the original 14 settings
        assert!(all.iter().any(|s| s.key == "temperature"));
        assert!(all
            .iter()
            .any(|s| s.key == "conversation.tool_output_max_tokens"));
    }

    #[test]
    fn json_secret_ref_round_trip() {
        let resolver = test_resolver();
        let json =
            r#"{"source":"exec","command":"security","args":["find-generic-password","-w"]}"#;
        resolver.set("llm.api_key", json).unwrap();
        let config = resolver.resolve().unwrap();
        assert!(config.llm.api_key.is_some());
        let (val, source) = resolver.get_with_source("llm.api_key").unwrap();
        assert_eq!(source, SettingSource::Database);
        assert!(val.contains("exec"));
    }

    #[test]
    fn json_gateway_bindings_round_trip() {
        let resolver = test_resolver();
        let json = r#"[{"channel":"telegram","provider":"anthropic","model":"claude-sonnet-4"}]"#;
        resolver.set("gateway.bindings", json).unwrap();
        let config = resolver.resolve().unwrap();
        assert_eq!(config.gateway.bindings.len(), 1);
        assert_eq!(config.gateway.bindings[0].channel, "telegram");
    }

    #[test]
    fn json_credentials_round_trip() {
        let resolver = test_resolver();
        let json = r#"{"SLACK_TOKEN":"SLACK_TOKEN"}"#;
        resolver.set("credentials", json).unwrap();
        let config = resolver.resolve().unwrap();
        assert!(config.credentials.contains_key("SLACK_TOKEN"));
    }

    #[test]
    fn user_name_round_trip() {
        let resolver = test_resolver();
        resolver.set("user.name", "mike").unwrap();
        let config = resolver.resolve().unwrap();
        assert_eq!(config.user.name.as_deref(), Some("mike"));
    }

    #[test]
    fn llm_thinking_round_trip() {
        let resolver = test_resolver();
        resolver.set("llm.thinking", "high").unwrap();
        let config = resolver.resolve().unwrap();
        assert!(config.llm.thinking.is_enabled());
    }

    #[test]
    fn tools_policy_allow_round_trip() {
        let resolver = test_resolver();
        let json = r#"["group:git","browser"]"#;
        resolver.set("tools.policy.allow", json).unwrap();
        let config = resolver.resolve().unwrap();
        assert_eq!(config.tools.policy.allow, vec!["group:git", "browser"]);
    }

    #[test]
    fn heartbeat_channels_round_trip() {
        let resolver = test_resolver();
        resolver
            .set("heartbeat.channels", r#"["telegram","slack"]"#)
            .unwrap();
        let config = resolver.resolve().unwrap();
        assert_eq!(config.heartbeat.channels, vec!["telegram", "slack"]);
    }

    #[test]
    fn security_blocked_paths_round_trip() {
        let resolver = test_resolver();
        resolver
            .set("security.blocked_paths", r#"[".ssh",".env"]"#)
            .unwrap();
        let config = resolver.resolve().unwrap();
        assert_eq!(config.security.blocked_paths, vec![".ssh", ".env"]);
    }

    #[test]
    fn invalid_json_setting_errors() {
        let resolver = test_resolver();
        assert!(resolver.set("gateway.bindings", "not json").is_err());
        assert!(resolver.set("llm.api_key", "{bad}").is_err());
    }

    #[test]
    fn new_scalar_settings_round_trip() {
        let resolver = test_resolver();

        resolver.set("llm.max_retries", "5").unwrap();
        resolver.set("gateway.port", "8080").unwrap();
        resolver.set("agents.enabled", "false").unwrap();
        resolver.set("debug.llm_logging", "true").unwrap();
        resolver.set("web.enabled", "false").unwrap();

        let config = resolver.resolve().unwrap();
        assert_eq!(config.llm.max_retries, 5);
        assert_eq!(config.gateway.port, 8080);
        assert!(!config.agents.enabled);
        assert!(config.debug.llm_logging);
        assert!(!config.web.enabled);
    }

    // ── DB-as-source-of-truth tests ──

    #[test]
    fn ensure_all_settings_seeds_defaults() {
        let resolver = test_resolver();
        let settings = resolver.database().list_settings().unwrap();
        // After from_connection (which calls ensure_all_settings via init_connection),
        // every SETTING_REGISTRY key should have a row.
        assert_eq!(
            settings.len(),
            SETTING_REGISTRY.len(),
            "expected {} settings rows, got {}",
            SETTING_REGISTRY.len(),
            settings.len()
        );
        let keys: Vec<&str> = settings.iter().map(|(k, _, _)| k.as_str()).collect();
        for &(key, _) in SETTING_REGISTRY.iter() {
            assert!(keys.contains(&key), "missing setting key: {key}");
        }
    }

    #[test]
    fn ensure_all_settings_preserves_overrides() {
        let resolver = test_resolver();
        resolver.set("temperature", "1.5").unwrap();
        // Re-seed — should not clobber the override
        resolver.database().ensure_all_settings().unwrap();
        let (val, source) = resolver.get_with_source("temperature").unwrap();
        assert_eq!(val, "1.5");
        assert_eq!(source, SettingSource::Database);
    }

    #[test]
    fn ensure_all_settings_idempotent() {
        let resolver = test_resolver();
        let before = resolver.database().list_settings().unwrap();
        resolver.database().ensure_all_settings().unwrap();
        let after = resolver.database().list_settings().unwrap();
        assert_eq!(before.len(), after.len());
        for ((k1, v1, _), (k2, v2, _)) in before.iter().zip(after.iter()) {
            assert_eq!(k1, k2);
            assert_eq!(v1, v2, "value changed for key {k1}");
        }
    }

    #[test]
    fn unset_writes_default_not_delete() {
        let resolver = test_resolver();
        resolver.set("temperature", "1.5").unwrap();
        resolver.unset("temperature").unwrap();
        // Row should still exist in DB with the default value
        let db_val = resolver.database().get_setting("temperature").unwrap();
        assert_eq!(db_val, Some("0.7".to_string()));
        // Source should report Default since value matches compiled default
        let (val, source) = resolver.get_with_source("temperature").unwrap();
        assert_eq!(val, "0.7");
        assert_eq!(source, SettingSource::Default);
    }

    #[test]
    fn get_source_default_for_seeded() {
        let resolver = test_resolver();
        // All settings are seeded with defaults — should report Default source
        let (_, source) = resolver.get_with_source("sandbox.enabled").unwrap();
        assert_eq!(source, SettingSource::Default);
    }

    #[test]
    fn get_source_database_for_modified() {
        let resolver = test_resolver();
        resolver.set("sandbox.enabled", "false").unwrap();
        let (val, source) = resolver.get_with_source("sandbox.enabled").unwrap();
        assert_eq!(val, "false");
        assert_eq!(source, SettingSource::Database);
    }

    #[test]
    fn list_all_complete_after_seeding() {
        let resolver = test_resolver();
        let all = resolver.list_all().unwrap();
        assert_eq!(
            all.len(),
            SETTING_REGISTRY.len(),
            "list_all should return exactly SETTING_REGISTRY.len() entries"
        );
    }

    #[test]
    fn data_version_available() {
        let resolver = test_resolver();
        let v1 = resolver.database().data_version().unwrap();
        assert!(v1 >= 0, "data_version should be non-negative");
    }
}
