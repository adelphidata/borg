pub mod apns;
pub mod computer_use;
pub mod consolidation;
pub mod data;
pub mod discovery;
pub mod gateway;
pub mod goals;
pub mod llm;
pub mod loop_guard;
pub mod maintenance;
pub mod mcp;
pub mod mcp_supply_chain;
pub mod media;
pub mod process_registry;
pub mod security;
#[macro_use]
mod settings_macro;
pub mod settings_table;
pub mod telephony;
pub mod tunnel;
pub mod ui;
pub mod update;
pub mod voice;
pub mod webhooks;

#[cfg(test)]
mod tests;

/// Serializes every test that sets or clears a native-channel credential
/// env var. Env vars are process-global, so tests in different modules race
/// unless they all take this one lock.
#[cfg(test)]
pub(crate) static CHANNEL_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes tests that repoint `$BORG_HOME` (and so the on-disk credentials
/// file) against tests that read it. Take this lock BEFORE
/// `CHANNEL_ENV_MUTEX` when both are needed.
#[cfg(test)]
pub(crate) fn creds_file_env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub use apns::ApnsConfig;
pub use computer_use::*;
pub use consolidation::*;
pub use data::*;
pub use discovery::DiscoveryConfig;
pub use gateway::*;
pub use goals::*;
pub use llm::*;
pub use loop_guard::*;
pub use maintenance::*;
pub use mcp::{HttpAuthSpec, McpConfig, McpServerConfig, TransportSpec};
pub use media::*;
pub use process_registry::*;
pub use security::*;
pub use telephony::TelephonyConfig;
pub use tunnel::{
    CloudflareTunnelConfig, CustomTunnelConfig, NgrokTunnelConfig, TailscaleTunnelConfig,
    TunnelConfig, TunnelProvider,
};
pub use ui::UiConfig;
pub use update::*;
pub use voice::VoiceConfig;
pub use webhooks::{WebhookEndpoint, WebhooksConfig};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use tracing::warn;

use crate::llm::provider::Provider;
use crate::paths::{self, BorgSubdir};
use crate::security::secrets_resolve::{ApiKeyRouting, SecretRef};

/// Settings-table prefix of the per-provider API key routing rows that
/// `Admin.SetApiKey` writes (`credentials.openrouter`, …).
pub const STORED_API_KEY_PREFIX: &str = "credentials.";
use crate::policy::ExecutionPolicy;

/// Top-level configuration, loaded from SQLite settings DB.
///
/// All sections default to sensible values. Settings are stored in the
/// `settings` table as key-value pairs and applied via `apply_setting()`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// LLM provider, model, temperature, retries, and fallback chain.
    #[serde(default)]
    pub llm: LlmConfig,
    /// Air-gap and provider-routing policy.
    #[serde(default)]
    pub providers: ProvidersConfig,
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
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// MCP (Model Context Protocol) client settings.
    #[serde(default)]
    pub mcp: McpConfig,
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
    /// Voice-mode loop (mic capture VAD + TTS-pause guard + auto-submit).
    #[serde(default)]
    pub voice: VoiceConfig,
    /// Outbound phone calls (`phone_call` tool); off until a provider is set.
    #[serde(default)]
    pub telephony: TelephonyConfig,
    /// Media understanding settings.
    #[serde(default)]
    pub media: MediaConfig,
    /// macOS-only `computer_use` tool — screen capture and UI control via cua-driver.
    #[serde(default)]
    pub computer_use: ComputerUseConfig,
    /// AI image generation settings.
    #[serde(default)]
    pub image_gen: ImageGenConfig,
    /// User-created script management.
    #[serde(default)]
    pub scripts: ScriptsConfig,
    /// User-authored lifecycle hooks loaded from `~/.borg/hooks.json`.
    #[serde(default)]
    pub hooks: HooksConfig,
    /// Outbound signed lifecycle webhooks.
    #[serde(default)]
    pub webhooks: WebhooksConfig,
    /// Compaction model overrides (use cheaper model for context compaction).
    #[serde(default)]
    pub compaction: CompactionConfig,
    /// Conversation evolution and personality drift settings.
    #[serde(default)]
    pub evolution: EvolutionConfig,
    /// Daily self-healing maintenance task retention settings.
    #[serde(default)]
    pub maintenance: MaintenanceConfig,
    /// Agent-loop safety guards: stuck-loop backoff + hallucinated-tool refusal.
    #[serde(default)]
    pub loop_guard: LoopGuardConfig,
    /// Per-session goal subsystem tunables.
    #[serde(default)]
    pub goals: GoalsConfig,
    /// Subprocess process registry — orphan thresholds and auto-reap
    /// whitelist. Consumed by the maintenance sweep.
    #[serde(default)]
    pub process_registry: ProcessRegistryConfig,
    /// Daemon-wide data retention + backup policy.
    #[serde(default)]
    pub data: DataConfig,
    /// Memory-consolidation runtime state (last-run timestamps).
    #[serde(default)]
    pub consolidation: ConsolidationConfig,
    /// Self-update prompt state (skipped-version sentinel).
    #[serde(default)]
    pub update: UpdateConfig,
    /// LAN mDNS announce + Tailscale peer enumeration.
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    /// Tunneling (cloudflared / ngrok / tailscale / custom) for exposing the
    /// gateway HTTP port publicly.
    #[serde(default)]
    pub tunnel: TunnelConfig,
    /// APNs push pipeline for waking offline iOS clients.
    #[serde(default)]
    pub apns: ApnsConfig,
    /// TUI presentation settings (currently: active theme name).
    #[serde(default)]
    pub ui: UiConfig,
    /// Credential store for resolving secrets from env, file, exec, or keychain.
    #[serde(default)]
    pub credentials: HashMap<String, CredentialValue>,
    /// Keys saved per provider by `Admin.SetApiKey` (`credentials.{provider}`
    /// rows), keyed by lowercase provider id. Kept beside the LLM fold so a
    /// voice or phone provider finds the key Settings saved for it.
    #[serde(skip)]
    pub stored_api_keys: HashMap<String, ApiKeyRouting>,
    /// Transient identity override (not serialized). Set by gateway routing.
    #[serde(skip)]
    pub identity_override: Option<std::path::PathBuf>,
}

