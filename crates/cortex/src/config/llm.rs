use serde::{Deserialize, Serialize};

use crate::constants::{
    DEFAULT_HEARTBEAT_INTERVAL, DEFAULT_HEARTBEAT_QUIET_END, DEFAULT_HEARTBEAT_QUIET_START,
    DEFAULT_LLM_API_KEY_ENV, DEFAULT_LLM_INITIAL_RETRY_DELAY_MS, DEFAULT_LLM_MAX_RETRIES,
    DEFAULT_LLM_MAX_TOKENS, DEFAULT_LLM_REQUEST_TIMEOUT_MS, DEFAULT_LLM_STREAM_CHUNK_TIMEOUT_SECS,
    DEFAULT_LLM_TEMPERATURE,
};
use crate::security::secrets_resolve::SecretRef;

/// Extended thinking level. Mapped per-provider at request-build time:
/// Anthropic `budget_tokens`, OpenAI `reasoning_effort`, Gemini 2.5
/// `thinkingBudget` (integer; `-1` is the dynamic-thinking sentinel), and
/// Gemini 3 `thinkingLevel` (`"LOW"`/`"MEDIUM"`/`"HIGH"`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    /// Smallest enabled budget. OpenAI: `"minimal"` (GPT-5 only).
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    /// Let the provider decide. Gemini 2.5: dynamic (`thinkingBudget: -1`).
    /// Anthropic/OpenAI have no native adaptive mode — mapped to a moderate
    /// default so the model can still spend tokens if it chooses.
    Adaptive,
    /// Provider's practical ceiling.
    Max,
}

impl ThinkingLevel {
    /// Anthropic `budget_tokens`. `None` when thinking is off. Minimum
    /// Anthropic accepts is 1024 so Minimal and Low both produce 1024 (they
    /// differ on OpenAI reasoning effort only). Existing Low/Medium/High/Xhigh
    /// budgets are unchanged from the pre-eight-level mapping so operator
    /// config keeps its old meaning.
    pub fn anthropic_budget_tokens(&self) -> Option<u32> {
        match self {
            Self::Off => None,
            Self::Minimal => Some(1024),
            Self::Low => Some(1024),
            Self::Medium => Some(4096),
            Self::High => Some(16384),
            Self::Xhigh => Some(32768),
            Self::Adaptive => Some(8192),
            Self::Max => Some(64000),
        }
    }

