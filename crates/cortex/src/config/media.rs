use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use crate::constants;
use crate::security::secrets_resolve::SecretRef;

/// Collaboration mode that controls how the agent interacts during a session.
///
/// Borg is autonomous by default — there is no "Default" mode. The only modes
/// are `Execute` (the standard, autonomous mode) and `Plan` (read-only).
/// A third "Default" mode previously existed and was removed deliberately;
/// `FromStr` accepts `"default"` as a silent alias for `Execute` so legacy
/// settings rows keep working, but it must not be re-added as a variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CollaborationMode {
    /// Autonomous execution — makes assumptions, executes independently, reports progress.
    #[default]
    Execute,
    /// Read-only planning — explores codebase, asks questions, produces a plan, blocks mutations.
    Plan,
}

impl CollaborationMode {
    /// Returns true if this mode blocks mutating tool calls.
    pub fn blocks_mutations(&self) -> bool {
        matches!(self, Self::Plan)
    }
}

impl FromStr for CollaborationMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            // "default" is a legacy alias mapped to Execute; do not reintroduce
            // a Default variant (see the type-level doc comment).
            "default" | "execute" => Ok(Self::Execute),
            "plan" => Ok(Self::Plan),
            other => {
                anyhow::bail!("Unknown collaboration mode '{other}'. Valid: execute, plan")
            }
        }
    }
}

impl fmt::Display for CollaborationMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Execute => write!(f, "execute"),
            Self::Plan => write!(f, "plan"),
        }
    }
}

/// A credential value that can be either a plain env var name (legacy) or a full SecretRef.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CredentialValue {
    /// Legacy: bare string = env var name (backward compat)
    EnvVar(String),
    /// Full SecretRef (env, file, exec)
    Ref(SecretRef),
}

impl CredentialValue {
    /// Resolve the credential to its secret string value.
    pub fn resolve(&self) -> Result<String> {
        match self {
            CredentialValue::EnvVar(var) => {
                std::env::var(var).with_context(|| format!("Env var {var} not set"))
            }
            CredentialValue::Ref(sr) => sr.resolve(),
        }
    }
}

/// Helper for serde default that returns `true`.
pub fn default_true() -> bool {
    true
}

/// Configuration for the headless Chrome browser automation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserConfig {
    /// Whether browser automation is enabled.
    pub enabled: bool,
    /// Run Chrome in headless mode (no visible window).
    pub headless: bool,
    /// Optional path to the Chrome executable.
    pub executable: Option<String>,
    /// Chrome DevTools Protocol port.
    pub cdp_port: u16,
    /// Disable Chrome's internal sandbox (needed in some containers).
    pub no_sandbox: bool,
    /// Default command timeout in milliseconds.
    pub timeout_ms: u64,
    /// Browser launch timeout in milliseconds.
    pub startup_timeout_ms: u64,
    /// Max console log entries to buffer (default: 500).
    pub console_buffer_size: usize,
    /// Max page error entries to buffer (default: 200).
    pub error_buffer_size: usize,
    /// Max network request entries to buffer (default: 500).
    pub network_buffer_size: usize,
    /// Inner JS evaluation timeout in ms for Promise.race wrapper (default: 10000).
    pub js_eval_timeout_ms: u64,
    /// Cap on the number of elements `browser.snapshot` returns. Default
    /// 100; dense pages would otherwise dominate the LLM's context budget.
    pub snapshot_max_elements: usize,
    /// Optional Chrome DevTools Protocol endpoint to attach to instead of
    /// launching a fresh headless Chrome. Set via `/browser connect` or
    /// `borg browser connect`; cleared by `/browser disconnect`. When `Some`,
    /// the browser tool uses `chromiumoxide::Browser::connect(url)` so the
    /// agent shares the user's already-running Chromium profile/cookies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_url: Option<String>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            headless: true,
            executable: None,
            cdp_port: constants::BROWSER_DEFAULT_CDP_PORT,
            no_sandbox: false,
            timeout_ms: constants::BROWSER_DEFAULT_TIMEOUT_MS,
            startup_timeout_ms: constants::BROWSER_STARTUP_TIMEOUT_MS,
            console_buffer_size: constants::BROWSER_CONSOLE_BUFFER_SIZE,
            error_buffer_size: constants::BROWSER_ERROR_BUFFER_SIZE,
            network_buffer_size: constants::BROWSER_NETWORK_BUFFER_SIZE,
            js_eval_timeout_ms: constants::BROWSER_JS_EVAL_TIMEOUT_MS,
            snapshot_max_elements: constants::BROWSER_SNAPSHOT_MAX_ELEMENTS,
            cdp_url: None,
        }
    }
}

