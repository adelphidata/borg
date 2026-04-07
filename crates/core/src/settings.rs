//! Settings resolution layer: DB → config.toml → constants.rs defaults.

use anyhow::{Context, Result};

use crate::config::Config;
use crate::db::Database;

/// Where a setting's effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingSource {
    Default,
    ConfigToml,
    Database,
}

impl std::fmt::Display for SettingSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingSource::Default => write!(f, "default"),
            SettingSource::ConfigToml => write!(f, "toml"),
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

type SettingExtractor = fn(&Config) -> String;

/// Single source of truth for setting keys and their config extractors.
///
/// Each entry is `(key, extractor_fn)`. `ALL_SETTING_KEYS` and `config_value_for_key()`
/// are both derived from this table.
const SETTING_REGISTRY: &[(&str, SettingExtractor)] = &[
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
];

/// All known setting keys, derived from `SETTING_REGISTRY`.
pub static ALL_SETTING_KEYS: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| SETTING_REGISTRY.iter().map(|(k, _)| *k).collect());

/// Merges settings from three layers: DB overrides → config.toml → compiled defaults.
pub struct SettingsResolver {
    db: Database,
    file_config: Config,
    has_toml: bool,
}

impl SettingsResolver {
    /// Load config from disk and open the database.
    pub fn load() -> Result<Self> {
        let config_path = Config::data_dir()?.join("config.toml");
        let has_toml = config_path.exists();
        let file_config = Config::load()?;
        let db = Database::open().with_context(|| "Failed to open database for settings")?;
        Ok(Self {
            db,
            file_config,
            has_toml,
        })
    }

    /// Build from pre-existing Config and Database.
    pub fn new(db: Database, file_config: Config, has_toml: bool) -> Self {
        Self {
            db,
            file_config,
            has_toml,
        }
    }

    /// Resolve a full Config with DB overrides applied on top of file_config.
    pub fn resolve(&self) -> Result<Config> {
        let mut config = self.file_config.clone();
        let db_settings = self.db.list_settings()?;
        for (key, value, _) in &db_settings {
            // Silently skip keys that no longer exist
            let _ = config.apply_setting(key, value);
        }
        Ok(config)
    }

    /// Validate and write a setting to the database.
    /// Returns the confirmation string from `apply_setting`.
    pub fn set(&self, key: &str, value: &str) -> Result<String> {
        // Validate by applying to a throwaway config
        let mut scratch = self.file_config.clone();
        let confirmation = scratch.apply_setting(key, value)?;
        self.db.set_setting(key, value)?;
        Ok(confirmation)
    }

    /// Remove a DB override, reverting to TOML/default value.
    pub fn unset(&self, key: &str) -> Result<()> {
        self.db.delete_setting(key)?;
        Ok(())
    }

    /// Get the effective value and its source for a single key.
    pub fn get_with_source(&self, key: &str) -> Result<(String, SettingSource)> {
        // Check DB first
        if let Some(value) = self.db.get_setting(key)? {
            return Ok((value, SettingSource::Database));
        }

        // Read from file config
        let default_config = Config::default();
        let file_value = config_value_for_key(&self.file_config, key);
        let default_value = config_value_for_key(&default_config, key);

        match file_value {
            Some(val) => {
                let source = if self.has_toml && default_value.as_deref() != Some(&val) {
                    SettingSource::ConfigToml
                } else {
                    SettingSource::Default
                };
                Ok((val, source))
            }
            None => anyhow::bail!("Unknown setting key: {key}"),
        }
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
                Err(_) => continue,
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
        let config = Config::default();
        SettingsResolver::new(db, config, false)
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
}
