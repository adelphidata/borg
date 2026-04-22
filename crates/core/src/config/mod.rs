pub mod gateway;
pub mod llm;
pub mod maintenance;
pub mod media;
pub mod security;
#[macro_use]
mod settings_macro;
pub mod settings_table;

#[cfg(test)]
mod tests;

pub use gateway::*;
pub use llm::*;
pub use maintenance::*;
pub use media::*;
pub use security::*;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tracing::warn;

use crate::constants::{DB_FILE, IDENTITY_FILE, MEMORY_INDEX_FILE};
use crate::policy::ExecutionPolicy;
use crate::provider::Provider;

/// Top-level configuration, loaded from SQLite settings DB.
///
/// All sections default to sensible values. Settings are stored in the
/// `settings` table as key-value pairs and applied via `apply_setting()`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// LLM provider, model, temperature, retries, and fallback chain.
    #[serde(default)]
    pub llm: LlmConfig,
    /// Proactive heartbeat scheduling and quiet hours.
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
    /// Tool execution settings (default timeout).
    #[serde(default)]
    pub tools: ToolsConfig,
    /// Sandbox isolation mode (strict/permissive/disabled).
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Memory system settings (token budget, embeddings, extra paths).
    #[serde(default)]
    pub memory: MemoryConfig,
    /// Skills loading and token budget.
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Conversation behavior (collaboration mode, compaction).
    #[serde(default)]
    pub conversation: ConversationConfig,
    /// User profile (name, timezone).
    #[serde(default)]
    pub user: UserConfig,
    /// Execution policy for collaboration modes.
    #[serde(default)]
    pub policy: ExecutionPolicy,
    /// Debug flags (verbose logging, token tracking).
    #[serde(default)]
    pub debug: DebugConfig,
    /// Security settings (secret detection, blocked paths, action limits).
    #[serde(default)]
    pub security: SecurityConfig,
    /// Web fetching and search capabilities.
    #[serde(default)]
    pub web: WebConfig,
    /// Scheduled task daemon settings (concurrency, catch-up).
    #[serde(default)]
    pub tasks: TasksConfig,
    /// Monthly token budget and warning threshold.
    #[serde(default)]
    pub budget: BudgetConfig,
    /// Gateway server settings (host, port, DM policy).
    #[serde(default)]
    pub gateway: GatewayConfig,
    /// Plugin marketplace settings.
    #[serde(default, alias = "customizations")]
    pub plugins: PluginsConfig,
    /// Multi-agent orchestration settings.
    #[serde(default)]
    pub agents: MultiAgentConfig,
    /// Anonymous telemetry collection.
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// Headless Chrome automation settings.
    #[serde(default)]
    pub browser: BrowserConfig,
    /// Audio input/output settings.
    #[serde(default)]
    pub audio: AudioConfig,
    /// Text-to-speech settings.
    #[serde(default)]
    pub tts: TtsConfig,
    /// Media understanding settings.
    #[serde(default)]
    pub media: MediaConfig,
    /// AI image generation settings.
    #[serde(default)]
    pub image_gen: ImageGenConfig,
    /// User-created script management.
    #[serde(default)]
    pub scripts: ScriptsConfig,
    /// User-authored lifecycle hooks loaded from `~/.borg/hooks.json`.
    #[serde(default)]
    pub hooks: HooksConfig,
    /// Compaction model overrides (use cheaper model for context compaction).
    #[serde(default)]
    pub compaction: CompactionConfig,
    /// Workflow engine settings (durable multi-step orchestration).
    #[serde(default)]
    pub workflow: WorkflowConfig,
    /// Conversation evolution and personality drift settings.
    #[serde(default)]
    pub evolution: EvolutionConfig,
    /// Daily self-healing maintenance task retention settings.
    #[serde(default)]
    pub maintenance: MaintenanceConfig,
    /// Credential store for resolving secrets from env, file, exec, or keychain.
    #[serde(default)]
    pub credentials: HashMap<String, CredentialValue>,
    /// Transient identity override (not serialized). Set by gateway routing.
    #[serde(skip)]
    pub identity_override: Option<std::path::PathBuf>,
}