/// Single transcription provider entry (cloud API or local CLI).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioModelConfig {
    /// Provider name: "openai", "groq", "deepgram".
    pub provider: String,
    /// Model name (e.g. "whisper-1", "nova-3").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Override API key env var for this provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Language hint (BCP-47, e.g. "en").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Per-provider timeout override in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// xAI-only: inverse text normalization — emit punctuation, casing,
    /// digits-as-numerals rather than raw lowercased words. Defaults on;
    /// flip off when a downstream consumer wants the rawer form. `None`
    /// keeps the previous hardcoded default (true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xai_stt_format: Option<bool>,
    /// xAI-only: speaker diarization. Paid feature; off by default to
    /// avoid surprise billing. `None` keeps the previous hardcoded
    /// default (false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xai_stt_diarize: Option<bool>,
}

/// Audio transcription configuration with multi-provider fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Whether audio transcription is enabled.
    pub enabled: bool,
    /// Ordered fallback chain of transcription providers.
    #[serde(default)]
    pub models: Vec<AudioModelConfig>,
    /// Maximum audio file size in bytes (default: 20 MB).
    pub max_file_size: u64,
    /// Minimum audio file size in bytes (default: 1024).
    pub min_file_size: u64,
    /// Global language hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Default timeout in milliseconds.
    pub timeout_ms: u64,
    /// Echo transcript back to the sender.
    pub echo_transcript: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            models: Vec::new(),
            max_file_size: 20 * 1024 * 1024,
            min_file_size: 1024,
            language: None,
            timeout_ms: constants::AUDIO_DEFAULT_TIMEOUT_MS,
            echo_transcript: false,
        }
    }
}

/// Single TTS provider entry in the fallback chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsModelConfig {
    /// Provider name: "openai", "elevenlabs".
    pub provider: String,
    /// Model name (e.g. "tts-1", "tts-1-hd", "eleven_multilingual_v2").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Voice identifier (e.g. "alloy", "nova", or ElevenLabs voice ID).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// Override API key env var for this provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Per-provider timeout override in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Custom provider name suffix (provider: "custom" rows can ship
    /// multiple distinct entries — `name = "piper-en"` etc).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Shell command template for `provider = "custom"`. Tokens:
    /// `{text_file}`, `{voice}`, `{out_path}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Output extension for custom providers (default `"wav"`). Used to
    /// name the tempfile passed in `{out_path}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_extension: Option<String>,
    /// xAI-only: when true, insert light `[pause]` speech tags between
    /// paragraphs and after the first sentence so Grok's TTS delivers a
    /// more natural cadence. Defaults to `false` (sends raw text); flip
    /// when the model is in conversational reply mode. Ignored by every
    /// non-xAI provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xai_auto_speech_tags: Option<bool>,
}

/// Text-to-speech configuration with multi-provider fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsConfig {
    /// Whether text-to-speech is enabled.
    pub enabled: bool,
    /// Ordered fallback chain of TTS providers.
    #[serde(default)]
    pub models: Vec<TtsModelConfig>,
    /// Default voice name/ID.
    pub default_voice: String,
    /// Default output format (mp3, opus, aac, flac, wav).
    pub default_format: String,
    /// Maximum input text length in characters.
    pub max_text_length: usize,
    /// Default timeout in milliseconds.
    pub timeout_ms: u64,
    /// Auto-TTS mode: convert all gateway responses to voice.
    pub auto_mode: bool,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            models: Vec::new(),
            default_voice: "alloy".into(),
            default_format: "mp3".into(),
            max_text_length: 4096,
            timeout_ms: constants::TTS_DEFAULT_TIMEOUT_MS,
            auto_mode: false,
        }
    }
}

