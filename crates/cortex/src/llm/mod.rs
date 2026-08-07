use std::borrow::Cow;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use futures_util::{FutureExt, StreamExt};
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

/// Whether the (provider, model) pair accepts Anthropic-style `cache_control`
/// markers in the request body. Always true for the native Anthropic
/// provider. True for OpenRouter only when the model is Anthropic-backed
/// (Claude family) — sending the marker on, say, `openai/gpt-4o-mini` via
/// OpenRouter is at best ignored and at worst rejected. False for every
/// other provider, which use implicit caching keyed by `prompt_cache_key`
/// or no cache at all.
pub(crate) fn supports_anthropic_cache_control(provider: Provider, model: &str) -> bool {
    match provider {
        Provider::Anthropic => true,
        Provider::OpenRouter => {
            // Anchor on prefix rather than `contains("claude")` so hypothetical
            // slugs like `anti-claude-7b` or vendor-prefixed `xai/claude-clone`
            // can't sneak past. Covers the canonical OpenRouter form
            // (`anthropic/claude-…`) and any router that strips the vendor
            // prefix down to a bare `claude-…` slug.
            let m = model.to_ascii_lowercase();
            m.starts_with("anthropic/") || m.starts_with("claude-")
        }
        _ => false,
    }
}

/// Inject Anthropic `cache_control` markers into an OpenAI-compatible chat
/// request body in place. Used when proxying Claude through OpenRouter.
///
/// Marks the system message (when `cache_system` is set in the layout) and
/// applies a rolling tail of breakpoints to the most recent
/// `rolling_messages` non-tool messages. Tool-role messages are skipped
/// because OpenRouter silently hangs when `cache_control` lands on them.
/// The tools-array breakpoint from the Anthropic-native strategy is *not*
/// applied here — OpenRouter's tools array uses OpenAI's
/// `{type: "function", function: {…}}` envelope which doesn't accept
/// `cache_control`.
///
/// The system breakpoint goes at the end of the *stable prefix* when one is
/// supplied, mirroring the native-Anthropic split: marking the whole system
/// message caches nothing because the prompt's dynamic suffix ends with a
/// second-resolution `Current Time` and is byte-different every turn. On a
/// missing or non-matching prefix this degrades to marking the whole
/// message — the split is lost, never correctness.
fn apply_openai_cache_markers(
    body: &mut serde_json::Value,
    cache_cfg: &crate::config::PromptCacheConfig,
    system_stable_prefix: Option<&str>,
) {
    let marker = build_cache_control_marker(cache_cfg.ttl);
    // cache_tools_eff is intentionally ignored: OpenAI's tools envelope
    // (`{type: "function", function: {…}}`) doesn't accept cache_control,
    // so the breakpoint that lands on the last tool definition for native
    // Anthropic has no analog here.
    let (cache_system_eff, _cache_tools_eff, rolling_eff) = cache_cfg.effective_layout();
    let Some(messages) = body.get_mut("messages").and_then(|v| v.as_array_mut()) else {
        return;
    };
    if cache_system_eff {
        if let Some(sys) = messages
            .iter_mut()
            .find(|m| m.get("role").and_then(serde_json::Value::as_str) == Some("system"))
        {
            let split = sys
                .get("content")
                .and_then(serde_json::Value::as_str)
                .zip(system_stable_prefix)
                .filter(|(text, prefix)| {
                    !prefix.is_empty() && prefix.len() < text.len() && text.starts_with(prefix)
                })
                .map(|(text, prefix)| {
                    serde_json::json!([
                        { "type": "text", "text": &text[..prefix.len()], "cache_control": marker },
                        { "type": "text", "text": &text[prefix.len()..] },
                    ])
                });
            match split {
                Some(blocks) => sys["content"] = blocks,
                None => apply_cache_control_to_message(sys, &marker),
            }
        }
    }
    apply_message_cache_control(messages, rolling_eff, &marker, true);
}

use crate::config::{Config, LlmConfig};
use crate::llm::provider::Provider;
use crate::types::{Message, Role, ToolDefinition};
use crate::util::retry::{backoff_with_config, BackoffConfig};

const MAX_SSE_BUFFER: usize = crate::constants::MAX_SSE_BUFFER;

mod anthropic;
mod aux_admission;
mod aux_model_picker;
pub(crate) mod bedrock;
mod blackhole;
pub(crate) mod cloud_auth;
mod cost;
pub mod credential_pool;
pub mod credential_sources;
pub mod deprecations;
pub(crate) mod gemini;
pub mod llm_error;
pub mod model_registry;
pub mod oauth_refresh;
mod pricing;
pub mod provider;
pub mod rate_limit_headers;
pub mod reasoning_timeouts;
mod transport;
pub(crate) mod vertex;

pub(crate) use anthropic::normalize_cache_tokens;
use anthropic::{
    apply_cache_control_to_message, apply_message_cache_control, build_anthropic_messages,
    build_cache_control_marker, parse_anthropic_response, repair_message_sequence,
    sanitize_openai_messages, strip_images,
};
pub use cost::{CostEvent, CostEventContext, CostEventSink};
pub use credential_pool::{
    CredentialPool, CredentialSource, CredentialStatus, CredentialStrategy, LeaseHandle,
    PooledCredential, ResolveError,
};
use transport::{Transport, TransportRequest};

pub use crate::llm::llm_error::*;

/// Which wire transport a request is routed to, once provider + model are
/// known. Produced by [`LlmClient::wire_path`] — the single classifier shared
/// by the streaming and non-streaming dispatch paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WirePath {
    /// AWS Bedrock (SigV4-signed, event-stream framing).
    Bedrock,
    /// Vertex AI serving an Anthropic (Claude) model — Anthropic wire format.
    VertexAnthropic,
    /// Native Gemini `generateContent` transport.
    Gemini,
    /// OpenAI-compatible chat-completions transport.
    OpenAi,
    /// Anthropic `/v1/messages` transport (also the default fallback).
    Anthropic,
}

impl WirePath {
    /// The transport implementation for this wire path. Keeping the mapping
    /// here means the streaming and non-streaming dispatch paths cannot
    /// route the same [`WirePath`] to different transports.
    fn transport(self) -> &'static dyn Transport {
        match self {
            WirePath::Bedrock => transport::bedrock_transport(),
            WirePath::VertexAnthropic => transport::vertex_anthropic_transport(),
            WirePath::Gemini => transport::gemini_transport(),
            WirePath::OpenAi => transport::openai_compat_transport(),
            WirePath::Anthropic => transport::anthropic_transport(),
        }
    }
}

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
    /// Provider signaled the response was ending because the model chose to
    /// call tools (OpenAI-compat `finish_reason == "tool_calls"`, Anthropic
    /// `stop_reason == "tool_use"`). Some providers emit this with an empty
    /// tool-call payload — the model signalled intent but no call shipped.
    /// The agent records the flag so a tool-intent turn that arrives with
    /// zero parsed tool calls can be nudged instead of silently treated as
    /// a finished text answer. Non-terminal.
    ToolIntent,
    /// The client failed over to a fallback provider/model mid-request. Emitted
    /// at the switch, before the (re)attempt that produces the response, so it
    /// arrives ahead of `Done`. The agent records `(provider, model)` and the
    /// next iteration's `<environment>` section reports the model that actually
    /// served the response instead of the configured primary — otherwise the
    /// model misidentifies itself during a primary outage. Non-terminal.
    Failover { provider: String, model: String },
    /// A request attempt is about to go out on the wire. Emitted once per
    /// attempt — the first try, every retry, and every provider-failover leg —
    /// with `attempt` counting retries within the current provider leg (0 for
    /// the first try).
    ///
    /// This exists so downstream liveness watchdogs can distinguish "waiting on
    /// a slow-but-healthy provider" from "wedged". Between attempts a retry
    /// storm or a long prefill produces no other event for minutes; without
    /// this marker that silence is indistinguishable from a frozen child. It is
    /// discrete (one per attempt) and originates in the transport, so a hung
    /// tool cannot forge it. Non-terminal.
    AttemptStarted { attempt: u32 },
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
    /// OpenRouter sticky routing key (mirrors `prompt_cache_key`). Per
    /// OpenRouter's prompt-caching docs, a present `session_id` is used
    /// directly as the routing key and activates provider stickiness on the
    /// first successful request — without it, routing falls back to hashing
    /// the opening messages and each call can land on a different upstream
    /// than the conversation it serves. Attached only for OpenRouter.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
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
    /// OpenRouter passes Anthropic's `cache_creation_input_tokens` field
    /// through verbatim when the upstream model is Claude. Without
    /// extracting it here, the cache *write* tokens charged on the first
    /// turn after a marker change would never reach the cost-event
    /// pipeline and cache spend would be invisible.
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
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
    /// Credential pool for this slot. `None` when the DB is unreachable at
    /// construction time (tests, embedded usage); the failover loop falls
    /// back to the legacy `keys` Vec.
    pool: Option<Arc<credential_pool::CredentialPool>>,
    temperature: f32,
    max_tokens: u32,
    base_url: Option<String>,
    cooldown: ProviderCooldown,
}

/// Identity of the primary provider head, snapshotted the first time the
/// client fails over away from it so the primary can be restored once its
/// cooldown expires. Without this, `try_failover_provider` overwrites the
/// client's active identity in place and the primary is unreachable for the
/// rest of the client's life even after a transient rate-limit window resets.
struct PrimaryIdentity {
    provider: Provider,
    model: String,
    api_key: String,
    fallback_keys: Vec<String>,
    pool: Option<Arc<credential_pool::CredentialPool>>,
    temperature: f32,
    max_tokens: u32,
    base_url: Option<String>,
}

/// Multi-provider streaming LLM client with failover and key rotation.
pub struct LlmClient {
    client: Client,
    llm_config: LlmConfig,
    provider: Provider,
    api_key: String,
    /// Additional fallback keys. Used only as the legacy rotation path when
    /// `primary_pool` is `None` (DB unreachable). When the pool is present,
    /// it owns rotation and this Vec is unused at runtime.
    fallback_keys: Vec<String>,
    /// Credential pool for the *active* provider. `None` only when the pool
    /// could not be constructed (DB unavailable at LlmClient creation time).
    primary_pool: Option<Arc<credential_pool::CredentialPool>>,
    /// Lease held during an in-flight request. Drops at the end of each
    /// attempt to return the credential's lease counter to the pool.
    _active_lease: Option<credential_pool::LeaseHandle>,
    /// Per-credential `base_url` override pulled from the active lease at
    /// acquire time. `None` outside an in-flight request, or when the
    /// leased credential uses the provider's default URL. `effective_base_url`
    /// consults this first.
    active_credential_base_url: Option<String>,
    /// Provider-level failover slots.
    provider_slots: Vec<ProviderSlot>,
    /// Index of the currently active provider slot (usize::MAX = primary).
    active_slot_index: usize,
    /// Cooldown state for the primary head — the counterpart of each
    /// fallback slot's `ProviderSlot::cooldown`. Gates when a failed-over
    /// client probes the primary again.
    primary_cooldown: ProviderCooldown,
    /// The primary head's identity, captured when failover first leaves it.
    /// `Some` only while the client is running on a fallback slot.
    primary_snapshot: Option<PrimaryIdentity>,
    debug_logging: bool,
    /// Optional stable cache key (typically the session / conversation id).
    /// Sent as `prompt_cache_key` on OpenAI-family requests to bias the
    /// provider's auto-cache toward a consistent shard across turns.
    prompt_cache_key: Option<String>,
    /// The byte-stable head of the system prompt for the turn in flight, set by
    /// [`Self::with_system_stable_prefix`]. Providers with explicit cache
    /// breakpoints anchor the system marker at the end of this instead of the
    /// end of the whole prompt.
    ///
    /// Stored as the prefix text rather than an offset so a value left over from
    /// a previous turn cannot corrupt anything: the transport honors it only
    /// while it still prefixes the system message being sent, and a prefix that
    /// still matches is still stable. Callers that never set it (aux
    /// summarizers, title generation) just don't get the split.
    system_stable_prefix: Option<String>,
    /// Optional best-effort sink for daemon cost events.
    cost_event_sink: Option<Arc<dyn CostEventSink>>,
    /// Per-call metadata supplied by borgd.
    cost_event_context: Option<CostEventContext>,
    /// Admission limit for auxiliary work. `Some` only on clients built by
    /// [`Self::for_auxiliary`]; primary/interactive clients are `None` and are
    /// never gated.
    aux_concurrency: Option<u32>,
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

fn bearer_headers(token: &str) -> std::result::Result<reqwest::header::HeaderMap, LlmError> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    let bearer = format!("Bearer {token}");
    let value = reqwest::header::HeaderValue::from_str(&bearer).map_err(|e| LlmError::Fatal {
        source: anyhow::anyhow!("cloud access token contains invalid header characters: {e}"),
        reason: FailoverReason::Auth,
        status: None,
    })?;
    headers.insert(reqwest::header::AUTHORIZATION, value);
    Ok(headers)
}