impl Config {
    /// Point the LLM route at `provider`, as a unit with its credentials.
    ///
    /// A same-provider call is a no-op: the operator's endpoint override and
    /// keys still apply. Switching provider drops the previous provider's
    /// `base_url`, `api_key`, `api_keys`, and re-points `api_key_env` at the
    /// new provider's own default variable. Carrying any of them across is
    /// worse than failing: a stale `base_url` sends the new model to the old
    /// endpoint, a stale key is either sent to a host it was never meant for
    /// (a credential leak) or makes `correct_provider_from_key` silently route
    /// the request straight back to the old provider — the override looks
    /// applied and isn't. Every override lane (compaction, sub-agent roles,
    /// scheduled-task pins) funnels through here so none of them can regress
    /// on its own.
    pub fn route_llm_to_provider(&mut self, provider: &str) {
        if self.llm.provider.as_deref() == Some(provider) {
            return;
        }
        match Provider::from_str(provider) {
            Ok(parsed) => self.llm.route_to_provider(parsed),
            Err(e) => {
                // Keeping the old provider's variable would send its key to
                // whatever this name resolves to; the default name is the
                // "unset" marker `resolve_provider` ranks last.
                warn!(provider, error = %e, "unknown LLM provider; route keeps no credentials");
                self.llm
                    .reset_route_credentials(crate::constants::DEFAULT_LLM_API_KEY_ENV);
            }
        }
        self.llm.provider = Some(provider.to_string());
    }