/// Image compression and media processing configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaConfig {
    /// Max image size in bytes after compression. Default: 6MB.
    pub max_image_bytes: usize,
    /// Enable/disable image compression. Default: true.
    pub compression_enabled: bool,
    /// Max image dimension in pixels (longest side). Default: 2048.
    pub max_dimension_px: u32,
    /// Max raw video size in bytes accepted by `video_analyze`. Default: 50MB.
    pub max_video_bytes: usize,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            max_image_bytes: 6 * 1024 * 1024,
            compression_enabled: true,
            max_dimension_px: 2048,
            max_video_bytes: 50 * 1024 * 1024,
        }
    }
}

/// Image generation configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageGenConfig {
    /// Enable image generation tools.
    pub enabled: bool,
    /// Provider override: "openai" | "fal". Auto-detects from API keys if omitted.
    pub provider: Option<String>,
    /// Model override (e.g. "dall-e-3", "fal-ai/flux/schnell").
    pub model: Option<String>,
    /// API key env var override.
    pub api_key_env: Option<String>,
    /// Default image size.
    pub default_size: String,
}

impl Default for ImageGenConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: None,
            model: None,
            api_key_env: None,
            default_size: constants::IMAGE_GEN_DEFAULT_SIZE.into(),
        }
    }
}

/// Configuration for the agent self-evolution system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EvolutionConfig {
    /// Whether agent self-evolution is enabled.
    pub enabled: bool,
    /// Whether the ambient status header (class: Name Lv.N — Mood — Archetype — hint)
    /// is rendered in the TUI. When false, the header is omitted entirely.
    #[serde(default = "default_true")]
    pub ambient_header_enabled: bool,
}

impl Default for EvolutionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ambient_header_enabled: true,
        }
    }
}

/// Configuration for tool execution behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// Default timeout in milliseconds for tool execution.
    pub default_timeout_ms: u64,
    /// Tool approval/deny policy configuration.
    #[serde(default)]
    pub policy: super::security::ToolPolicyConfig,
    /// When true, only include tool groups relevant to the current user message
    /// (based on keyword matching and recent tool usage). Saves 500-1500 tokens/turn.
    #[serde(default = "default_true")]
    pub conditional_loading: bool,
    /// When true, strip redundant metadata from tool schemas before sending to
    /// the LLM (remove defaults, shorten enum descriptions). Saves 200-600 tokens/turn.
    #[serde(default = "default_true")]
    pub compact_schemas: bool,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            default_timeout_ms: constants::TOOLS_DEFAULT_TIMEOUT_MS,
            policy: super::security::ToolPolicyConfig::default(),
            conditional_loading: true,
            compact_schemas: true,
        }
    }
}

/// Configuration for the memory system (loading, embeddings, scoping).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Maximum tokens allocated for memory context in the system prompt.
    pub max_context_tokens: usize,
    /// Embedding and semantic search configuration.
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
    /// When set, load memory from ~/.borg/memory/scopes/{scope}/ instead of ~/.borg/memory/.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_scope: Option<String>,
    /// When true, extract durable information from messages about to be dropped before compaction.
    pub flush_before_compaction: bool,
    /// Minimum token count of dropped messages to trigger a pre-compaction flush.
    pub flush_soft_threshold_tokens: usize,
    /// Minimum number of dropped messages to trigger a pre-compaction flush.
    pub flush_min_messages: usize,
    /// Additional directories to scan for .md files and index alongside memory.
    #[serde(default)]
    pub extra_paths: Vec<String>,
    /// When true, load memory at section granularity instead of file granularity.
    /// Sections are split by `## ` headers or double newlines. Requires embeddings
    /// to rank sections. Saves 1K-4K tokens when memory files are large but only
    /// parts are relevant. Default: false (opt-in due to higher risk of missing context).
    #[serde(default)]
    pub chunk_level_selection: bool,
    /// Maximum age (in days) for embedding cache entries. Entries not accessed within
    /// this period are pruned on startup. 0 = no TTL pruning.
    pub cache_ttl_days: u32,
    /// Maximum number of entries in the embedding cache. Oldest entries by
    /// last_accessed_at are pruned on startup. 0 = unlimited.
    pub cache_max_entries: usize,
    /// Proactive ("active") memory injection: per-turn search of memory + past
    /// session transcripts using the current user message as query, surfaced
    /// in the system prompt as `<active_memory trust="retrieved">`.
    #[serde(default)]
    pub active_injection: ActiveInjectionConfig,
}