/// Compose the Azure OpenAI chat-completions URL from
/// `llm.azure_resource` / `llm.azure_deployment` / `llm.azure_api_version`.
///
/// Azure's URL is operator-derived (one resource hosts many deployments
/// at distinct paths), so it cannot be encoded as a static `base_url()`.
/// Missing fields surface as a clear config error rather than a runtime
/// 400 from Azure — `LlmClient::new` invokes this at construction time
/// for `Provider::AzureOpenAi`.
/// Compose an Ollama OpenAI-compatible endpoint from a host or host+path
/// string (the ecosystem convention is `OLLAMA_HOST=http://10.0.0.5:11434`,
/// scheme optional). Already-suffixed inputs are returned unchanged so an
/// operator pasting the full URL from existing docs keeps working.
///
/// Pure `host[:port]` specs route through [`provider::ollama_origin`] so
/// the endpoint follows the same schemeless conventions the reachability
/// probe uses (default port 11434, bracketed IPv6) — requests and health
/// checks must never dial different targets. Specs carrying an explicit
/// scheme or path (reverse-proxy prefixes like `http://h/ollama`) are kept
/// verbatim apart from the appended completions path.
pub(crate) fn compose_ollama_url(host: &str) -> String {
    let trimmed = host.trim().trim_end_matches('/');
    if trimmed.contains("/v1/chat/completions") {
        return trimmed.to_string();
    }
    let has_scheme = trimmed.starts_with("http://") || trimmed.starts_with("https://");
    if !has_scheme && !trimmed.contains('/') {
        return format!(
            "{}/v1/chat/completions",
            provider::ollama_origin(Some(trimmed))
        );
    }
    let with_scheme = if has_scheme {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };
    format!("{with_scheme}/v1/chat/completions")
}

/// Redirect policy for credential-bearing HTTP clients: follow same-host
/// redirects (legitimate path-level 3xx) but refuse to follow a redirect whose
/// host differs from the origin host we authenticated to.
///
/// Provider API keys travel as `x-api-key` (Anthropic), `x-goog-api-key`
/// (Gemini), and `api-key` (Azure) headers, and OAuth refresh clients carry the
/// `refresh_token` in the request *body*. reqwest's built-in cross-host
/// stripping only removes `Authorization`/`Cookie`/`Proxy-Authorization` — it
/// never touches those custom headers or the body — so following a cross-host
/// 3xx (from a compromised/misconfigured endpoint or a poisoned, operator- or
/// credential-supplied `base_url`) would forward the raw secret to the redirect
/// target. reqwest's public redirect API can't mutate per-hop headers, so we
/// fail closed instead: the cross-host request is simply never sent. The
/// comparison is against the full *origin* (scheme + host + port) of the first
/// URL in the chain, so a redirect to a different port or scheme on the same
/// host is refused too.
pub(crate) fn cross_host_safe_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        // Compare full origin (scheme + host + port) against the origin we
        // authenticated to; empty history (shouldn't happen inside a redirect)
        // fails closed.
        let same_origin = attempt
            .previous()
            .first()
            .is_some_and(|origin| origin.origin() == attempt.url().origin());
        if same_origin {
            attempt.follow()
        } else {
            // Cross-origin (or unknown) redirect: stop here rather than re-send
            // the credentialed request to a different origin.
            attempt.stop()
        }
    })
}

pub(crate) fn compose_azure_openai_url(cfg: &crate::config::LlmConfig) -> anyhow::Result<String> {
    let resource = cfg
        .azure_resource
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Azure OpenAI: missing required setting `llm.azure_resource` \
             (e.g. `borg settings set llm.azure_resource my-resource`)"
            )
        })?;
    let deployment = cfg
        .azure_deployment
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Azure OpenAI: missing required setting `llm.azure_deployment` \
                 (e.g. `gpt-4o-prod`)"
            )
        })?;
    let api_version = cfg
        .azure_api_version
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Azure OpenAI: missing required setting `llm.azure_api_version` \
                 (e.g. `2024-10-21`)"
            )
        })?;

    // Validate per Azure's per-field rules so a typo surfaces as a config
    // error instead of a runtime 404 — and so a `.` in `azure_resource`
    // can't inject a subdomain (e.g. `evil.attacker` →
    // `https://evil.attacker.openai.azure.com`).
    //
    // - resource: RFC 1123 DNS label — lowercase alphanumeric + `-`.
    // - deployment: operator-chosen, Azure portal allows alphanumeric +
    //   `-` + `_`.
    // - api_version: `YYYY-MM-DD[-preview]` — alphanumeric + `-`.
    fn is_dns_label(s: &str) -> bool {
        !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    }
    fn is_deployment_safe(s: &str) -> bool {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }
    if !is_dns_label(resource) {
        anyhow::bail!(
            "Azure OpenAI: setting `llm.azure_resource` must be a DNS label \
             (alphanumeric and `-` only — no `.`, `_`, or other characters)"
        );
    }
    if !is_deployment_safe(deployment) {
        anyhow::bail!(
            "Azure OpenAI: setting `llm.azure_deployment` contains invalid \
             characters (allowed: alphanumeric, `-`, `_`)"
        );
    }
    if !is_dns_label(api_version) {
        anyhow::bail!(
            "Azure OpenAI: setting `llm.azure_api_version` contains invalid \
             characters (expected `YYYY-MM-DD[-preview]`)"
        );
    }
    Ok(format!(
        "https://{resource}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={api_version}"
    ))
}

/// Whether a configured Bedrock endpoint override provably targets a private
/// or loopback address — the only case where Bedrock is permitted under
/// air-gap mode (`providers.outbound_allowed = false`).
///
/// The effective override is the explicit `llm.bedrock_endpoint_url` or, when
/// that is unset, the `AWS_ENDPOINT_URL_BEDROCK` env var — the same precedence
/// the request path applies, so an env-var-configured PrivateLink host is
/// admitted rather than silently refused.
///
/// Fail-closed: a missing override, an https-invalid or cloud-metadata host, a
/// host that does not resolve, or *any* resolved address that is publicly
/// routable all return `false`, so the caller falls back to the standard
/// air-gap refusal. Requiring every resolved address to be private prevents a
/// split-horizon / dual-homed name from smuggling a public route past the gate.
///
/// Residual risk: the name is resolved here and re-resolved independently by
/// reqwest at send time, so a DNS-rebinding operator endpoint that answers
/// private now and public later is not fully closed off. Exploiting it needs
/// DNS control over the configured endpoint plus a real public route, which a
/// true air-gap does not provide; connect-to-pinned-IP would close it if that
/// threat model ever applies.
async fn bedrock_endpoint_is_private(endpoint_override: Option<&str>) -> bool {
    let raw = endpoint_override
        .map(str::to_owned)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("AWS_ENDPOINT_URL_BEDROCK")
                .ok()
                .filter(|s| !s.trim().is_empty())
        });
    let Some(raw) = raw else {
        return false;
    };
    // Same guard the request path applies: https-only, non cloud-metadata.
    let base = match crate::llm::bedrock::validated_endpoint_override(&raw) {
        Ok(base) => base,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Bedrock endpoint override rejected by validation; refusing under air-gap policy"
            );
            return false;
        }
    };
    let Ok(url) = url::Url::parse(&base) else {
        tracing::warn!(%base, "Bedrock endpoint override did not parse; refusing under air-gap policy");
        return false;
    };
    let Some(host) = url.host_str().map(str::to_owned) else {
        tracing::warn!(%base, "Bedrock endpoint override has no host; refusing under air-gap policy");
        return false;
    };
    let port = url.port_or_known_default().unwrap_or(443);
    // VPC / split-horizon DNS resolves the endpoint to its private address
    // locally, so this lookup does not itself breach the air-gap.
    let addrs: Vec<std::net::SocketAddr> =
        match tokio::net::lookup_host((host.as_str(), port)).await {
            Ok(addrs) => addrs.collect(),
            Err(e) => {
                tracing::warn!(
                    host = host.as_str(),
                    error = %e,
                    "Bedrock endpoint override did not resolve; refusing under air-gap policy"
                );
                return false;
            }
        };
    all_addrs_private(&addrs)
}

/// Whether a Custom-provider `llm.base_url` provably targets a private or
/// loopback address — the only case where the Custom (BYO) provider is
/// permitted under air-gap mode. This implements the address-based
/// enforcement documented on [`crate::providers::ProviderCategory`]: a local
/// llama.cpp / LM Studio / vLLM endpoint is exactly as air-gap-safe as
/// Ollama, and the category alone can't prove that at compile time.
///
/// Fail-closed, mirroring [`bedrock_endpoint_is_private`]: a missing URL, a
/// non-http(s) scheme, a cloud-metadata host (refused by name *and* by
/// resolved link-local address — link-local counts as "private" in
/// [`all_addrs_private`], so IMDS must be excluded explicitly), an
/// unresolvable host, or *any* publicly-routable resolved address all return
/// `false` and fall through to the standard air-gap refusal. Unlike Bedrock,
/// plain `http` is permitted — the whole point is loopback/RFC1918 servers,
/// where TLS is commonly absent (same trust model as Ollama's default
/// `http://localhost:11434`).
///
/// Residual risk (same acceptance as the Bedrock gate): the name is resolved
/// here and re-resolved by reqwest at send time, so a DNS-rebinding endpoint
/// that answers private now and public later is not fully closed off.
/// Exploiting it needs DNS control over the operator-configured endpoint
/// plus a real public route, which a true air-gap does not provide.
async fn custom_endpoint_is_private(base_url: Option<&str>) -> bool {
    let Some(raw) = base_url.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let Ok(url) = url::Url::parse(raw) else {
        tracing::warn!(%raw, "custom base_url did not parse; refusing under air-gap policy");
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        tracing::warn!(
            scheme = url.scheme(),
            "custom base_url scheme not http(s); refusing under air-gap policy"
        );
        return false;
    }
    let Some(host) = url.host_str().map(str::to_owned) else {
        tracing::warn!(%raw, "custom base_url has no host; refusing under air-gap policy");
        return false;
    };
    if crate::llm::bedrock::is_cloud_metadata_host(&host) {
        tracing::warn!(
            host = host.as_str(),
            "custom base_url is a cloud metadata endpoint; refusing under air-gap policy"
        );
        return false;
    }
    let Some(port) = url.port_or_known_default() else {
        return false;
    };
    let addrs: Vec<std::net::SocketAddr> =
        match tokio::net::lookup_host((host.as_str(), port)).await {
            Ok(addrs) => addrs.collect(),
            Err(e) => {
                tracing::warn!(
                    host = host.as_str(),
                    error = %e,
                    "custom base_url did not resolve; refusing under air-gap policy"
                );
                return false;
            }
        };
    // The by-name metadata guard above only catches IP literals and known
    // FQDNs; a hostname RESOLVING to link-local (IMDS et al.) would still
    // pass `all_addrs_private`. No legitimate local LLM server lives on a
    // link-local address, so reject those outright.
    if any_addr_link_local(&addrs) {
        tracing::warn!(
            host = host.as_str(),
            "custom base_url resolves to a link-local (cloud-metadata range) address; refusing under air-gap policy"
        );
        return false;
    }
    all_addrs_private(&addrs)
}