    /// OpenAI `reasoning_effort` string for o-series / GPT-5 models. `None`
    /// when thinking is off.
    pub fn openai_reasoning_effort(&self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Minimal => Some("minimal"),
            Self::Low => Some("low"),
            Self::Medium | Self::Adaptive => Some("medium"),
            Self::High | Self::Xhigh | Self::Max => Some("high"),
        }
    }

    /// Gemini 2.5 `thinkingBudget` (integer). `None` when thinking is off
    /// (caller omits the entire `thinkingConfig`). `-1` is Gemini's
    /// dynamic-thinking sentinel — let the model decide how much to spend.
    pub fn gemini_2_5_budget(&self) -> Option<i32> {
        match self {
            Self::Off => None,
            Self::Minimal => Some(512),
            Self::Low => Some(2048),
            Self::Medium => Some(8192),
            Self::High => Some(16384),
            Self::Xhigh => Some(24576),
            Self::Adaptive => Some(-1),
            Self::Max => Some(24576),
        }
    }

    /// Gemini 3 `thinkingLevel` enum. `None` when thinking is off.
    pub fn gemini_3_level(&self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Minimal | Self::Low => Some("LOW"),
            Self::Medium | Self::Adaptive => Some("MEDIUM"),
            Self::High | Self::Xhigh | Self::Max => Some("HIGH"),
        }
    }

    /// True for any variant other than `Off`.
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
    /// AWS Bedrock region override. When unset, Borg falls back to the
    /// standard AWS region chain (`AWS_REGION`, `AWS_DEFAULT_REGION`, profile).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bedrock_region: Option<String>,
    /// Azure OpenAI resource name — e.g. `"my-resource"` →
    /// `https://my-resource.openai.azure.com`. Required when `provider =
    /// azure-openai` unless `llm.base_url` is set directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub azure_resource: Option<String>,
    /// Azure OpenAI deployment name (operator-chosen label from the Azure
    /// portal). Becomes the `/deployments/{name}/` URL path segment.
    /// Required when `provider = azure-openai` unless `llm.base_url` is
    /// set directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub azure_deployment: Option<String>,
    /// Azure OpenAI API version — e.g. `"2024-10-21"`. Required by Azure on
    /// every request. No default: API-version drift between Azure releases
    /// changes the response shape, so an explicit version surface forces
    /// the operator to upgrade deliberately rather than silently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub azure_api_version: Option<String>,
    /// Google Cloud project id for Vertex AI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertex_project: Option<String>,
    /// Google Cloud location for Vertex AI. Defaults to `global` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertex_location: Option<String>,
    /// Provider-level failover chain (tried in order when primary provider fails).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback: Vec<LlmFallback>,
    /// Auxiliary chain for background side-tasks (memory consolidation,
    /// heartbeat, scheduled prompts). When set, `LlmClient::for_auxiliary`
    /// returns a separate client whose cooldown/error state is isolated from
    /// the primary chain — so a primary outage doesn't kill nightly cron.
    /// Unset = back-compat: aux callers fall through to the primary chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aux: Option<AuxLlmConfig>,
    /// Extended thinking level: off / minimal / low / medium / high / xhigh
    /// / adaptive / max. Enables native thinking for supported providers
    /// (Anthropic extended thinking, OpenAI reasoning effort, Gemini
    /// `thinkingConfig`).
    #[serde(default)]
    pub thinking: ThinkingLevel,
    /// Per-model overrides for `thinking`. Key is a case-insensitive
    /// substring of the model id (e.g. `"gpt-5"`, `"claude"`, `"gemini-2.5"`).
    /// Resolution at request time: first matching override wins (deterministic
    /// BTreeMap ordering) → `llm.thinking` → `Off`. Lets an operator run a
    /// fast non-thinking default globally while still extracting reasoning
    /// out of specific models.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub thinking_overrides: std::collections::BTreeMap<String, ThinkingLevel>,
    /// Path to the `claude` CLI binary (auto-detected if omitted).
    /// Only used when provider is `claude-cli`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_cli_path: Option<String>,
    /// Prompt caching configuration. Honored by the native Anthropic
    /// provider and by OpenRouter when the selected model is Claude-family
    /// (the OpenAI-compat path injects Anthropic-style `cache_control`
    /// markers on the system prompt and a rolling tail of recent messages).
    /// Other OpenAI-compatible providers ignore the markers and rely on
    /// their own implicit caching keyed by `prompt_cache_key`.
    #[serde(default)]
    pub cache: PromptCacheConfig,
    /// Credential-pool selection strategy. Applies uniformly to the primary,
    /// every `fallback` slot, and the aux head — each provider gets its own
    /// pool but they share these knobs.
    #[serde(default)]
    pub credential_strategy: crate::llm::credential_pool::CredentialStrategy,
    /// Default per-credential cooldown when a 402/429 response carries no
    /// `Retry-After` hint. Provider-supplied hints always win.
    #[serde(default = "default_credential_cooldown_secs")]
    pub credential_cooldown_secs: u64,
    /// Soft cap on concurrent leases per credential. Above this, leases
    /// fan out across other keys; once every key is at the cap, the
    /// least-leased key wins ties.
    #[serde(default = "default_max_concurrent_per_credential")]
    pub max_concurrent_per_credential: u32,
    /// Proactive rate-limit avoidance: skip a credential during `pick()`
    /// when the provider's `x-ratelimit-remaining-*` (or `anthropic-ratelimit-*`)
    /// headers report less than this percentage of bucket headroom. Set to
    /// `0` to disable proactive avoidance entirely (the pool stays purely
    /// reactive to 429s).
    #[serde(default = "default_proactive_rate_limit_threshold_pct")]
    pub proactive_rate_limit_threshold_pct: u8,
    /// Minimum seconds-until-reset for proactive avoidance to kick in.
    /// Below this, waiting out the bucket is cheaper than rotating to a
    /// sibling key, so we keep using the near-empty credential.
    #[serde(default = "default_proactive_rate_limit_min_window_secs")]
    pub proactive_rate_limit_min_window_secs: u64,
}

fn default_credential_cooldown_secs() -> u64 {
    60
}

fn default_max_concurrent_per_credential() -> u32 {
    1
}

fn default_proactive_rate_limit_threshold_pct() -> u8 {
    5
}

fn default_proactive_rate_limit_min_window_secs() -> u64 {
    5
}

/// TTL for Anthropic `cache_control` markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CacheTtl {
    /// Adaptive: 5 minutes for interactive REPL, 1 hour for gateway/scheduled sessions.
    #[default]
    Auto,
    /// Explicit 5-minute ephemeral cache.
    FiveMin,
    /// Extended cache (1 hour). Anthropic charges a higher write cost but reads are cheaper.
    OneHour,
}

impl CacheTtl {
    /// Resolve this TTL for a given session context. `is_gateway` should be true
    /// for gateway, channel, or scheduled sessions where inter-turn latency is higher.
    pub fn resolve(&self, is_gateway: bool) -> CacheTtl {
        match self {
            Self::Auto => {
                if is_gateway {
                    Self::OneHour
                } else {
                    Self::FiveMin
                }
            }
            other => *other,
        }
    }

    /// Returns the TTL string used in the Anthropic `cache_control` object,
    /// or `None` for the default 5-minute TTL (which should not be serialized).
    /// For `Auto`, resolves as FiveMin (caller should use `resolve()` first).
    pub fn as_ttl_str(&self) -> Option<&'static str> {
        match self {
            Self::FiveMin | Self::Auto => None,
            Self::OneHour => Some("1h"),
        }
    }
}