/// Settings for proactive memory injection (top-k hybrid search per turn).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ActiveInjectionConfig {
    /// When true, every turn searches memory + past sessions with the user's
    /// last message and injects top-k results into the system prompt's
    /// dynamic suffix. The block is omitted entirely when no results match.
    pub enabled: bool,
    /// Number of results to fetch (passed as `max_results` to memory_search).
    pub top_k: usize,
    /// Hard cap on the injected block size; truncated mid-result if exceeded.
    pub max_tokens: usize,
}

impl Default for ActiveInjectionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            top_k: crate::constants::ACTIVE_INJECTION_DEFAULT_TOP_K,
            max_tokens: crate::constants::ACTIVE_INJECTION_DEFAULT_MAX_TOKENS,
        }
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            max_context_tokens: 8000,
            embeddings: EmbeddingsConfig::default(),
            memory_scope: None,
            flush_before_compaction: false,
            flush_soft_threshold_tokens: 2000,
            flush_min_messages: 4,
            extra_paths: Vec::new(),
            chunk_level_selection: false,
            cache_ttl_days: 30,
            cache_max_entries: 10_000,
            active_injection: ActiveInjectionConfig::default(),
        }
    }
}

/// Configuration for embedding-based semantic memory search.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingsConfig {
    /// Whether semantic search via embeddings is enabled.
    pub enabled: bool,
    /// Override embedding provider (auto-detected from API keys if omitted).
    pub provider: Option<String>,
    /// Override embedding model name.
    pub model: Option<String>,
    /// Override embedding vector dimension.
    pub dimension: Option<usize>,
    /// Override API key env var for the embedding provider.
    pub api_key_env: Option<String>,
    /// Weight for recency vs similarity (0.0 = pure similarity, 1.0 = pure recency).
    pub recency_weight: f32,
    /// Target token size per chunk when generating chunked embeddings.
    pub chunk_size_tokens: usize,
    /// Overlap tokens between adjacent chunks.
    pub chunk_overlap_tokens: usize,
    /// Weight for BM25/FTS scores in hybrid search (default 0.3).
    pub bm25_weight: f32,
    /// Weight for vector similarity scores in hybrid search (default 0.7).
    pub vector_weight: f32,
    /// Enable MMR diversity re-ranking of search results.
    pub mmr_enabled: bool,
    /// MMR lambda: 1.0 = pure relevance, 0.0 = pure diversity (default 0.7).
    pub mmr_lambda: f32,
    /// Multiplier applied to `min_score` before filtering vector-similarity
    /// candidates. Cosine-similarity scores skew lower than BM25-normalized
    /// scores so the vector threshold is softer than the hybrid threshold.
    /// Default: 0.5.
    pub vector_threshold_factor: f32,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: None,
            model: None,
            dimension: None,
            api_key_env: None,
            recency_weight: 0.2,
            chunk_size_tokens: 400,
            chunk_overlap_tokens: 80,
            bm25_weight: 0.3,
            vector_weight: 0.7,
            mmr_enabled: true,
            mmr_lambda: 0.7,
            vector_threshold_factor: 0.5,
        }
    }
}