impl Config {
    /// Build a config with compaction overrides applied to the LLM section.
    /// If no compaction overrides are set, returns a clone of self.
    pub fn with_compaction_overrides(&self) -> Config {
        if !self.compaction.has_overrides() {
            return self.clone();
        }
        let mut cfg = self.clone();
        if let Some(ref provider) = self.compaction.provider {
            cfg.llm.provider = Some(provider.clone());
        }
        if let Some(ref model) = self.compaction.model {
            cfg.llm.model = model.clone();
        }
        if let Some(ref api_key_env) = self.compaction.api_key_env {
            cfg.llm.api_key_env = api_key_env.clone();
        }
        if let Some(temp) = self.compaction.temperature {
            cfg.llm.temperature = temp;
        }
        if let Some(max_tok) = self.compaction.max_tokens {
            cfg.llm.max_tokens = max_tok;
        }
        if let Some(timeout) = self.compaction.timeout_ms {
            cfg.llm.request_timeout_ms = timeout;
        }
        cfg
    }
}

pub fn parse_value<T: FromStr>(value: &str, key: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|e: T::Err| anyhow::anyhow!("Invalid value for {key}: {e}"))
}

pub fn parse_nonzero<T: FromStr + Default + PartialEq>(value: &str, key: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    let v = parse_value::<T>(value, key)?;
    if v == T::default() {
        anyhow::bail!("{key} must be greater than 0");
    }
    Ok(v)
}

pub fn parse_range<T: FromStr + PartialOrd + std::fmt::Display>(
    value: &str,
    key: &str,
    min: T,
    max: T,
) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    let v = parse_value::<T>(value, key)?;
    if v < min || v > max {
        anyhow::bail!("{key} must be between {min} and {max}");
    }
    Ok(v)
}

impl Config {
    /// Returns the borg data directory (`~/.borg/` or `$BORG_DATA_DIR`).
    pub fn data_dir() -> Result<PathBuf> {
        if let Ok(dir) = std::env::var("BORG_DATA_DIR") {
            return Ok(PathBuf::from(dir));
        }
        Ok(dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
            .join(".borg"))
    }

    /// Returns the memory directory path (`~/.borg/memory/`).
    pub fn memory_dir() -> Result<PathBuf> {
        Ok(Self::data_dir()?.join("memory"))
    }

    /// Resolve the user's configured timezone, falling back to UTC.
    pub fn user_timezone(&self) -> chrono_tz::Tz {
        self.user
            .timezone
            .as_deref()
            .and_then(|s| {
                s.parse::<chrono_tz::Tz>()
                    .inspect_err(|e| {
                        tracing::warn!("Invalid timezone '{s}': {e}, defaulting to UTC");
                    })
                    .ok()
            })
            .unwrap_or(chrono_tz::Tz::UTC)
    }

    /// Returns the skills directory path (`~/.borg/skills/`).
    pub fn skills_dir() -> Result<PathBuf> {
        Ok(Self::data_dir()?.join("skills"))
    }

    /// Returns the channels directory path (`~/.borg/channels/`).
    pub fn channels_dir() -> Result<PathBuf> {
        Ok(Self::data_dir()?.join("channels"))
    }

    /// Returns the scripts directory path (`~/.borg/scripts/`).
    pub fn scripts_dir() -> Result<PathBuf> {
        Ok(Self::data_dir()?.join("scripts"))
    }

    /// Returns the logs directory path (`~/.borg/logs/`).
    pub fn logs_dir() -> Result<PathBuf> {
        Ok(Self::data_dir()?.join("logs"))
    }

    /// Returns the sessions directory path (`~/.borg/sessions/`).
    pub fn sessions_dir() -> Result<PathBuf> {
        Ok(Self::data_dir()?.join("sessions"))
    }

    /// Returns the database file path (`~/.borg/borg.db`).
    pub fn db_path() -> Result<PathBuf> {
        Ok(Self::data_dir()?.join(DB_FILE))
    }

