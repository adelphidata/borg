use std::borrow::Cow;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

/// Provider-lifecycle log helper. Routes a single `tracing` call to `info!`
/// when `verbose` is true (i.e. `[debug] llm_logging = true` in config), and
/// `debug!` otherwise so `RUST_LOG=cortex::llm=debug` still surfaces them
/// without touching config. Lets us record request → response-headers →
/// first-byte → stream-end timings without spamming production logs.
macro_rules! lc_log {
    ($verbose:expr, $($arg:tt)*) => {{
        if $verbose {
            tracing::info!(target: "cortex::llm::lifecycle", $($arg)*);
        } else {
            tracing::debug!(target: "cortex::llm::lifecycle", $($arg)*);
        }
    }};
}

fn short_request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

use crate::config::{Config, LlmConfig};
use crate::provider::Provider;
use crate::retry::{backoff_with_config, BackoffConfig};
use crate::types::{Message, Role, ToolDefinition};

const MAX_SSE_BUFFER: usize = crate::constants::MAX_SSE_BUFFER;

mod anthropic;
mod cost;
mod pricing;

pub(crate) use anthropic::normalize_cache_tokens;
use anthropic::{
    apply_message_cache_control, build_anthropic_messages, build_cache_control_marker,
    parse_anthropic_response, sanitize_openai_messages, strip_images,
};
pub use cost::{CostEvent, CostEventContext, CostEventSink};

pub use crate::llm_error::*;

// ── Data types ──

/// Token usage statistics from an LLM call.
#[derive(Debug, Clone, Default)]
pub struct UsageData {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// Cached prompt tokens read from the provider's prompt cache (cache hits).
    /// Populated for Anthropic (`cache_read_input_tokens`) and
    /// OpenAI-compatible providers (`prompt_tokens_details.cached_tokens`).
    pub cached_input_tokens: u64,
    /// Tokens written to the cache on this turn. Anthropic-only
    /// (`cache_creation_input_tokens`). Other providers report 0.
    pub cache_creation_tokens: u64,
    pub provider: String,
    pub model: String,
    /// Provider-reported authoritative USD cost when available (OpenRouter).
    pub provider_cost_usd: Option<String>,
}

/// Events emitted during SSE streaming from the LLM.
#[derive(Debug)]
pub enum StreamEvent {
    /// Incremental text content.
    TextDelta(String),
    /// Incremental reasoning/thinking content.
    ThinkingDelta(String),
    /// Incremental tool call data.
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    },
    /// Token usage report.
    Usage(UsageData),
    /// Provider signaled the response was cut off because the model hit
    /// `max_tokens` / `max_output_tokens`. Emitted alongside the normal
    /// stream events; the agent loop converts this into actionable feedback
    /// for the model rather than the cryptic JSON parse failure that
    /// otherwise surfaces when a tool call's argument string is unclosed.
    /// OpenAI-compat: `finish_reason == "length"`. Anthropic:
    /// `delta.stop_reason == "max_tokens"`. Non-terminal — the SSE driver
    /// still relies on `[DONE]` / `message_stop` to end the stream.
    Truncated,
    /// Stream completed.
    Done,
    /// Stream error.
    Error(String),
}

// ── OpenAI-compatible request/response types ──

#[derive(Debug, Clone, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDefinition>>,
    temperature: f32,
    max_tokens: u32,
    stream: bool,
    /// OpenAI o-series reasoning effort: "low", "medium", "high".
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    /// OpenAI `prompt_cache_key`: a stable identifier (typically the session
    /// id) that biases the provider's auto-cache toward a consistent shard
    /// across turns. OpenAI-compatible providers that don't recognize this
    /// field will simply ignore it.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    /// OpenAI `user` field (mirrors `prompt_cache_key`) — hints the provider
    /// to route consistent traffic to the same shard.
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    /// OpenAI streaming option. When `stream: true`, set
    /// `stream_options.include_usage = true` so the provider emits a final
    /// `usage` chunk — without this, OpenAI-compatible providers (OpenRouter,
    /// DeepSeek, Groq, ...) never report token counts and `/usage` shows zeros.
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    /// OpenRouter usage accounting opt-in. Only attached when cost-event
    /// streaming is enabled at runtime.
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<UsageOptions>,
}

#[derive(Debug, Clone, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Debug, Clone, Serialize)]
struct UsageOptions {
    include: bool,
}

#[derive(Debug, Deserialize)]
struct StreamChunk {
    choices: Option<Vec<StreamChoice>>,
    usage: Option<StreamUsage>,
}

#[derive(Debug, Deserialize)]
struct StreamUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
    /// OpenAI-compatible providers report cache hits inside this nested object
    /// (`prompt_tokens_details.cached_tokens`). DeepSeek and a few others
    /// surface a top-level `cached_tokens` instead.
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    cached_tokens: Option<u64>,
    /// OpenRouter's streaming usage chunk reports the dollar total under
    /// `total_cost`; older / non-streaming shapes use `cost`. Accept both
    /// so cost-event attribution stays `openrouter_usage` instead of
    /// silently regressing to `unknown` when the upstream field is renamed.
    #[serde(default, alias = "total_cost")]
    cost: Option<serde_json::Value>,
    /// OpenRouter also reports a per-source breakdown under `cost_details`.
    /// Used as a fallback when the top-level `cost` field is null or
    /// absent — without it we mis-attribute as `unknown`.
    #[serde(default)]
    cost_details: Option<CostDetails>,
}

#[derive(Debug, Deserialize)]
struct CostDetails {
    #[serde(default)]
    total_cost: Option<serde_json::Value>,
    #[serde(default)]
    upstream_inference_cost: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: Option<Delta>,
    /// Present on the terminal chunk(s). NOT used to terminate: OpenRouter
    /// splits termination across two `finish_reason` chunks (the second
    /// carries `usage`), so exiting early on the first would drop cost
    /// attribution. Termination still rides the `[DONE]` sentinel. We DO
    /// inspect this field to detect `"length"` (max-tokens cut-off) and
    /// emit `StreamEvent::Truncated` so the agent loop can give the model
    /// actionable feedback rather than a cryptic JSON-parse error.
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Delta {
    content: Option<String>,
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Debug, Deserialize)]
struct DeltaToolCall {
    index: Option<usize>,
    id: Option<String>,
    function: Option<DeltaFunction>,
}

#[derive(Debug, Deserialize)]
struct DeltaFunction {
    name: Option<String>,
    arguments: Option<String>,
}

/// Per-provider cooldown state for the circuit breaker pattern.
#[derive(Debug, Default)]
struct ProviderCooldown {
    /// When this provider can be retried.
    cooldown_until: Option<std::time::Instant>,
    /// Consecutive failure count (reset on cooldown expiry).
    error_count: u32,
    /// Why the provider is in cooldown.
    reason: Option<FailoverReason>,
}

impl ProviderCooldown {
    /// Whether the provider is currently in cooldown (cannot be used).
    fn is_active(&self) -> bool {
        match self.cooldown_until {
            Some(until) => std::time::Instant::now() < until,
            None => false,
        }
    }