/// Per-skill override configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEntryConfig {
    /// Whether this skill is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Extra environment variables injected when this skill runs.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Env var names a user skill is explicitly allowed to request via `requires.env`.
    /// Only needed for user-defined skills; built-in skills are always trusted.
    #[serde(default)]
    pub allowed_env: Vec<String>,
}

impl Default for SkillEntryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            env: HashMap::new(),
            allowed_env: Vec::new(),
        }
    }
}

/// Configuration for the skills system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Whether the skills system is enabled.
    pub enabled: bool,
    /// Absolute cap on tokens for skill instructions in the system prompt.
    /// Always enforced; combined with `budget_pct` via `min(...)`.
    pub max_context_tokens: usize,
    /// Optional fraction of the active model's context window to allocate for
    /// skills (e.g. `0.02` = 2%). When `Some` and a context window is known,
    /// the effective skill budget is `min(max_context_tokens, window * pct)`.
    /// `None` falls back to `max_context_tokens` only.
    #[serde(default)]
    pub budget_pct: Option<f32>,
    /// Per-skill overrides keyed by skill name.
    #[serde(default)]
    pub entries: HashMap<String, SkillEntryConfig>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_context_tokens: constants::SKILLS_MAX_CONTEXT_TOKENS,
            budget_pct: Some(0.02),
            entries: HashMap::new(),
        }
    }
}

/// Per-turn concurrent tool execution settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ConcurrentToolsConfig {
    /// When true, non-conflicting tool calls in the same assistant response
    /// are fanned out in parallel. Conflict detection uses file-path overlap
    /// and read-only / mutating / unknown effect classification.
    pub enabled: bool,
    /// Upper bound on concurrent tool tasks per group. 1 effectively disables
    /// parallelism without needing to flip `enabled`.
    pub max_workers: usize,
}

impl Default for ConcurrentToolsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_workers: 8,
        }
    }
}

/// Configuration for conversation behavior and context management.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConversationConfig {
    /// Maximum tokens retained in conversation history before compaction.
    pub max_history_tokens: usize,
    /// Maximum agent loop iterations per user turn.
    pub max_iterations: u32,
    /// Whether to display LLM thinking/reasoning tokens.
    pub show_thinking: bool,
    /// Maximum tokens for a single tool output before truncation.
    pub tool_output_max_tokens: usize,
    /// Tokens reserved for the compaction summary marker.
    pub compaction_marker_tokens: usize,
    /// Maximum characters from the transcript sent to the LLM summarizer.
    pub max_transcript_chars: usize,
    /// Active collaboration mode (default, execute, or plan).
    #[serde(default)]
    pub collaboration_mode: CollaborationMode,
    /// When true, progressively degrade old tool results to save tokens.
    /// Recent results stay full-fidelity; older ones get truncated or summarized.
    /// Saves 2K-8K tokens/turn in long conversations.
    #[serde(default = "default_true")]
    pub age_based_degradation: bool,
    /// Number of earliest history messages to preserve verbatim across
    /// compactions. 0 = legacy behavior (head can be dropped into the
    /// summary). Non-zero = protect the first N messages and iteratively
    /// update the summary instead.
    #[serde(default)]
    pub protect_first_n: usize,
    /// Concurrent tool execution settings.
    #[serde(default)]
    pub concurrent_tools: ConcurrentToolsConfig,
}

impl Default for ConversationConfig {
    fn default() -> Self {
        Self {
            max_history_tokens: 32000,
            max_iterations: 25,
            show_thinking: true,
            tool_output_max_tokens: constants::TOOL_OUTPUT_MAX_TOKENS,
            compaction_marker_tokens: constants::COMPACTION_MARKER_TOKENS,
            max_transcript_chars: constants::MAX_TRANSCRIPT_CHARS,
            collaboration_mode: CollaborationMode::Execute,
            age_based_degradation: true,
            protect_first_n: 3,
            concurrent_tools: ConcurrentToolsConfig::default(),
        }
    }
}

/// User identity and locale preferences.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserConfig {
    /// The user's display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Custom name for the agent.
    #[serde(default)]
    pub agent_name: Option<String>,
    /// IANA timezone string (e.g. "America/New_York").
    #[serde(default)]
    pub timezone: Option<String>,
}