    /// Applies compaction overrides to the LLM section; if none are set,
    /// returns a clone of self unchanged.
    pub fn with_compaction_overrides(&self) -> Config {
        if !self.compaction.has_overrides() {
            return self.clone();
        }
        let mut cfg = self.clone();
        if let Some(ref provider) = self.compaction.provider {
            cfg.route_llm_to_provider(provider);
        }
        if let Some(ref model) = self.compaction.model {
            cfg.llm.model = model.clone();
        }
        // An explicit compaction credential wins over the route's default.
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
    /// Returns the borg data directory.
    ///
    /// Delegates to [`paths::resolve_borg_home_readonly`] — see that module
    /// for the env-var resolution order. Does NOT create the directory.
    pub fn data_dir() -> Result<PathBuf> {
        paths::resolve_borg_home_readonly(None)
    }

    /// Returns the memory directory path (`~/.borg/memory/`).
    pub fn memory_dir() -> Result<PathBuf> {
        Ok(paths::subdir(&Self::data_dir()?, BorgSubdir::Memory))
    }

    /// Detect the host's IANA timezone via the OS.
    ///
    /// Returns `Some(name)` only if the OS reports a string that parses as a
    /// valid `chrono_tz::Tz` (e.g. `"America/New_York"`). Headless containers
    /// without a usable tzdata lookup return `None`, so callers should treat
    /// `None` as "leave the existing setting alone, fall back to UTC."
    pub fn detect_host_timezone() -> Option<String> {
        let raw = iana_time_zone::get_timezone().ok()?;
        raw.parse::<chrono_tz::Tz>().ok().map(|_| raw)
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
        Ok(paths::subdir(&Self::data_dir()?, BorgSubdir::Skills))
    }

    /// Returns the channels directory path (`~/.borg/channels/`).
    pub fn channels_dir() -> Result<PathBuf> {
        Ok(paths::subdir(&Self::data_dir()?, BorgSubdir::Channels))
    }

    /// Returns the scripts directory path (`~/.borg/scripts/`).
    pub fn scripts_dir() -> Result<PathBuf> {
        Ok(paths::subdir(&Self::data_dir()?, BorgSubdir::Scripts))
    }

    /// Returns the logs directory path (`~/.borg/logs/`).
    pub fn logs_dir() -> Result<PathBuf> {
        Ok(paths::subdir(&Self::data_dir()?, BorgSubdir::Logs))
    }

    /// Returns the sessions directory path (`~/.borg/sessions/`).
    pub fn sessions_dir() -> Result<PathBuf> {
        Ok(paths::subdir(&Self::data_dir()?, BorgSubdir::Sessions))
    }

    /// Returns the database file path (`~/.borg/borg.db`).
    pub fn db_path() -> Result<PathBuf> {
        Ok(paths::db_path(&Self::data_dir()?))
    }

    /// Returns the identity file path (`~/.borg/IDENTITY.md`).
    pub fn identity_path() -> Result<PathBuf> {
        Ok(paths::identity_path(&Self::data_dir()?))
    }

    /// Returns the memory index file path (`~/.borg/MEMORY.md`).
    pub fn memory_index_path() -> Result<PathBuf> {
        Ok(paths::memory_index_path(&Self::data_dir()?))
    }

    /// Load config from the database (DB overrides + compiled defaults).
    ///
    /// Opens the DB **read-only** so cli processes never race the daemon's
    /// writer. Consumers that need to mutate settings must go through the
    /// daemon over gRPC (`Admin.SetSetting` / `Admin.UnsetSetting`).
    pub async fn load_from_db() -> Result<Self> {
        let db = match crate::db::Database::open_readonly().await {
            Ok(db) => db,
            Err(e) => {
                // Fall back to the writer path so the daemon's own startup
                // (which legitimately needs to create the DB on first run)
                // keeps working. CLI processes hit this path only when the
                // daemon hasn't created the DB yet.
                tracing::debug!(error = %e, "open_readonly failed; falling back to open()");
                crate::db::Database::open().await?
            }
        };
        let rows = db.list_settings().await?;
        let config = Self::from_setting_rows(rows.into_iter().map(|(k, v, _)| (k, v)));
        config.validate()?;
        Ok(config)
    }

    /// Compiled defaults overlaid with settings-table rows.
    ///
    /// Every loader that replays the settings table goes through here, so the
    /// `credentials.{provider}` fold cannot be present in one and missing in
    /// another: a config watcher that skipped it would broadcast a config
    /// with no key moments after `Admin.SetApiKey` stored one.
    pub fn from_setting_rows(rows: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut config = Self::default();
        let mut stored_keys = Vec::new();
        for (key, value) in rows {
            // `credentials.{provider}` rows are per-provider key routing, not
            // registry settings; they are folded in once the provider is known.
            if let Some(provider) = key.strip_prefix(STORED_API_KEY_PREFIX) {
                stored_keys.push((provider.to_string(), value));
                continue;
            }
            if let Err(e) = config.apply_setting(&key, &value) {
                crate::settings::warn_invalid_setting_once(&key, &e);
            }
        }
        config.adopt_stored_api_key(&stored_keys);
        config.keep_stored_api_keys(&stored_keys);
        config.adopt_default_model();
        config
    }

    /// Remember every parseable `credentials.{provider}` row for
    /// [`Config::resolve_provider_key`]. A malformed row is already reported
    /// by [`Config::adopt_stored_api_key`] when it is the LLM's, so it is only
    /// logged at debug here.
    fn keep_stored_api_keys(&mut self, rows: &[(String, String)]) {
        for (provider, json) in rows {
            match serde_json::from_str::<ApiKeyRouting>(json) {
                Ok(routing) => {
                    self.stored_api_keys
                        .insert(provider.to_ascii_lowercase(), routing);
                }
                Err(e) => tracing::debug!("{STORED_API_KEY_PREFIX}{provider} skipped: {e}"),
            }
        }
    }

    /// Resolve the API key of a non-LLM provider (speech, transcription,
    /// telephony).
    ///
    /// Each of `env_names` (the provider's conventional variables, or a
    /// per-row override) is tried in order through
    /// [`Config::resolve_credential_or_env`], so an operator's explicit
    /// routing is never overridden; the key Settings saved for `provider` is
    /// the fallback. Because chat and voice read the same
    /// `credentials.{provider}` row, an OpenAI key saved for chat also serves
    /// OpenAI speech.
    pub fn resolve_provider_key(&self, provider: &str, env_names: &[&str]) -> Option<String> {
        if let Some(key) = env_names
            .iter()
            .find_map(|name| self.resolve_credential_or_env(name))
        {
            return Some(key);
        }
        let routing = self.stored_api_keys.get(&provider.to_ascii_lowercase())?;
        match SecretRef::from(routing.clone()).resolve() {
            Ok(v) if !v.is_empty() => Some(v),
            Ok(_) => None,
            Err(e) => {
                warn!("Saved key for {provider} could not be read: {e}");
                None
            }
        }
    }

    /// Fill an unset `model` with the provider's recommended catalog model.
    ///
    /// A provider picked without a model (`borg settings set provider …`, or a
    /// Settings change that never reached the model picker) otherwise sends an
    /// empty model id, and every turn and scheduled run fails upstream with a
    /// 400. Providers with no recommended model (custom, local) stay empty.
    pub fn adopt_default_model(&mut self) {
        if !self.llm.model.is_empty() {
            return;
        }
        let Some(provider) = self.llm.provider.as_deref() else {
            return;
        };
        if let Some(model) = crate::llm::model_catalog::default_model(provider) {
            self.llm.model = model;
        }
    }

    /// Fold the `credentials.{provider}` row written by `Admin.SetApiKey`
    /// into `llm.api_key` when nothing more explicit is configured.
    ///
    /// The desktop and web onboarding store a key this way and nothing else;
    /// without this fold a GUI-onboarded install finishes setup and then has
    /// no key to send. Because the row is keyed by provider, switching the
    /// provider in Settings picks up that provider's saved key on its own.
    /// An explicit `llm.api_key` still wins, so an operator's own routing is
    /// never overridden.
    pub fn adopt_stored_api_key(&mut self, rows: &[(String, String)]) {
        if self.llm.api_key.is_some() {
            return;
        }
        let Some(provider) = self.llm.provider.as_deref() else {
            return;
        };
        let Some((_, json)) = rows.iter().find(|(p, _)| p.eq_ignore_ascii_case(provider)) else {
            return;
        };
        match serde_json::from_str::<ApiKeyRouting>(json) {
            Ok(routing) => self.llm.api_key = Some(routing.into()),
            Err(e) => warn!("{STORED_API_KEY_PREFIX}{provider} is not valid key routing JSON: {e}"),
        }
    }

    /// Load config from a specific TOML file path.
    #[cfg(test)]
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
    #[cfg(test)]
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
                "No LLM provider selected. Open `/settings` or `/model` in the TUI to pick one (OpenRouter, OpenAI, Anthropic, Bedrock, Azure OpenAI, Vertex AI, Gemini, DeepSeek, Groq, xAI, Ollama, or Claude CLI). Borg won't guess."
            );
        };
        let provider = Provider::from_str(provider_str)?;

        if provider.uses_aws_credential_chain() {
            self.resolve_bedrock_region()?;
            return Ok((provider, String::new()));
        }

        // Keyless providers (e.g., Ollama, Claude CLI) don't need API key resolution.
        if !provider.requires_api_key() {
            return Ok((provider, String::new()));
        }

        // `api_key_env` defaults to `OPENROUTER_API_KEY`, and every saved
        // config carries that default whether anyone chose it or not. Treated
        // as explicit, it outranks the chosen provider's own variable: pick
        // Anthropic on a machine that has both keys and the OpenRouter key is
        // sent, `correct_provider_from_key` reroutes the turn to OpenRouter,
        // and the user is billed by a provider they just moved away from. So
        // the default only counts after the provider's own variable.
        let default_env = self.llm.api_key_env == crate::constants::DEFAULT_LLM_API_KEY_ENV;
        if let Some(key) = self
            .try_resolve_api_key_secret_ref("api_key_env")
            .or_else(|| (!default_env).then(|| try_resolve_env(&self.llm.api_key_env))?)
            .or_else(|| try_resolve_env(provider.default_env_var()))
            .or_else(|| {
                // For a Custom endpoint, the built-in default must not apply
                // at all: it would send an unrelated provider's key as a
                // Bearer credential to a user-supplied URL. An operator who
                // genuinely wants that env var there can still say so via
                // `llm.api_key = { source = "env", var = "OPENROUTER_API_KEY" }`
                // — we can't distinguish "explicitly set to the default" here.
                (default_env && provider != Provider::Custom)
                    .then(|| try_resolve_env(&self.llm.api_key_env))?
            })
        {
            return Ok((provider, key));
        }

        if provider.supports_cloud_auth() {
            return Ok((provider, String::new()));
        }

        // Custom endpoints (llama.cpp server, LM Studio, vLLM, …) are
        // commonly keyless local servers: with `llm.base_url` set, proceed
        // without a key — an endpoint that does require auth will reject the
        // request with its own message.
        if provider == Provider::Custom {
            if self
                .llm
                .base_url
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
            {
                return Ok((provider, String::new()));
            }
            anyhow::bail!(
                "Provider `custom` needs `llm.base_url` pointing at an OpenAI-compatible endpoint (e.g. http://localhost:8080/v1/chat/completions for llama.cpp). Set it via `/settings` or `borg settings set llm.base_url <url>`; add an API key only if the endpoint requires one."
            );
        }

        anyhow::bail!(
            "No API key found for {provider}. Open `/settings` in the TUI and paste your key under \"API key\", or export {} in your shell and restart borg.",
            provider.default_env_var()
        );
    }

    /// Resolve the Bedrock region from config, env, or the AWS shared config
    /// chain. Kept in sync with the Bedrock transport so early validation
    /// accepts the same setups that runtime requests do.
    pub fn resolve_bedrock_region(&self) -> Result<String> {
        crate::llm::bedrock::resolve_region(self.llm.bedrock_region.as_deref()).ok_or_else(|| {
                anyhow::anyhow!(
                    "No AWS region found for Bedrock. Set llm.bedrock_region in `/settings`, or export AWS_REGION / AWS_DEFAULT_REGION."
                )
            })
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
        // Keyless and AWS-chain providers never consult `llm.api_keys`: a
        // stray key row must not flip the operator's explicit provider choice
        // via `correct_provider_from_key` — provider resolution is strict.
        if let Some(provider) = self
            .llm
            .provider
            .as_deref()
            .and_then(|s| Provider::from_str(s).ok())
        {
            if !provider.requires_api_key() {
                let (provider, key) = self.resolve_provider()?;
                return Ok((provider, vec![key]));
            }
        }

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

    /// Maps credential key → plugin ID for keychain service-name derivation.
    /// Plugin IDs must match those in the plugin catalog (`crates/plugins/src/catalog.rs`)
    /// and the gateway's `resolve_credential` call sites (`crates/gateway/src/channel/channel_init.rs`).
    pub const KEY_PLUGIN_MAP: &'static [(&'static str, &'static str)] = &[
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

    /// Returns the plugin ID associated with a credential key, if any.
    pub fn keychain_plugin_for_key(key: &str) -> Option<&'static str> {
        Self::KEY_PLUGIN_MAP
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, pid)| *pid)
    }

    /// Try to resolve a credential from the OS keychain using the plugin naming
    /// convention: service = `borg-{plugin_id with / → -}`, account = `borg-{key}`.
    fn resolve_keychain_fallback(&self, key: &str) -> Option<String> {
        let plugin_id = Self::keychain_plugin_for_key(key)?;

        let service = format!("borg-{}", plugin_id.replace('/', "-"));
        let account = format!("borg-{key}");

        let sr = SecretRef::Keychain {
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
                tracing::trace!("Keychain fallback for {key} failed: {e}");
            }
        }

        // Final fallback: the on-disk credentials file written by
        // `plugins::credential_store` when the keychain was unavailable
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
///
/// An example placeholder (`your-api-key-here`) also counts as unset, so a
/// copied example file gives the "No API key found" guidance and the
/// placeholder does not go to a provider.
fn try_resolve_env(var: &str) -> Option<String> {
    let value = std::env::var(var).ok().filter(|v| !v.is_empty())?;
    if crate::llm::credential_detect::is_placeholder_secret(&value) {
        warn!("{var} holds an example placeholder, not a key; ignoring it");
        return None;
    }
    Some(value)
}