impl std::fmt::Display for CacheTtl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::FiveMin => f.write_str("5m"),
            Self::OneHour => f.write_str("1h"),
        }
    }
}

/// Which cache-breakpoint layout to apply on Anthropic requests.
///
/// Anthropic permits up to 4 `cache_control` breakpoints per request.
/// Different layouts trade coverage of the tool schema against how many
/// recent messages stay cached as the conversation grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CacheStrategy {
    /// Tools + system + last 2 messages. Best when the tool schema is large
    /// and stable; leaves 2 breakpoints for the rolling tail.
    #[default]
    #[serde(rename = "tools_system_and_2")]
    ToolsSystemAnd2,
    /// System + last 3 messages, no tools marker. Trades the tools breakpoint
    /// for deeper tail coverage — helpful on long conversations with a
    /// relatively small tool list.
    #[serde(rename = "system_and_3")]
    SystemAnd3,
}

/// Prompt caching configuration.
///
/// When enabled and the provider supports it, the LLM client attaches
/// `cache_control` markers across the stable prefix (tools array and system
/// prompt) plus a rolling window of recent messages, allowing the provider to
/// reuse cached prefixes across turns.
///
/// Anthropic currently allows up to 4 cache breakpoints per request. The
/// defaults below use all 4: tools + system + last 2 messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PromptCacheConfig {
    /// Whether prompt caching is enabled.
    pub enabled: bool,
    /// Cache TTL (`5m` default or `1h` extended).
    pub ttl: CacheTtl,
    /// Attach a cache marker to the last tool definition (caches the whole
    /// tools array). Anthropic only; ignored by other providers. Overridden
    /// when `strategy` is not the default.
    pub cache_tools: bool,
    /// Attach a cache marker to the system prompt block. Anthropic only.
    /// Overridden when `strategy` is not the default.
    pub cache_system: bool,
    /// Number of trailing messages to mark with a rolling cache breakpoint.
    /// Clamped to at most 2 so tools + system + rolling stays within
    /// Anthropic's 4-breakpoint budget. Overridden when `strategy` is not
    /// the default.
    pub rolling_messages: u8,
    /// Cache breakpoint layout. When set to a non-default variant, it
    /// overrides `cache_tools`, `cache_system`, and `rolling_messages`.
    pub strategy: CacheStrategy,
}

impl Default for PromptCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl: CacheTtl::Auto,
            cache_tools: true,
            cache_system: true,
            rolling_messages: 2,
            strategy: CacheStrategy::default(),
        }
    }
}

impl PromptCacheConfig {
    /// Returns the rolling message count, clamped to `[0, 2]`.
    pub fn rolling_messages_clamped(&self) -> usize {
        self.rolling_messages.min(2) as usize
    }

    /// Resolve the effective `(cache_system, cache_tools, rolling_messages)`
    /// layout for this config. Non-default strategies override the individual
    /// fields; the default strategy keeps the legacy per-flag behavior.
    pub fn effective_layout(&self) -> (bool, bool, usize) {
        match self.strategy {
            CacheStrategy::ToolsSystemAnd2 => (
                self.cache_system,
                self.cache_tools,
                self.rolling_messages_clamped(),
            ),
            CacheStrategy::SystemAnd3 => (true, false, 3),
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: None,
            api_key_env: DEFAULT_LLM_API_KEY_ENV.into(),
            api_key: None,
            api_keys: Vec::new(),
            model: String::new(),
            temperature: DEFAULT_LLM_TEMPERATURE,
            max_tokens: DEFAULT_LLM_MAX_TOKENS,
            max_retries: DEFAULT_LLM_MAX_RETRIES,
            initial_retry_delay_ms: DEFAULT_LLM_INITIAL_RETRY_DELAY_MS,
            request_timeout_ms: DEFAULT_LLM_REQUEST_TIMEOUT_MS,
            stream_chunk_timeout_secs: DEFAULT_LLM_STREAM_CHUNK_TIMEOUT_SECS,
            base_url: None,
            bedrock_region: None,
            azure_resource: None,
            azure_deployment: None,
            azure_api_version: None,
            vertex_project: None,
            vertex_location: None,
            fallback: Vec::new(),
            aux: None,
            thinking: ThinkingLevel::Off,
            thinking_overrides: std::collections::BTreeMap::new(),
            claude_cli_path: None,
            cache: PromptCacheConfig::default(),
            credential_strategy: crate::llm::credential_pool::CredentialStrategy::default(),
            credential_cooldown_secs: default_credential_cooldown_secs(),
            max_concurrent_per_credential: default_max_concurrent_per_credential(),
            proactive_rate_limit_threshold_pct: default_proactive_rate_limit_threshold_pct(),
            proactive_rate_limit_min_window_secs: default_proactive_rate_limit_min_window_secs(),
        }
    }
}