/// Configuration for web search and fetch capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    /// Whether web tools are enabled.
    pub enabled: bool,
    /// Search provider name or "auto" for auto-detection.
    pub search_provider: String,
    /// Override API key env var for the search provider.
    pub search_api_key_env: Option<String>,
    /// TTL in seconds for cached `web_search` results. A cached row older
    /// than this is treated as a miss in `auto` mode and refetched.
    /// 0 disables caching entirely (every call is live).
    pub search_cache_ttl_secs: u64,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            search_provider: "auto".into(),
            search_api_key_env: None,
            search_cache_ttl_secs: 3600,
        }
    }
}

/// Configuration for the scheduled tasks daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TasksConfig {
    /// Maximum number of tasks that can run concurrently.
    pub max_concurrent: usize,
}

impl Default for TasksConfig {
    fn default() -> Self {
        Self { max_concurrent: 3 }
    }
}

/// Token budget configuration for usage tracking and limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    /// Monthly token limit. 0 = unlimited.
    pub monthly_token_limit: u64,
    /// Fraction of budget at which to warn (0.0–1.0).
    pub warning_threshold: f64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            monthly_token_limit: 1_000_000,
            warning_threshold: 0.8,
        }
    }
}

/// Configuration for the plugin marketplace system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    /// Whether the plugin system is enabled.
    pub enabled: bool,
    /// Automatically verify plugin integrity on install.
    pub auto_verify: bool,
    /// WASM-plugin subsystem settings.
    pub wasm: WasmPluginsConfig,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_verify: true,
            wasm: WasmPluginsConfig::default(),
        }
    }
}

/// WASM-plugin runtime kill switches and rate limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WasmPluginsConfig {
    /// When false, WASM tool definitions are hidden from the LLM and dispatch
    /// returns "tool not found" — global incident-response kill switch.
    pub enabled: bool,
    /// Per-plugin disable list (exact `plugin_id` match). A disabled plugin's
    /// tools are hidden from the LLM and dispatch refuses to execute, but the
    /// install row and on-disk files remain so re-enable is a one-flag toggle.
    pub disabled_plugin_ids: Vec<String>,
    /// Per-publisher disable list (base64 ed25519 verifying key). Hides every
    /// plugin signed by the key without removing the key from
    /// `security.trusted_publisher_keys` — for paused investigations rather
    /// than full distrust.
    pub disabled_publisher_keys: Vec<String>,
    /// Refill rate for the per-plugin token bucket. Refused calls emit
    /// `wasm_plugin_rate_limited` and never reach the wasmtime sandbox.
    pub rate_limit_per_minute: u32,
    /// Maximum bucket depth — controls burst tolerance.
    pub rate_limit_burst: u32,
}

impl Default for WasmPluginsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            disabled_plugin_ids: Vec::new(),
            disabled_publisher_keys: Vec::new(),
            rate_limit_per_minute: 60,
            rate_limit_burst: 10,
        }
    }
}

/// Configuration for multi-agent spawning and orchestration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MultiAgentConfig {
    /// Whether sub-agent spawning is enabled.
    pub enabled: bool,
    /// Maximum nesting depth for spawned sub-agents.
    pub max_spawn_depth: u32,
    /// Maximum child agents a single parent can spawn.
    pub max_children_per_agent: u32,
    /// Maximum sub-agents running concurrently.
    pub max_concurrent: u32,
    /// Default timeout (seconds) used by `spawn_agent` in blocking and batch
    /// modes when the caller does not supply `timeout_secs`.
    pub delegate_timeout_secs: u64,
    /// Global kill switch for orchestrator-role re-delegation. When `false`,
    /// every spawned child is treated as a leaf regardless of role — they
    /// cannot themselves call `spawn_agent`. The numeric `max_spawn_depth`
    /// remains the terminal safety net.
    pub orchestrator_enabled: bool,
    /// Optional per-spawn model override. Used when neither the role nor
    /// the `spawn_agent` call supplies an explicit `model`. Precedence:
    /// explicit arg > role.model > this > parent's model.
    pub delegation_model: Option<String>,
    /// Optional per-spawn provider override. Same precedence rules as
    /// `delegation_model`.
    pub delegation_provider: Option<String>,
    /// Interval (seconds) between liveness heartbeat events from a running
    /// child to the parent's `AgentTreeBus`. TUI subscribers use the cadence
    /// to render a "still working" indicator.
    pub heartbeat_interval_secs: u64,
    /// Number of consecutive heartbeat intervals with no LLM/tool activity
    /// after which the child publishes a `StalledChild` event so the operator
    /// can intervene.
    pub heartbeat_stale_cycles: u32,
}

