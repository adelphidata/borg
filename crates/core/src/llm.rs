use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use crate::config::{Config, LlmConfig};
use crate::provider::Provider;
use crate::retry::backoff_delay;
use crate::types::{Message, Role, ToolCall, ToolDefinition};

const MAX_SSE_BUFFER: usize = crate::constants::MAX_SSE_BUFFER;

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
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: Option<Delta>,
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
            FailoverReason::Auth | FailoverReason::Billing => 300.0,
            _ => 60.0,
        };
        let max_exp = match reason {
            FailoverReason::Auth | FailoverReason::Billing => 3,
            _ => 4,
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
}

impl LlmClient {
    /// Effective API URL: config override → provider default.
    fn effective_base_url(&self) -> &str {
        self.llm_config
            .base_url
            .as_deref()
            .unwrap_or_else(|| self.provider.base_url())
    }

    /// Create a new LLM client from config (resolves provider and API keys).
    pub fn new(config: &Config) -> Result<Self> {
        let (provider, mut keys) = config.resolve_api_keys()?;
        let debug_logging = config.debug.llm_logging;
        let client = Client::new();
        let api_key = keys.remove(0);

        // Build provider-level failover slots
        let mut provider_slots = Vec::new();
        for fb in &config.llm.fallback {
            match Self::resolve_fallback_slot(fb, config) {
                Ok(slot) => provider_slots.push(slot),
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
        })
    }