impl LlmConfig {
    /// Resolve the thinking level for `model`: most-specific matching
    /// `thinking_overrides` entry → global `thinking` → `Off`.
    ///
    /// Match is case-insensitive substring. When multiple needles match,
    /// the **longest** one wins (operator intent: a `gemini-3` override is
    /// more specific than a `gemini` override and should beat it). Ties on
    /// length fall back to BTreeMap key order so resolution stays
    /// deterministic.
    pub fn effective_thinking(&self, model: &str) -> ThinkingLevel {
        let m = model.to_ascii_lowercase();
        let mut best: Option<(usize, &ThinkingLevel)> = None;
        for (needle, level) in &self.thinking_overrides {
            if needle.is_empty() {
                continue;
            }
            if !m.contains(&needle.to_ascii_lowercase()) {
                continue;
            }
            let len = needle.len();
            match best {
                Some((best_len, _)) if best_len >= len => {}
                _ => best = Some((len, level)),
            }
        }
        best.map(|(_, lvl)| lvl.clone())
            .unwrap_or_else(|| self.thinking.clone())
    }
}

/// Air-gap and provider-routing policy. Lives next to [`LlmConfig`] and
/// controls which provider categories the daemon may reach over the
/// network.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProvidersConfig {
    /// When `false`, the LLM transport refuses any provider whose category is
    /// not [`crate::providers::ProviderCategory::Local`]. Defaults to `true`
    /// for parity with current behavior; flipping it to `false` is the v1
    /// air-gap switch.
    pub outbound_allowed: bool,
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self {
            outbound_allowed: true,
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

/// Sentinel string for `AuxLlmConfig::model` that triggers runtime resolution
/// against `aux_model_picker::pick_aux_model`. Set by onboarding so users get
/// a working aux chain without picking a model id by hand.
pub const AUX_MODEL_AUTO: &str = "auto";

/// Auxiliary LLM chain — a parallel provider chain dedicated to background
/// side-tasks (memory consolidation, heartbeat, scheduled prompts).
///
/// Lives next to [`LlmConfig`] but materializes into its own [`crate::llm::LlmClient`]
/// instance with isolated `provider_slots`, so a primary outage cannot bleed
/// into the aux client's cooldown state and vice versa.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuxLlmConfig {
    /// Provider name. When `None`, inherits the primary's provider — the
    /// same-provider default that onboarding emits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model id, or `"auto"` to delegate to `aux_model_picker::pick_aux_model`.
    pub model: String,
    /// Env var holding the API key (e.g. "ANTHROPIC_API_KEY").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key_env: String,
    /// Single SecretRef for API key (takes priority over `api_key_env`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SecretRef>,
    /// Multiple API keys for rotation on this aux head.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api_keys: Vec<SecretRef>,
    /// Override temperature (defaults to primary's `temperature` when unset).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Override max tokens (defaults to primary's `max_tokens` when unset).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Override base URL (proxies, Ollama on remote host, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Aux-chain fallback list — providers tried in order when the aux head
    /// fails. Independent of `LlmConfig::fallback`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback: Vec<LlmFallback>,
}