impl Default for MultiAgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_spawn_depth: 1,
            max_children_per_agent: 5,
            max_concurrent: 3,
            delegate_timeout_secs: 600,
            orchestrator_enabled: true,
            delegation_model: None,
            delegation_provider: None,
            heartbeat_interval_secs: 30,
            heartbeat_stale_cycles: 15,
        }
    }
}

/// OpenTelemetry tracing and metrics configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TelemetryConfig {
    /// Enable distributed tracing export.
    pub tracing_enabled: bool,
    /// Enable metrics export.
    pub metrics_enabled: bool,
    /// Enable daemon cost-event streaming.
    pub cost_events: bool,
    /// OTLP collector endpoint URL.
    pub otlp_endpoint: String,
    /// Service name reported in telemetry data.
    pub service_name: String,
    /// Trace sampling ratio (0.0 to 1.0).
    pub sampling_ratio: f64,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            tracing_enabled: false,
            metrics_enabled: false,
            cost_events: false,
            otlp_endpoint: "http://localhost:4317".to_string(),
            service_name: "borg".to_string(),
            sampling_ratio: 1.0,
        }
    }
}

/// Debug and diagnostic configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DebugConfig {
    /// When true, log full LLM request/response to ~/.borg/logs/debug/
    #[serde(default)]
    pub llm_logging: bool,
}

/// Configuration for the user scripts system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScriptsConfig {
    /// Enable the scripts system (default: true)
    pub enabled: bool,
    /// Default sandbox profile for new scripts: "default", "trusted", or "custom"
    pub default_sandbox_profile: String,
    /// Auto-cleanup ephemeral scripts older than this (seconds, default: 86400 = 24h)
    pub ephemeral_ttl_secs: u64,
    /// Maximum number of scripts allowed (default: 100)
    pub max_scripts: usize,
    /// Default timeout for script execution in ms (default: 60000)
    pub default_timeout_ms: u64,
}

impl Default for ScriptsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_sandbox_profile: "default".to_string(),
            ephemeral_ttl_secs: 86400,
            max_scripts: 100,
            default_timeout_ms: constants::SCRIPTS_DEFAULT_TIMEOUT_MS,
        }
    }
}

/// Configuration for user-authored lifecycle hooks loaded from `~/.borg/hooks.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HooksConfig {
    /// Enable loading hooks from `~/.borg/hooks.json` (default: true).
    pub enabled: bool,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_entry_config_default_allowed_env_empty() {
        let config = SkillEntryConfig::default();
        assert!(config.allowed_env.is_empty());
        assert!(config.enabled);
    }

    #[test]
    fn skill_entry_config_deserialize_allowed_env() {
        let toml_str = r#"
            enabled = true
            allowed_env = ["MY_API_KEY", "OTHER_KEY"]
        "#;
        let config: SkillEntryConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.allowed_env, vec!["MY_API_KEY", "OTHER_KEY"]);
        assert!(config.enabled);
    }

    #[test]
    fn skill_entry_config_deserialize_without_allowed_env() {
        let toml_str = r#"
            enabled = false
        "#;
        let config: SkillEntryConfig = toml::from_str(toml_str).unwrap();
        assert!(config.allowed_env.is_empty());
        assert!(!config.enabled);
    }
}
