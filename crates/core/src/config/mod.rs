pub mod gateway;
pub mod llm;
pub mod media;
pub mod security;

#[cfg(test)]
mod tests;

pub use gateway::*;
pub use llm::*;
pub use media::*;
pub use security::*;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tracing::warn;

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
    /// Compaction model overrides (use cheaper model for context compaction).
    #[serde(default)]
    pub compaction: CompactionConfig,
    /// Workflow engine settings (durable multi-step orchestration).
    #[serde(default)]
    pub workflow: WorkflowConfig,
    /// Conversation evolution and personality drift settings.
    #[serde(default)]
    pub evolution: EvolutionConfig,
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

fn parse_value<T: FromStr>(value: &str, key: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|e: T::Err| anyhow::anyhow!("Invalid value for {key}: {e}"))
}

fn parse_nonzero<T: FromStr + Default + PartialEq>(value: &str, key: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    let v = parse_value::<T>(value, key)?;
    if v == T::default() {
        anyhow::bail!("{key} must be greater than 0");
    }
    Ok(v)
}

fn parse_range<T: FromStr + PartialOrd + std::fmt::Display>(
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
        Ok(Self::data_dir()?.join("borg.db"))
    }

    /// Returns the identity file path (`~/.borg/IDENTITY.md`).
    pub fn identity_path() -> Result<PathBuf> {
        Ok(Self::data_dir()?.join("IDENTITY.md"))
    }

    /// Returns the memory index file path (`~/.borg/MEMORY.md`).
    pub fn memory_index_path() -> Result<PathBuf> {
        Ok(Self::data_dir()?.join("MEMORY.md"))
    }

    /// Load config from the database (DB overrides + compiled defaults).
    pub fn load_from_db() -> Result<Self> {
        let db = crate::db::Database::open()?;
        let mut config = Self::default();
        for (key, value, _) in db.list_settings()? {
            if let Err(e) = config.apply_setting(&key, &value) {
                tracing::warn!("Ignoring invalid setting {key}: {e}");
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

    /// Apply a single key=value setting, returning a confirmation string.
    pub fn apply_setting(&mut self, key: &str, value: &str) -> Result<String> {
        // Helper macros to reduce repetition across the ~100 match arms.
        macro_rules! set_parsed {
            ($field:expr, $T:ty) => {{
                $field = parse_value::<$T>(value, key)?;
                Ok(format!("{key} = {}", $field))
            }};
        }
        macro_rules! set_string {
            ($field:expr) => {{
                $field = value.to_string();
                Ok(format!("{key} = {value}"))
            }};
        }
        macro_rules! set_opt_string {
            ($field:expr) => {{
                $field = if value.is_empty() {
                    None
                } else {
                    Some(value.to_string())
                };
                Ok(format!("{key} = {value}"))
            }};
        }
        macro_rules! set_range {
            ($field:expr, $T:ty, $min:expr, $max:expr) => {{
                $field = parse_range(value, key, $min, $max)?;
                Ok(format!("{key} = {}", $field))
            }};
        }
        macro_rules! set_nonzero {
            ($field:expr, $T:ty) => {{
                $field = parse_nonzero::<$T>(value, key)?;
                Ok(format!("{key} = {}", $field))
            }};
        }
        macro_rules! set_json {
            ($field:expr) => {{
                $field = serde_json::from_str(value)
                    .with_context(|| format!("Invalid JSON for {key}"))?;
                Ok(format!("{key} = {value}"))
            }};
            ($field:expr, set) => {{
                $field = serde_json::from_str(value)
                    .with_context(|| format!("Invalid JSON for {key}"))?;
                Ok(format!("{key} = (set)"))
            }};
            ($field:expr, count $label:expr) => {{
                $field = serde_json::from_str(value)
                    .with_context(|| format!("Invalid JSON for {key}"))?;
                Ok(format!("{key} = ({} {})", $field.len(), $label))
            }};
        }
        macro_rules! set_json_quoted {
            ($field:expr, $err:expr) => {{
                $field = serde_json::from_str(&format!("\"{value}\""))
                    .with_context(|| format!("{}: {value}", $err))?;
                Ok(format!("{key} = {value}"))
            }};
        }

        match key {
            // ── LLM core ──
            "model" => set_string!(self.llm.model),
            "provider" => {
                self.llm.provider = Some(value.to_string());
                Ok(format!("{key} = {value}"))
            }
            "temperature" => set_range!(self.llm.temperature, f32, 0.0_f32, 2.0),
            "max_tokens" => set_nonzero!(self.llm.max_tokens, u32),
            "llm.api_key_env" => set_string!(self.llm.api_key_env),
            "llm.max_retries" => set_parsed!(self.llm.max_retries, u32),
            "llm.initial_retry_delay_ms" => set_parsed!(self.llm.initial_retry_delay_ms, u64),
            "llm.request_timeout_ms" => set_parsed!(self.llm.request_timeout_ms, u64),
            "llm.stream_chunk_timeout_secs" => set_parsed!(self.llm.stream_chunk_timeout_secs, u64),
            "llm.claude_cli_path" => set_opt_string!(self.llm.claude_cli_path),
            "llm.base_url" => set_opt_string!(self.llm.base_url),
            "llm.thinking" => set_json_quoted!(self.llm.thinking, "Invalid thinking level"),
            "llm.fallback" => set_json!(self.llm.fallback, count "providers"),
            "llm.api_key" => {
                if value.is_empty() {
                    self.llm.api_key = None;
                } else {
                    self.llm.api_key = Some(
                        serde_json::from_str(value)
                            .with_context(|| format!("Invalid JSON for {key}"))?,
                    );
                }
                Ok(format!("{key} = (set)"))
            }
            "llm.api_keys" => set_json!(self.llm.api_keys, count "keys"),

            // ── LLM cache ──
            "llm.cache.enabled" => set_parsed!(self.llm.cache.enabled, bool),
            "llm.cache.ttl" => set_json_quoted!(self.llm.cache.ttl, "Invalid cache TTL"),
            "llm.cache.cache_tools" => set_parsed!(self.llm.cache.cache_tools, bool),
            "llm.cache.cache_system" => set_parsed!(self.llm.cache.cache_system, bool),
            "llm.cache.rolling_messages" => set_parsed!(self.llm.cache.rolling_messages, u8),

            // ── Sandbox ──
            "sandbox.enabled" => set_parsed!(self.sandbox.enabled, bool),
            "sandbox.mode" => {
                match value {
                    "strict" | "permissive" => {}
                    other => {
                        anyhow::bail!("Unknown sandbox mode '{other}'. Valid: strict, permissive")
                    }
                }
                self.sandbox.mode = value.to_string();
                Ok(format!("{key} = {value}"))
            }

            // ── Memory ──
            "memory.max_context_tokens" => set_nonzero!(self.memory.max_context_tokens, usize),
            "memory.flush_before_compaction" => {
                set_parsed!(self.memory.flush_before_compaction, bool)
            }
            "memory.flush_min_messages" => set_parsed!(self.memory.flush_min_messages, usize),
            "memory.flush_soft_threshold_tokens" => {
                set_parsed!(self.memory.flush_soft_threshold_tokens, usize)
            }
            "memory.chunk_level_selection" => set_parsed!(self.memory.chunk_level_selection, bool),
            "memory.extra_paths" => {
                let paths: Vec<String> = value
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                self.memory.extra_paths = paths.clone();
                Ok(format!("{key} = {}", paths.join(", ")))
            }

            // ── Memory embeddings ──
            "memory.embeddings.enabled" => set_parsed!(self.memory.embeddings.enabled, bool),
            "memory.embeddings.mmr_enabled" => {
                set_parsed!(self.memory.embeddings.mmr_enabled, bool)
            }
            "memory.embeddings.mmr_lambda" => {
                set_range!(self.memory.embeddings.mmr_lambda, f32, 0.0_f32, 1.0)
            }
            "memory.embeddings.recency_weight" => {
                set_range!(self.memory.embeddings.recency_weight, f32, 0.0_f32, 1.0)
            }
            "memory.embeddings.bm25_weight" => {
                set_range!(self.memory.embeddings.bm25_weight, f32, 0.0_f32, 1.0)
            }
            "memory.embeddings.vector_weight" => {
                set_range!(self.memory.embeddings.vector_weight, f32, 0.0_f32, 1.0)
            }
            "memory.embeddings.chunk_size_tokens" => {
                set_parsed!(self.memory.embeddings.chunk_size_tokens, usize)
            }
            "memory.embeddings.chunk_overlap_tokens" => {
                set_parsed!(self.memory.embeddings.chunk_overlap_tokens, usize)
            }

            // ── Skills ──
            "skills.enabled" => set_parsed!(self.skills.enabled, bool),
            "skills.max_context_tokens" => set_parsed!(self.skills.max_context_tokens, usize),
            key if key.starts_with("skills.entries.") && key.ends_with(".enabled") => {
                let name = key
                    .strip_prefix("skills.entries.")
                    .and_then(|s| s.strip_suffix(".enabled"))
                    .ok_or_else(|| anyhow::anyhow!("Invalid skill entry key: {key}"))?
                    .to_string();
                let enabled = parse_value::<bool>(value, key)?;
                self.skills.entries.entry(name).or_default().enabled = enabled;
                Ok(format!("{key} = {enabled}"))
            }

            // ── Conversation ──
            "conversation.max_iterations" => set_parsed!(self.conversation.max_iterations, u32),
            "conversation.show_thinking" => set_parsed!(self.conversation.show_thinking, bool),
            "conversation.max_history_tokens" => {
                set_parsed!(self.conversation.max_history_tokens, usize)
            }
            "conversation.tool_output_max_tokens" => {
                set_parsed!(self.conversation.tool_output_max_tokens, usize)
            }
            "conversation.compaction_marker_tokens" => {
                set_parsed!(self.conversation.compaction_marker_tokens, usize)
            }
            "conversation.max_transcript_chars" => {
                set_parsed!(self.conversation.max_transcript_chars, usize)
            }
            "conversation.age_based_degradation" => {
                set_parsed!(self.conversation.age_based_degradation, bool)
            }
            "conversation.collaboration_mode" => {
                let mode: CollaborationMode = value.parse()?;
                self.conversation.collaboration_mode = mode;
                Ok(format!("{key} = {mode}"))
            }

            // ── Security ──
            "security.secret_detection" => set_parsed!(self.security.secret_detection, bool),
            "security.host_audit" => set_parsed!(self.security.host_audit, bool),
            "security.blocked_paths" => set_json!(self.security.blocked_paths),
            "security.allowed_paths" => set_json!(self.security.allowed_paths),
            "security.action_limits" => set_json!(self.security.action_limits, set),
            "security.gateway_action_limits" => set_json!(self.security.gateway_action_limits, set),

            // ── Budget ──
            "budget.monthly_token_limit" => set_parsed!(self.budget.monthly_token_limit, u64),
            "budget.warning_threshold" => {
                set_range!(self.budget.warning_threshold, f64, 0.0_f64, 1.0)
            }

            // ── Browser ──
            "browser.enabled" => set_parsed!(self.browser.enabled, bool),
            "browser.headless" => set_parsed!(self.browser.headless, bool),

            // ── TTS ──
            "tts.enabled" => set_parsed!(self.tts.enabled, bool),
            "tts.auto_mode" => set_parsed!(self.tts.auto_mode, bool),
            "tts.default_voice" => set_string!(self.tts.default_voice),
            "tts.max_text_length" => set_parsed!(self.tts.max_text_length, usize),
            "tts.timeout_ms" => set_parsed!(self.tts.timeout_ms, u64),
            "tts.models" => set_json!(self.tts.models, count "models"),
            "tts.default_format" => {
                let allowed = ["mp3", "opus", "aac", "flac", "wav"];
                if !allowed.contains(&value) {
                    anyhow::bail!("Invalid format: {value}. Allowed: {}", allowed.join(", "));
                }
                self.tts.default_format = value.to_string();
                Ok(format!("{key} = {value}"))
            }

            // ── Evolution ──
            "evolution.enabled" => set_parsed!(self.evolution.enabled, bool),

            // ── Workflow ──
            "workflow.enabled" => match value {
                "auto" | "on" | "off" => {
                    self.workflow.enabled = value.to_string();
                    Ok(format!("{key} = {value}"))
                }
                _ => anyhow::bail!(
                    "Invalid value for workflow.enabled: {value}. Use 'auto', 'on', or 'off'."
                ),
            },

            // ── Tools ──
            "tools.default_timeout_ms" => set_parsed!(self.tools.default_timeout_ms, u64),
            "tools.conditional_loading" => set_parsed!(self.tools.conditional_loading, bool),
            "tools.compact_schemas" => set_parsed!(self.tools.compact_schemas, bool),
            "tools.policy.profile" => set_string!(self.tools.policy.profile),
            "tools.policy.allow" => set_json!(self.tools.policy.allow),
            "tools.policy.deny" => set_json!(self.tools.policy.deny),
            "tools.policy.subagent_deny" => set_json!(self.tools.policy.subagent_deny),

            // ── Heartbeat ──
            "heartbeat.interval" => set_string!(self.heartbeat.interval),
            "heartbeat.quiet_hours_start" => set_opt_string!(self.heartbeat.quiet_hours_start),
            "heartbeat.quiet_hours_end" => set_opt_string!(self.heartbeat.quiet_hours_end),
            "heartbeat.cron" => set_opt_string!(self.heartbeat.cron),
            "heartbeat.channels" => set_json!(self.heartbeat.channels),
            "heartbeat.recipients" => set_json!(self.heartbeat.recipients, set),

            // ── User ──
            "user.name" => set_opt_string!(self.user.name),
            "user.agent_name" => set_opt_string!(self.user.agent_name),
            "user.timezone" => set_opt_string!(self.user.timezone),

            // ── Web ──
            "web.enabled" => set_parsed!(self.web.enabled, bool),
            "web.search_provider" => set_string!(self.web.search_provider),

            // ── Tasks ──
            "tasks.max_concurrent" => set_parsed!(self.tasks.max_concurrent, usize),

            // ── Gateway ──
            "gateway.host" => set_string!(self.gateway.host),
            "gateway.port" => set_parsed!(self.gateway.port, u16),
            "gateway.max_concurrent" => set_parsed!(self.gateway.max_concurrent, usize),
            "gateway.request_timeout_ms" => set_parsed!(self.gateway.request_timeout_ms, u64),
            "gateway.rate_limit_per_minute" => set_parsed!(self.gateway.rate_limit_per_minute, u32),
            "gateway.public_url" => set_opt_string!(self.gateway.public_url),
            "gateway.pairing_ttl_secs" => set_parsed!(self.gateway.pairing_ttl_secs, i64),
            "gateway.error_cooldown_ms" => set_parsed!(self.gateway.error_cooldown_ms, u64),
            "gateway.error_policy" => {
                self.gateway.error_policy = value.parse()?;
                Ok(format!("{key} = {value}"))
            }
            "gateway.dm_policy" => set_json_quoted!(self.gateway.dm_policy, "Invalid DM policy"),
            "gateway.group_activation" => {
                set_json_quoted!(self.gateway.group_activation, "Invalid activation mode")
            }
            "gateway.bindings" => set_json!(self.gateway.bindings, count "bindings"),
            "gateway.channel_policies" => set_json!(self.gateway.channel_policies, set),
            "gateway.auto_reply" => set_json!(self.gateway.auto_reply, set),
            "gateway.link_understanding" => set_json!(self.gateway.link_understanding, set),
            "gateway.channel_error_policies" => set_json!(self.gateway.channel_error_policies, set),

            // ── Agents ──
            "agents.enabled" => set_parsed!(self.agents.enabled, bool),
            "agents.max_spawn_depth" => set_parsed!(self.agents.max_spawn_depth, u32),
            "agents.max_children_per_agent" => set_parsed!(self.agents.max_children_per_agent, u32),
            "agents.max_concurrent" => set_parsed!(self.agents.max_concurrent, u32),

            // ── Debug ──
            "debug.llm_logging" => set_parsed!(self.debug.llm_logging, bool),

            // ── Audio ──
            "audio.enabled" => set_parsed!(self.audio.enabled, bool),
            "audio.models" => set_json!(self.audio.models, count "models"),

            // ── Media ──
            "media.max_image_bytes" => set_parsed!(self.media.max_image_bytes, usize),
            "media.compression_enabled" => set_parsed!(self.media.compression_enabled, bool),
            "media.max_dimension_px" => set_parsed!(self.media.max_dimension_px, u32),

            // ── Image Gen ──
            "image_gen.enabled" => set_parsed!(self.image_gen.enabled, bool),
            "image_gen.default_size" => set_string!(self.image_gen.default_size),

            // ── Scripts ──
            "scripts.enabled" => set_parsed!(self.scripts.enabled, bool),
            "scripts.default_timeout_ms" => set_parsed!(self.scripts.default_timeout_ms, u64),

            // ── Compaction ──
            "compaction.provider" => set_opt_string!(self.compaction.provider),
            "compaction.model" => set_opt_string!(self.compaction.model),

            // ── Plugins ──
            "plugins.enabled" => set_parsed!(self.plugins.enabled, bool),
            "plugins.auto_verify" => set_parsed!(self.plugins.auto_verify, bool),

            // ── Credentials ──
            "credentials" => set_json!(self.credentials, count "entries"),

            _ => anyhow::bail!(
                "Unknown setting: {key}\nAvailable: {}",
                crate::settings::ALL_SETTING_KEYS.join(", ")
            ),
        }
    }

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

    /// Resolve the provider and API key from config + environment.
    /// Resolution priority: api_key (SecretRef) → api_key_env → provider default env var → auto-detect.
    pub fn resolve_provider(&self) -> Result<(Provider, String)> {
        if let Some(ref provider_str) = self.llm.provider {
            let provider = Provider::from_str(provider_str)?;

            // Keyless providers (e.g., Ollama) don't need API key resolution
            if !provider.requires_api_key() {
                return Ok((provider, String::new()));
            }

            // Try SecretRef first
            if let Some(ref secret_ref) = self.llm.api_key {
                match secret_ref.resolve() {
                    Ok(key) if !key.is_empty() => return Ok((provider, key)),
                    Ok(_) => {
                        warn!("api_key SecretRef resolved to empty string, falling back to api_key_env");
                    }
                    Err(e) => {
                        warn!(
                            "api_key SecretRef failed to resolve: {e}, falling back to api_key_env"
                        );
                    }
                }
            }

            let key = std::env::var(&self.llm.api_key_env)
                .or_else(|_| std::env::var(provider.default_env_var()))
                .with_context(|| {
                    format!(
                        "API key not found for provider {provider}. Set {} or {} or configure api_key in config.toml.",
                        self.llm.api_key_env,
                        provider.default_env_var()
                    )
                })?;
            return Ok((provider, key));
        }

        // Try SecretRef with auto-detect
        if let Some(ref secret_ref) = self.llm.api_key {
            match secret_ref.resolve() {
                Ok(key) if !key.is_empty() => {
                    // Infer provider from api_key_env name
                    let provider = Provider::from_env_var_name(&self.llm.api_key_env)
                        .unwrap_or_else(|| {
                            warn!("Could not infer provider from api_key_env '{}', defaulting to OpenRouter", self.llm.api_key_env);
                            Provider::OpenRouter
                        });
                    return Ok((provider, key));
                }
                Ok(_) => {
                    warn!(
                        "api_key SecretRef resolved to empty string, falling back to env detection"
                    );
                }
                Err(e) => {
                    warn!(
                        "api_key SecretRef failed to resolve: {e}, falling back to env detection"
                    );
                }
            }
        }

        if self.llm.api_key_env != LlmConfig::default().api_key_env {
            if let Ok(key) = std::env::var(&self.llm.api_key_env) {
                if !key.is_empty() {
                    let provider = Provider::from_env_var_name(&self.llm.api_key_env)
                        .unwrap_or_else(|| {
                            warn!("Could not infer provider from api_key_env '{}', defaulting to OpenRouter", self.llm.api_key_env);
                            Provider::OpenRouter
                        });
                    return Ok((provider, key));
                }
            }
        }

        Provider::detect_from_env()
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
                let provider = if let Some(ref provider_str) = self.llm.provider {
                    Provider::from_str(provider_str)?
                } else {
                    // Infer provider via resolve_provider; ignore errors since we have keys
                    self.resolve_provider().map(|(p, _)| p).unwrap_or_else(|_| {
                        warn!("Could not infer provider from config, defaulting to OpenRouter");
                        Provider::OpenRouter
                    })
                };
                return Ok((provider, keys));
            }
        }

        // Fall back to single key via resolve_provider
        let (provider, key) = self.resolve_provider()?;
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

    /// Resolve a credential by name: credential store first, then env var fallback.
    /// Returns `None` if not found or empty.
    pub fn resolve_credential_or_env(&self, name: &str) -> Option<String> {
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
        std::env::var(name).ok().filter(|t| !t.is_empty())
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