impl AuxLlmConfig {
    /// True when `model` is the `"auto"` sentinel (or empty string), meaning
    /// the runtime resolver should pick a default for this provider.
    pub fn model_is_auto(&self) -> bool {
        self.model.is_empty() || self.model == AUX_MODEL_AUTO
    }
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
    /// Per-channel recipient overrides. Map of channel name → list of sender
    /// IDs to deliver the heartbeat to. Use `"*"` as the sole entry to
    /// broadcast to every approved sender for that channel. If a channel is
    /// listed in `channels` but absent from `recipients`, heartbeat delivery
    /// defaults to the first approved sender (legacy behavior).
    pub recipients: std::collections::BTreeMap<String, Vec<String>>,
    /// Fire a proactive greeting when the TUI opens post-onboarding.
    pub session_start_enabled: bool,
    /// Minimum minutes between any heartbeat (scheduled, poke, session-start)
    /// and the next session-start greeting. Shared with the scheduled
    /// heartbeat so the user doesn't get double-nudged.
    pub session_start_throttle_minutes: u32,
    /// Last heartbeat fire timestamp (unix seconds). Persisted state used to
    /// share the throttle across scheduled, poke, and session-start sources.
    /// Not user-facing — managed internally by `execute_heartbeat_turn`.
    pub last_fired_at: i64,
    /// Minimum allowed scheduler interval, in seconds. Floors to 1s in the
    /// scheduler regardless of the configured value. Default 60 matches
    /// `cortex::constants::MIN_HEARTBEAT_INTERVAL_SECS` and should be left
    /// in place in production — sub-minute firings exist to let the
    /// smorgasborg eval harness exercise real scheduler timing without
    /// burning ≥60s of wall time per assertion.
    pub min_interval_secs: u64,
    /// Optional shell preamble run before each heartbeat firing. When
    /// non-empty, stdout is injected into the LLM prompt as a
    /// `<preprocessed_context>` block. Fail-soft: a non-zero exit / timeout
    /// / spawn failure still lets the LLM run, but the block reflects the
    /// failure. Empty string disables — no block is added.
    /// Mirrors the `pre_run_command` field on user-level scheduled tasks.
    pub pre_run_command: String,
    /// Working directory for `pre_run_command`. Empty defaults to `$HOME`.
    pub pre_run_cwd: String,
    /// Timeout in milliseconds for `pre_run_command`. Default 60_000.
    pub pre_run_timeout_ms: u64,
    /// Wall-clock cap on a single heartbeat agent turn, in seconds. The bus
    /// always emits a terminal `DaemonHeartbeatEvent` within this window —
    /// when the agent stalls (e.g. it calls `request_user_input`, whose
    /// oneshot waits 5 minutes for a reply that never arrives because
    /// heartbeats are non-interactive), the dispatcher cancels the turn
    /// and publishes a `failed` event instead of letting subscribers wait.
    /// Default 75 keeps a 15s margin under the smorgasborg eval's 90s
    /// `collect_samples` timeout.
    pub turn_timeout_secs: u64,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval: DEFAULT_HEARTBEAT_INTERVAL.into(),
            quiet_hours_start: Some(DEFAULT_HEARTBEAT_QUIET_START.into()),
            quiet_hours_end: Some(DEFAULT_HEARTBEAT_QUIET_END.into()),
            cron: None,
            channels: Vec::new(),
            recipients: std::collections::BTreeMap::new(),
            session_start_enabled: true,
            session_start_throttle_minutes: 30,
            last_fired_at: 0,
            min_interval_secs: crate::constants::MIN_HEARTBEAT_INTERVAL_SECS,
            pre_run_command: String::new(),
            pre_run_cwd: String::new(),
            pre_run_timeout_ms: 60_000,
            turn_timeout_secs: 75,
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
    fn thinking_level_parses_all_eight_variants() {
        // Each lowercase serde name MUST round-trip back to the matching
        // variant. Guards against accidental rename/removal that would
        // silently downgrade unknown levels to Off via #[default].
        let cases = [
            ("off", ThinkingLevel::Off),
            ("minimal", ThinkingLevel::Minimal),
            ("low", ThinkingLevel::Low),
            ("medium", ThinkingLevel::Medium),
            ("high", ThinkingLevel::High),
            ("xhigh", ThinkingLevel::Xhigh),
            ("adaptive", ThinkingLevel::Adaptive),
            ("max", ThinkingLevel::Max),
        ];
        for (s, expected) in cases {
            let parsed: ThinkingLevel = serde_json::from_value(serde_json::json!(s))
                .unwrap_or_else(|e| panic!("parse {s}: {e}"));
            assert_eq!(parsed, expected, "round-trip for {s}");
        }
    }

    #[test]
    fn thinking_level_rejects_unknown_string() {
        // A typo in `llm.thinking` must surface as an error rather than
        // silently parsing to Off (which would hide misconfiguration and
        // mislead the operator into thinking thinking is enabled when it
        // is not).
        let result: Result<ThinkingLevel, _> = serde_json::from_value(serde_json::json!("ultra"));
        assert!(result.is_err(), "unknown level should not parse");
    }

    #[test]
    fn thinking_level_anthropic_budget_non_decreasing() {
        // Going up the level scale must never *decrease* the Anthropic
        // budget — an operator who upgrades from Medium to High shouldn't
        // suddenly get fewer thinking tokens. Minimal and Low both pin to
        // 1024 because Anthropic's minimum budget_tokens is 1024; they
        // differ only on OpenAI's reasoning_effort surface. Adaptive is off
        // this chain (provider-best moderate default) and is asserted
        // separately.
        let chain = [
            (ThinkingLevel::Off, None),
            (ThinkingLevel::Minimal, Some(1024_u32)),
            (ThinkingLevel::Low, Some(1024)),
            (ThinkingLevel::Medium, Some(4096)),
            (ThinkingLevel::High, Some(16384)),
            (ThinkingLevel::Xhigh, Some(32768)),
            (ThinkingLevel::Max, Some(64000)),
        ];
        let mut last = 0_u32;
        for (lvl, expected) in chain {
            assert_eq!(lvl.anthropic_budget_tokens(), expected, "{lvl:?}");
            if let Some(b) = expected {
                assert!(b >= last, "{lvl:?} budget {b} must not regress from {last}");
                last = b;
            }
        }
        assert_eq!(
            ThinkingLevel::Adaptive.anthropic_budget_tokens(),
            Some(8192),
            "Adaptive is a moderate provider-best default on Anthropic"
        );
    }