/// Read a credential from the on-disk fallback file
/// (`~/.borg/.credentials.json`) written by the plugin installer when the OS
/// keychain was unavailable. Mirror of `plugins::credential_store::read`'s
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
    // `custom` names an explicit operator endpoint; a recognizable key prefix
    // (e.g. an OpenRouter key used as the endpoint's bearer credential) must
    // not silently reroute traffic away from it.
    if configured == Provider::Custom {
        return configured;
    }
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

#[cfg(test)]
mod route_llm_tests {
    use super::*;

    fn routed_from_openai() -> Config {
        let mut cfg = Config::default();
        cfg.llm.provider = Some("openai".into());
        cfg.llm.base_url = Some("https://proxy.example/v1".into());
        cfg.llm.api_key = Some(SecretRef::Env {
            var: "OLD_PRIMARY_KEY".into(),
        });
        cfg.llm.api_keys = vec![SecretRef::Env {
            var: "OLD_POOL_KEY".into(),
        }];
        cfg.llm.api_key_env = "OPENAI_API_KEY".into();
        cfg
    }

    #[test]
    fn route_to_known_provider_switches_credentials_as_a_unit() {
        let mut cfg = routed_from_openai();
        cfg.route_llm_to_provider("anthropic");
        assert_eq!(cfg.llm.provider.as_deref(), Some("anthropic"));
        assert_eq!(cfg.llm.base_url, None);
        assert!(cfg.llm.api_key.is_none());
        assert!(cfg.llm.api_keys.is_empty());
        assert_eq!(
            cfg.llm.api_key_env,
            Provider::Anthropic.default_env_var(),
            "the new provider's own variable must replace the old one"
        );
    }