    /// Record a failure and compute the next cooldown duration.
    fn record_failure(&mut self, reason: FailoverReason) {
        // If cooldown expired, reset counter (half-open circuit breaker).
        if !self.is_active() {
            self.error_count = 0;
        }
        self.error_count += 1;
        self.reason = Some(reason);

        let base_secs = match reason {
            FailoverReason::Auth | FailoverReason::AuthPermanent | FailoverReason::Billing => {
                crate::constants::PROVIDER_COOLDOWN_AUTH_BASE_SECS
            }
            _ => crate::constants::PROVIDER_COOLDOWN_TRANSIENT_BASE_SECS,
        };
        let max_exp = match reason {
            FailoverReason::Auth | FailoverReason::AuthPermanent | FailoverReason::Billing => {
                crate::constants::PROVIDER_COOLDOWN_AUTH_MAX_EXP
            }
            _ => crate::constants::PROVIDER_COOLDOWN_TRANSIENT_MAX_EXP,
        };
        let exp = self.error_count.saturating_sub(1).min(max_exp);
        let backoff_secs = base_secs * 2.0_f64.powi(exp as i32);
        self.cooldown_until =
            Some(std::time::Instant::now() + Duration::from_secs_f64(backoff_secs));
    }

    /// Reset cooldown on success.
    fn record_success(&mut self) {
        self.cooldown_until = None;
        self.error_count = 0;
        self.reason = None;
    }
}

/// A provider slot in the failover chain.
struct ProviderSlot {
    provider: Provider,
    model: String,
    keys: Vec<String>,
    temperature: f32,
    max_tokens: u32,
    base_url: Option<String>,
    cooldown: ProviderCooldown,
}

/// Multi-provider streaming LLM client with failover and key rotation.
pub struct LlmClient {
    client: Client,
    llm_config: LlmConfig,
    provider: Provider,
    api_key: String,
    /// Additional fallback keys (rotated on 401/429 errors).
    fallback_keys: Vec<String>,
    /// Provider-level failover slots.
    provider_slots: Vec<ProviderSlot>,
    /// Index of the currently active provider slot (usize::MAX = primary).
    active_slot_index: usize,
    debug_logging: bool,
    /// Optional stable cache key (typically the session / conversation id).
    /// Sent as `prompt_cache_key` on OpenAI-family requests to bias the
    /// provider's auto-cache toward a consistent shard across turns.
    prompt_cache_key: Option<String>,
    /// Optional best-effort sink for daemon cost events.
    cost_event_sink: Option<Arc<dyn CostEventSink>>,
    /// Per-call metadata supplied by borgd.
    cost_event_context: Option<CostEventContext>,
}

/// Normalize a Gemini OpenAI-compat base URL by ensuring it resolves to the
/// chat-completions endpoint.
///
/// Users commonly paste shorter roots (`https://generativelanguage.googleapis.com`,
/// `…/v1beta`, `…/v1beta/openai`) expecting them to "just work". Without
/// normalization the POST lands on a non-completions path and fails. Only
/// applies to Google's own host — proxy deployments keep their URL verbatim.
fn normalize_gemini_base(url: &str) -> Cow<'_, str> {
    const GOOGLE_HOST: &str = "generativelanguage.googleapis.com";
    const CANONICAL_SUFFIX: &str = "/v1beta/openai/chat/completions";

    if !url.contains(GOOGLE_HOST) {
        return Cow::Borrowed(url);
    }
    if url.ends_with(CANONICAL_SUFFIX) {
        return Cow::Borrowed(url);
    }

    let trimmed = url.trim_end_matches('/');
    let stripped = ["/v1beta/openai", "/openai", "/v1beta", "/v1"]
        .iter()
        .find_map(|s| trimmed.strip_suffix(s))
        .unwrap_or(trimmed);
    Cow::Owned(format!("{stripped}{CANONICAL_SUFFIX}"))
}