    #[test]
    fn thinking_level_openai_minimal_maps_to_minimal_string() {
        // GPT-5 added a "minimal" reasoning_effort tier; map our Minimal
        // variant onto it exactly rather than aliasing to "low" "to be safe".
        // Aliasing would lose the cost/latency win that "minimal" provides.
        assert_eq!(
            ThinkingLevel::Minimal.openai_reasoning_effort(),
            Some("minimal")
        );
        assert_eq!(
            ThinkingLevel::Adaptive.openai_reasoning_effort(),
            Some("medium")
        );
        assert_eq!(ThinkingLevel::Max.openai_reasoning_effort(), Some("high"));
    }

    #[test]
    fn thinking_level_gemini_2_5_adaptive_is_dynamic_sentinel() {
        // Gemini 2.5 treats `thinkingBudget: -1` as dynamic thinking (model
        // decides). Anyone "fixing" the negative number to 0 would silently
        // disable thinking on Adaptive.
        assert_eq!(
            ThinkingLevel::Adaptive.gemini_2_5_budget(),
            Some(-1),
            "Adaptive must serialize as Gemini's dynamic-thinking sentinel"
        );
        assert_eq!(ThinkingLevel::Off.gemini_2_5_budget(), None);
        assert_eq!(ThinkingLevel::Max.gemini_2_5_budget(), Some(24576));
    }

    #[test]
    fn thinking_level_gemini_3_buckets() {
        // Gemini 3 only accepts LOW / MEDIUM / HIGH. Our 8 variants collapse
        // into those three buckets. Boundary moves must be deliberate.
        let cases = [
            (ThinkingLevel::Off, None),
            (ThinkingLevel::Minimal, Some("LOW")),
            (ThinkingLevel::Low, Some("LOW")),
            (ThinkingLevel::Medium, Some("MEDIUM")),
            (ThinkingLevel::Adaptive, Some("MEDIUM")),
            (ThinkingLevel::High, Some("HIGH")),
            (ThinkingLevel::Xhigh, Some("HIGH")),
            (ThinkingLevel::Max, Some("HIGH")),
        ];
        for (lvl, expected) in cases {
            assert_eq!(lvl.gemini_3_level(), expected, "{lvl:?}");
        }
    }

    #[test]
    fn thinking_level_is_enabled() {
        assert!(!ThinkingLevel::Off.is_enabled());
        for lvl in [
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
            ThinkingLevel::Adaptive,
            ThinkingLevel::Max,
        ] {
            assert!(lvl.is_enabled(), "{lvl:?} must report enabled");
        }
    }

    // ── effective_thinking ──

    #[test]
    fn effective_thinking_per_model_override_wins() {
        // Per-model substring match beats the global default — the core
        // promise of `thinking_overrides`. Without this, an operator running
        // global=Off can't selectively enable thinking on a single model.
        let mut cfg = LlmConfig::default();
        cfg.thinking = ThinkingLevel::Off;
        cfg.thinking_overrides
            .insert("gpt-5".into(), ThinkingLevel::High);
        assert_eq!(
            cfg.effective_thinking("openai/gpt-5-pro"),
            ThinkingLevel::High
        );
    }

    #[test]
    fn effective_thinking_case_insensitive() {
        // Operators write model ids in mixed case in TOML; matching MUST
        // normalize. Otherwise an "Anthropic"-keyed override silently
        // misses a "anthropic/..." model id.
        let mut cfg = LlmConfig::default();
        cfg.thinking_overrides
            .insert("Claude".into(), ThinkingLevel::Max);
        assert_eq!(
            cfg.effective_thinking("claude-sonnet-4"),
            ThinkingLevel::Max
        );
    }

    #[test]
    fn effective_thinking_falls_back_to_global_when_no_match() {
        // When no override needle matches the model id, we MUST fall back to
        // the global `thinking` — not Off — so the global default remains
        // load-bearing.
        let mut cfg = LlmConfig::default();
        cfg.thinking = ThinkingLevel::Medium;
        cfg.thinking_overrides
            .insert("gpt".into(), ThinkingLevel::High);
        assert_eq!(
            cfg.effective_thinking("claude-sonnet-4"),
            ThinkingLevel::Medium
        );
    }

    #[test]
    fn effective_thinking_longest_needle_wins() {
        // Operator pattern: a broad `"gemini"` fallback plus a specific
        // `"gemini-3"` override. The specific one MUST win for Gemini 3
        // models, otherwise the operator's intent (set fallback, then
        // refine) is silently inverted. BTreeMap key order alone would
        // pick `"gemini"` first (alphabetical), so this test pins the
        // longest-match contract.
        let mut cfg = LlmConfig::default();
        cfg.thinking = ThinkingLevel::Off;
        cfg.thinking_overrides
            .insert("gemini".into(), ThinkingLevel::Low);
        cfg.thinking_overrides
            .insert("gemini-3".into(), ThinkingLevel::High);
        assert_eq!(
            cfg.effective_thinking("gemini-3-flash-preview"),
            ThinkingLevel::High,
            "longer needle 'gemini-3' must beat shorter 'gemini'"
        );
        // The broader override still applies to other Gemini models.
        assert_eq!(
            cfg.effective_thinking("gemini-2.5-pro"),
            ThinkingLevel::Low,
            "shorter needle still wins when no longer needle matches"
        );
    }