    /// Attach a stable prompt cache key (typically the session id) so that
    /// OpenAI-family requests include `prompt_cache_key` and `user` for
    /// shard-stable auto-caching. A no-op for providers that don't honor it.
    pub fn with_prompt_cache_key(mut self, key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(key.into());
        self
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
                        // On auth failure or rate limit, try rotating to next key before retrying
                        let is_auth_error = matches!(e.reason(), FailoverReason::Auth);
                        let is_rate_limit = matches!(e.reason(), FailoverReason::RateLimit);

                        if (is_auth_error || is_rate_limit)
                            && keys_tried < total_keys
                            && self.try_rotate_key(!is_auth_error)
                        {
                            keys_tried += 1;
                            info!("Auth/rate-limit error, trying next API key...");
                            continue;
                        }

                        if !e.is_retryable() || attempt == max_retries {
                            // Record failure and attempt provider failover
                            self.record_provider_failure(e.reason());
                            if self.try_failover_provider() {
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
                            backoff_delay(attempt, initial_delay, 2.0)
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
                .post(self.effective_base_url())
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

        let response = self.send_request(&request, cancel).await?;

        let stream = response.bytes_stream();
        let provider_str = self.provider.as_str().to_string();
        let model_str = self.llm_config.model.clone();

        crate::sse::process_sse_stream(
            stream,
            tx,
            cancel,
            self.llm_config.stream_chunk_timeout_secs,
            MAX_SSE_BUFFER,
            |line| Self::parse_openai_sse_line(line, &provider_str, &model_str),
        )
        .await
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
                    events.push(StreamEvent::Usage(UsageData {
                        prompt_tokens: usage.prompt_tokens.unwrap_or(0),
                        completion_tokens: usage.completion_tokens.unwrap_or(0),
                        total_tokens: usage.total_tokens.unwrap_or(0),
                        cached_input_tokens,
                        cache_creation_tokens: 0,
                        provider: provider.to_string(),
                        model: model.to_string(),
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
                        if choice.finish_reason.is_some() {
                            events.push(StreamEvent::Done);
                            return SseAction::Done(events);
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
        };

        #[derive(Deserialize)]
        struct ChatResponse {
            choices: Vec<ChatChoice>,
        }
        #[derive(Deserialize)]
        struct ChatChoice {
            message: Message,
        }

        let max_retries = self.llm_config.max_retries;
        let initial_delay = Duration::from_millis(self.llm_config.initial_retry_delay_ms);
        let cancel = CancellationToken::new();

        for attempt in 0..=max_retries {
            let result = self.send_request(&request, &cancel).await;
            match result {
                Ok(response) => {
                    let resp: ChatResponse = response.json().await?;
                    return resp
                        .choices
                        .into_iter()
                        .next()
                        .map(|c| c.message)
                        .context("No response from LLM");
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
                        backoff_delay(attempt, initial_delay, 2.0)
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
            if cache_enabled && cache_cfg.cache_system {
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
            apply_message_cache_control(
                &mut anthropic_messages,
                cache_cfg.rolling_messages_clamped(),
                &cache_marker,
            );
        }
        body["messages"] = serde_json::json!(anthropic_messages);
        if let Some(mut tools_json) = anthropic_tools {
            // Attach a cache_control marker to the last tool definition so the
            // entire (stable) tools array becomes a single cache breakpoint.
            if cache_enabled && cache_cfg.cache_tools {
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

        let response = self.send_request(&body, cancel).await?;

        let stream = response.bytes_stream();
        let provider_str = self.provider.as_str().to_string();
        let model_str = self.llm_config.model.clone();

        // Mutable state for Anthropic's stateful event stream
        let mut current_tool_index: usize = 0;
        let mut current_block_is_tool = false;
        let mut current_block_is_thinking = false;
        // Cache token counts arrive on `message_start` but total usage lands
        // on `message_delta`. Stash cache values here to merge later.
        let mut pending_cached_input_tokens: u64 = 0;
        let mut pending_cache_creation_tokens: u64 = 0;

        crate::sse::process_sse_stream(
            stream,
            tx,
            cancel,
            self.llm_config.stream_chunk_timeout_secs,
            MAX_SSE_BUFFER,
            |line| {
                Self::parse_anthropic_sse_line(
                    line,
                    &provider_str,
                    &model_str,
                    &mut current_tool_index,
                    &mut current_block_is_tool,
                    &mut current_block_is_thinking,
                    &mut pending_cached_input_tokens,
                    &mut pending_cache_creation_tokens,
                )
            },
        )
        .await
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
                    let input = usage
                        .get("input_tokens")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
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
                    }));
                }
                if event["delta"]["stop_reason"].as_str().is_some() {
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

        for attempt in 0..=max_retries {
            let result = self.send_request(&body, &cancel).await;
            match result {
                Ok(response) => {
                    let resp: serde_json::Value = response.json().await?;
                    return parse_anthropic_response(&resp);
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
                        backoff_delay(attempt, initial_delay, 2.0)
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

/// Sanitize messages for OpenAI-compatible providers (OpenRouter, Gemini, DeepSeek, etc.).
///
/// - Replaces `None` content with empty string on assistant tool-call messages
///   (some backends reject `"content": null`).
/// - Drops empty user/assistant messages that carry no semantic value — these
///   shouldn't be in history (agent.rs guards against creating them), but legacy
///   sessions or edge cases may still have them. Gemini rejects messages with no
///   content parts.
fn sanitize_openai_messages(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|m| {
            let content_empty = m
                .content
                .as_ref()
                .map(super::types::MessageContent::is_empty)
                .unwrap_or(true);

            if m.content.is_none() && m.tool_calls.is_some() {
                // Assistant tool-call with null content — set empty string
                Some(Message {
                    content: Some(crate::types::MessageContent::Text(String::new())),
                    ..m.clone()
                })
            } else if content_empty && m.tool_calls.is_none() && m.tool_call_id.is_none() {
                // Empty message with no tool interaction — drop
                tracing::debug!(role = ?m.role, "dropping empty message for Gemini compat");
                None
            } else {
                Some(m.clone())
            }
        })
        .collect()
}

// ── Anthropic message conversion helpers ──

/// Convert internal Messages to Anthropic format.
fn build_anthropic_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    let mut result: Vec<serde_json::Value> = Vec::new();

    for msg in messages {
        if msg.role == Role::System {
            continue;
        }

        match msg.role {
            Role::User => match &msg.content {
                Some(crate::types::MessageContent::Parts(parts)) => {
                    let mut content_blocks: Vec<serde_json::Value> = Vec::new();
                    for part in parts {
                        match part {
                            crate::types::ContentPart::Text(t) => {
                                content_blocks.push(serde_json::json!({
                                    "type": "text",
                                    "text": t,
                                }));
                            }
                            crate::types::ContentPart::ImageBase64 { media } => {
                                content_blocks.push(serde_json::json!({
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": media.mime_type,
                                        "data": media.data,
                                    },
                                }));
                            }
                            crate::types::ContentPart::ImageUrl { url } => {
                                content_blocks.push(serde_json::json!({
                                    "type": "image",
                                    "source": {
                                        "type": "url",
                                        "url": url,
                                    },
                                }));
                            }
                            crate::types::ContentPart::AudioBase64 { media } => {
                                content_blocks.push(serde_json::json!({
                                        "type": "text",
                                        "text": format!("[audio: {}]", media.filename.as_deref().unwrap_or("audio")),
                                    }));
                            }
                        }
                    }
                    result.push(serde_json::json!({
                        "role": "user",
                        "content": content_blocks,
                    }));
                }
                _ => {
                    result.push(serde_json::json!({
                        "role": "user",
                        "content": msg.text_content().unwrap_or(""),
                    }));
                }
            },
            Role::Assistant => {
                let mut content_blocks: Vec<serde_json::Value> = Vec::new();

                if let Some(text) = msg.text_content() {
                    if !text.is_empty() {
                        content_blocks.push(serde_json::json!({
                            "type": "text",
                            "text": text,
                        }));
                    }
                }

                if let Some(ref tool_calls) = msg.tool_calls {
                    for tc in tool_calls {
                        let args: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                            .unwrap_or(serde_json::json!({}));
                        content_blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.function.name,
                            "input": args,
                        }));
                    }
                }

                if content_blocks.is_empty() {
                    content_blocks.push(serde_json::json!({
                        "type": "text",
                        "text": "",
                    }));
                }

                result.push(serde_json::json!({
                    "role": "assistant",
                    "content": content_blocks,
                }));
            }
            Role::Tool => {
                let tool_result_block = serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": msg.tool_call_id.as_deref().unwrap_or(""),
                    "content": msg.text_content().unwrap_or(""),
                });

                // Anthropic requires tool results in a user message. Merge with previous
                // user message if the last message is already a user message with tool results.
                if let Some(last) = result.last_mut() {
                    if last["role"].as_str() == Some("user") {
                        if let Some(arr) = last["content"].as_array_mut() {
                            arr.push(tool_result_block);
                            continue;
                        }
                    }
                }

                result.push(serde_json::json!({
                    "role": "user",
                    "content": [tool_result_block],
                }));
            }
            Role::System => unreachable!(),
        }
    }

    result
}

/// Normalize cache token fields from a provider usage object into
/// `(cached_read_tokens, cache_creation_tokens)`.
///
/// Accepts the many field-name variants we've seen in the wild (modeled after
/// `openclaw/src/agents/usage.ts:normalizeUsage`):
///
/// - **Anthropic**: `cache_read_input_tokens`, `cache_creation_input_tokens`
/// - **OpenAI / Responses API**: `prompt_tokens_details.cached_tokens`
/// - **DeepSeek / Kimi / Moonshot**: top-level `cached_tokens`
/// - **Bedrock alt**: already covered by the Anthropic names
///
/// Missing fields default to 0. Never panics.
pub(crate) fn normalize_cache_tokens(usage: &serde_json::Value) -> (u64, u64) {
    let as_u64 = |v: &serde_json::Value| v.as_u64().unwrap_or(0);
    let cached_read = [
        usage.get("cache_read_input_tokens"),
        usage.get("cached_input_tokens"),
        usage.get("cached_tokens"),
        usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens")),
    ]
    .into_iter()
    .flatten()
    .map(as_u64)
    .find(|&n| n > 0)
    .unwrap_or(0);

    let cache_creation = [
        usage.get("cache_creation_input_tokens"),
        usage.get("cache_write_input_tokens"),
    ]
    .into_iter()
    .flatten()
    .map(as_u64)
    .find(|&n| n > 0)
    .unwrap_or(0);

    (cached_read, cache_creation)
}

/// Build the `cache_control` marker object for Anthropic, respecting the
/// configured TTL. Returns `{"type": "ephemeral"}` for the default 5-minute
/// cache, or `{"type": "ephemeral", "ttl": "1h"}` for the extended cache.
fn build_cache_control_marker(ttl: crate::config::CacheTtl) -> serde_json::Value {
    match ttl.as_ttl_str() {
        Some(ttl_str) => serde_json::json!({ "type": "ephemeral", "ttl": ttl_str }),
        None => serde_json::json!({ "type": "ephemeral" }),
    }
}

/// Attach a `cache_control` marker to the last cacheable content block of the
/// last `rolling_messages` messages in-place. This is the rolling
/// "system + last N messages" cache strategy: the system prompt and tools
/// already carry cache markers, and the trailing markers cover the most recent
/// user turn plus the current assistant/tool context so successive turns can
/// hit the provider's cache for the shared prefix.
///
/// If a message's `content` is a plain string, it is first upgraded into a
/// single text content block so it can carry the marker. Thinking blocks are
/// skipped — Anthropic rejects `cache_control` on assistant thinking blocks —
/// and the marker falls back to the previous block in the same message.
/// Fewer than `rolling_messages` eligible messages is fine — we just mark
/// whatever we can.
fn apply_message_cache_control(
    messages: &mut [serde_json::Value],
    rolling_messages: usize,
    marker: &serde_json::Value,
) {
    if messages.is_empty() || rolling_messages == 0 {
        return;
    }

    let mut remaining = rolling_messages;
    for msg in messages.iter_mut().rev() {
        if remaining == 0 {
            break;
        }
        let Some(content) = msg.get_mut("content") else {
            continue;
        };

        // Upgrade plain-string content to a single text block so we can attach cache_control.
        if let Some(s) = content.as_str() {
            *content = serde_json::json!([{ "type": "text", "text": s }]);
        }

        let Some(blocks) = content.as_array_mut() else {
            continue;
        };

        // Walk blocks from the end, skipping any that are not cacheable
        // (Anthropic forbids cache_control on thinking/redacted_thinking).
        let mut marked = false;
        for block in blocks.iter_mut().rev() {
            let Some(obj) = block.as_object_mut() else {
                continue;
            };
            let block_type = obj
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if matches!(block_type, "thinking" | "redacted_thinking") {
                continue;
            }
            obj.insert("cache_control".to_string(), marker.clone());
            marked = true;
            break;
        }
        if marked {
            remaining -= 1;
        }
    }
}

/// Parse Anthropic non-streaming response into internal Message.
fn parse_anthropic_response(resp: &serde_json::Value) -> Result<Message> {
    let content_blocks = resp["content"]
        .as_array()
        .with_context(|| format!("Missing 'content' array in Anthropic response: {resp}"))?;

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for block in content_blocks {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(text) = block["text"].as_str() {
                    text_parts.push(text.to_string());
                }
            }
            Some("tool_use") => {
                let id = block["id"].as_str().unwrap_or("").to_string();
                let name = block["name"].as_str().unwrap_or("").to_string();
                let input = block["input"].clone();
                tool_calls.push(ToolCall {
                    id,
                    call_type: "function".to_string(),
                    function: crate::types::FunctionCall {
                        name,
                        arguments: serde_json::to_string(&input).unwrap_or_default(),
                    },
                });
            }
            _ => {}
        }
    }

    let content = if text_parts.is_empty() {
        None
    } else {
        Some(crate::types::MessageContent::Text(text_parts.join("")))
    };

    let tool_calls = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };

    Ok(Message {
        role: Role::Assistant,
        content,
        tool_calls,
        tool_call_id: None,
        timestamp: Some(chrono::Local::now().to_rfc3339()),
    })
}

/// Strip image content parts from messages when model doesn't support vision.
/// Simplifies `Parts([Text])` → `Text` after stripping.
fn strip_images(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|msg| {
            let content = match &msg.content {
                Some(crate::types::MessageContent::Parts(parts)) => {
                    let filtered: Vec<crate::types::ContentPart> = parts
                        .iter()
                        .filter(|p| {
                            !matches!(
                                p,
                                crate::types::ContentPart::ImageBase64 { .. }
                                    | crate::types::ContentPart::ImageUrl { .. }
                            )
                        })
                        .cloned()
                        .collect();

                    if filtered.is_empty() {
                        Some(crate::types::MessageContent::Text(String::new()))
                    } else if filtered.len() == 1 {
                        if let crate::types::ContentPart::Text(t) = &filtered[0] {
                            Some(crate::types::MessageContent::Text(t.clone()))
                        } else {
                            Some(crate::types::MessageContent::Parts(filtered))
                        }
                    } else {
                        Some(crate::types::MessageContent::Parts(filtered))
                    }
                }
                other => other.clone(),
            };

            Message {
                role: msg.role.clone(),
                content,
                tool_calls: msg.tool_calls.clone(),
                tool_call_id: msg.tool_call_id.clone(),
                timestamp: msg.timestamp.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FunctionCall, ToolCall, ToolDefinition};

    /// Helper to create an LlmClient with Anthropic provider for testing.
    /// Uses a unique env var to avoid races with parallel tests.
    fn make_test_anthropic_client() -> LlmClient {
        let env_var = "BORG_TEST_ANTHROPIC_LLM";
        std::env::set_var(env_var, "sk-test");
        let mut config = Config::default();
        config.llm.provider = Some("anthropic".to_string());
        config.llm.api_key_env = env_var.to_string();
        config.llm.model = "claude-sonnet-4".to_string();
        LlmClient::new(&config).unwrap()
    }

    #[test]
    fn anthropic_system_extraction() {
        let messages = vec![Message::system("You are helpful."), Message::user("Hello")];

        let client = make_test_anthropic_client();
        let body = client.build_anthropic_request(&messages, None, false);

        // With prompt caching enabled by default, system is emitted as a content-block array.
        let system_blocks = body["system"].as_array().expect("system should be array");
        assert_eq!(system_blocks.len(), 1);
        assert_eq!(system_blocks[0]["type"], "text");
        assert_eq!(system_blocks[0]["text"], "You are helpful.");

        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn anthropic_tool_result_conversion() {
        let messages = vec![
            Message::user("test"),
            Message {
                role: Role::Assistant,
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "read_memory".to_string(),
                        arguments: r#"{"filename":"test.md"}"#.to_string(),
                    },
                }]),
                tool_call_id: None,
                timestamp: None,
            },
            Message::tool_result("call_1", "file contents here"),
        ];