impl LlmClient {
    /// Effective API URL: config override → provider default, with provider-
    /// specific normalization (e.g. Gemini base-URL suffix correction).
    fn effective_base_url(&self) -> Cow<'_, str> {
        let raw = self
            .llm_config
            .base_url
            .as_deref()
            .unwrap_or_else(|| self.provider.base_url());
        match self.provider {
            Provider::Gemini => normalize_gemini_base(raw),
            _ => Cow::Borrowed(raw),
        }
    }

    /// Create a new LLM client from config (resolves provider and API keys).
    pub fn new(config: &Config) -> Result<Self> {
        let (provider, mut keys) = config.resolve_api_keys()?;
        // Air-gap policy: when `providers.outbound_allowed = false` we refuse
        // any non-Local provider before constructing the client so no HTTP
        // socket is opened against a public endpoint.
        crate::providers::ProviderRegistry::check_builtin_air_gap(
            provider,
            config.providers.outbound_allowed,
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;

        let debug_logging = config.debug.llm_logging;
        let client = Client::new();
        let api_key = keys.remove(0);

        // Build provider-level failover slots
        let mut provider_slots = Vec::new();
        for fb in &config.llm.fallback {
            match Self::resolve_fallback_slot(fb, config) {
                Ok(slot) => {
                    if let Err(e) = crate::providers::ProviderRegistry::check_builtin_air_gap(
                        slot.provider,
                        config.providers.outbound_allowed,
                    ) {
                        warn!(
                            "Skipping fallback provider '{}' under air-gap policy: {e}",
                            fb.provider
                        );
                        continue;
                    }
                    provider_slots.push(slot);
                }
                Err(e) => {
                    warn!("Skipping fallback provider '{}': {e}", fb.provider);
                }
            }
        }

        Ok(Self {
            client,
            llm_config: config.llm.clone(),
            provider,
            api_key,
            fallback_keys: keys,
            provider_slots,
            active_slot_index: usize::MAX,
            debug_logging,
            prompt_cache_key: None,
            cost_event_sink: None,
            cost_event_context: None,
        })
    }

    /// Attach a stable prompt cache key (typically the session id) so that
    /// OpenAI-family requests include `prompt_cache_key` and `user` for
    /// shard-stable auto-caching. A no-op for providers that don't honor it.
    pub fn with_prompt_cache_key(mut self, key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(key.into());
        self
    }

    /// Attach a best-effort daemon sink for per-call cost events.
    pub fn with_cost_event_sink(
        mut self,
        sink: Arc<dyn CostEventSink>,
        context: CostEventContext,
    ) -> Self {
        self.cost_event_sink = Some(sink);
        self.cost_event_context = Some(context);
        self
    }

    fn cost_events_enabled(&self) -> bool {
        self.cost_event_sink.is_some() && self.cost_event_context.is_some()
    }

    /// Practical context window of the active model in tokens. Resolves through
    /// the model registry; falls back to a conservative default for unknown
    /// models so downstream budget calculations always have a non-zero value.
    pub fn context_window(&self) -> u32 {
        crate::model_registry::context_window_for(&self.llm_config.model)
    }

    /// Hard-cap context window of the active model (≥ `context_window` for
    /// tiered models like GPT-5).
    pub fn max_context_window(&self) -> u32 {
        crate::model_registry::max_context_window_for(&self.llm_config.model)
    }

    /// Resolve a fallback config into a ProviderSlot.
    fn resolve_fallback_slot(
        fb: &crate::config::LlmFallback,
        config: &Config,
    ) -> Result<ProviderSlot> {
        let provider = Provider::from_str(&fb.provider)?;
        let mut slot_keys = Vec::new();

        // Try api_keys first
        for sr in &fb.api_keys {
            if let Ok(key) = sr.resolve() {
                if !key.is_empty() {
                    slot_keys.push(key);
                }
            }
        }

        // Try api_key SecretRef
        if slot_keys.is_empty() {
            if let Some(ref sr) = fb.api_key {
                if let Ok(key) = sr.resolve() {
                    if !key.is_empty() {
                        slot_keys.push(key);
                    }
                }
            }
        }

        // Try api_key_env
        if slot_keys.is_empty() {
            let env_var = fb
                .api_key_env
                .as_deref()
                .unwrap_or(provider.default_env_var());
            if let Ok(key) = std::env::var(env_var) {
                if !key.is_empty() {
                    slot_keys.push(key);
                }
            }
            // Also try credential store
            if slot_keys.is_empty() {
                if let Some(key) = config.resolve_credential_or_env(env_var) {
                    slot_keys.push(key);
                }
            }
        }

        // Keyless providers (e.g., Ollama) don't need API keys
        if slot_keys.is_empty() && !provider.requires_api_key() {
            slot_keys.push(String::new());
        }

        if slot_keys.is_empty() {
            bail!("No API keys found for fallback provider {provider}");
        }

        Ok(ProviderSlot {
            provider,
            model: fb.model.clone(),
            keys: slot_keys,
            temperature: fb.temperature.unwrap_or(config.llm.temperature),
            max_tokens: fb.max_tokens.unwrap_or(config.llm.max_tokens),
            base_url: fb.base_url.clone(),
            cooldown: ProviderCooldown::default(),
        })
    }

    /// Try to failover to the next available (non-cooled-down) provider slot.
    /// Returns true if a failover occurred.
    fn try_failover_provider(&mut self) -> bool {
        for (i, slot) in self.provider_slots.iter().enumerate() {
            if i == self.active_slot_index {
                continue;
            }
            if slot.cooldown.is_active() {
                debug!(
                    "Provider {} in cooldown ({:?}), skipping",
                    slot.provider, slot.cooldown.reason
                );
                continue;
            }
            // Found a usable slot — switch to it
            self.provider = slot.provider;
            self.api_key = slot.keys[0].clone();
            self.fallback_keys = slot.keys[1..].to_vec();
            self.llm_config.model = slot.model.clone();
            self.llm_config.temperature = slot.temperature;
            self.llm_config.max_tokens = slot.max_tokens;
            self.llm_config.base_url = slot.base_url.clone();
            self.active_slot_index = i;
            info!(
                "Failover to provider {} (model: {})",
                slot.provider, slot.model
            );
            return true;
        }
        false
    }

    /// Record a failure on the currently active provider slot.
    fn record_provider_failure(&mut self, reason: FailoverReason) {
        if self.active_slot_index < self.provider_slots.len() {
            self.provider_slots[self.active_slot_index]
                .cooldown
                .record_failure(reason);
        }
        // If active_slot_index == usize::MAX, it's the primary provider — no cooldown tracking
    }

    /// Record success on the currently active provider slot.
    fn record_provider_success(&mut self) {
        if self.active_slot_index < self.provider_slots.len() {
            self.provider_slots[self.active_slot_index]
                .cooldown
                .record_success();
        }
    }

    fn debug_log(&self, label: &str, content: &str) {
        if !self.debug_logging {
            return;
        }
        let dir = match crate::config::Config::data_dir() {
            Ok(d) => d.join("logs").join("debug"),
            Err(_) => return,
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::debug!("llm: failed to create debug log dir: {e}");
            return;
        }
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S_%3f");
        let path = dir.join(format!("{timestamp}_{label}.json"));
        let redacted = crate::secrets::redact_secrets(content);
        if let Err(e) = std::fs::write(&path, redacted) {
            tracing::debug!("llm: failed to write debug log: {e}");
        }
    }

    fn parse_usage_cost(value: &serde_json::Value) -> Option<String> {
        match value {
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
            _ => None,
        }
    }

    /// Resolve provider-reported cost; prefer top-level `cost`/`total_cost`, fall back to `cost_details`.
    fn extract_provider_cost_usd(usage: &StreamUsage) -> Option<String> {
        if let Some(c) = usage.cost.as_ref().and_then(Self::parse_usage_cost) {
            return Some(c);
        }
        let details = usage.cost_details.as_ref()?;
        if let Some(c) = details.total_cost.as_ref().and_then(Self::parse_usage_cost) {
            return Some(c);
        }
        details
            .upstream_inference_cost
            .as_ref()
            .and_then(Self::parse_usage_cost)
    }

    fn maybe_capture_usage_from_action(
        action: &crate::sse::SseAction,
        last_usage: &mut Option<UsageData>,
    ) {
        match action {
            crate::sse::SseAction::Emit(events) | crate::sse::SseAction::Done(events) => {
                for event in events {
                    if let StreamEvent::Usage(usage) = event {
                        *last_usage = Some(usage.clone());
                    }
                }
            }
            crate::sse::SseAction::Continue | crate::sse::SseAction::Error(_) => {}
        }
    }

    fn emit_cost_event_best_effort(
        &self,
        request_id: &str,
        usage: Option<&UsageData>,
        fallback_model: &str,
    ) {
        let (Some(sink), Some(context)) = (&self.cost_event_sink, &self.cost_event_context) else {
            return;
        };

        let provider_str = usage
            .map(|u| u.provider.clone())
            .unwrap_or_else(|| self.provider.as_str().to_string());
        let model = usage
            .map(|u| u.model.clone())
            .unwrap_or_else(|| fallback_model.to_string());
        let prompt_tokens = usage.map(|u| u.prompt_tokens as u32).unwrap_or(0);
        let completion_tokens = usage.map(|u| u.completion_tokens as u32).unwrap_or(0);

        let (cost_usd, cost_usd_present, cost_source) = match self.provider {
            Provider::OpenRouter => match usage.and_then(|u| u.provider_cost_usd.clone()) {
                Some(cost) => (Some(cost), true, "openrouter_usage".to_string()),
                None => {
                    // Diagnostic: surface why OpenRouter attribution dropped to
                    // `unknown`. Distinguishes "no usage chunk arrived" from
                    // "usage chunk had no cost field" so the eval failure mode
                    // is observable without enabling full LLM debug logging.
                    warn!(
                        target: "cortex::llm::cost",
                        req_id = %request_id,
                        usage_present = usage.is_some(),
                        prompt_tokens = prompt_tokens,
                        completion_tokens = completion_tokens,
                        "OpenRouter cost attribution fell through to 'unknown' — \
                         provider_cost_usd is None"
                    );
                    (None, false, "unknown".to_string())
                }
            },
            Provider::OpenAi => match pricing::estimate_cost_usd(
                Provider::OpenAi,
                &model,
                u64::from(prompt_tokens),
                u64::from(completion_tokens),
            ) {
                Some(cost) => (Some(cost), true, "table_openai".to_string()),
                None => (None, false, "unknown".to_string()),
            },
            Provider::Anthropic => match pricing::estimate_anthropic_cost_usd(
                &model,
                u64::from(prompt_tokens),
                u64::from(completion_tokens),
                usage.map(|u| u.cached_input_tokens).unwrap_or(0),
                usage.map(|u| u.cache_creation_tokens).unwrap_or(0),
                self.llm_config.cache.ttl,
            ) {
                Some(cost) => (Some(cost), true, "table_anthropic".to_string()),
                None => (None, false, "unknown".to_string()),
            },
            _ => (None, false, "unknown".to_string()),
        };

        let event = CostEvent {
            request_id: request_id.to_string(),
            session_id: context.session_id.clone(),
            provider: provider_str,
            model,
            cost_usd,
            cost_usd_present,
            cost_source,
            prompt_tokens,
            completion_tokens,
            timestamp_unix_ms: chrono::Utc::now().timestamp_millis(),
            kind: context.kind.clone(),
        };

        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink.emit(event))).is_err() {
            warn!("Cost-event sink panicked; dropping event");
        }
    }

    #[instrument(skip_all, fields(llm.provider = %self.provider, llm.model = %self.llm_config.model))]
    /// Stream a chat completion, sending events to the channel.
    pub async fn stream_chat(
        &mut self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        self.stream_chat_with_cancel(messages, tools, tx, CancellationToken::new())
            .await
    }

    /// Try rotating to the next fallback API key. Returns true if rotation succeeded.
    /// If `keep_old` is false, the current key is discarded (e.g. on 401/403 auth failure).
    /// If `keep_old` is true, the current key is pushed to the back of the pool (e.g. on 429 rate limit).
    fn try_rotate_key(&mut self, keep_old: bool) -> bool {
        if self.fallback_keys.is_empty() {
            return false;
        }
        let old_key = std::mem::replace(&mut self.api_key, self.fallback_keys.remove(0));
        if keep_old {
            self.fallback_keys.push(old_key);
        } else {
            info!("Discarding revoked/invalid API key");
        }
        info!(
            "Rotated to next API key ({} fallbacks remaining)",
            self.fallback_keys.len()
        );
        true
    }

    /// Stream chat with cancellation support, retry logic, and provider-level failover.
    /// Supports multi-key fallback: on 401/429 errors, rotates to the next available key.
    /// On exhausted retries, attempts failover to the next provider in the fallback chain.
    #[instrument(skip_all, fields(llm.provider = %self.provider, llm.model = %self.llm_config.model))]
    /// Stream a chat completion with cancellation support.
    pub async fn stream_chat_with_cancel(
        &mut self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        tx: mpsc::Sender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        // Strip images for non-vision models (safety net)
        let messages = if !self.provider.supports_vision(&self.llm_config.model) {
            strip_images(messages)
        } else {
            messages.to_vec()
        };
        let messages = &messages;

        // Reject models that don't support tool calling — Borg requires tools
        if !self.provider.supports_tools(&self.llm_config.model) {
            let msg = format!(
                "Model \"{}\" does not support tool calling. Borg requires tool use — pick a different model.",
                self.llm_config.model
            );
            if tx.send(StreamEvent::Error(msg.clone())).await.is_err() {
                tracing::debug!("llm: stream receiver closed while sending tool-support error");
            }
            anyhow::bail!("{msg}");
        }

        let max_provider_attempts = 1 + self.provider_slots.len();

        for _provider_attempt in 0..max_provider_attempts {
            let max_retries = self.llm_config.max_retries;
            let initial_delay = Duration::from_millis(self.llm_config.initial_retry_delay_ms);
            let total_keys = 1 + self.fallback_keys.len();
            let mut keys_tried = 0_usize;
            let mut should_failover = false;

            for attempt in 0..=max_retries {
                if cancel.is_cancelled() {
                    if tx.send(StreamEvent::Done).await.is_err() {
                        tracing::debug!("llm: stream receiver closed");
                    }
                    return Ok(());
                }

                let result = if matches!(self.provider, Provider::ClaudeCli) {
                    self.stream_chat_claude_cli_inner(messages, tools, &tx, &cancel)
                        .await
                } else if self.provider.is_openai_compatible() {
                    self.stream_chat_openai_inner(messages, tools, &tx, &cancel)
                        .await
                } else {
                    self.stream_chat_anthropic_inner(messages, tools, &tx, &cancel)
                        .await
                };

                match result {
                    Ok(()) => {
                        self.record_provider_success();
                        return Ok(());
                    }
                    Err(e) => {
                        // Rotate to next key only when the classifier says it
                        // could help (Auth / RateLimit). AuthPermanent (403)
                        // explicitly skips rotation — the account is denied.
                        let reason = e.reason();
                        let is_auth_error = matches!(reason, FailoverReason::Auth);

                        if e.should_rotate_credential()
                            && keys_tried < total_keys
                            && self.try_rotate_key(!is_auth_error)
                        {
                            keys_tried += 1;
                            info!("{reason} error, trying next API key...");
                            continue;
                        }

                        if !e.is_retryable() || attempt == max_retries {
                            // Record failure and attempt provider failover.
                            // ModelNotFound / AuthPermanent opt out — they're
                            // user-fixable, and pointlessly burning the whole
                            // fallback chain just delays the real error.
                            self.record_provider_failure(e.reason());
                            if e.should_fallback() && self.try_failover_provider() {
                                should_failover = true;
                                break;
                            }
                            // No more providers — propagate error
                            let msg = format!("{e}");
                            if tx.send(StreamEvent::Error(msg.clone())).await.is_err() {
                                tracing::debug!("llm: stream receiver closed while sending error");
                            }
                            bail!("{msg}");
                        }

                        let delay = if let LlmError::Retryable {
                            retry_after: Some(ra),
                            ..
                        } = &e
                        {
                            *ra
                        } else {
                            backoff_with_config(
                                attempt,
                                &BackoffConfig {
                                    initial: initial_delay,
                                    ..Default::default()
                                },
                            )
                        };

                        info!(
                            "Retryable error (attempt {}/{}): {e}. Retrying in {}ms...",
                            attempt + 1,
                            max_retries,
                            delay.as_millis()
                        );

                        tokio::select! {
                            _ = cancel.cancelled() => {
                                if tx.send(StreamEvent::Done).await.is_err() {
                                    tracing::debug!("llm: stream receiver closed");
                                }
                                return Ok(());
                            }
                            _ = tokio::time::sleep(delay) => {}
                        }
                    }
                }
            }

            if !should_failover {
                break;
            }
            // Continue outer loop with new provider
        }

        let msg =
            "All LLM providers exhausted. Please try again later or check your configuration."
                .to_string();
        if tx.send(StreamEvent::Error(msg.clone())).await.is_err() {
            tracing::debug!("llm: stream receiver closed while sending exhaustion error");
        }
        bail!("{msg}")
    }

    /// Non-streaming call for heartbeat and simple requests
    #[instrument(skip_all, fields(llm.provider = %self.provider, llm.model = %self.llm_config.model))]
    /// Non-streaming chat completion, returning assembled tool calls.
    pub async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<Message> {
        if self.provider.is_openai_compatible() {
            self.chat_openai(messages, tools).await
        } else {
            self.chat_anthropic(messages, tools).await
        }
    }

    // ── OpenAI-compatible path ──

    #[instrument(skip_all, fields(llm.provider = %self.provider))]
    async fn send_request(
        &self,
        body: &impl Serialize,
        cancel: &CancellationToken,
    ) -> std::result::Result<reqwest::Response, LlmError> {
        let timeout = Duration::from_millis(self.llm_config.request_timeout_ms);

        let fut =
            self.client
                .post(self.effective_base_url().as_ref())
                .headers(self.provider.build_headers(&self.api_key).map_err(|e| {
                    LlmError::Fatal {
                        source: e,
                        reason: FailoverReason::Unknown,
                    }
                })?)
                .json(body)
                .send();

        let response = tokio::select! {
            _ = cancel.cancelled() => {
                return Err(LlmError::Interrupted);
            }
            result = tokio::time::timeout(timeout, fut) => {
                match result {
                    Ok(Ok(resp)) => resp,
                    Ok(Err(e)) => {
                        return Err(classify_network_error(
                            anyhow::anyhow!("Failed to connect to {}: {e}", self.provider)
                        ));
                    }
                    Err(_) => {
                        return Err(LlmError::Retryable {
                            source: anyhow::anyhow!(
                                "Request to {} timed out after {}ms",
                                self.provider,
                                self.llm_config.request_timeout_ms
                            ),
                            retry_after: None,
                            reason: FailoverReason::Timeout,
                        });
                    }
                }
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_else(|e| {
                tracing::warn!("Failed to read error response body: {e}");
                String::new()
            });
            return Err(classify_status(status, &body, self.provider));
        }

        Ok(response)
    }

    #[instrument(skip_all, fields(llm.provider = "claude-cli"))]
    async fn stream_chat_claude_cli_inner(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        tx: &mpsc::Sender<StreamEvent>,
        cancel: &CancellationToken,
    ) -> std::result::Result<(), LlmError> {
        use crate::claude_cli;

        let cli_path = self
            .llm_config
            .claude_cli_path
            .as_ref()
            .map(std::path::PathBuf::from)
            .or_else(claude_cli::detect_cli_path)
            .ok_or_else(|| LlmError::Fatal {
                source: anyhow::anyhow!(
                    "Claude CLI binary not found. Install Claude Code or set CLAUDE_CLI_PATH."
                ),
                reason: FailoverReason::Unknown,
            })?;

        let model = self.provider.normalize_model(&self.llm_config.model);

        claude_cli::stream_claude_cli(
            &cli_path,
            messages,
            tools,
            &model,
            self.llm_config.temperature,
            tx,
            cancel,
        )
        .await
    }

    #[instrument(skip_all, fields(llm.provider = "openai"))]
    async fn stream_chat_openai_inner(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        tx: &mpsc::Sender<StreamEvent>,
        cancel: &CancellationToken,
    ) -> std::result::Result<(), LlmError> {
        let model = self.provider.normalize_model(&self.llm_config.model);
        let reasoning_effort = self
            .llm_config
            .thinking
            .openai_reasoning_effort()
            .map(String::from);
        let sanitized_messages = sanitize_openai_messages(messages);

        let request = ChatRequest {
            model: model.clone(),
            messages: sanitized_messages,
            tools: tools.map(<[ToolDefinition]>::to_vec),
            temperature: self.llm_config.temperature,
            max_tokens: self.llm_config.max_tokens,
            stream: true,
            reasoning_effort,
            prompt_cache_key: self.prompt_cache_key.clone(),
            user: self.prompt_cache_key.clone(),
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            usage: (self.provider == Provider::OpenRouter && self.cost_events_enabled())
                .then_some(UsageOptions { include: true }),
        };

        debug!(
            "Sending streaming request to {} (model: {})",
            self.provider, model
        );

        if self.debug_logging {
            if let Ok(json) = serde_json::to_string_pretty(&request) {
                self.debug_log("request_openai", &json);
            }
        }

        let req_id = short_request_id();
        let verbose = self.debug_logging;
        let lc_start = Instant::now();
        lc_log!(
            verbose,
            req_id = %req_id,
            provider = %self.provider,
            model = %model,
            base_url = %self.effective_base_url(),
            msg_count = messages.len(),
            tool_count = tools.map(<[ToolDefinition]>::len).unwrap_or(0),
            "openai-compat: request start"
        );

        let response = match self.send_request(&request, cancel).await {
            Ok(r) => {
                lc_log!(
                    verbose,
                    req_id = %req_id,
                    elapsed_ms = lc_start.elapsed().as_millis() as u64,
                    "openai-compat: response headers received"
                );
                r
            }
            Err(e) => {
                warn!(
                    target: "cortex::llm::lifecycle",
                    req_id = %req_id,
                    elapsed_ms = lc_start.elapsed().as_millis() as u64,
                    "openai-compat: request failed before stream: {e}"
                );
                return Err(e);
            }
        };

        let stream = response.bytes_stream();
        let provider_str = self.provider.as_str().to_string();
        let model_str = self.llm_config.model.clone();

        let mut first_byte_ms: Option<u64> = None;
        let mut line_count: u64 = 0;
        let mut last_usage: Option<UsageData> = None;
        let result = crate::sse::process_sse_stream(
            stream,
            tx,
            cancel,
            self.llm_config.stream_chunk_timeout_secs,
            MAX_SSE_BUFFER,
            |line| {
                if first_byte_ms.is_none() {
                    first_byte_ms = Some(lc_start.elapsed().as_millis() as u64);
                }
                line_count += 1;
                let action = Self::parse_openai_sse_line(line, &provider_str, &model_str);
                Self::maybe_capture_usage_from_action(&action, &mut last_usage);
                action
            },
        )
        .await;

        match &result {
            Ok(()) => lc_log!(
                verbose,
                req_id = %req_id,
                elapsed_ms = lc_start.elapsed().as_millis() as u64,
                first_byte_ms = ?first_byte_ms,
                line_count = line_count,
                "openai-compat: stream end ok"
            ),
            Err(e) => warn!(
                target: "cortex::llm::lifecycle",
                req_id = %req_id,
                elapsed_ms = lc_start.elapsed().as_millis() as u64,
                first_byte_ms = ?first_byte_ms,
                line_count = line_count,
                "openai-compat: stream end err: {e}"
            ),
        }
        if result.is_ok() {
            self.emit_cost_event_best_effort(&req_id, last_usage.as_ref(), &model_str);
        }
        result
    }

    /// Parse a single SSE line from an OpenAI-compatible provider.
    fn parse_openai_sse_line(line: &str, provider: &str, model: &str) -> crate::sse::SseAction {
        use crate::sse::SseAction;

        let data = match line.strip_prefix("data: ") {
            Some(d) => d,
            None => return SseAction::Continue,
        };

        if data.trim() == "[DONE]" {
            return SseAction::Done(vec![StreamEvent::Done]);
        }

        match serde_json::from_str::<StreamChunk>(data) {
            Ok(chunk) => {
                let mut events = Vec::new();

                if let Some(usage) = chunk.usage {
                    let cached_input_tokens = usage
                        .prompt_tokens_details
                        .as_ref()
                        .and_then(|d| d.cached_tokens)
                        .or(usage.cached_tokens)
                        .unwrap_or(0);
                    let provider_cost_usd = Self::extract_provider_cost_usd(&usage);
                    if provider == "openrouter" && provider_cost_usd.is_none() {
                        // Diagnostic: capture the raw `usage` JSON when an
                        // OpenRouter chunk lacks a recognizable cost field, so
                        // the actual response shape is observable in
                        // production logs without enabling full debug logging.
                        let raw_usage = serde_json::from_str::<serde_json::Value>(data)
                            .ok()
                            .and_then(|v| v.get("usage").cloned())
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "<unparseable>".to_string());
                        warn!(
                            target: "cortex::llm::cost",
                            usage = %raw_usage,
                            "OpenRouter usage chunk has no recognizable cost field — \
                             cost-event attribution will be 'unknown'"
                        );
                    }
                    events.push(StreamEvent::Usage(UsageData {
                        prompt_tokens: usage.prompt_tokens.unwrap_or(0),
                        completion_tokens: usage.completion_tokens.unwrap_or(0),
                        total_tokens: usage.total_tokens.unwrap_or(0),
                        cached_input_tokens,
                        cache_creation_tokens: 0,
                        provider: provider.to_string(),
                        model: model.to_string(),
                        provider_cost_usd,
                    }));
                }

                if let Some(choices) = chunk.choices {
                    for choice in choices {
                        if let Some(delta) = choice.delta {
                            if let Some(content) = delta.content {
                                events.push(StreamEvent::TextDelta(content));
                            }
                            if let Some(tool_calls) = delta.tool_calls {
                                for tc in tool_calls {
                                    events.push(StreamEvent::ToolCallDelta {
                                        index: tc.index.unwrap_or(0),
                                        id: tc.id,
                                        name: tc.function.as_ref().and_then(|f| f.name.clone()),
                                        arguments_delta: tc
                                            .function
                                            .as_ref()
                                            .and_then(|f| f.arguments.clone())
                                            .unwrap_or_default(),
                                    });
                                }
                            }
                        }
                        // OpenRouter (and others) split termination across
                        // two chunks: the first carries `finish_reason`, the
                        // second carries `finish_reason` + `usage`. Returning
                        // `Done` here would race past the usage chunk and
                        // permanently strand cost attribution at "unknown".
                        // Rely solely on the `[DONE]` sentinel to terminate.
                        // We DO inspect `finish_reason == "length"` so the
                        // agent loop can convert the inevitable truncated
                        // tool-call JSON into a clear "split your work" hint
                        // for the model.
                        if choice.finish_reason.as_deref() == Some("length") {
                            events.push(StreamEvent::Truncated);
                        }
                    }
                }

                if events.is_empty() {
                    SseAction::Continue
                } else {
                    SseAction::Emit(events)
                }
            }
            Err(e) => {
                warn!("Failed to parse SSE chunk: {e}");
                SseAction::Continue
            }
        }
    }

    #[instrument(skip_all, fields(llm.provider = "openai"))]
    async fn chat_openai(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<Message> {
        let model = self.provider.normalize_model(&self.llm_config.model);
        let reasoning_effort = self
            .llm_config
            .thinking
            .openai_reasoning_effort()
            .map(String::from);
        let request = ChatRequest {
            model,
            messages: messages.to_vec(),
            tools: tools.map(<[ToolDefinition]>::to_vec),
            temperature: self.llm_config.temperature,
            max_tokens: self.llm_config.max_tokens,
            stream: false,
            reasoning_effort,
            prompt_cache_key: self.prompt_cache_key.clone(),
            user: self.prompt_cache_key.clone(),
            stream_options: None,
            usage: (self.provider == Provider::OpenRouter && self.cost_events_enabled())
                .then_some(UsageOptions { include: true }),
        };

        #[derive(Deserialize)]
        struct ChatResponse {
            choices: Vec<ChatChoice>,
            #[serde(default)]
            usage: Option<StreamUsage>,
        }
        #[derive(Deserialize)]
        struct ChatChoice {
            message: Message,
        }

        let max_retries = self.llm_config.max_retries;
        let initial_delay = Duration::from_millis(self.llm_config.initial_retry_delay_ms);
        let cancel = CancellationToken::new();
        let req_id = short_request_id();
        let request_model = self.llm_config.model.clone();

        for attempt in 0..=max_retries {
            let result = self.send_request(&request, &cancel).await;
            match result {
                Ok(response) => {
                    let resp: ChatResponse = response.json().await?;
                    let message = resp
                        .choices
                        .into_iter()
                        .next()
                        .map(|c| c.message)
                        .context("No response from LLM")?;
                    let usage = resp.usage.map(|usage| UsageData {
                        prompt_tokens: usage.prompt_tokens.unwrap_or(0),
                        completion_tokens: usage.completion_tokens.unwrap_or(0),
                        total_tokens: usage.total_tokens.unwrap_or(0),
                        cached_input_tokens: usage
                            .prompt_tokens_details
                            .as_ref()
                            .and_then(|d| d.cached_tokens)
                            .or(usage.cached_tokens)
                            .unwrap_or(0),
                        cache_creation_tokens: 0,
                        provider: self.provider.as_str().to_string(),
                        model: request_model.clone(),
                        provider_cost_usd: Self::extract_provider_cost_usd(&usage),
                    });
                    self.emit_cost_event_best_effort(&req_id, usage.as_ref(), &request_model);
                    return Ok(message);
                }
                Err(e) => {
                    if !e.is_retryable() || attempt == max_retries {
                        bail!("{e}");
                    }
                    let delay = if let LlmError::Retryable {
                        retry_after: Some(ra),
                        ..
                    } = &e
                    {
                        *ra
                    } else {
                        backoff_with_config(
                            attempt,
                            &BackoffConfig {
                                initial: initial_delay,
                                ..Default::default()
                            },
                        )
                    };
                    info!(
                        "Non-streaming retryable error (attempt {}/{}): {e}. Retrying in {}ms...",
                        attempt + 1,
                        max_retries,
                        delay.as_millis()
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
        bail!("All retries exhausted for non-streaming OpenAI request")
    }

    // ── Anthropic path ──

    fn build_anthropic_request(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        stream: bool,
    ) -> serde_json::Value {
        let model = self.provider.normalize_model(&self.llm_config.model);

        // Extract system message
        let system_text: Option<String> = messages
            .iter()
            .find(|m| m.role == Role::System)
            .and_then(|m| m.text_content().map(String::from));

        // Convert messages (skip system)
        let anthropic_messages = build_anthropic_messages(messages);

        // Convert tools to Anthropic format
        let anthropic_tools: Option<Vec<serde_json::Value>> = tools.map(|ts| {
            ts.iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.function.name,
                        "description": t.function.description,
                        "input_schema": t.function.parameters,
                    })
                })
                .collect()
        });

        let cache_cfg = &self.llm_config.cache;
        let cache_enabled = cache_cfg.enabled;
        let cache_marker = build_cache_control_marker(cache_cfg.ttl);
        let (cache_system_eff, cache_tools_eff, rolling_eff) = cache_cfg.effective_layout();

        let mut body = if let Some(budget) = self.llm_config.thinking.budget_tokens() {
            // When thinking is enabled: omit temperature (Anthropic requirement),
            // ensure max_tokens accommodates both thinking budget and response.
            let max_tokens = self.llm_config.max_tokens.max(budget + 1024);
            serde_json::json!({
                "model": model,
                "max_tokens": max_tokens,
                "stream": stream,
                "thinking": {
                    "type": "enabled",
                    "budget_tokens": budget
                }
            })
        } else {
            serde_json::json!({
                "model": model,
                "max_tokens": self.llm_config.max_tokens,
                "temperature": self.llm_config.temperature,
                "stream": stream,
            })
        };

        if let Some(sys) = system_text {
            if cache_enabled && cache_system_eff {
                // Structured system block with cache_control so the provider can
                // reuse the (stable) system prompt across turns.
                body["system"] = serde_json::json!([
                    {
                        "type": "text",
                        "text": sys,
                        "cache_control": &cache_marker,
                    }
                ]);
            } else {
                body["system"] = serde_json::json!(sys);
            }
        }

        let mut anthropic_messages = anthropic_messages;
        if cache_enabled {
            apply_message_cache_control(&mut anthropic_messages, rolling_eff, &cache_marker);
        }
        body["messages"] = serde_json::json!(anthropic_messages);
        if let Some(mut tools_json) = anthropic_tools {
            // Attach a cache_control marker to the last tool definition so the
            // entire (stable) tools array becomes a single cache breakpoint.
            if cache_enabled && cache_tools_eff {
                if let Some(last) = tools_json.last_mut() {
                    if let Some(obj) = last.as_object_mut() {
                        obj.insert("cache_control".to_string(), cache_marker);
                    }
                }
            }
            body["tools"] = serde_json::json!(tools_json);
        }

        body
    }

    #[instrument(skip_all, fields(llm.provider = "anthropic"))]
    async fn stream_chat_anthropic_inner(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        tx: &mpsc::Sender<StreamEvent>,
        cancel: &CancellationToken,
    ) -> std::result::Result<(), LlmError> {
        let body = self.build_anthropic_request(messages, tools, true);
        let model = self.provider.normalize_model(&self.llm_config.model);

        debug!("Sending streaming request to Anthropic (model: {})", model);

        if self.debug_logging {
            if let Ok(json) = serde_json::to_string_pretty(&body) {
                self.debug_log("request_anthropic", &json);
            }
        }

        let req_id = short_request_id();
        let verbose = self.debug_logging;
        let lc_start = Instant::now();
        lc_log!(
            verbose,
            req_id = %req_id,
            provider = %self.provider,
            model = %model,
            base_url = %self.effective_base_url(),
            msg_count = messages.len(),
            tool_count = tools.map(<[ToolDefinition]>::len).unwrap_or(0),
            "anthropic: request start"
        );

        let response = match self.send_request(&body, cancel).await {
            Ok(r) => {
                lc_log!(
                    verbose,
                    req_id = %req_id,
                    elapsed_ms = lc_start.elapsed().as_millis() as u64,
                    "anthropic: response headers received"
                );
                r
            }
            Err(e) => {
                warn!(
                    target: "cortex::llm::lifecycle",
                    req_id = %req_id,
                    elapsed_ms = lc_start.elapsed().as_millis() as u64,
                    "anthropic: request failed before stream: {e}"
                );
                return Err(e);
            }
        };

        let stream = response.bytes_stream();
        let provider_str = self.provider.as_str().to_string();
        let model_str = self.llm_config.model.clone();

        // Mutable state for Anthropic's stateful event stream
        let mut current_tool_index: usize = 0;
        let mut current_block_is_tool = false;
        let mut current_block_is_thinking = false;
        // `input_tokens` and cache counts arrive on `message_start`; only
        // `output_tokens` lands on `message_delta`. Stash here to merge —
        // without this the emitted Usage has `prompt_tokens = 0`.
        let mut pending_input_tokens: u64 = 0;
        let mut pending_cached_input_tokens: u64 = 0;
        let mut pending_cache_creation_tokens: u64 = 0;
        let mut first_byte_ms: Option<u64> = None;
        let mut line_count: u64 = 0;
        let mut last_usage: Option<UsageData> = None;

        let result = crate::sse::process_sse_stream(
            stream,
            tx,
            cancel,
            self.llm_config.stream_chunk_timeout_secs,
            MAX_SSE_BUFFER,
            |line| {
                if first_byte_ms.is_none() {
                    first_byte_ms = Some(lc_start.elapsed().as_millis() as u64);
                }
                line_count += 1;
                let action = Self::parse_anthropic_sse_line(
                    line,
                    &provider_str,
                    &model_str,
                    &mut current_tool_index,
                    &mut current_block_is_tool,
                    &mut current_block_is_thinking,
                    &mut pending_input_tokens,
                    &mut pending_cached_input_tokens,
                    &mut pending_cache_creation_tokens,
                );
                Self::maybe_capture_usage_from_action(&action, &mut last_usage);
                action
            },
        )
        .await;

        match &result {
            Ok(()) => lc_log!(
                verbose,
                req_id = %req_id,
                elapsed_ms = lc_start.elapsed().as_millis() as u64,
                first_byte_ms = ?first_byte_ms,
                line_count = line_count,
                "anthropic: stream end ok"
            ),
            Err(e) => warn!(
                target: "cortex::llm::lifecycle",
                req_id = %req_id,
                elapsed_ms = lc_start.elapsed().as_millis() as u64,
                first_byte_ms = ?first_byte_ms,
                line_count = line_count,
                "anthropic: stream end err: {e}"
            ),
        }
        if result.is_ok() {
            self.emit_cost_event_best_effort(&req_id, last_usage.as_ref(), &model_str);
        }
        result
    }

    /// Parse a single SSE line from Anthropic's streaming API.
    #[allow(clippy::too_many_arguments)]
    fn parse_anthropic_sse_line(
        line: &str,
        provider: &str,
        model: &str,
        current_tool_index: &mut usize,
        current_block_is_tool: &mut bool,
        current_block_is_thinking: &mut bool,
        pending_input_tokens: &mut u64,
        pending_cached_input_tokens: &mut u64,
        pending_cache_creation_tokens: &mut u64,
    ) -> crate::sse::SseAction {
        use crate::sse::SseAction;

        let data = match line.strip_prefix("data: ") {
            Some(d) => d,
            None => return SseAction::Continue,
        };

        let event = match serde_json::from_str::<serde_json::Value>(data) {
            Ok(e) => e,
            Err(_) => return SseAction::Continue,
        };

        let event_type = event["type"].as_str().unwrap_or("");

        match event_type {
            "message_start" => {
                if let Some(usage) = event["message"]["usage"].as_object() {
                    let (cached, created) =
                        normalize_cache_tokens(&serde_json::Value::Object(usage.clone()));
                    *pending_cached_input_tokens = cached;
                    *pending_cache_creation_tokens = created;
                    *pending_input_tokens = usage
                        .get("input_tokens")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                }
                SseAction::Continue
            }
            "content_block_start" => {
                let block = &event["content_block"];
                let block_type = block["type"].as_str().unwrap_or("");
                *current_block_is_tool = false;
                *current_block_is_thinking = false;
                match block_type {
                    "tool_use" => {
                        *current_block_is_tool = true;
                        let id = block["id"].as_str().map(String::from);
                        let name = block["name"].as_str().map(String::from);
                        SseAction::Emit(vec![StreamEvent::ToolCallDelta {
                            index: *current_tool_index,
                            id,
                            name,
                            arguments_delta: String::new(),
                        }])
                    }
                    "thinking" => {
                        *current_block_is_thinking = true;
                        SseAction::Continue
                    }
                    _ => SseAction::Continue,
                }
            }
            "content_block_delta" => {
                let delta = &event["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        if let Some(text) = delta["text"].as_str() {
                            SseAction::Emit(vec![StreamEvent::TextDelta(text.to_string())])
                        } else {
                            SseAction::Continue
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta["thinking"].as_str() {
                            SseAction::Emit(vec![StreamEvent::ThinkingDelta(text.to_string())])
                        } else {
                            SseAction::Continue
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(json_delta) = delta["partial_json"].as_str() {
                            SseAction::Emit(vec![StreamEvent::ToolCallDelta {
                                index: *current_tool_index,
                                id: None,
                                name: None,
                                arguments_delta: json_delta.to_string(),
                            }])
                        } else {
                            SseAction::Continue
                        }
                    }
                    _ => {
                        // For thinking blocks, text comes as text_delta
                        if *current_block_is_thinking {
                            if let Some(text) = delta["text"].as_str() {
                                return SseAction::Emit(vec![StreamEvent::ThinkingDelta(
                                    text.to_string(),
                                )]);
                            }
                        }
                        SseAction::Continue
                    }
                }
            }
            "content_block_stop" => {
                if *current_block_is_tool {
                    *current_tool_index += 1;
                }
                SseAction::Continue
            }
            "message_stop" => SseAction::Done(vec![StreamEvent::Done]),
            "message_delta" => {
                let mut events = Vec::new();
                if let Some(usage) = event["usage"].as_object() {
                    let delta_input = usage
                        .get("input_tokens")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    // Anthropic reports `input_tokens` on `message_start` and
                    // omits it from `message_delta`. Prefer whichever is larger
                    // so cache-refresh paths that re-emit input still win.
                    let input = delta_input.max(*pending_input_tokens);
                    let output = usage
                        .get("output_tokens")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    let (delta_cached, delta_created) =
                        normalize_cache_tokens(&serde_json::Value::Object(usage.clone()));
                    let cached_input_tokens = delta_cached.max(*pending_cached_input_tokens);
                    let cache_creation_tokens = delta_created.max(*pending_cache_creation_tokens);
                    events.push(StreamEvent::Usage(UsageData {
                        prompt_tokens: input,
                        completion_tokens: output,
                        total_tokens: input + output,
                        cached_input_tokens,
                        cache_creation_tokens,
                        provider: provider.to_string(),
                        model: model.to_string(),
                        provider_cost_usd: None,
                    }));
                }
                if let Some(stop_reason) = event["delta"]["stop_reason"].as_str() {
                    if stop_reason == "max_tokens" {
                        events.push(StreamEvent::Truncated);
                    }
                    events.push(StreamEvent::Done);
                    return SseAction::Done(events);
                }
                if events.is_empty() {
                    SseAction::Continue
                } else {
                    SseAction::Emit(events)
                }
            }
            "error" => {
                let err_msg = event["error"]["message"]
                    .as_str()
                    .unwrap_or("unknown error");
                SseAction::Error(LlmError::Retryable {
                    source: anyhow::anyhow!("Anthropic stream error: {err_msg}"),
                    retry_after: None,
                    reason: FailoverReason::Overloaded,
                })
            }
            _ => SseAction::Continue,
        }
    }

    #[instrument(skip_all, fields(llm.provider = "anthropic"))]
    async fn chat_anthropic(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<Message> {
        let body = self.build_anthropic_request(messages, tools, false);
        let max_retries = self.llm_config.max_retries;
        let initial_delay = Duration::from_millis(self.llm_config.initial_retry_delay_ms);
        let cancel = CancellationToken::new();
        let req_id = short_request_id();
        let request_model = self.llm_config.model.clone();

        for attempt in 0..=max_retries {
            let result = self.send_request(&body, &cancel).await;
            match result {
                Ok(response) => {
                    let resp: serde_json::Value = response.json().await?;
                    let message = parse_anthropic_response(&resp)?;
                    let usage = resp["usage"].as_object().map(|usage| {
                        let usage_value = serde_json::Value::Object(usage.clone());
                        let (cached_input_tokens, cache_creation_tokens) =
                            normalize_cache_tokens(&usage_value);
                        UsageData {
                            prompt_tokens: usage
                                .get("input_tokens")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0),
                            completion_tokens: usage
                                .get("output_tokens")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0),
                            total_tokens: usage
                                .get("input_tokens")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0)
                                + usage
                                    .get("output_tokens")
                                    .and_then(serde_json::Value::as_u64)
                                    .unwrap_or(0),
                            cached_input_tokens,
                            cache_creation_tokens,
                            provider: self.provider.as_str().to_string(),
                            model: request_model.clone(),
                            provider_cost_usd: None,
                        }
                    });
                    self.emit_cost_event_best_effort(&req_id, usage.as_ref(), &request_model);
                    return Ok(message);
                }
                Err(e) => {
                    if !e.is_retryable() || attempt == max_retries {
                        bail!("{e}");
                    }
                    let delay = if let LlmError::Retryable {
                        retry_after: Some(ra),
                        ..
                    } = &e
                    {
                        *ra
                    } else {
                        backoff_with_config(
                            attempt,
                            &BackoffConfig {
                                initial: initial_delay,
                                ..Default::default()
                            },
                        )
                    };
                    info!(
                        "Non-streaming retryable error (attempt {}/{}): {e}. Retrying in {}ms...",
                        attempt + 1,
                        max_retries,
                        delay.as_millis()
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
        bail!("All retries exhausted for non-streaming Anthropic request")
    }
}

// ── OpenAI message sanitization ──

#[cfg(test)]
mod tests;