    #[test]
    fn effective_thinking_ignores_empty_needle() {
        // An empty-string needle would match every model id (substring of
        // anything). Skip it so a stray "" entry in TOML doesn't override
        // every model in the catalog.
        let mut cfg = LlmConfig::default();
        cfg.thinking = ThinkingLevel::Off;
        cfg.thinking_overrides
            .insert(String::new(), ThinkingLevel::Max);
        assert_eq!(
            cfg.effective_thinking("claude-sonnet-4"),
            ThinkingLevel::Off
        );
    }

    // ── thinking_overrides via apply_setting round-trip ──

    /// Apply a well-formed `llm.thinking_overrides` JSON blob and round-trip
    /// it into `effective_thinking`. Locks the `json_count` setter wiring +
    /// the override-resolution end-to-end.
    #[test]
    fn thinking_overrides_apply_setting_round_trip() {
        let mut cfg = crate::config::Config::default();
        cfg.apply_setting(
            "llm.thinking_overrides",
            r#"{"gpt-5":"high","claude":"adaptive"}"#,
        )
        .expect("valid JSON should apply");
        assert_eq!(cfg.llm.thinking_overrides.len(), 2);
        assert_eq!(
            cfg.llm.effective_thinking("openai/gpt-5-pro"),
            ThinkingLevel::High
        );
        assert_eq!(
            cfg.llm.effective_thinking("claude-sonnet-4"),
            ThinkingLevel::Adaptive
        );
    }

    /// A typo in a level value (e.g. `"ultra"`) MUST surface as an error
    /// rather than silently no-op the whole map or apply only the valid
    /// entries. Otherwise a paste-error in `/settings` leaves the operator
    /// thinking they configured overrides when nothing landed.
    #[test]
    fn thinking_overrides_apply_setting_rejects_unknown_level() {
        let mut cfg = crate::config::Config::default();
        let result = cfg.apply_setting("llm.thinking_overrides", r#"{"gpt-5":"ultra"}"#);
        assert!(result.is_err(), "unknown level must reject the whole blob");
        assert!(
            cfg.llm.thinking_overrides.is_empty(),
            "failed apply MUST NOT mutate config"
        );
    }

    // ── LlmConfig defaults ──

    #[test]
    fn llm_config_defaults() {
        let cfg = LlmConfig::default();
        assert_eq!(cfg.provider, None);
        assert_eq!(cfg.api_key_env, "OPENROUTER_API_KEY");
        assert_eq!(cfg.model, "");
        assert!((cfg.temperature - 0.7).abs() < f32::EPSILON);
        assert_eq!(cfg.max_tokens, 16_384);
        assert_eq!(cfg.max_retries, 3);
        assert_eq!(cfg.initial_retry_delay_ms, 200);
        assert_eq!(cfg.request_timeout_ms, 120_000);
        assert_eq!(cfg.stream_chunk_timeout_secs, 90);
        assert!(cfg.base_url.is_none());
        assert!(cfg.fallback.is_empty());
        assert!(
            cfg.aux.is_none(),
            "aux must default to None for back-compat"
        );
        assert_eq!(cfg.thinking, ThinkingLevel::Off);
    }

    // ── AuxLlmConfig ──

    #[test]
    fn aux_default_model_is_auto_sentinel() {
        // The default-constructed aux config has an empty model string, which
        // model_is_auto() must treat the same as the explicit "auto" sentinel
        // — otherwise an aux block written without a model field would surface
        // an empty model id to the provider request.
        let aux = AuxLlmConfig::default();
        assert!(aux.model_is_auto());
    }

    #[test]
    fn aux_explicit_auto_sentinel_recognized() {
        let aux = AuxLlmConfig {
            model: AUX_MODEL_AUTO.to_string(),
            ..Default::default()
        };
        assert!(aux.model_is_auto());
    }

    #[test]
    fn aux_concrete_model_is_not_auto() {
        // Power-user override path: a concrete model id must bypass the
        // resolver, otherwise their explicit choice gets overwritten.
        let aux = AuxLlmConfig {
            model: "claude-haiku-4-5".to_string(),
            ..Default::default()
        };
        assert!(!aux.model_is_auto());
    }

    #[test]
    fn aux_round_trips_through_toml() {
        // Serialization must preserve enough to reconstruct the resolver
        // contract: `model` field present, even when it's the empty/auto
        // default. skip_serializing_if on Option fields keeps the blob clean.
        // Compared via field-level asserts since SecretRef/LlmFallback don't
        // implement PartialEq (and adding it would cascade).
        let aux = AuxLlmConfig {
            provider: Some("anthropic".into()),
            model: AUX_MODEL_AUTO.into(),
            api_key_env: "ANTHROPIC_API_KEY".into(),
            ..Default::default()
        };
        let s = toml::to_string(&aux).expect("serialize");
        let parsed: AuxLlmConfig = toml::from_str(&s).expect("parse");
        assert_eq!(parsed.provider.as_deref(), Some("anthropic"));
        assert!(parsed.model_is_auto());
        assert_eq!(parsed.api_key_env, "ANTHROPIC_API_KEY");
        assert!(parsed.fallback.is_empty());
    }

    #[test]
    fn llm_config_with_aux_round_trips() {
        // Whole-LlmConfig round-trip with aux populated. Locks the
        // settings-registry contract: writing llm.aux via the JSON path then
        // reloading Config must yield the same struct.
        let toml_str = r#"
            model = "claude-sonnet-4-7"
            api_key_env = "ANTHROPIC_API_KEY"

            [aux]
            provider = "anthropic"
            model = "auto"
            api_key_env = "ANTHROPIC_API_KEY"
        "#;
        let cfg: LlmConfig = toml::from_str(toml_str).expect("parse");
        let aux = cfg.aux.expect("aux must deserialize");
        assert_eq!(aux.provider.as_deref(), Some("anthropic"));
        assert!(aux.model_is_auto());
        assert_eq!(aux.api_key_env, "ANTHROPIC_API_KEY");
    }

    #[test]
    fn llm_config_without_aux_block_deserializes_to_none() {
        // Back-compat: existing TOML / DB rows that predate this feature
        // must keep working. aux must materialize as None, not panic / not
        // populate a default that fires the resolver unexpectedly.
        let toml_str = r#"
            model = "gpt-4"
            api_key_env = "OPENAI_API_KEY"
        "#;
        let cfg: LlmConfig = toml::from_str(toml_str).expect("parse");
        assert!(cfg.aux.is_none());
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
        assert_eq!(cfg.max_tokens, 16_384);
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
        assert_eq!(cfg.interval, "60m");
        assert_eq!(cfg.quiet_hours_start.as_deref(), Some("00:00"));
        assert_eq!(cfg.quiet_hours_end.as_deref(), Some("06:00"));
        assert!(cfg.cron.is_none());
        assert!(cfg.channels.is_empty());
        assert!(cfg.recipients.is_empty());
        assert_eq!(
            cfg.min_interval_secs,
            crate::constants::MIN_HEARTBEAT_INTERVAL_SECS
        );
    }

    #[test]
    fn heartbeat_config_from_toml() {
        let toml_str = r#"
            interval = "1h"
            channels = ["telegram", "slack"]
            min_interval_secs = 5
        "#;
        let cfg: HeartbeatConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.interval, "1h");
        assert_eq!(cfg.channels, vec!["telegram", "slack"]);
        assert_eq!(cfg.min_interval_secs, 5);
    }