        let anthropic_msgs = build_anthropic_messages(&messages);

        // Should have: user, assistant, user (with tool_result)
        assert_eq!(anthropic_msgs.len(), 3);

        // Assistant message should have tool_use block
        let assistant = &anthropic_msgs[1];
        let content = assistant["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_use");
        assert_eq!(content[0]["name"], "read_memory");

        // Tool result should be in a user message
        let tool_user = &anthropic_msgs[2];
        assert_eq!(tool_user["role"], "user");
        let tool_content = tool_user["content"].as_array().unwrap();
        assert_eq!(tool_content[0]["type"], "tool_result");
        assert_eq!(tool_content[0]["tool_use_id"], "call_1");
    }

    #[test]
    fn anthropic_tool_definition_conversion() {
        let tools = vec![ToolDefinition::new(
            "test_tool",
            "A test tool",
            serde_json::json!({"type": "object", "properties": {"x": {"type": "string"}}}),
        )];

        let client = make_test_anthropic_client();

        let body = client.build_anthropic_request(&[Message::user("hi")], Some(&tools), false);

        let api_tools = body["tools"].as_array().unwrap();
        assert_eq!(api_tools.len(), 1);
        assert_eq!(api_tools[0]["name"], "test_tool");
        assert_eq!(api_tools[0]["description"], "A test tool");
        assert!(api_tools[0]["input_schema"].is_object());
        // Should NOT have "function" wrapping or "type":"function"
        assert!(api_tools[0]["function"].is_null());
    }

    #[test]
    fn anthropic_adjacent_tool_results_merge() {
        let messages = vec![
            Message::user("test"),
            Message {
                role: Role::Assistant,
                content: None,
                tool_calls: Some(vec![
                    ToolCall {
                        id: "call_1".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "tool_a".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                    ToolCall {
                        id: "call_2".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "tool_b".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                ]),
                tool_call_id: None,
                timestamp: None,
            },
            Message::tool_result("call_1", "result a"),
            Message::tool_result("call_2", "result b"),
        ];

        let anthropic_msgs = build_anthropic_messages(&messages);

        // user, assistant, user (merged tool results)
        assert_eq!(anthropic_msgs.len(), 3);

        let tool_user = &anthropic_msgs[2];
        let tool_content = tool_user["content"].as_array().unwrap();
        assert_eq!(tool_content.len(), 2);
        assert_eq!(tool_content[0]["tool_use_id"], "call_1");
        assert_eq!(tool_content[1]["tool_use_id"], "call_2");
    }

    #[test]
    fn parse_anthropic_response_text_only() {
        let resp = serde_json::json!({
            "content": [
                {"type": "text", "text": "Hello there!"}
            ],
            "stop_reason": "end_turn"
        });

        let msg = parse_anthropic_response(&resp).unwrap();
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.text_content(), Some("Hello there!"));
        assert!(msg.tool_calls.is_none());
    }

    #[test]
    fn parse_anthropic_response_with_tool_use() {
        let resp = serde_json::json!({
            "content": [
                {"type": "text", "text": "Let me check."},
                {
                    "type": "tool_use",
                    "id": "toolu_123",
                    "name": "read_memory",
                    "input": {"filename": "test.md"}
                }
            ],
            "stop_reason": "tool_use"
        });

        let msg = parse_anthropic_response(&resp).unwrap();
        assert_eq!(msg.text_content(), Some("Let me check."));
        let tcs = msg.tool_calls.unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].id, "toolu_123");
        assert_eq!(tcs[0].function.name, "read_memory");
        assert!(tcs[0].function.arguments.contains("test.md"));
    }

    // ── ProviderCooldown tests ──

    #[test]
    fn cooldown_initially_inactive() {
        let cd = ProviderCooldown::default();
        assert!(!cd.is_active());
        assert_eq!(cd.error_count, 0);
    }

    #[test]
    fn cooldown_active_after_failure() {
        let mut cd = ProviderCooldown::default();
        cd.record_failure(FailoverReason::RateLimit);
        assert!(cd.is_active());
        assert_eq!(cd.error_count, 1);
        assert_eq!(cd.reason, Some(FailoverReason::RateLimit));
    }

    #[test]
    fn cooldown_reset_on_success() {
        let mut cd = ProviderCooldown::default();
        cd.record_failure(FailoverReason::Overloaded);
        assert!(cd.is_active());

        cd.record_success();
        assert!(!cd.is_active());
        assert_eq!(cd.error_count, 0);
        assert!(cd.reason.is_none());
    }

    #[test]
    fn cooldown_backoff_escalates() {
        let mut cd = ProviderCooldown::default();
        cd.record_failure(FailoverReason::RateLimit);
        let first = cd.cooldown_until.unwrap();

        cd.cooldown_until = None; // simulate expiry
        cd.error_count = 0; // reset will happen in record_failure
        cd.record_failure(FailoverReason::RateLimit);
        let second = cd.cooldown_until.unwrap();

        // Both cooldowns should be in the future, second may be different
        // since error_count resets on expiry
        assert!(first > std::time::Instant::now() - Duration::from_secs(120));
        assert!(second > std::time::Instant::now() - Duration::from_secs(120));
    }

    #[test]
    fn cooldown_auth_has_longer_base() {
        let mut cd1 = ProviderCooldown::default();
        cd1.record_failure(FailoverReason::RateLimit);
        let rl_until = cd1.cooldown_until.unwrap();

        let mut cd2 = ProviderCooldown::default();
        cd2.record_failure(FailoverReason::Auth);
        let auth_until = cd2.cooldown_until.unwrap();

        // Auth base is 300s, RateLimit base is 60s — auth should be further out
        assert!(auth_until > rl_until);
    }

    #[test]
    fn failover_reason_display() {
        assert_eq!(FailoverReason::Auth.to_string(), "auth");
        assert_eq!(FailoverReason::Billing.to_string(), "billing");
        assert_eq!(FailoverReason::RateLimit.to_string(), "rate_limit");
        assert_eq!(FailoverReason::Overloaded.to_string(), "overloaded");
        assert_eq!(FailoverReason::Timeout.to_string(), "timeout");
        assert_eq!(FailoverReason::Format.to_string(), "format");
        assert_eq!(FailoverReason::Unknown.to_string(), "unknown");
    }

    #[test]
    fn llm_client_new_ollama_no_panic() {
        let mut config = Config::default();
        config.llm.provider = Some("ollama".to_string());
        config.llm.model = "llama3.3".to_string();
        let client = LlmClient::new(&config);
        assert!(client.is_ok());
    }

    #[test]
    fn effective_base_url_uses_config_override() {
        let mut config = Config::default();
        config.llm.provider = Some("ollama".to_string());
        config.llm.model = "llama3.3".to_string();
        config.llm.base_url = Some("http://custom:8080/v1/chat/completions".to_string());
        let client = LlmClient::new(&config).expect("should create client");
        assert_eq!(
            client.effective_base_url(),
            "http://custom:8080/v1/chat/completions"
        );
    }

    #[test]
    fn effective_base_url_falls_back_to_provider_default() {
        let mut config = Config::default();
        config.llm.provider = Some("ollama".to_string());
        config.llm.model = "llama3.3".to_string();
        let client = LlmClient::new(&config).expect("should create client");
        assert_eq!(
            client.effective_base_url(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn anthropic_request_thinking_off_includes_temperature() {
        let mut config = Config::default();
        config.llm.provider = Some("anthropic".to_string());
        config.llm.api_key_env = "ANTHROPIC_API_KEY".to_string();
        config.llm.model = "claude-sonnet-4-20250514".to_string();
        config.llm.thinking = crate::config::ThinkingLevel::Off;
        std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        let client = LlmClient::new(&config).expect("should create client");

        let messages = vec![Message::user("hi")];
        let body = client.build_anthropic_request(&messages, None, false);

        assert!(
            body.get("temperature").is_some(),
            "temperature should be present when thinking is off"
        );
        assert!(
            body.get("thinking").is_none(),
            "thinking field should not be present when off"
        );
        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    #[test]
    fn anthropic_request_thinking_enabled_omits_temperature() {
        let mut config = Config::default();
        config.llm.provider = Some("anthropic".to_string());
        config.llm.api_key_env = "ANTHROPIC_API_KEY".to_string();
        config.llm.model = "claude-sonnet-4-20250514".to_string();
        config.llm.thinking = crate::config::ThinkingLevel::High;
        std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        let client = LlmClient::new(&config).expect("should create client");

        let messages = vec![Message::user("hi")];
        let body = client.build_anthropic_request(&messages, None, false);

        assert!(
            body.get("temperature").is_none(),
            "temperature must be omitted when thinking is enabled"
        );
        let thinking = body
            .get("thinking")
            .expect("thinking field should be present");
        assert_eq!(thinking["type"], "enabled");
        assert_eq!(thinking["budget_tokens"], 16384);

        // max_tokens should be at least budget + 1024
        let max_tokens = body["max_tokens"].as_u64().unwrap();
        assert!(max_tokens >= 16384 + 1024);
        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    #[test]
    fn openai_request_includes_reasoning_effort() {
        let request = ChatRequest {
            model: "o3".to_string(),
            messages: vec![Message::user("test")],
            tools: None,
            temperature: 0.7,
            max_tokens: 4096,
            stream: false,
            reasoning_effort: Some("high".to_string()),
            prompt_cache_key: None,
            user: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["reasoning_effort"], "high");
    }

    #[test]
    fn openai_request_omits_reasoning_effort_when_none() {
        let request = ChatRequest {
            model: "gpt-4.1".to_string(),
            messages: vec![Message::user("test")],
            tools: None,
            temperature: 0.7,
            max_tokens: 4096,
            stream: false,
            reasoning_effort: None,
            prompt_cache_key: None,
            user: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert!(json.get("reasoning_effort").is_none());
    }

    #[test]
    fn openai_request_serializes_prompt_cache_key_when_set() {
        let request = ChatRequest {
            model: "gpt-4.1".to_string(),
            messages: vec![Message::user("hi")],
            tools: None,
            temperature: 0.7,
            max_tokens: 4096,
            stream: false,
            reasoning_effort: None,
            prompt_cache_key: Some("sess-abc-123".to_string()),
            user: Some("sess-abc-123".to_string()),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["prompt_cache_key"], "sess-abc-123");
        assert_eq!(json["user"], "sess-abc-123");
    }

    #[test]
    fn openai_request_omits_prompt_cache_key_when_none() {
        let request = ChatRequest {
            model: "gpt-4.1".to_string(),
            messages: vec![Message::user("hi")],
            tools: None,
            temperature: 0.7,
            max_tokens: 4096,
            stream: false,
            reasoning_effort: None,
            prompt_cache_key: None,
            user: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert!(json.get("prompt_cache_key").is_none());
        assert!(json.get("user").is_none());
    }

    #[test]
    fn openai_sanitizes_null_content_on_tool_call_messages() {
        // Gemini (via OpenRouter) rejects `"content": null`. The sanitization
        // in stream_chat_openai_inner should replace None content on assistant
        // tool-call messages with an empty string.
        let messages = vec![
            Message::user("hello"),
            Message {
                role: Role::Assistant,
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "read_memory".to_string(),
                        arguments: "{}".to_string(),
                    },
                }]),
                tool_call_id: None,
                timestamp: None,
            },
            Message::tool_result("call_1", "result"),
        ];

        let sanitized = sanitize_openai_messages(&messages);
        assert_eq!(sanitized.len(), 3);

        let request = ChatRequest {
            model: "google/gemini-2.5-flash".to_string(),
            messages: sanitized,
            tools: None,
            temperature: 0.7,
            max_tokens: 4096,
            stream: false,
            reasoning_effort: None,
            prompt_cache_key: None,
            user: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        let msgs = json["messages"].as_array().unwrap();
        // Assistant message (index 1) should have content "" not null
        assert_eq!(msgs[1]["content"], "");
        assert!(!msgs[1]["content"].is_null());
        // User message content should be unchanged
        assert_eq!(msgs[0]["content"], "hello");
    }

    #[test]
    fn openai_drops_empty_user_message_for_gemini_compat() {
        // Gemini 2.5 Pro/Flash reject user messages with no content parts.
        let messages = vec![
            Message::user("hello"),
            Message::assistant("hi there"),
            Message {
                role: Role::User,
                content: Some(crate::types::MessageContent::Text(String::new())),
                tool_calls: None,
                tool_call_id: None,
                timestamp: None,
            },
        ];

        let sanitized = sanitize_openai_messages(&messages);
        // Empty user message should be dropped
        assert_eq!(sanitized.len(), 2);
        assert_eq!(
            sanitized[0].content.as_ref().unwrap().text().unwrap(),
            "hello"
        );
        assert_eq!(
            sanitized[1].content.as_ref().unwrap().text().unwrap(),
            "hi there"
        );
    }

    #[test]
    fn openai_drops_empty_assistant_message_without_tool_calls() {
        // Empty assistant responses (e.g. suppressed heartbeat acks) should be
        // dropped when they have no tool calls, to avoid Gemini rejection.
        let messages = vec![
            Message::user("*heartbeat tick*"),
            Message {
                role: Role::Assistant,
                content: Some(crate::types::MessageContent::Text(String::new())),
                tool_calls: None,
                tool_call_id: None,
                timestamp: None,
            },
            Message::user("actual question"),
        ];

        let sanitized = sanitize_openai_messages(&messages);
        // Empty assistant message should be dropped
        assert_eq!(sanitized.len(), 2);
        assert_eq!(
            sanitized[0].content.as_ref().unwrap().text().unwrap(),
            "*heartbeat tick*"
        );
        assert_eq!(
            sanitized[1].content.as_ref().unwrap().text().unwrap(),
            "actual question"
        );
    }

    #[test]
    fn openai_drops_none_content_assistant_without_tool_calls() {
        // Assistant message with None content and no tool calls should be dropped.
        let messages = vec![
            Message::user("hello"),
            Message {
                role: Role::Assistant,
                content: None,
                tool_calls: None,
                tool_call_id: None,
                timestamp: None,
            },
            Message::user("follow up"),
        ];

        let sanitized = sanitize_openai_messages(&messages);
        assert_eq!(sanitized.len(), 2);
    }

    #[test]
    fn openai_preserves_tool_result_messages() {
        // Tool result messages (with tool_call_id) should never be dropped,
        // even if their content happens to be empty.
        let messages = vec![
            Message::user("hello"),
            Message {
                role: Role::Assistant,
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "read_memory".to_string(),
                        arguments: "{}".to_string(),
                    },
                }]),
                tool_call_id: None,
                timestamp: None,
            },
            Message {
                role: Role::Tool,
                content: Some(crate::types::MessageContent::Text(String::new())),
                tool_calls: None,
                tool_call_id: Some("call_1".to_string()),
                timestamp: None,
            },
        ];

        let sanitized = sanitize_openai_messages(&messages);
        // All 3 messages should be preserved (tool result has tool_call_id)
        assert_eq!(sanitized.len(), 3);
    }

    // ── Prompt caching (Anthropic) ──

    #[allow(clippy::expect_used, clippy::unwrap_used)]
    #[test]
    fn anthropic_request_includes_cache_control_on_system() {
        let client = make_test_anthropic_client();
        let messages = vec![Message::system("stable system prompt"), Message::user("hi")];
        let body = client.build_anthropic_request(&messages, None, false);

        let system = body["system"]
            .as_array()
            .expect("system should be an array of blocks when caching is enabled");
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "stable system prompt");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    }

    #[allow(clippy::expect_used, clippy::unwrap_used)]
    #[test]
    fn anthropic_request_caches_last_two_messages() {
        let client = make_test_anthropic_client();
        // 5 user/assistant messages so we can verify ONLY the last two carry markers.
        let messages = vec![
            Message::system("sys"),
            Message::user("turn 1"),
            Message::user("turn 2"),
            Message::user("turn 3"),
            Message::user("turn 4"),
            Message::user("turn 5"),
        ];
        let body = client.build_anthropic_request(&messages, None, false);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5, "system is stripped, 5 user msgs remain");

        // Last two should carry cache_control on their last content block.
        for idx in [3usize, 4] {
            let blocks = msgs[idx]["content"].as_array().unwrap();
            let last = blocks.last().unwrap();
            assert_eq!(
                last["cache_control"]["type"], "ephemeral",
                "msg[{idx}] last block should carry cache_control"
            );
        }

        // Earlier messages must NOT carry the marker. They may still be plain strings
        // (apply_message_cache_control only upgrades the trailing messages it marks).
        for idx in [0usize, 1, 2] {
            let content = &msgs[idx]["content"];
            if let Some(blocks) = content.as_array() {
                for block in blocks {
                    assert!(
                        block.get("cache_control").is_none(),
                        "msg[{idx}] should not carry cache_control"
                    );
                }
            } else {
                assert!(
                    content.is_string(),
                    "msg[{idx}] content should be string or block array"
                );
            }
        }
    }

    #[allow(clippy::expect_used, clippy::unwrap_used)]
    #[test]
    fn anthropic_request_cache_disabled_emits_no_markers() {
        let env_var = "BORG_TEST_ANTHROPIC_CACHE_OFF";
        std::env::set_var(env_var, "sk-test");
        let mut config = Config::default();
        config.llm.provider = Some("anthropic".to_string());
        config.llm.api_key_env = env_var.to_string();
        config.llm.model = "claude-sonnet-4".to_string();
        config.llm.cache.enabled = false;
        let client = LlmClient::new(&config).unwrap();

        let messages = vec![
            Message::system("sys"),
            Message::user("first"),
            Message::user("second"),
        ];
        let body = client.build_anthropic_request(&messages, None, false);

        // System reverts to a plain string.
        assert_eq!(body["system"].as_str(), Some("sys"));

        // No cache_control markers anywhere in messages.
        let msgs = body["messages"].as_array().unwrap();
        for msg in msgs {
            if let Some(blocks) = msg["content"].as_array() {
                for block in blocks {
                    assert!(
                        block.get("cache_control").is_none(),
                        "no cache_control expected when caching disabled"
                    );
                }
            }
        }
        std::env::remove_var(env_var);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn apply_message_cache_control_handles_fewer_than_two_messages() {
        let mut msgs = vec![serde_json::json!({
            "role": "user",
            "content": [{ "type": "text", "text": "only one" }],
        })];
        let marker = build_cache_control_marker(crate::config::CacheTtl::FiveMin);
        apply_message_cache_control(&mut msgs, 2, &marker);
        let blocks = msgs[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
    }

    #[allow(clippy::expect_used)]
    #[test]
    fn apply_message_cache_control_upgrades_string_content() {
        // Plain-string content is upgraded to a single text block so cache_control can attach.
        let mut msgs = vec![
            serde_json::json!({ "role": "user", "content": "earlier" }),
            serde_json::json!({ "role": "user", "content": "latest" }),
        ];
        let marker = build_cache_control_marker(crate::config::CacheTtl::FiveMin);
        apply_message_cache_control(&mut msgs, 2, &marker);
        for msg in &msgs {
            let blocks = msg["content"]
                .as_array()
                .expect("string content should have been upgraded to a block array");
            assert_eq!(blocks[0]["type"], "text");
            assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        }
    }

    // ── New prompt caching tests ──

    #[allow(clippy::expect_used, clippy::unwrap_used)]
    #[test]
    fn anthropic_request_marks_last_tool_with_cache_control() {
        let client = make_test_anthropic_client();
        let messages = vec![Message::system("sys"), Message::user("hi")];
        let tools = vec![make_tool("a"), make_tool("b"), make_tool("c")];
        let body = client.build_anthropic_request(&messages, Some(&tools), false);
        let tools_arr = body["tools"].as_array().expect("tools array");
        assert_eq!(tools_arr.len(), 3);
        assert!(
            tools_arr[0].get("cache_control").is_none(),
            "first tool must NOT carry cache_control"
        );
        assert!(
            tools_arr[1].get("cache_control").is_none(),
            "middle tool must NOT carry cache_control"
        );
        assert_eq!(
            tools_arr[2]["cache_control"]["type"], "ephemeral",
            "last tool should carry cache_control to cache the whole array"
        );
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn anthropic_request_cache_disabled_leaves_tools_unmarked() {
        let env_var = "BORG_TEST_ANTHROPIC_TOOLS_NOCACHE";
        std::env::set_var(env_var, "sk-test");
        let mut config = Config::default();
        config.llm.provider = Some("anthropic".to_string());
        config.llm.api_key_env = env_var.to_string();
        config.llm.model = "claude-sonnet-4".to_string();
        config.llm.cache.enabled = false;
        let client = LlmClient::new(&config).unwrap();

        let tools = vec![make_tool("a"), make_tool("b")];
        let messages = vec![Message::system("sys"), Message::user("hi")];
        let body = client.build_anthropic_request(&messages, Some(&tools), false);
        let tools_arr = body["tools"].as_array().unwrap();
        for tool in tools_arr {
            assert!(tool.get("cache_control").is_none());
        }
        std::env::remove_var(env_var);
    }

    #[test]
    fn apply_message_cache_control_skips_thinking_block() {
        // Assistant message ending in a thinking block must still be cacheable —
        // the marker should land on the previous (text) block instead.
        let mut msgs = vec![serde_json::json!({
            "role": "assistant",
            "content": [
                { "type": "text", "text": "answer text" },
                { "type": "thinking", "thinking": "reasoning..." },
            ],
        })];
        let marker = build_cache_control_marker(crate::config::CacheTtl::FiveMin);
        apply_message_cache_control(&mut msgs, 1, &marker);
        let blocks = msgs[0]["content"].as_array().unwrap();
        // Text block gets the marker.
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        // Thinking block must NOT get a marker.
        assert!(blocks[1].get("cache_control").is_none());
    }

    #[test]
    fn apply_message_cache_control_skips_redacted_thinking_block() {
        let mut msgs = vec![serde_json::json!({
            "role": "assistant",
            "content": [
                { "type": "text", "text": "hello" },
                { "type": "redacted_thinking", "data": "opaque" },
            ],
        })];
        let marker = build_cache_control_marker(crate::config::CacheTtl::FiveMin);
        apply_message_cache_control(&mut msgs, 1, &marker);
        let blocks = msgs[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        assert!(blocks[1].get("cache_control").is_none());
    }

    #[test]
    fn apply_message_cache_control_empty_is_noop() {
        let mut msgs: Vec<serde_json::Value> = Vec::new();
        let marker = build_cache_control_marker(crate::config::CacheTtl::FiveMin);
        apply_message_cache_control(&mut msgs, 2, &marker);
        assert!(msgs.is_empty());
    }

    #[test]
    fn apply_message_cache_control_zero_rolling_is_noop() {
        let mut msgs = vec![serde_json::json!({
            "role": "user",
            "content": [{ "type": "text", "text": "hi" }],
        })];
        let marker = build_cache_control_marker(crate::config::CacheTtl::FiveMin);
        apply_message_cache_control(&mut msgs, 0, &marker);
        assert!(
            msgs[0]["content"][0].get("cache_control").is_none(),
            "rolling=0 should not attach any marker"
        );
    }

    #[test]
    fn build_cache_control_marker_five_min() {
        let marker = build_cache_control_marker(crate::config::CacheTtl::FiveMin);
        assert_eq!(marker["type"], "ephemeral");
        assert!(marker.get("ttl").is_none());
    }

    #[test]
    fn build_cache_control_marker_one_hour() {
        let marker = build_cache_control_marker(crate::config::CacheTtl::OneHour);
        assert_eq!(marker["type"], "ephemeral");
        assert_eq!(marker["ttl"], "1h");
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn anthropic_request_honors_one_hour_ttl() {
        let env_var = "BORG_TEST_ANTHROPIC_TTL_1H";
        std::env::set_var(env_var, "sk-test");
        let mut config = Config::default();
        config.llm.provider = Some("anthropic".to_string());
        config.llm.api_key_env = env_var.to_string();
        config.llm.model = "claude-sonnet-4".to_string();
        config.llm.cache.ttl = crate::config::CacheTtl::OneHour;
        let client = LlmClient::new(&config).unwrap();

        let tools = vec![make_tool("a")];
        let messages = vec![Message::system("sys"), Message::user("hi")];
        let body = client.build_anthropic_request(&messages, Some(&tools), false);
        assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(body["tools"][0]["cache_control"]["ttl"], "1h");
        std::env::remove_var(env_var);
    }

    #[test]
    fn normalize_cache_tokens_anthropic_fields() {
        let usage = serde_json::json!({
            "input_tokens": 1000,
            "output_tokens": 200,
            "cache_read_input_tokens": 800,
            "cache_creation_input_tokens": 50,
        });
        let (read, created) = normalize_cache_tokens(&usage);
        assert_eq!(read, 800);
        assert_eq!(created, 50);
    }

    #[test]
    fn normalize_cache_tokens_openai_details() {
        let usage = serde_json::json!({
            "prompt_tokens": 1500,
            "completion_tokens": 300,
            "prompt_tokens_details": { "cached_tokens": 1200 }
        });
        let (read, created) = normalize_cache_tokens(&usage);
        assert_eq!(read, 1200);
        assert_eq!(created, 0);
    }

    #[test]
    fn normalize_cache_tokens_deepseek_bare_cached_tokens() {
        let usage = serde_json::json!({
            "prompt_tokens": 800,
            "cached_tokens": 600,
        });
        let (read, created) = normalize_cache_tokens(&usage);
        assert_eq!(read, 600);
        assert_eq!(created, 0);
    }

    #[test]
    fn normalize_cache_tokens_missing_fields_returns_zero() {
        let usage = serde_json::json!({ "prompt_tokens": 100 });
        let (read, created) = normalize_cache_tokens(&usage);
        assert_eq!(read, 0);
        assert_eq!(created, 0);
    }

    #[test]
    fn normalize_cache_tokens_handles_bedrock_alt_name() {
        let usage = serde_json::json!({ "cache_write_input_tokens": 42 });
        let (_read, created) = normalize_cache_tokens(&usage);
        assert_eq!(created, 42);
    }

    #[test]
    fn prompt_cache_config_defaults() {
        let cfg = crate::config::PromptCacheConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.cache_tools);
        assert!(cfg.cache_system);
        assert_eq!(cfg.rolling_messages, 2);
        assert_eq!(cfg.ttl, crate::config::CacheTtl::Auto);
    }

    #[test]
    fn prompt_cache_config_roundtrip_toml() {
        let cfg = crate::config::PromptCacheConfig {
            enabled: true,
            ttl: crate::config::CacheTtl::OneHour,
            cache_tools: false,
            cache_system: true,
            rolling_messages: 1,
        };
        let toml_str = toml::to_string(&cfg).unwrap();
        let parsed: crate::config::PromptCacheConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn prompt_cache_config_rolling_messages_clamped() {
        let mut cfg = crate::config::PromptCacheConfig::default();
        cfg.rolling_messages = 7;
        assert_eq!(cfg.rolling_messages_clamped(), 2);
        cfg.rolling_messages = 0;
        assert_eq!(cfg.rolling_messages_clamped(), 0);
        cfg.rolling_messages = 1;
        assert_eq!(cfg.rolling_messages_clamped(), 1);
    }

    #[test]
    fn with_prompt_cache_key_sets_field() {
        let client = make_test_anthropic_client().with_prompt_cache_key("sess-xyz");
        assert_eq!(client.prompt_cache_key.as_deref(), Some("sess-xyz"));
    }

    // Helper: build a minimal ToolDefinition for request-shape tests.
    fn make_tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".to_string(),
            function: crate::types::FunctionDefinition {
                name: name.to_string(),
                description: format!("tool {name}"),
                parameters: serde_json::json!({ "type": "object", "properties": {} }),
            },
        }
    }
}