    /// Returns the identity file path (`~/.borg/IDENTITY.md`).
    pub fn identity_path() -> Result<PathBuf> {
        Ok(Self::data_dir()?.join(IDENTITY_FILE))
    }

    /// Returns the memory index file path (`~/.borg/MEMORY.md`).
    pub fn memory_index_path() -> Result<PathBuf> {
        Ok(Self::data_dir()?.join(MEMORY_INDEX_FILE))
    }

    /// Load config from the database (DB overrides + compiled defaults).
    pub fn load_from_db() -> Result<Self> {
        let db = crate::db::Database::open()?;
        let mut config = Self::default();
        for (key, value, _) in db.list_settings()? {
            if let Err(e) = config.apply_setting(&key, &value) {
                crate::settings::warn_invalid_setting_once(&key, &e);
            }
        }
        config.validate()?;
        Ok(config)
    }

    /// Load config from a specific TOML file path (used by tests).
    #[allow(dead_code)]
    pub(crate) fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config from {}", path.display()))?;
        let content = Self::dedup_toml_tables(&content);
        let config: Config =
            toml::from_str(&content).with_context(|| "Failed to parse config.toml")?;
        config.validate()?;
        Ok(config)
    }

    /// Validate config values after loading. Returns an error for fatal
    /// misconfigurations and logs warnings for non-fatal issues.
    pub fn validate(&self) -> Result<()> {
        if !(0.0..=2.0).contains(&self.llm.temperature) {
            anyhow::bail!(
                "llm.temperature must be between 0.0 and 2.0, got {}",
                self.llm.temperature
            );
        }
        if self.llm.max_tokens == 0 {
            anyhow::bail!("llm.max_tokens must be greater than 0");
        }
        if self.memory.max_context_tokens == 0 {
            anyhow::bail!("memory.max_context_tokens must be greater than 0");
        }
        self.security.action_limits.validate_thresholds();
        Ok(())
    }

    /// Remove duplicate TOML table headers that would cause parse errors.
    /// Keeps the first occurrence of each `[table]` header and drops subsequent
    /// duplicates along with their content until the next section header.
    pub(crate) fn dedup_toml_tables(input: &str) -> String {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        let mut output = String::with_capacity(input.len());
        let mut skip = false;

        for line in input.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && !trimmed.starts_with("[[") {
                if seen.contains(trimmed) {
                    tracing::warn!("Duplicate config table '{trimmed}' found — keeping first occurrence, dropping duplicate");
                    skip = true;
                    continue;
                }
                seen.insert(trimmed.to_string());
                skip = false;
            } else if skip {
                continue;
            }

            output.push_str(line);
            output.push('\n');
        }
        output
    }

    /// Format current settings as a human-readable string.
    pub fn display_settings(&self) -> String {
        let provider = self.llm.provider.as_deref().unwrap_or("(auto-detect)");
        format!(
            "Settings:\n  \
             provider       = {provider}\n  \
             model          = {}\n  \
             temperature    = {}\n  \
             max_tokens     = {}\n  \
             sandbox.enabled = {}\n  \
             sandbox.mode   = {}\n  \
             memory.max_context_tokens = {}\n  \
             skills.enabled = {}\n  \
             skills.max_context_tokens = {}\n  \
             conversation.max_iterations = {}\n  \
             conversation.show_thinking = {}\n  \
             security.secret_detection = {}\n  \
             security.host_audit = {}\n  \
             budget.monthly_token_limit = {}\n  \
             budget.warning_threshold = {}\n  \
             browser.enabled = {}\n  \
             browser.headless = {}",
            self.llm.model,
            self.llm.temperature,
            self.llm.max_tokens,
            self.sandbox.enabled,
            self.sandbox.mode,
            self.memory.max_context_tokens,
            self.skills.enabled,
            self.skills.max_context_tokens,
            self.conversation.max_iterations,
            self.conversation.show_thinking,
            self.security.secret_detection,
            self.security.host_audit,
            self.budget.monthly_token_limit,
            self.budget.warning_threshold,
            self.browser.enabled,
            self.browser.headless,
        )
    }

    // apply_setting() is generated by define_settings! macro in settings_table.rs

    /// Resolve the API key from config or environment.
    pub fn api_key(&self) -> Result<String> {
        // Try SecretRef first, then fall back to env var name
        if let Some(ref secret_ref) = self.llm.api_key {
            return secret_ref.resolve();
        }
        std::env::var(&self.llm.api_key_env).with_context(|| {
            format!(
                "API key not found. Set the {} environment variable or configure api_key in config.toml.",
                self.llm.api_key_env
            )
        })
    }

    /// Resolve the provider and API key from config.
    ///
    /// Strict: `llm.provider` MUST be set (via onboarding, `/settings`, `/model`,
    /// or `borg settings set llm.provider <name>`). There is no auto-detection
    /// or TCP-probe fallback — the provider is always the one the user chose,
    /// never inferred from which API keys happen to be in the environment or
    /// which local daemons happen to be listening.
    ///
    /// API key resolution order for the configured provider:
    /// `llm.api_key` SecretRef → `llm.api_key_env` env var → provider's default env var.
    pub fn resolve_provider(&self) -> Result<(Provider, String)> {
        let Some(provider_str) = self.llm.provider.as_deref() else {
            anyhow::bail!(
                "No LLM provider selected. Open `/settings` or `/model` in the TUI to pick one (OpenRouter, OpenAI, Anthropic, Gemini, DeepSeek, Groq, Ollama, or Claude CLI). Borg won't guess."
            );
        };
        let provider = Provider::from_str(provider_str)?;

        // Keyless providers (e.g., Ollama, Claude CLI) don't need API key resolution.
        if !provider.requires_api_key() {
            return Ok((provider, String::new()));
        }

        if let Some(key) = self
            .try_resolve_api_key_secret_ref("api_key_env")
            .or_else(|| try_resolve_env(&self.llm.api_key_env))
            .or_else(|| try_resolve_env(provider.default_env_var()))
        {
            return Ok((provider, key));
        }

        anyhow::bail!(
            "No API key found for {provider}. Open `/settings` in the TUI and paste your key under \"API key\", or export {} in your shell and restart borg.",
            provider.default_env_var()
        );
    }

    /// Try to resolve `self.llm.api_key` via its `SecretRef`. Logs a warning
    /// with `fallback_label` naming the next resolution step when the secret
    /// resolves to empty or fails. Returns `None` when not configured or
    /// unusable so the caller can continue trying other sources.
    fn try_resolve_api_key_secret_ref(&self, fallback_label: &str) -> Option<String> {
        let secret_ref = self.llm.api_key.as_ref()?;
        match secret_ref.resolve() {
            Ok(key) if !key.is_empty() => Some(key),
            Ok(_) => {
                warn!(
                    "api_key SecretRef resolved to empty string, falling back to {fallback_label}"
                );
                None
            }
            Err(e) => {
                warn!("api_key SecretRef failed to resolve: {e}, falling back to {fallback_label}");
                None
            }
        }
    }

    /// Resolve all available API keys for the configured provider.
    /// Returns the provider and a list of resolved keys (for multi-key fallback).
    /// Falls back to a single key from `resolve_provider` if no `api_keys` are configured.
    pub fn resolve_api_keys(&self) -> Result<(Provider, Vec<String>)> {
        // Try multi-key resolution first (avoids requiring a default API key env var)
        if !self.llm.api_keys.is_empty() {
            let mut keys = Vec::new();
            for secret_ref in &self.llm.api_keys {
                if let Ok(key) = secret_ref.resolve() {
                    if !key.is_empty() {
                        keys.push(key);
                    }
                }
            }
            if !keys.is_empty() {
                let Some(ref provider_str) = self.llm.provider else {
                    anyhow::bail!(
                        "No LLM provider selected. Open `/settings` or `/model` in the TUI to pick one — Borg won't guess it from your API key."
                    );
                };
                let provider = Provider::from_str(provider_str)?;
                let provider = correct_provider_from_key(provider, &keys[0]);
                return Ok((provider, keys));
            }
        }

        // Fall back to single key via resolve_provider
        let (provider, key) = self.resolve_provider()?;
        let provider = correct_provider_from_key(provider, &key);
        Ok((provider, vec![key]))
    }

    /// Resolve all credentials to their plaintext values.
    /// Logs warnings for credentials that fail to resolve.
    pub fn resolve_credentials(&self) -> HashMap<String, String> {
        self.credentials
            .iter()
            .filter_map(|(name, cv)| match cv.resolve() {
                Ok(v) => Some((name.clone(), v)),
                Err(e) => {
                    tracing::warn!("Failed to resolve credential '{name}': {e}");
                    None
                }
            })
            .collect()
    }

    /// Resolve a credential by name: credential store first, then env var,
    /// then OS keychain fallback using the plugin naming convention.
    /// Returns `None` if not found or empty.
    pub fn resolve_credential_or_env(&self, name: &str) -> Option<String> {
        // 1. Config credential store (may itself be a keychain SecretRef)
        if let Some(cv) = self.credentials.get(name) {
            match cv.resolve() {
                Ok(v) if !v.is_empty() => return Some(v),
                Ok(_) => {
                    tracing::warn!("Credential '{name}' resolved to empty string");
                }
                Err(e) => {
                    tracing::warn!("Failed to resolve credential '{name}': {e}");
                }
            }
        }

        // 2. Environment variable
        if let Some(v) = std::env::var(name).ok().filter(|t| !t.is_empty()) {
            return Some(v);
        }

        // 3. OS keychain fallback — try the plugin naming convention
        //    (service: borg-messaging-{channel}, account: borg-{KEY})
        self.resolve_keychain_fallback(name)
    }

    /// Try to resolve a credential from the OS keychain using the plugin naming
    /// convention: service = `borg-{plugin_id with / → -}`, account = `borg-{key}`.
    fn resolve_keychain_fallback(&self, key: &str) -> Option<String> {
        /// Maps credential key → plugin ID for keychain service name derivation.
        /// Plugin IDs must match those in the plugin catalog (`crates/plugins/src/catalog.rs`)
        /// and the gateway's `resolve_credential` call sites (`crates/gateway/src/channel_init.rs`).
        const KEY_PLUGIN_MAP: &[(&str, &str)] = &[
            ("TELEGRAM_BOT_TOKEN", "messaging/telegram"),
            ("TELEGRAM_WEBHOOK_SECRET", "messaging/telegram"),
            ("SLACK_BOT_TOKEN", "messaging/slack"),
            ("SLACK_SIGNING_SECRET", "messaging/slack"),
            ("DISCORD_BOT_TOKEN", "messaging/discord"),
            ("DISCORD_PUBLIC_KEY", "messaging/discord"),
            ("TEAMS_APP_ID", "messaging/teams"),
            ("TEAMS_APP_SECRET", "messaging/teams"),
            ("TWILIO_ACCOUNT_SID", "messaging/whatsapp"),
            ("TWILIO_AUTH_TOKEN", "messaging/whatsapp"),
            ("TWILIO_PHONE_NUMBER", "messaging/whatsapp"),
            ("TWILIO_WHATSAPP_NUMBER", "messaging/whatsapp"),
            ("GOOGLE_CHAT_SERVICE_TOKEN", "messaging/google-chat"),
            ("GOOGLE_CHAT_WEBHOOK_TOKEN", "messaging/google-chat"),
            ("SIGNAL_ACCOUNT", "messaging/signal"),
        ];

        let plugin_id = KEY_PLUGIN_MAP
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, pid)| *pid)?;

        let service = format!("borg-{}", plugin_id.replace('/', "-"));
        let account = format!("borg-{key}");

        let sr = crate::secrets_resolve::SecretRef::Keychain {
            service: service.clone(),
            account: account.clone(),
        };
        match sr.resolve() {
            Ok(v) if !v.is_empty() => {
                tracing::info!(
                    "Resolved {key} from keychain fallback (credential ref missing from config)"
                );
                return Some(v);
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!("Keychain fallback for {key} failed: {e}");
            }
        }

        // Final fallback: the on-disk credentials file written by
        // `borg_plugins::credential_store` when the keychain was unavailable
        // at install time. Mirror of the same file format — kept inline so
        // core does not take a hard dep on the plugins crate.
        read_credentials_file(&service, &account)
    }

    /// Returns true if any native channel credentials are configured.
    pub fn has_any_native_channel(&self) -> bool {
        const NATIVE_KEYS: &[&str] = &[
            "TELEGRAM_BOT_TOKEN",
            "SLACK_BOT_TOKEN",
            "DISCORD_BOT_TOKEN",
            "TWILIO_ACCOUNT_SID",
            "TEAMS_APP_ID",
            "GOOGLE_CHAT_SERVICE_TOKEN",
            "SIGNAL_ACCOUNT",
        ];
        NATIVE_KEYS
            .iter()
            .any(|k| self.resolve_credential_or_env(k).is_some())
    }

    /// Returns a list of detected native channels with their names and descriptions.
    pub fn detected_native_channels(&self) -> Vec<(&'static str, &'static str)> {
        const NATIVE_CHANNELS: &[(&str, &str, &str)] = &[
            ("telegram", "Telegram (native)", "TELEGRAM_BOT_TOKEN"),
            ("slack", "Slack (native)", "SLACK_BOT_TOKEN"),
            ("discord", "Discord (native)", "DISCORD_BOT_TOKEN"),
            (
                "twilio",
                "Twilio SMS/WhatsApp (native)",
                "TWILIO_ACCOUNT_SID",
            ),
            ("teams", "Microsoft Teams (native)", "TEAMS_APP_ID"),
            (
                "google-chat",
                "Google Chat (native)",
                "GOOGLE_CHAT_SERVICE_TOKEN",
            ),
            (
                "signal",
                "Signal Messenger (native via signal-cli)",
                "SIGNAL_ACCOUNT",
            ),
        ];
        NATIVE_CHANNELS
            .iter()
            .filter(|(_, _, key)| self.resolve_credential_or_env(key).is_some())
            .map(|(name, desc, _)| (*name, *desc))
            .collect()
    }
}