    #[test]
    fn heartbeat_config_recipients_from_toml() {
        let toml_str = r#"
            channels = ["telegram", "slack", "discord"]
            [recipients]
            telegram = ["123456789"]
            slack = ["U01ABC", "U02DEF"]
            discord = ["*"]
        "#;
        let cfg: HeartbeatConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.recipients.len(), 3);
        assert_eq!(cfg.recipients.get("telegram").unwrap(), &vec!["123456789"]);
        assert_eq!(
            cfg.recipients.get("slack").unwrap(),
            &vec!["U01ABC", "U02DEF"]
        );
        assert_eq!(cfg.recipients.get("discord").unwrap(), &vec!["*"]);
    }

    // ── CacheTtl ──

    #[test]
    fn cache_ttl_default_is_auto() {
        assert_eq!(CacheTtl::default(), CacheTtl::Auto);
    }

    #[test]
    fn auto_ttl_repl_uses_five_min() {
        let resolved = CacheTtl::Auto.resolve(false);
        assert_eq!(resolved, CacheTtl::FiveMin);
    }

    #[test]
    fn auto_ttl_gateway_uses_one_hour() {
        let resolved = CacheTtl::Auto.resolve(true);
        assert_eq!(resolved, CacheTtl::OneHour);
    }

    #[test]
    fn explicit_ttl_ignores_context() {
        assert_eq!(CacheTtl::FiveMin.resolve(true), CacheTtl::FiveMin);
        assert_eq!(CacheTtl::OneHour.resolve(false), CacheTtl::OneHour);
    }

    #[test]
    fn cache_ttl_display() {
        assert_eq!(CacheTtl::Auto.to_string(), "auto");
        assert_eq!(CacheTtl::FiveMin.to_string(), "5m");
        assert_eq!(CacheTtl::OneHour.to_string(), "1h");
    }

    #[test]
    fn cache_ttl_as_ttl_str() {
        assert_eq!(CacheTtl::Auto.as_ttl_str(), None);
        assert_eq!(CacheTtl::FiveMin.as_ttl_str(), None);
        assert_eq!(CacheTtl::OneHour.as_ttl_str(), Some("1h"));
    }
}