    #[test]
    fn route_to_unknown_provider_drops_the_old_env_var() {
        // A typo'd provider used to keep `OPENAI_API_KEY`, so the OpenAI key
        // was resolved for a route that is not OpenAI.
        let mut cfg = routed_from_openai();
        cfg.route_llm_to_provider("not-a-provider");
        assert_eq!(cfg.llm.provider.as_deref(), Some("not-a-provider"));
        assert!(cfg.llm.api_key.is_none());
        assert!(cfg.llm.api_keys.is_empty());
        assert_eq!(cfg.llm.base_url, None);
        assert_eq!(
            cfg.llm.api_key_env,
            crate::constants::DEFAULT_LLM_API_KEY_ENV,
            "an unknown provider must fall back to the unset marker"
        );
    }

    #[test]
    fn route_to_same_provider_keeps_overrides() {
        let mut cfg = routed_from_openai();
        cfg.route_llm_to_provider("openai");
        assert_eq!(
            cfg.llm.base_url.as_deref(),
            Some("https://proxy.example/v1")
        );
        assert!(
            matches!(&cfg.llm.api_key, Some(SecretRef::Env { var }) if var == "OLD_PRIMARY_KEY"),
            "same-provider route must keep the configured key"
        );
        assert_eq!(cfg.llm.api_keys.len(), 1);
        assert_eq!(cfg.llm.api_key_env, "OPENAI_API_KEY");
    }
}