/// Read an env var and return its value only when it's set AND non-empty.
/// Centralizes the "empty env var counts as unset" rule that both explicit
/// and auto-detect API-key resolution paths need.
fn try_resolve_env(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.is_empty())
}

/// Read a credential from the on-disk fallback file
/// (`~/.borg/.credentials.json`) written by the plugin installer when the OS
/// keychain was unavailable. Mirror of `borg_plugins::credential_store::read`'s
/// file branch — duplicated here to keep core free of a plugins dep.
fn read_credentials_file(service: &str, account: &str) -> Option<String> {
    let path = Config::data_dir().ok()?.join(".credentials.json");
    if !path.exists() {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let value = json.get(service)?.get(account)?.as_str()?;
    if value.is_empty() {
        return None;
    }
    tracing::info!(
        "Resolved credential from ~/.borg/.credentials.json fallback (service={service})"
    );
    Some(value.to_string())
}

/// Override the configured provider when the API key's prefix points to a
/// different, unambiguous provider (e.g. `sk-or-*` is always OpenRouter).
///
/// Prevents the surprising "Authentication with OpenAI failed" flow when an
/// OpenRouter key ends up stored under `provider = "openai"`.
fn correct_provider_from_key(configured: Provider, key: &str) -> Provider {
    match Provider::from_api_key_prefix(key) {
        Some(detected) if detected != configured => {
            warn!(
                "API key prefix indicates {detected}, but provider is set to {configured}. \
                 Routing through {detected}."
            );
            detected
        }
        _ => configured,
    }
}