/// True when any resolved address is link-local: v4 `169.254/16` (covers
/// IMDS 169.254.169.254 and ECS 169.254.170.2), v6 `fe80::/10`, the AWS
/// IMDS v6 literal, or a v4-mapped equivalent.
fn any_addr_link_local(addrs: &[std::net::SocketAddr]) -> bool {
    addrs.iter().any(|a| match a.ip() {
        std::net::IpAddr::V4(v4) => v4.is_link_local(),
        std::net::IpAddr::V6(v6) => {
            (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.segments() == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254]
                || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_link_local())
        }
    })
}

/// True only when `addrs` is non-empty and *every* address is private/loopback.
/// Split out so the "all private, not just the first" guarantee is unit-testable
/// without a live resolver.
fn all_addrs_private(addrs: &[std::net::SocketAddr]) -> bool {
    !addrs.is_empty()
        && addrs
            .iter()
            .all(|a| crate::url_policy::is_private_class(&a.ip()))
}

impl LlmClient {
    /// Effective API URL: config override → provider default, with provider-
    /// specific normalization (e.g. Gemini base-URL suffix correction).
    fn effective_base_url(&self) -> Cow<'_, str> {
        // Precedence: per-credential override (from the active lease) →
        // config override → provider static default. The per-credential
        // override exists for `Provider::Custom` and OAuth flavors whose
        // wire endpoint differs from the static default (e.g. the OpenAI
        // OAuth flavor targets the ChatGPT backend, not api.openai.com).
        let raw = self
            .active_credential_base_url
            .as_deref()
            .or(self.llm_config.base_url.as_deref())
            .unwrap_or_else(|| self.provider.base_url());
        match self.provider {
            Provider::Gemini => normalize_gemini_base(raw),
            Provider::VertexAi if !vertex::is_anthropic_model(&self.llm_config.model) => {
                if self
                    .llm_config
                    .base_url
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .is_some()
                {
                    Cow::Borrowed(raw)
                } else {
                    Cow::Owned(
                        vertex::compose_gemini_url(&self.llm_config)
                            .unwrap_or_else(|_| raw.to_string()),
                    )
                }
            }
            _ => Cow::Borrowed(raw),
        }
    }

    /// Create a new LLM client from config (resolves provider and API keys).
    pub async fn new(config: &Config) -> Result<Self> {
        let (provider, keys) = config.resolve_api_keys()?;
        // Air-gap policy: when `providers.outbound_allowed = false` we refuse
        // any non-Local provider before constructing the client so no HTTP
        // socket is opened against a public endpoint.
        //
        // Bedrock is the one documented exception: an airgapped GovCloud
        // deployment reaches Bedrock over a PrivateLink / VPC interface endpoint
        // whose host resolves to a private address. When `bedrock_endpoint_url`
        // provably resolves to a private/loopback address we permit it; every
        // other case (no override, public host, unresolvable host) fails closed
        // through the standard refusal.
        let bedrock_privatelink_ok = provider == Provider::Bedrock
            && !config.providers.outbound_allowed
            && bedrock_endpoint_is_private(config.llm.bedrock_endpoint_url.as_deref()).await;
        // Custom (BYO) endpoints are statically refused under air-gap, but a
        // base_url that provably resolves to loopback/RFC1918 (local
        // llama.cpp, LM Studio, vLLM) is as air-gap-safe as Ollama.
        let custom_private_ok = provider == Provider::Custom
            && !config.providers.outbound_allowed
            && custom_endpoint_is_private(config.llm.base_url.as_deref()).await;
        if !(bedrock_privatelink_ok || custom_private_ok) {
            crate::providers::ProviderRegistry::check_builtin_air_gap(
                provider,
                config.providers.outbound_allowed,
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        }

        // Compose the Azure OpenAI URL from settings at construction time so
        // misconfiguration surfaces as "borg refuses to start" rather than a
        // runtime 400 on first request. The composed URL is stashed in
        // `llm_config.base_url` so the existing override path picks it up.
        // Skip when `llm.base_url` is already set — operators occasionally
        // need to point Azure at a proxy.
        let mut llm_config = config.llm.clone();
        if provider == Provider::AzureOpenAi
            && llm_config
                .base_url
                .as_deref()
                .filter(|s| !s.is_empty())
                .is_none()
        {
            llm_config.base_url = Some(compose_azure_openai_url(&llm_config)?);
        }
        if provider == Provider::VertexAi
            && llm_config
                .base_url
                .as_deref()
                .filter(|s| !s.is_empty())
                .is_none()
        {
            if vertex::is_anthropic_model(&llm_config.model) {
                let model = provider.normalize_model(&llm_config.model);
                vertex::compose_anthropic_url(&llm_config, &model, true)?;
            } else {
                llm_config.base_url = Some(vertex::compose_gemini_url(&llm_config)?);
            }
        }
        // Ollama ecosystem convention: `OLLAMA_HOST` is a host[:port] string.
        // When the operator sets it but hasn't pinned `llm.base_url`, compose
        // the OpenAI-compat endpoint so a remote Ollama just works.
        if provider == Provider::Ollama
            && llm_config
                .base_url
                .as_deref()
                .filter(|s| !s.is_empty())
                .is_none()
        {
            if let Some(host) = std::env::var("OLLAMA_HOST")
                .ok()
                .filter(|s| !s.trim().is_empty())
            {
                llm_config.base_url = Some(compose_ollama_url(&host));
            }
        }

        let debug_logging = config.debug.llm_logging;
        // Credential-bearing client: refuse cross-host redirects so provider
        // API-key headers / OAuth bodies can't leak to a redirect target.
        // `connect_timeout` bounds only the TCP/TLS handshake — without it a
        // host that blackholes SYNs consumes the whole request timeout, which
        // is sized for a model that may think for minutes.
        let client = Client::builder()
            .redirect(cross_host_safe_redirect_policy())
            .connect_timeout(blackhole::CONNECT_TIMEOUT)
            .build()
            .context("build credentialed LLM http client")?;
        let mut keys_remaining = keys.clone();
        let api_key = keys_remaining.remove(0);

        // Try to build the primary credential pool. Best-effort: falls back
        // to legacy rotation when the DB is unreachable (tests, headless
        // embedded usage, first-boot before borgd has stamped a DB file).
        let primary_pool =
            if provider.supports_credential_pool() && keys.iter().any(|k| !k.is_empty()) {
                credential_pool::try_build_from_keys(provider, keys.clone(), &config.llm).await
            } else {
                None
            };

        // Build provider-level failover slots
        let mut provider_slots = Vec::new();
        for fb in &config.llm.fallback {
            match Self::resolve_fallback_slot(fb, config).await {
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
            llm_config,
            provider,
            api_key,
            fallback_keys: keys_remaining,
            primary_pool,
            _active_lease: None,
            active_credential_base_url: None,
            provider_slots,
            active_slot_index: usize::MAX,
            primary_cooldown: ProviderCooldown::default(),
            primary_snapshot: None,
            debug_logging,
            prompt_cache_key: None,
            system_stable_prefix: None,
            cost_event_sink: None,
            cost_event_context: None,
            aux_concurrency: None,
        })
    }

    /// Accessor for the active credential pool (e.g. for admin RPCs / CLI).
    pub fn primary_pool(&self) -> Option<&Arc<credential_pool::CredentialPool>> {
        self.primary_pool.as_ref()
    }

    /// Build an auxiliary LLM client dedicated to background tasks (memory
    /// consolidation, heartbeats, scheduled prompts). Routes through the
    /// `[llm.aux]` chain when configured, isolating its provider-slot
    /// cooldown state from the primary client so a primary-provider outage
    /// can't take down nightly cron.
    ///
    /// Falls through to [`LlmClient::new`] (primary chain) when:
    /// - `config.llm.aux` is `None` (back-compat: pre-aux-feature configs)
    /// - aux's resolved provider has no entry in [`aux_model_picker`]
    ///   (local providers like Ollama / Claude CLI — no cheap sibling to
    ///   pin to)
    ///
    /// Both fall-throughs are silent — auxiliary failover must never block
    /// the daemon at boot.
    pub async fn for_auxiliary(config: &Config) -> Result<Self> {
        // Set on the wrapper, not inside, because the body has four
        // `Self::new(config)` fall-through paths — tagging each one is exactly
        // the kind of thing a later edit silently forgets.
        let mut client = Self::for_auxiliary_inner(config).await?;
        client.aux_concurrency = Some(config.llm.aux_max_concurrent);
        Ok(client)
    }

    async fn for_auxiliary_inner(config: &Config) -> Result<Self> {
        let Some(aux) = config.llm.aux.as_ref() else {
            return Self::new(config).await;
        };

        // Resolve provider: explicit aux override → primary's provider.
        let provider_str = aux.provider.clone().or_else(|| config.llm.provider.clone());
        let Some(provider_str) = provider_str else {
            // Neither aux nor primary names a provider (rare — primary
            // is also unset). Fall through so the existing primary-path
            // error surfaces a coherent message rather than us inventing
            // one here.
            return Self::new(config).await;
        };

        // Resolve the `"auto"` sentinel via the hardcoded provider→model
        // map. `None` means "no aux default for this provider" (Ollama,
        // Claude CLI, custom strings) — fall through to the primary chain.
        let resolved_model = if aux.model_is_auto() {
            match aux_model_picker::pick_aux_model(&provider_str) {
                Some(m) => m.to_string(),
                None => return Self::new(config).await,
            }
        } else {
            aux.model.clone()
        };

        // Synthesize an LlmConfig that mirrors the aux blob, then reuse
        // `Self::new`. This funnels through the same air-gap check, key
        // resolution, and fallback-slot construction the primary path
        // uses — no duplicated logic, and the aux client's
        // `provider_slots` vec is its own (cooldowns can't bleed into
        // the primary instance).
        let mut synthetic = config.llm.clone();
        // A different aux provider must not inherit the primary's endpoint or
        // credentials. `Self::new` → `resolve_api_keys` → `correct_provider_from_key`
        // sniffs the inherited key's prefix and silently reverts the provider,
        // so `[llm.aux] provider = "anthropic"` over an OpenAI primary routed
        // straight back to OpenAI — the aux pin looked applied and wasn't.
        // Same reasoning as `resolve_scheduled_model` in the task runner.
        if config.llm.provider.as_deref() != Some(provider_str.as_str()) {
            synthetic.base_url = None;
            synthetic.api_key = None;
            synthetic.api_keys.clear();
            if let Ok(p) = provider::Provider::from_str(&provider_str) {
                synthetic.api_key_env = p.default_env_var().to_string();
            }
        }
        synthetic.provider = Some(provider_str);
        synthetic.model = resolved_model;
        // Explicit aux credentials still win over the defaults chosen above.
        if !aux.api_key_env.is_empty() {
            synthetic.api_key_env = aux.api_key_env.clone();
        }
        if aux.api_key.is_some() {
            synthetic.api_key = aux.api_key.clone();
        }
        if !aux.api_keys.is_empty() {
            synthetic.api_keys = aux.api_keys.clone();
        }
        if let Some(t) = aux.temperature {
            synthetic.temperature = t;
        }
        if let Some(t) = aux.max_tokens {
            synthetic.max_tokens = t;
        }
        if aux.base_url.is_some() {
            synthetic.base_url = aux.base_url.clone();
        }
        synthetic.fallback = aux.fallback.clone();
        // Aux work (consolidation, heartbeat, scheduled prompts) is
        // structured/templated and doesn't benefit from extended
        // reasoning — and the cheap-sibling models in `aux_model_picker`
        // either don't support it (haiku-4-5, gpt-5-mini) or pay a
        // latency tax for nothing. Force `Off` so primary's
        // `thinking="high"` doesn't bleed into nightly cron.
        synthetic.thinking = crate::config::llm::ThinkingLevel::Off;
        // Per-model thinking overrides target the primary chain; for aux
        // we want a hard Off across every aux model. Clearing the map
        // prevents a primary-tuned override (e.g. "gpt-5"→High) from
        // re-enabling thinking on the aux head when the picker happens to
        // pick a same-prefix model.
        synthetic.thinking_overrides.clear();
        // Block re-entry: a synthetic config feeding back through
        // `for_auxiliary` would loop. The aux client only ever has a
        // primary head + its own fallback chain.
        synthetic.aux = None;

        let mut synthetic_config = config.clone();
        synthetic_config.llm = synthetic;
        Self::new(&synthetic_config).await
    }

    /// Attach a stable prompt cache key (typically the session id) so that
    /// OpenAI-family requests include `prompt_cache_key` and `user` for
    /// shard-stable auto-caching. A no-op for providers that don't honor it.
    pub fn with_prompt_cache_key(mut self, key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(key.into());
        self
    }

    /// Declare the byte-stable head of the system prompt for the next call, so
    /// providers with explicit cache breakpoints anchor the system marker at the
    /// end of the *prefix* instead of the end of the whole prompt. Marking the
    /// whole prompt caches nothing, because its dynamic tail carries a
    /// second-resolution clock.
    ///
    /// A builder rather than a call parameter because only the main agent turn
    /// has a prefix worth caching; the aux paths (summarization, title
    /// generation, classification) would all pass `None`. Safe even on a reused
    /// client: the transport honors the value only while it still prefixes the
    /// system message actually being sent, so a stale prefix degrades to no
    /// split rather than to a misplaced breakpoint.
    pub fn with_system_stable_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.system_stable_prefix = Some(prefix.into());
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

    /// The active model id, reflecting any provider/model failover that has
    /// occurred. Used to stamp the model onto durable records (e.g. a
    /// compaction checkpoint) so they name the model that actually produced
    /// the output.
    pub fn model(&self) -> &str {
        &self.llm_config.model
    }

    /// Practical context window of the active model in tokens. Resolves through
    /// the model registry; falls back to a conservative default for unknown
    /// models so downstream budget calculations always have a non-zero value.
    pub fn context_window(&self) -> u32 {
        crate::llm::model_registry::context_window_for(&self.llm_config.model)
    }

    /// Hard-cap context window of the active model (≥ `context_window` for
    /// tiered models like GPT-5).
    pub fn max_context_window(&self) -> u32 {
        crate::llm::model_registry::max_context_window_for(&self.llm_config.model)
    }

    /// Resolve a fallback config into a ProviderSlot.
    async fn resolve_fallback_slot(
        fb: &crate::config::LlmFallback,
        config: &Config,
    ) -> Result<ProviderSlot> {
        let provider = Provider::from_str(&fb.provider)?;
        let mut slot_keys = Vec::new();

        // Precedence is explicit-list > single ref > env var > credential store:
        // the more specific the operator was, the earlier it wins.
        for sr in &fb.api_keys {
            if let Ok(key) = sr.resolve() {
                if !key.is_empty() {
                    slot_keys.push(key);
                }
            }
        }

        if slot_keys.is_empty() {
            if let Some(ref sr) = fb.api_key {
                if let Ok(key) = sr.resolve() {
                    if !key.is_empty() {
                        slot_keys.push(key);
                    }
                }
            }
        }

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
            if slot_keys.is_empty() {
                if let Some(key) = config.resolve_credential_or_env(env_var) {
                    slot_keys.push(key);
                }
            }
        }

        // Keyless/cloud-chain providers (Ollama, Bedrock, Vertex, Azure AAD)
        // don't need static API keys in the fallback slot.
        if slot_keys.is_empty() && (!provider.requires_api_key() || provider.supports_cloud_auth())
        {
            slot_keys.push(String::new());
        }

        if slot_keys.is_empty() {
            bail!("No API keys found for fallback provider {provider}");
        }

        let pool = if provider.supports_credential_pool() && slot_keys.iter().any(|k| !k.is_empty())
        {
            credential_pool::try_build_from_keys(provider, slot_keys.clone(), &config.llm).await
        } else {
            None
        };

        Ok(ProviderSlot {
            provider,
            model: fb.model.clone(),
            keys: slot_keys,
            pool,
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
            // Leaving the primary head: snapshot its identity so it can be
            // restored once its cooldown expires. Fallback→fallback switches
            // keep the existing snapshot.
            if self.active_slot_index == usize::MAX {
                self.primary_snapshot = Some(PrimaryIdentity {
                    provider: self.provider,
                    model: self.llm_config.model.clone(),
                    api_key: self.api_key.clone(),
                    fallback_keys: std::mem::take(&mut self.fallback_keys),
                    pool: self.primary_pool.clone(),
                    temperature: self.llm_config.temperature,
                    max_tokens: self.llm_config.max_tokens,
                    base_url: self.llm_config.base_url.clone(),
                });
            }
            // Found a usable slot — switch to it
            self.provider = slot.provider;
            self.api_key = slot.keys[0].clone();
            self.fallback_keys = slot.keys[1..].to_vec();
            self.primary_pool = slot.pool.clone();
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
        } else {
            // Primary head. Its cooldown gates when a failed-over client
            // probes it again (see `maybe_restore_primary`).
            self.primary_cooldown.record_failure(reason);
        }
    }

    /// Record success on the currently active provider slot.
    fn record_provider_success(&mut self) {
        if self.active_slot_index < self.provider_slots.len() {
            self.provider_slots[self.active_slot_index]
                .cooldown
                .record_success();
        } else {
            self.primary_cooldown.record_success();
        }
    }

    /// Restore the primary head if the client is running on a fallback slot
    /// and the primary's cooldown has expired. Returns true when restored.
    ///
    /// Auth-class failures (`Auth`, `AuthPermanent`) and `ModelNotFound`
    /// never restore — retrying them wastes a request and re-degrades the
    /// turn. For rate-limit/billing failures the credential pool's own
    /// recovery clock is honored too: if every key is still cooling, the
    /// probe is deferred even after the provider-level cooldown expires.
    /// A failed probe re-records into `primary_cooldown`, so the next probe
    /// waits a fresh window.
    fn maybe_restore_primary(&mut self) -> bool {
        if self.active_slot_index == usize::MAX || self.primary_snapshot.is_none() {
            return false;
        }
        if matches!(
            self.primary_cooldown.reason,
            Some(
                FailoverReason::Auth
                    | FailoverReason::AuthPermanent
                    | FailoverReason::ModelNotFound
            )
        ) {
            return false;
        }
        if self.primary_cooldown.is_active() {
            return false;
        }
        if matches!(
            self.primary_cooldown.reason,
            Some(FailoverReason::RateLimit | FailoverReason::Billing)
        ) {
            let pool_still_cooling = self
                .primary_snapshot
                .as_ref()
                .and_then(|p| p.pool.as_ref())
                .is_some_and(|pool| {
                    !pool.has_available()
                        && pool
                            .next_recovery_at_ms()
                            .is_some_and(|at| at > chrono::Utc::now().timestamp_millis())
                });
            if pool_still_cooling {
                return false;
            }
        }
        let Some(p) = self.primary_snapshot.take() else {
            return false;
        };
        self.provider = p.provider;
        self.api_key = p.api_key;
        self.fallback_keys = p.fallback_keys;
        self.primary_pool = p.pool;
        self.llm_config.model = p.model;
        self.llm_config.temperature = p.temperature;
        self.llm_config.max_tokens = p.max_tokens;
        self.llm_config.base_url = p.base_url;
        self.active_slot_index = usize::MAX;
        info!(
            "Primary provider {} (model: {}) cooldown expired — restoring from fallback",
            self.provider, self.llm_config.model
        );
        true
    }

    /// Switch to the next usable provider slot AND announce the switch on the
    /// stream, returning whether a failover occurred. Couples the slot switch to
    /// the [`StreamEvent::Failover`] notification structurally so a new failover
    /// call site cannot switch slots without keeping the agent's reported model
    /// identity in sync — every failover path must go through here, not bare
    /// [`Self::try_failover_provider`].
    async fn failover_and_notify(&mut self, tx: &mpsc::Sender<StreamEvent>) -> bool {
        if self.try_failover_provider() {
            self.notify_failover(tx).await;
            true
        } else {
            false
        }
    }

    /// Announce the now-active slot after a failover so the agent can keep the
    /// model's self-reported identity in sync. Called right after
    /// [`Self::try_failover_provider`] switches slots — `self.provider` and
    /// `self.llm_config.model` already reflect the new slot. Best-effort: a
    /// closed receiver only means the turn is being torn down.
    async fn notify_failover(&self, tx: &mpsc::Sender<StreamEvent>) {
        if tx
            .send(StreamEvent::Failover {
                provider: self.provider.to_string(),
                model: self.llm_config.model.clone(),
            })
            .await
            .is_err()
        {
            tracing::trace!("llm: stream receiver closed before failover notice");
        }
    }

    fn debug_log(&self, label: &str, content: &str) {
        if !self.debug_logging {
            return;
        }
        let dir = match crate::config::Config::data_dir() {
            Ok(d) => crate::paths::subdir(&d, crate::paths::BorgSubdir::Logs).join("debug"),
            Err(_) => return,
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::debug!("llm: failed to create debug log dir: {e}");
            return;
        }
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S_%3f");
        let path = dir.join(format!("{timestamp}_{label}.json"));
        let redacted = crate::security::secrets::redact_secrets(content);
        if let Err(e) = std::fs::write(&path, redacted) {
            tracing::debug!("llm: failed to write debug log: {e}");
        }
    }

    #[cfg(test)]
    /// Resolve provider-reported cost; prefer top-level `cost`/`total_cost`, fall back to `cost_details`.
    fn extract_provider_cost_usd(usage: &StreamUsage) -> Option<String> {
        transport::extract_provider_cost_usd(usage)
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

    async fn process_bedrock_stream<S, B, E, F>(
        mut stream: S,
        tx: &mpsc::Sender<StreamEvent>,
        cancel: &CancellationToken,
        chunk_timeout_secs: u64,
        max_buffer: usize,
        mut parse_payload: F,
    ) -> std::result::Result<(), LlmError>
    where
        S: futures_util::Stream<Item = Result<B, E>> + Unpin,
        B: AsRef<[u8]>,
        E: std::fmt::Display,
        F: FnMut(&[u8]) -> crate::sse::SseAction,
    {
        let mut buffer = Vec::new();

        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => {
                    // Best-effort: a closed receiver just means the turn was torn down first.
                    let _ = tx.send(StreamEvent::Done).await;
                    return Ok(());
                }
                maybe_chunk = async {
                    if chunk_timeout_secs > 0 {
                        tokio::time::timeout(
                            Duration::from_secs(chunk_timeout_secs),
                            stream.next(),
                        ).await
                    } else {
                        Ok(stream.next().await)
                    }
                } => {
                    match maybe_chunk {
                        Ok(Some(Ok(c))) => c,
                        Ok(Some(Err(e))) => {
                            return Err(LlmError::Retryable {
                                source: anyhow::anyhow!("Bedrock stream read error: {e}"),
                                retry_after: None,
                                reason: FailoverReason::Timeout,
                                status: None,
                            });
                        }
                        Ok(None) => {
                            let _ = tx.send(StreamEvent::Done).await;
                            return Ok(());
                        }
                        Err(_) => {
                            return Err(LlmError::Retryable {
                                source: anyhow::anyhow!("No Bedrock stream data received for {chunk_timeout_secs}s"),
                                retry_after: None,
                                reason: FailoverReason::Timeout,
                                status: None,
                            });
                        }
                    }
                }
            };

            buffer.extend_from_slice(chunk.as_ref());
            if buffer.len() > max_buffer {
                return Err(LlmError::Retryable {
                    source: anyhow::anyhow!("Bedrock stream buffer exceeded {max_buffer} bytes"),
                    retry_after: None,
                    reason: FailoverReason::Overloaded,
                    status: None,
                });
            }

            while let Some(payload) =
                crate::llm::bedrock::try_take_eventstream_payload(&mut buffer)?
            {
                match parse_payload(&payload) {
                    crate::sse::SseAction::Continue => {}
                    crate::sse::SseAction::Emit(events) => {
                        for event in events {
                            if tx.send(event).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    crate::sse::SseAction::Done(events) => {
                        for event in events {
                            if tx.send(event).await.is_err() {
                                return Ok(());
                            }
                        }
                        return Ok(());
                    }
                    crate::sse::SseAction::Error(e) => return Err(e),
                }
            }
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
                usage.map(|u| u.cached_input_tokens).unwrap_or(0),
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
            Provider::Gemini => match pricing::estimate_cost_usd(
                Provider::Gemini,
                &model,
                u64::from(prompt_tokens),
                u64::from(completion_tokens),
                usage.map(|u| u.cached_input_tokens).unwrap_or(0),
            ) {
                Some(cost) => (Some(cost), true, "table_gemini".to_string()),
                None => (None, false, "unknown".to_string()),
            },
            // Bedrock's first-class models are the `anthropic.*` Converse
            // models, which normalize onto the Anthropic rate table (including
            // prompt-cache tiers). Non-Anthropic Bedrock models return None and
            // fall through to "unknown".
            Provider::Bedrock => match pricing::estimate_anthropic_cost_usd(
                &model,
                u64::from(prompt_tokens),
                u64::from(completion_tokens),
                usage.map(|u| u.cached_input_tokens).unwrap_or(0),
                usage.map(|u| u.cache_creation_tokens).unwrap_or(0),
                self.llm_config.cache.ttl,
            ) {
                Some(cost) => (Some(cost), true, "table_bedrock_anthropic".to_string()),
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

    /// Run the streaming path and aggregate the deltas into one complete
    /// message, for callers that want a non-streaming result with
    /// liveness-based timeouts (context compaction, title generation).
    ///
    /// The plain [`Self::chat`] wall-clock budget cuts a slow-but-healthy
    /// summary model off mid-generation; here the streaming layer's
    /// per-chunk stall timeout (with its reasoning-model floor) bounds
    /// *inactivity* instead, so tokens flowing = alive, while a wedged
    /// stream still dies. A total ceiling — `max(600s, 4 × per-chunk
    /// budget)` — bounds degenerate trickle streams with no new config
    /// knob. Models without tool support fall back to plain `chat` (the
    /// streaming entrypoint rejects them).
    ///
    /// Text-only by design: no `tools` parameter, because tool-call stream
    /// events would be silently discarded by the aggregation below. Callers
    /// that need tools use [`Self::chat`] or [`Self::stream_chat`].
    pub async fn chat_streamed(&mut self, messages: &[Message]) -> Result<Message> {
        if !self.provider.supports_tools(&self.llm_config.model) {
            return self.chat(messages, None).await;
        }

        // Ceiling: 0 chunk-timeout means the operator disabled idle
        // timeouts entirely — respect that by not imposing a ceiling.
        let chunk_secs = {
            let configured = self.llm_config.stream_chunk_timeout_secs;
            if configured == 0 {
                0
            } else {
                configured.max(reasoning_timeouts::stale_timeout_floor_secs(
                    &self.llm_config.model,
                ))
            }
        };
        let ceiling = (chunk_secs > 0).then(|| Duration::from_secs((4 * chunk_secs).max(600)));

        let provider = self.provider;
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);
        let cancel = CancellationToken::new();
        let deadline = ceiling.map(|c| tokio::time::Instant::now() + c);

        let stream_fut = self.stream_chat_with_cancel(messages, None, tx, cancel.clone());
        tokio::pin!(stream_fut);

        let mut text = String::new();
        let mut stream_result: Option<Result<()>> = None;
        loop {
            tokio::select! {
                r = &mut stream_fut, if stream_result.is_none() => {
                    // The sender is dropped with the future; keep draining
                    // rx until it reports closed so no tail deltas are lost.
                    stream_result = Some(r);
                }
                ev = rx.recv() => {
                    match ev {
                        Some(StreamEvent::TextDelta(delta)) => text.push_str(&delta),
                        Some(_) => {}
                        None => break,
                    }
                }
                () = async {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        // Ceiling disabled: park this arm forever.
                        None => std::future::pending().await,
                    }
                } => {
                    cancel.cancel();
                    bail!(
                        "streamed completion from {provider} exceeded its total ceiling \
                         ({}s) while still trickling output",
                        ceiling.map(|c| c.as_secs()).unwrap_or(0)
                    );
                }
            }
        }
        if let Some(result) = stream_result {
            result?;
        } else {
            // Channel closed before the driving future resolved — settle it.
            stream_fut.await?;
        }
        Ok(Message::assistant(&text))
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
    pub async fn stream_chat_with_cancel(
        &mut self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        tx: mpsc::Sender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        // Cancellation-aware: a cancelled background turn must not sit in the
        // admission queue waiting for a slot it will never use.
        let _aux_slot = match self.aux_concurrency {
            Some(limit) => tokio::select! {
                permit = aux_admission::acquire(limit) => Some(permit),
                _ = cancel.cancelled() => {
                    return Err(anyhow::anyhow!("cancelled while waiting for an auxiliary slot"));
                }
            },
            None => None,
        };

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
                tracing::trace!("llm: stream receiver closed while sending tool-support error");
            }
            anyhow::bail!("{msg}");
        }

        // Probe the primary again once its cooldown window has passed —
        // otherwise a single transient rate limit strands the client on the
        // (usually degraded) fallback chain for its whole lifetime. No
        // Failover event is emitted: the agent resets its reported identity
        // at the start of every stream, so an event-free stream already
        // reports the primary.
        self.maybe_restore_primary();

        let max_provider_attempts = 1 + self.provider_slots.len();
        // One last-gasp salvage per call, shared across every provider leg. A
        // pool that is exhausted now will still be exhausted on the next retry,
        // so re-salvaging would just hammer a benched key.
        let mut salvage_used = false;

        for _provider_attempt in 0..max_provider_attempts {
            let max_retries = self.llm_config.max_retries;
            let initial_delay = Duration::from_millis(self.llm_config.initial_retry_delay_ms);
            let total_keys = 1 + self.fallback_keys.len();
            let mut keys_tried = 0_usize;
            let mut should_failover = false;

            for attempt in 0..=max_retries {
                if cancel.is_cancelled() {
                    if tx.send(StreamEvent::Done).await.is_err() {
                        tracing::trace!("llm: stream receiver closed");
                    }
                    return Ok(());
                }
                // Liveness marker for stall watchdogs — see `AttemptStarted`.
                // Sent before the credential lease so a pool that blocks on a
                // refresh still reports the attempt.
                if tx
                    .send(StreamEvent::AttemptStarted { attempt })
                    .await
                    .is_err()
                {
                    tracing::trace!("llm: stream receiver closed");
                }

                // Pool path: pick a fresh credential before each attempt. The
                // lease is held for the request duration so concurrent calls
                // can fan out across keys (max_concurrent_per_credential
                // governs the cap).
                let lease_id: Option<String> = if let Some(pool) = self.primary_pool.clone() {
                    if let Some(lease) = pool.acquire_lease_refreshing(None).await {
                        if let Some(k) = lease.key() {
                            self.api_key = k;
                        }
                        // Pick up any per-credential base_url override (used
                        // for Custom endpoints and OAuth flavors whose wire
                        // endpoint differs from the provider's static
                        // default). `None` means the lease uses the
                        // provider's default URL.
                        self.active_credential_base_url = lease.base_url();
                        let id = lease.id().to_string();
                        // Park the lease on `self` so its Drop fires after the
                        // result is processed below.
                        self._active_lease = Some(lease);
                        Some(id)
                    } else {
                        // No credential is currently usable. That means either
                        // the pool is fully removed, or every surviving entry
                        // is cooling and fallback/retry policy must decide what
                        // to do next.
                        let msg = if pool.has_credentials() {
                            format!(
                                "{} credential pool exhausted (all keys cooling)",
                                self.provider
                            )
                        } else {
                            format!(
                                "{} credential pool exhausted (all keys removed)",
                                self.provider
                            )
                        };
                        warn!(target: "cortex::llm", "{msg}");
                        let reason = if pool.has_credentials() {
                            FailoverReason::RateLimit
                        } else {
                            FailoverReason::AuthPermanent
                        };
                        self.record_provider_failure(reason);
                        if self.failover_and_notify(&tx).await {
                            should_failover = true;
                            break;
                        }
                        // No fallback to move to, so the alternative to trying a
                        // cooling key is going dark. That is the wrong trade for
                        // a single-key install, where an ordinary burst throttle
                        // benches the only credential the agent has. `pick_force`
                        // refuses entries benched for billing or permanent auth
                        // failure, so this cannot hammer a key that needs a human.
                        let salvaged = if salvage_used {
                            None
                        } else {
                            pool.pick_force()
                        };
                        if let Some(cred) = salvaged {
                            salvage_used = true;
                            warn!(
                                target: "cortex::llm",
                                id = %cred.id,
                                label = %cred.label,
                                "no fallback available; making one salvage attempt on a cooling credential"
                            );
                            self.api_key = cred.key.clone();
                            self.active_credential_base_url = cred.base_url.clone();
                            Some(cred.id)
                        } else {
                            if tx.send(StreamEvent::Error(msg.clone())).await.is_err() {
                                tracing::trace!("llm: stream receiver closed");
                            }
                            bail!("{msg}");
                        }
                    }
                } else {
                    None
                };

                // ClaudeCli is streaming-only, so it's dispatched here before
                // consulting the shared `wire_path` classifier.
                let result = if matches!(self.provider, Provider::ClaudeCli) {
                    self.stream_chat_claude_cli_inner(messages, tools, &tx, &cancel)
                        .await
                } else {
                    self.stream_http_transport_inner(messages, tools, &tx, &cancel)
                        .await
                };

                // Lease drop happens here as we re-bind below; report outcome
                // to the pool by id before the lease's Drop returns it.
                match result {
                    Ok(()) => {
                        if let (Some(pool), Some(id)) =
                            (self.primary_pool.as_ref(), lease_id.as_deref())
                        {
                            pool.report_success(id);
                        }
                        self._active_lease = None;
                        self.active_credential_base_url = None;
                        self.record_provider_success();
                        return Ok(());
                    }
                    Err(e) => {
                        // Rotate to next key only when the classifier says it
                        // could help (Auth / RateLimit). AuthPermanent (403)
                        // explicitly skips rotation — the account is denied.
                        let reason = e.reason();
                        let is_auth_error = matches!(reason, FailoverReason::Auth);

                        // Pool path: report failure and re-pick next iter.
                        if let (Some(pool), Some(id)) =
                            (self.primary_pool.as_ref(), lease_id.as_deref())
                        {
                            let retry_after = match &e {
                                LlmError::Retryable { retry_after, .. } => *retry_after,
                                _ => None,
                            };
                            pool.report_failure(
                                id,
                                reason,
                                e.status_code(),
                                retry_after,
                                Some(&e.to_string()),
                            );
                            self._active_lease = None;
                            self.active_credential_base_url = None;
                            if e.should_rotate_credential() && pool.has_available() {
                                info!("{reason} error, pool re-picking next credential");
                                continue;
                            }
                        } else if e.should_rotate_credential()
                            && keys_tried < total_keys
                            && self.try_rotate_key(!is_auth_error)
                        {
                            // Legacy rotation path (pool unavailable).
                            keys_tried += 1;
                            info!("{reason} error, trying next API key...");
                            continue;
                        }

                        if !e.is_retryable() || attempt == max_retries {
                            // Record failure and attempt provider failover.
                            // AuthPermanent opts out — the account is denied
                            // and burning the fallback chain just delays the
                            // real error. ModelNotFound cools this slot and
                            // moves on: a sibling slot's model may serve.
                            self.record_provider_failure(e.reason());
                            // Defer fallback when the primary pool will
                            // recover soon — jumping providers while a
                            // sibling key is about to cool off burns budget
                            // on the degraded fallback for no reason.
                            let defer_fallback = matches!(
                                e.reason(),
                                FailoverReason::RateLimit | FailoverReason::Billing
                            ) && self
                                .primary_pool
                                .as_ref()
                                .map(|p| p.may_recover_within(Duration::from_secs(300)))
                                .unwrap_or(false);
                            if !defer_fallback
                                && e.should_fallback()
                                && self.failover_and_notify(&tx).await
                            {
                                should_failover = true;
                                break;
                            }
                            // No more providers — propagate error
                            let msg = format!("{e}");
                            if tx.send(StreamEvent::Error(msg.clone())).await.is_err() {
                                tracing::trace!("llm: stream receiver closed while sending error");
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
                                    tracing::trace!("llm: stream receiver closed");
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
        }

        let msg =
            "All LLM providers exhausted. Please try again later or check your configuration."
                .to_string();
        if tx.send(StreamEvent::Error(msg.clone())).await.is_err() {
            tracing::trace!("llm: stream receiver closed while sending exhaustion error");
        }
        bail!("{msg}")
    }

    /// The wire transport a request takes once provider + model are known.
    ///
    /// This is the single source of truth for transport routing — both the
    /// streaming ([`Self::stream_chat_with_cancel`]) and non-streaming
    /// ([`Self::chat`]) paths consult it, so the provider/model ordering and
    /// the Vertex-Anthropic split can't drift between them. `ClaudeCli` is not
    /// represented here: it is streaming-only and handled before this is
    /// consulted (in non-streaming `chat` it falls through to `Anthropic`,
    /// matching the historical default).
    fn wire_path(&self) -> WirePath {
        if matches!(self.provider, Provider::Bedrock) {
            WirePath::Bedrock
        } else if self.provider == Provider::VertexAi
            && vertex::is_anthropic_model(&self.llm_config.model)
        {
            WirePath::VertexAnthropic
        } else if matches!(self.provider, Provider::Gemini) {
            WirePath::Gemini
        } else if self.provider.is_openai_compatible() {
            WirePath::OpenAi
        } else {
            WirePath::Anthropic
        }
    }

    /// True when the transport composes the full request URL itself (via
    /// [`Transport::endpoint`]) rather than using [`Self::effective_base_url`].
    ///
    /// These are exactly the SigV4-signed Bedrock path and the Vertex-Anthropic
    /// publisher path; deriving it from [`Self::wire_path`] keeps the rule in
    /// lockstep with transport routing.
    fn transport_composes_url(&self) -> bool {
        matches!(
            self.wire_path(),
            WirePath::Bedrock | WirePath::VertexAnthropic
        )
    }

    /// Non-streaming call for heartbeat and simple requests
    #[instrument(skip_all, fields(llm.provider = %self.provider, llm.model = %self.llm_config.model))]
    /// Non-streaming chat completion, returning assembled tool calls.
    pub async fn chat(
        &mut self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<Message> {
        let _aux_slot = self.acquire_aux_slot().await;
        self.chat_http_transport_inner(messages, tools).await
    }

    /// Take an auxiliary admission slot, if this client is an aux client.
    ///
    /// Held for the whole call — including every fallback-slot attempt inside
    /// it — and dropped on every exit path because it is a guard binding.
    ///
    /// Deliberately NOT taken in `chat_http_transport_inner`: that sits below
    /// the fallback-chain loop, so acquiring there would re-enter the gate
    /// while already holding a permit and self-deadlock at a limit of 1.
    /// `chat_streamed` doesn't take one either — it delegates to `chat` or
    /// `stream_chat_with_cancel`, both of which do.
    async fn acquire_aux_slot(&self) -> Option<tokio::sync::SemaphorePermit<'static>> {
        match self.aux_concurrency {
            Some(limit) => Some(aux_admission::acquire(limit).await),
            None => None,
        }
    }

    // ── Shared HTTP request path ──

    #[instrument(skip_all, fields(llm.provider = %self.provider))]
    async fn send_request(
        &self,
        url: &str,
        body: &impl Serialize,
        cancel: &CancellationToken,
    ) -> std::result::Result<reqwest::Response, LlmError> {
        let timeout = Duration::from_millis(self.llm_config.request_timeout_ms);

        let request_url = url.to_string();
        // The reachability checks below key off `url` rather than `request_url`,
        // which the Bedrock branch moves into its signing future. Bedrock
        // rebuilds the URL against the resolved region inside that future, so
        // the key is the pre-rebuild endpoint — same host in every
        // non-pathological config.
        //
        // A host we just watched swallow a connect is almost certainly still
        // swallowing them. Fail now so the fallback chain gets its turn,
        // instead of spending another connect timeout finding that out.
        if blackhole::is_blackholed(url, std::time::Instant::now()) {
            return Err(LlmError::Retryable {
                source: anyhow::anyhow!(
                    "Skipping {}: its endpoint refused to answer a TCP connect moments ago \
                     (retrying it costs a full connect timeout). Will retry once the \
                     endpoint is reachable.",
                    self.provider
                ),
                retry_after: None,
                reason: FailoverReason::Timeout,
                status: None,
            });
        }
        let body_bytes = serde_json::to_vec(body).map_err(|e| LlmError::Fatal {
            source: anyhow::anyhow!("serialize request body: {e}"),
            reason: FailoverReason::Format,
            status: None,
        })?;
        let fut = if self.provider == Provider::Bedrock {
            let client = self.client.clone();
            let region_hint = self.llm_config.bedrock_region.clone();
            let endpoint_override = self.llm_config.bedrock_endpoint_url.clone();
            let use_fips = self.llm_config.bedrock_use_fips;
            async move {
                let (region, credentials) = crate::llm::bedrock::load_region_and_credentials(
                    &client,
                    region_hint.as_deref(),
                )
                .await
                .map_err(|e| anyhow::anyhow!("resolve AWS credentials for Bedrock: {e}"))?;
                // Rebuild the URL against the *resolved* region/endpoint so the
                // SigV4 credential scope and host match — the endpoint() hint may
                // have been built with a placeholder region before the async
                // chain resolved the real one. Honors a custom endpoint override
                // (PrivateLink / VPC) and the FIPS toggle rather than pinning the
                // commercial public host.
                let base = crate::llm::bedrock::resolve_endpoint_base(
                    &region,
                    endpoint_override.as_deref(),
                    use_fips,
                )?;
                let final_url = crate::llm::bedrock::rebuild_request_url(&request_url, &base)?;
                let path = url::Url::parse(&final_url)
                    .context("parse rebuilt Bedrock URL")?
                    .path()
                    .to_string();
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert(
                    reqwest::header::CONTENT_TYPE,
                    reqwest::header::HeaderValue::from_static("application/json"),
                );
                headers.insert(
                    reqwest::header::ACCEPT,
                    reqwest::header::HeaderValue::from_static(
                        if path.ends_with("/converse-stream") {
                            "application/vnd.amazon.eventstream"
                        } else {
                            "application/json"
                        },
                    ),
                );
                if path.ends_with("/converse-stream") {
                    headers.insert(
                        reqwest::header::HeaderName::from_static("x-amzn-bedrock-accept"),
                        reqwest::header::HeaderValue::from_static("application/json"),
                    );
                }
                let request = crate::llm::bedrock::signed_request(
                    &client,
                    &final_url,
                    &body_bytes,
                    &region,
                    &credentials,
                    headers,
                )?;
                client.execute(request).await.map_err(anyhow::Error::from)
            }
            .boxed()
        } else {
            let headers = self.request_headers().await?;
            let builder = self
                .client
                .post(request_url.as_str())
                .headers(headers)
                .body(body_bytes);
            async move { builder.send().await.map_err(anyhow::Error::from) }.boxed()
        };

        let response = tokio::select! {
            _ = cancel.cancelled() => {
                return Err(LlmError::Interrupted);
            }
            result = tokio::time::timeout(timeout, fut) => {
                match result {
                    Ok(Ok(resp)) => resp,
                    Ok(Err(e)) => {
                        let transport_err = e.downcast_ref::<reqwest::Error>();
                        let is_connect_failure =
                            transport_err.is_some_and(reqwest::Error::is_connect);
                        // Only a connect that *timed out* marks the endpoint —
                        // that is the blackhole this cache exists for. A read
                        // timeout means the socket was accepted and the server
                        // is merely slow; a refusal or a DNS failure already
                        // fails instantly and carries a far better message
                        // (e.g. the Ollama hint below), which a 30s blackhole
                        // would replace with a generic one even after the
                        // operator started the server.
                        let is_connect_blackhole = is_connect_failure
                            && transport_err.is_some_and(reqwest::Error::is_timeout);
                        if is_connect_blackhole {
                            blackhole::record_connect_failure(url, std::time::Instant::now());
                        }
                        // A refused connection to a local Ollama almost always
                        // means the server isn't running — say so instead of
                        // leaving the operator to decode a transport error.
                        let hint = if self.provider == Provider::Ollama && is_connect_failure {
                            format!(
                                " — is the Ollama server running at {}? Start it with `ollama serve`.",
                                self.effective_base_url()
                            )
                        } else {
                            String::new()
                        };
                        return Err(classify_network_error(
                            anyhow::anyhow!("Failed to connect to {}: {e}{hint}", self.provider)
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
                            status: None,
                        });
                    }
                }
            }
        };

        // The endpoint answered, so whatever we recorded about it is stale —
        // a host that recovers early shouldn't stay marked dead for the rest
        // of the TTL. Status doesn't matter here: a 500 is still proof the
        // socket connects.
        blackhole::clear(url);

        // Capture rate-limit headers (success or failure — providers emit
        // them on both, and the 429 itself carries the freshest bucket state
        // for the proactive avoidance layer). This must happen before
        // `response.text()` moves the response and drops the headers.
        self.report_rate_limit_snapshot(response.headers());

        if !response.status().is_success() {
            let status = response.status();
            // Capture the `Retry-After` header BEFORE consuming the body —
            // `response.text()` moves the response and drops the headers.
            // RFC 7231 makes this the authoritative cooldown signal, so it
            // overrides anything we sniff out of the body below.
            let header_retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(crate::llm::llm_error::parse_retry_after_header);
            // Bound the error-body read too — an error status with a stalled
            // body must not hang the failover chain. Best-effort: a timeout
            // degrades to classifying on the status alone.
            let body = match tokio::time::timeout(timeout, response.text()).await {
                Ok(Ok(text)) => text,
                Ok(Err(e)) => {
                    tracing::warn!("Failed to read error response body: {e}");
                    String::new()
                }
                Err(_) => {
                    tracing::warn!("Error response body stalled; classifying on status only");
                    String::new()
                }
            };
            let err =
                self.explain_missing_vendor_prefix(classify_status(status, &body, self.provider));
            return Err(crate::llm::llm_error::override_retry_after(
                err,
                header_retry_after,
            ));
        }

        Ok(response)
    }

    /// Name a missing vendor prefix as the cause of a model-not-found error.
    ///
    /// On a prefix-routed provider a bare model id answers with a plain 404,
    /// which reads as an outage or an auth failure — the generic
    /// "unexpected error" it renders as sends the user looking at the wrong
    /// thing. `normalize_model` repairs ids whose family we recognise, so
    /// anything reaching here is one we can't repair; say what is wrong rather
    /// than pass the 404 through unexplained.
    ///
    /// Only rewrites the message. The `FailoverReason` is untouched, so
    /// fast-fail and failover behave exactly as before.
    fn explain_missing_vendor_prefix(&self, err: LlmError) -> LlmError {
        if !matches!(err.reason(), FailoverReason::ModelNotFound)
            || !self.provider.requires_vendor_prefix()
            || self.llm_config.model.contains('/')
            // A family `normalize_model` recognises already went out with a
            // vendor prefix, so the 404 is about the id, not the prefix.
            // Blaming the prefix here would tell the user to set exactly the
            // id the request already used.
            || self
                .provider
                .repair_missing_vendor_prefix(&self.llm_config.model)
                .is_some()
        {
            return err;
        }
        let model = &self.llm_config.model;
        let provider = self.provider;
        LlmError::Fatal {
            source: anyhow::anyhow!(
                "{provider} model ids are `vendor/model` paths, but `llm.model` is set to \
                 '{model}' with no vendor — that is why this 404s. Set the full id (for \
                 example `anthropic/{model}` or `openai/{model}`, whichever vendor serves \
                 it) via `borg settings set llm.model <vendor>/{model}`. Original error: {err}"
            ),
            reason: FailoverReason::ModelNotFound,
            status: None,
        }
    }

    /// Parse provider rate-limit headers from `headers` and report the
    /// resulting snapshot to the active credential's pool entry. No-op when
    /// the pool isn't wired up (legacy rotation path, tests without a pool)
    /// or when the headers don't carry recognizable bucket state.
    fn report_rate_limit_snapshot(&self, headers: &reqwest::header::HeaderMap) {
        let Some(pool) = self.primary_pool.as_ref() else {
            return;
        };
        let Some(lease) = self._active_lease.as_ref() else {
            return;
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        if let Some(snap) =
            crate::llm::rate_limit_headers::parse_rate_limit_headers(self.provider, headers, now_ms)
        {
            pool.report_rate_limit_headers(lease.id(), snap);
        }
    }

    fn transport_request<'a>(
        &'a self,
        messages: &'a [Message],
        tools: Option<&'a [ToolDefinition]>,
        model: &'a str,
    ) -> TransportRequest<'a> {
        TransportRequest {
            provider: self.provider,
            model,
            messages,
            tools,
            llm_config: &self.llm_config,
            prompt_cache_key: self.prompt_cache_key.as_deref(),
            system_stable_prefix: self.system_stable_prefix.as_deref(),
            cost_events_enabled: self.cost_events_enabled(),
        }
    }

    async fn request_headers(&self) -> std::result::Result<reqwest::header::HeaderMap, LlmError> {
        if self.provider == Provider::AzureOpenAi && self.api_key.is_empty() {
            let token = cloud_auth::azure_openai_token(&self.client)
                .await
                .map_err(|e| LlmError::Fatal {
                    source: e,
                    reason: FailoverReason::Auth,
                    status: None,
                })?;
            return bearer_headers(&token.access_token);
        }
        if self.provider == Provider::VertexAi {
            let token = cloud_auth::gcp_access_token(&self.client)
                .await
                .map_err(|e| LlmError::Fatal {
                    source: e,
                    reason: FailoverReason::Auth,
                    status: None,
                })?;
            return bearer_headers(&token.access_token);
        }
        let mut headers =
            self.provider
                .build_headers(&self.api_key)
                .map_err(|e| LlmError::Fatal {
                    source: e,
                    reason: FailoverReason::Unknown,
                    status: None,
                })?;
        // xAI cache-affinity: `x-grok-conv-id` pins consecutive requests for
        // the same conversation to the same cache shard. Model-gated to the
        // grok family (the only models the header is documented for) and
        // best-effort — an unrepresentable key just skips the header.
        if self.provider == Provider::XAi {
            if let Some(key) = self.prompt_cache_key.as_deref() {
                if self.llm_config.model.to_ascii_lowercase().contains("grok") {
                    if let Ok(val) = reqwest::header::HeaderValue::from_str(key) {
                        headers.insert("x-grok-conv-id", val);
                    }
                }
            }
        }
        Ok(headers)
    }

    #[instrument(skip_all, fields(llm.wire_path = self.wire_path().transport().protocol_name()))]
    async fn stream_http_transport_inner(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        tx: &mpsc::Sender<StreamEvent>,
        cancel: &CancellationToken,
    ) -> std::result::Result<(), LlmError> {
        let transport = self.wire_path().transport();
        let model = self.provider.normalize_model(&self.llm_config.model);
        let request = self.transport_request(messages, tools, &model);
        let request_url = if self.transport_composes_url() {
            transport
                .endpoint(&request, true)
                .map_err(|e| LlmError::Fatal {
                    source: anyhow::anyhow!("build {} endpoint: {e}", transport.protocol_name()),
                    reason: FailoverReason::Format,
                    status: None,
                })?
        } else {
            self.effective_base_url().into_owned()
        };
        let body = transport.build_streaming_request(&request)?;

        debug!(
            "Sending streaming request to {} (model: {})",
            transport.protocol_name(),
            model
        );

        if self.debug_logging {
            if let Ok(json) = serde_json::to_string_pretty(&body) {
                self.debug_log(transport.debug_request_label(), &json);
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
            base_url = %request_url,
            msg_count = messages.len(),
            tool_count = tools.map(<[ToolDefinition]>::len).unwrap_or(0),
            "{}: request start",
            transport.protocol_name()
        );

        let response = match self.send_request(&request_url, &body, cancel).await {
            Ok(r) => {
                lc_log!(
                    verbose,
                    req_id = %req_id,
                    elapsed_ms = lc_start.elapsed().as_millis() as u64,
                    "{}: response headers received",
                    transport.protocol_name()
                );
                r
            }
            Err(e) => {
                warn!(
                    target: "cortex::llm::lifecycle",
                    req_id = %req_id,
                    elapsed_ms = lc_start.elapsed().as_millis() as u64,
                    "{}: request failed before stream: {e}",
                    transport.protocol_name()
                );
                return Err(e);
            }
        };

        let stream = response.bytes_stream();
        let provider_str = self.provider.as_str().to_string();
        let model_str = self.llm_config.model.clone();
        // Reasoning models can think for minutes before the first token; lift the
        // per-chunk stall timeout to the model's floor so we don't idle-kill the
        // socket mid-think and fire a spurious fallback. Never lowers a higher
        // user config; a configured 0 (no timeout) is left as 0.
        let chunk_timeout_secs = {
            let configured = self.llm_config.stream_chunk_timeout_secs;
            if configured == 0 {
                0
            } else {
                configured.max(reasoning_timeouts::stale_timeout_floor_secs(&model_str))
            }
        };
        let mut state = transport.new_stream_state();
        let mut first_byte_ms: Option<u64> = None;
        let mut line_count: u64 = 0;
        let mut last_usage: Option<UsageData> = None;

        let result = match transport.stream_protocol() {
            transport::StreamProtocol::Sse => {
                crate::sse::process_sse_stream(
                    stream,
                    tx,
                    cancel,
                    chunk_timeout_secs,
                    MAX_SSE_BUFFER,
                    |line| {
                        if first_byte_ms.is_none() {
                            first_byte_ms = Some(lc_start.elapsed().as_millis() as u64);
                        }
                        line_count += 1;
                        let action = transport.parse_stream_line(
                            line,
                            &provider_str,
                            &model_str,
                            &mut state,
                        );
                        Self::maybe_capture_usage_from_action(&action, &mut last_usage);
                        action
                    },
                )
                .await
            }
            transport::StreamProtocol::AwsEventStream => {
                Self::process_bedrock_stream(
                    stream,
                    tx,
                    cancel,
                    chunk_timeout_secs,
                    MAX_SSE_BUFFER,
                    |payload| {
                        if first_byte_ms.is_none() {
                            first_byte_ms = Some(lc_start.elapsed().as_millis() as u64);
                        }
                        line_count += 1;
                        let action = transport.parse_stream_payload(
                            payload,
                            &provider_str,
                            &model_str,
                            &mut state,
                        );
                        Self::maybe_capture_usage_from_action(&action, &mut last_usage);
                        action
                    },
                )
                .await
            }
        };

        match &result {
            Ok(()) => lc_log!(
                verbose,
                req_id = %req_id,
                elapsed_ms = lc_start.elapsed().as_millis() as u64,
                first_byte_ms = ?first_byte_ms,
                line_count = line_count,
                "{}: stream end ok",
                transport.protocol_name()
            ),
            Err(e) => warn!(
                target: "cortex::llm::lifecycle",
                req_id = %req_id,
                elapsed_ms = lc_start.elapsed().as_millis() as u64,
                first_byte_ms = ?first_byte_ms,
                line_count = line_count,
                "{}: stream end err: {e}",
                transport.protocol_name()
            ),
        }
        if result.is_ok() {
            self.emit_cost_event_best_effort(&req_id, last_usage.as_ref(), &model_str);
        }
        result
    }

    /// One non-streaming round trip against the currently active provider,
    /// credential, and base URL.
    ///
    /// Everything that a credential lease or a provider failover can change —
    /// the endpoint, the auth header, the model — is resolved here rather than
    /// hoisted, so each attempt made by [`Self::chat_http_transport_inner`]
    /// uses the credential it was just handed.
    async fn chat_once(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        req_id: &str,
    ) -> std::result::Result<Message, LlmError> {
        let transport = self.wire_path().transport();
        let model = self.provider.normalize_model(&self.llm_config.model);
        let request = self.transport_request(messages, tools, &model);
        let request_url = if self.transport_composes_url() {
            transport
                .endpoint(&request, false)
                .map_err(LlmError::fatal)?
        } else {
            self.effective_base_url().into_owned()
        };
        let body = transport
            .build_chat_request(&request)
            .map_err(LlmError::fatal)?;
        let request_model = self.llm_config.model.clone();
        let cancel = CancellationToken::new();

        let response = self.send_request(&request_url, &body, &cancel).await?;
        // The `send_request` timeout only bounds the wait for response
        // *headers* — a provider that returns 200 and then stalls the body
        // would otherwise hang this read forever. Same budget, body phase.
        let response_json: serde_json::Value = tokio::time::timeout(
            Duration::from_millis(self.llm_config.request_timeout_ms),
            response.json(),
        )
        .await
        .map_err(|_| {
            LlmError::retryable(anyhow::anyhow!(
                "Response body from {} stalled past {}ms",
                self.provider,
                self.llm_config.request_timeout_ms
            ))
        })?
        .map_err(|e| LlmError::retryable(anyhow::Error::new(e)))?;

        let parsed = transport
            .parse_chat_response(response_json, self.provider.as_str(), &request_model)
            .map_err(LlmError::fatal)?;
        self.emit_cost_event_best_effort(req_id, parsed.usage.as_ref(), &request_model);
        Ok(parsed.message)
    }

    /// Non-streaming chat with the same resilience the streaming path has:
    /// credential leasing (which is also what refreshes an expired OAuth
    /// token), per-key cooldown reporting, key rotation, and the
    /// `llm.fallback` provider chain.
    ///
    /// This path carries the autonomous lanes — scheduled prompt tasks, the
    /// heartbeat turn, and the pre-compaction flush. It used to retry a single
    /// `self.api_key` in place, so a rate-limited or expired key was retried
    /// until exhaustion and never cooled, rotated, refreshed, or failed over;
    /// an expired OAuth token 401'd forever because only the lease path
    /// refreshes it.
    #[instrument(skip_all, fields(llm.wire_path = self.wire_path().transport().protocol_name()))]
    async fn chat_http_transport_inner(
        &mut self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<Message> {
        self.maybe_restore_primary();
        let req_id = short_request_id();
        let max_provider_attempts = 1 + self.provider_slots.len();
        let mut last_error: Option<String> = None;

        for _provider_attempt in 0..max_provider_attempts {
            let max_retries = self.llm_config.max_retries;
            let initial_delay = Duration::from_millis(self.llm_config.initial_retry_delay_ms);
            let total_keys = 1 + self.fallback_keys.len();
            let mut keys_tried = 0_usize;
            let mut should_failover = false;

            for attempt in 0..=max_retries {
                // Lease a credential per attempt so a cooling key is skipped
                // and an expired OAuth token gets refreshed before use.
                let lease_id: Option<String> = if let Some(pool) = self.primary_pool.clone() {
                    match pool.acquire_lease_refreshing(None).await {
                        Some(lease) => {
                            if let Some(k) = lease.key() {
                                self.api_key = k;
                            }
                            self.active_credential_base_url = lease.base_url();
                            let id = lease.id().to_string();
                            self._active_lease = Some(lease);
                            Some(id)
                        }
                        None => {
                            let msg = if pool.has_credentials() {
                                format!(
                                    "{} credential pool exhausted (all keys cooling)",
                                    self.provider
                                )
                            } else {
                                format!(
                                    "{} credential pool exhausted (all keys removed)",
                                    self.provider
                                )
                            };
                            warn!(target: "cortex::llm", "{msg}");
                            let reason = if pool.has_credentials() {
                                FailoverReason::RateLimit
                            } else {
                                FailoverReason::AuthPermanent
                            };
                            self.record_provider_failure(reason);
                            last_error = Some(msg);
                            if self.try_failover_provider() {
                                should_failover = true;
                                break;
                            }
                            bail!("{}", last_error.unwrap_or_default());
                        }
                    }
                } else {
                    None
                };

                let result = self.chat_once(messages, tools, &req_id).await;

                match result {
                    Ok(message) => {
                        if let (Some(pool), Some(id)) =
                            (self.primary_pool.as_ref(), lease_id.as_deref())
                        {
                            pool.report_success(id);
                        }
                        self._active_lease = None;
                        self.active_credential_base_url = None;
                        self.record_provider_success();
                        return Ok(message);
                    }
                    Err(e) => {
                        let reason = e.reason();
                        let is_auth_error = matches!(reason, FailoverReason::Auth);
                        last_error = Some(e.to_string());

                        if let (Some(pool), Some(id)) =
                            (self.primary_pool.as_ref(), lease_id.as_deref())
                        {
                            pool.report_failure(
                                id,
                                reason,
                                e.status_code(),
                                e.retry_after(),
                                Some(&e.to_string()),
                            );
                            self._active_lease = None;
                            self.active_credential_base_url = None;
                            if e.should_rotate_credential() && pool.has_available() {
                                info!("{reason} error, pool re-picking next credential");
                                continue;
                            }
                        } else {
                            self._active_lease = None;
                            self.active_credential_base_url = None;
                            // Legacy rotation path (pool unavailable).
                            if e.should_rotate_credential()
                                && keys_tried < total_keys
                                && self.try_rotate_key(!is_auth_error)
                            {
                                keys_tried += 1;
                                continue;
                            }
                        }

                        if !e.is_retryable() || attempt == max_retries {
                            self.record_provider_failure(reason);
                            if self.try_failover_provider() {
                                should_failover = true;
                                break;
                            }
                            bail!("{}", last_error.unwrap_or_default());
                        }

                        let delay = e.retry_after().unwrap_or_else(|| {
                            backoff_with_config(
                                attempt,
                                &BackoffConfig {
                                    initial: initial_delay,
                                    ..Default::default()
                                },
                            )
                        });
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

            if should_failover {
                continue;
            }
            break;
        }

        bail!(
            "{}",
            last_error.unwrap_or_else(|| format!(
                "All retries exhausted for non-streaming {} request",
                self.provider
            ))
        )
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
                status: None,
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

    #[cfg(test)]
    fn build_anthropic_request(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        stream: bool,
    ) -> serde_json::Value {
        let model = self.provider.normalize_model(&self.llm_config.model);
        let request = self.transport_request(messages, tools, &model);
        transport::build_anthropic_request(&request, stream)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod bedrock_air_gap_tests {
    use super::{all_addrs_private, any_addr_link_local, bedrock_endpoint_is_private};
    use std::net::SocketAddr;

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("socket addr")
    }

    // IP-literal endpoints resolve without touching the network, so these
    // exercise the classification logic hermetically.

    #[tokio::test]
    async fn private_when_override_is_loopback() {
        assert!(bedrock_endpoint_is_private(Some("https://127.0.0.1:8443")).await);
    }

    #[tokio::test]
    async fn private_when_override_is_rfc1918() {
        // The airgapped GovCloud PrivateLink case: a private VPC-endpoint IP.
        assert!(bedrock_endpoint_is_private(Some("https://10.20.30.40:8443")).await);
    }

    #[tokio::test]
    // Holds the env lock across the resolve await so AWS_ENDPOINT_URL_BEDROCK
    // stays cleared for the whole gate call.
    #[allow(clippy::await_holding_lock)]
    async fn not_private_when_override_absent() {
        let _lock = crate::llm::bedrock::env_test_lock();
        let saved = std::env::var("AWS_ENDPOINT_URL_BEDROCK").ok();
        std::env::remove_var("AWS_ENDPOINT_URL_BEDROCK");
        // Fail-closed: no override and no env fallback → refuse.
        assert!(!bedrock_endpoint_is_private(None).await);
        assert!(!bedrock_endpoint_is_private(Some("   ")).await);
        if let Some(v) = saved {
            std::env::set_var("AWS_ENDPOINT_URL_BEDROCK", v);
        }
    }

    #[tokio::test]
    // Env var is read inside the gate; hold the lock across the await.
    #[allow(clippy::await_holding_lock)]
    async fn private_via_env_var_fallback_when_override_unset() {
        let _lock = crate::llm::bedrock::env_test_lock();
        let saved = std::env::var("AWS_ENDPOINT_URL_BEDROCK").ok();
        // A PrivateLink endpoint configured purely via the AWS env var must be
        // admitted under air-gap, matching the request path's precedence.
        std::env::set_var("AWS_ENDPOINT_URL_BEDROCK", "https://10.20.30.40:8443");
        assert!(bedrock_endpoint_is_private(None).await);
        // A public env endpoint is still refused.
        std::env::set_var("AWS_ENDPOINT_URL_BEDROCK", "https://52.94.1.1");
        assert!(!bedrock_endpoint_is_private(None).await);
        match saved {
            Some(v) => std::env::set_var("AWS_ENDPOINT_URL_BEDROCK", v),
            None => std::env::remove_var("AWS_ENDPOINT_URL_BEDROCK"),
        }
    }

    #[tokio::test]
    async fn not_private_when_override_is_public_ip() {
        // A publicly-routable literal must fail the gate even though it is a
        // valid https endpoint.
        assert!(!bedrock_endpoint_is_private(Some("https://52.94.1.1")).await);
    }

    #[tokio::test]
    async fn not_private_when_override_is_http() {
        // http:// is rejected by the shared validation before resolution.
        assert!(!bedrock_endpoint_is_private(Some("http://10.20.30.40:8443")).await);
    }

    #[tokio::test]
    async fn not_private_when_override_is_metadata_host() {
        // IMDS must never be treated as a legitimate private endpoint.
        assert!(!bedrock_endpoint_is_private(Some("https://169.254.169.254")).await);
    }

    #[test]
    fn all_addrs_private_requires_every_address_private() {
        // The whole point of the gate: a dual-homed name resolving to one public
        // address must fail even if the others are private.
        assert!(all_addrs_private(&[
            addr("10.0.0.1:443"),
            addr("192.168.1.9:443")
        ]));
        assert!(!all_addrs_private(&[
            addr("10.0.0.1:443"),
            addr("52.94.1.1:443")
        ]));
        assert!(!all_addrs_private(&[addr("52.94.1.1:443")]));
        // Empty resolution is not "all private" — it must fail closed.
        assert!(!all_addrs_private(&[]));
    }

    #[test]
    fn any_addr_link_local_rejects_metadata_ranges() {
        // A hostname resolving to link-local (IMDS 169.254.169.254, ECS
        // 169.254.170.2, fe80::/10) would pass `all_addrs_private` — the
        // custom air-gap gate must catch it by address, not just by name.
        assert!(any_addr_link_local(&[addr("169.254.169.254:80")]));
        assert!(any_addr_link_local(&[addr("169.254.170.2:80")]));
        assert!(any_addr_link_local(&[addr("[fe80::1]:80")]));
        assert!(any_addr_link_local(&[addr("[fd00:ec2::254]:80")]));
        // One link-local among private addrs still rejects.
        assert!(any_addr_link_local(&[
            addr("10.0.0.1:80"),
            addr("169.254.169.254:80")
        ]));
        // Ordinary loopback/RFC1918 stays allowed.
        assert!(!any_addr_link_local(&[
            addr("127.0.0.1:8080"),
            addr("10.0.0.5:8080")
        ]));
    }
}

#[cfg(test)]
mod ollama_url_tests {
    use super::compose_ollama_url;

    #[test]
    fn compose_ollama_url_accepts_host_only() {
        // Ecosystem convention: `OLLAMA_HOST=10.0.0.5:11434` must work
        // without forcing the operator to remember the OpenAI-compat path.
        assert_eq!(
            compose_ollama_url("10.0.0.5:11434"),
            "http://10.0.0.5:11434/v1/chat/completions"
        );
        // A bare host without a port follows the Ollama client convention
        // (11434), matching where the reachability probe dials — not the
        // http default of 80.
        assert_eq!(
            compose_ollama_url("myhost"),
            "http://myhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn compose_ollama_url_preserves_explicit_scheme() {
        // HTTPS-fronted Ollama (Tailscale Funnel, Caddy, etc.) must not
        // be silently downgraded to plaintext.
        assert_eq!(
            compose_ollama_url("https://ollama.tailnet.example"),
            "https://ollama.tailnet.example/v1/chat/completions"
        );
    }

    #[test]
    fn compose_ollama_url_passes_through_fully_qualified_url() {
        // Operators copy-pasting the full endpoint from existing docs
        // should not get a doubled `/v1/chat/completions` suffix.
        let url = "http://localhost:11434/v1/chat/completions";
        assert_eq!(compose_ollama_url(url), url);
    }

    #[test]
    fn compose_ollama_url_strips_trailing_slash_before_append() {
        // Both `http://h:11434` and `http://h:11434/` must produce the
        // same canonical endpoint — otherwise we'd POST to a doubled `//`.
        assert_eq!(
            compose_ollama_url("http://h:11434/"),
            "http://h:11434/v1/chat/completions"
        );
    }
}

#[cfg(test)]
mod redirect_policy_tests {
    use super::cross_host_safe_redirect_policy;
    use wiremock::matchers::{any, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn credentialed_client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(cross_host_safe_redirect_policy())
            .build()
            .expect("build client")
    }

    #[tokio::test]
    async fn cross_origin_redirect_is_stopped_and_key_never_forwarded() {
        // The origin a redirect points to. Two wiremock servers share host
        // 127.0.0.1 but bind different ports, so they are distinct origins —
        // exactly the "same host, other service" case the port-aware compare
        // must catch. If the redirect were followed, the credential header
        // would land here.
        let attacker = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string("attacker"))
            .expect(0) // verified on drop: must never be hit
            .mount(&attacker)
            .await;

        let victim = MockServer::start().await;
        let redirect_to = attacker.uri();
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", redirect_to.as_str()),
            )
            .mount(&victim)
            .await;

        let resp = credentialed_client()
            .get(victim.uri())
            .header("x-api-key", "SECRET-ANTHROPIC-KEY")
            .header("x-goog-api-key", "SECRET-GEMINI-KEY")
            .header("api-key", "SECRET-AZURE-KEY")
            .send()
            .await
            .expect("send");

        // Redirect stopped: we get the 302 back, not the attacker's 200 body.
        assert_eq!(
            resp.status().as_u16(),
            302,
            "cross-origin redirect must not be followed"
        );
    }

    #[tokio::test]
    async fn same_origin_redirect_is_followed_and_keeps_key_header() {
        let server = MockServer::start().await;
        // /first -> (same-origin, relative) /second
        Mock::given(method("GET"))
            .and(path("/first"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/second"))
            .mount(&server)
            .await;
        // /second only answers 200 when the credential header survived the
        // same-origin redirect; the header matcher is the assertion.
        Mock::given(method("GET"))
            .and(path("/second"))
            .and(header("x-api-key", "SECRET-ANTHROPIC-KEY"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;

        let resp = credentialed_client()
            .get(format!("{}/first", server.uri()))
            .header("x-api-key", "SECRET-ANTHROPIC-KEY")
            .send()
            .await
            .expect("send");

        assert_eq!(
            resp.status().as_u16(),
            200,
            "same-origin redirect must be followed"
        );
        assert_eq!(resp.text().await.expect("body"), "ok");
    }
}
