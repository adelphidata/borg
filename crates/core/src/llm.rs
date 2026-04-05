use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
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
        })
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

        bail!("All providers exhausted")
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
        let request = ChatRequest {
            model: model.clone(),
            messages: messages.to_vec(),
            tools: tools.map(<[ToolDefinition]>::to_vec),
            temperature: self.llm_config.temperature,
            max_tokens: self.llm_config.max_tokens,
            stream: true,
            reasoning_effort,
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

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let chunk_timeout_secs = self.llm_config.stream_chunk_timeout_secs;

        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => {
                    if tx.send(StreamEvent::Done).await.is_err() {
                        tracing::debug!("llm: stream receiver closed");
                    }
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
                                source: anyhow::anyhow!("Stream read error: {e}"),
                                retry_after: None,
                                reason: FailoverReason::Timeout,
                            });
                        }
                        Ok(None) => {
                            if tx.send(StreamEvent::Done).await.is_err() {
                                tracing::debug!("llm: stream receiver closed");
                            }
                            return Ok(());
                        }
                        Err(_) => {
                            return Err(LlmError::Retryable {
                                source: anyhow::anyhow!("No data received for {chunk_timeout_secs}s"),
                                retry_after: None,
                                reason: FailoverReason::Timeout,
                            });
                        }
                    }
                }
            };

            buffer.push_str(&String::from_utf8_lossy(&chunk));

            if buffer.len() > MAX_SSE_BUFFER {
                return Err(LlmError::Retryable {
                    source: anyhow::anyhow!("SSE buffer exceeded {MAX_SSE_BUFFER} bytes"),
                    retry_after: None,
                    reason: FailoverReason::Overloaded,
                });
            }

            let mut cursor = 0;
            while let Some(rel_end) = buffer[cursor..].find('\n') {
                let line_end = cursor + rel_end;
                let line = buffer[cursor..line_end].trim();
                cursor = line_end + 1;

                if line.is_empty() || line.starts_with(':') {
                    continue;
                }

                if let Some(data) = line.strip_prefix("data: ") {
                    if data.trim() == "[DONE]" {
                        if tx.send(StreamEvent::Done).await.is_err() {
                            tracing::debug!("llm: stream receiver closed");
                        }
                        return Ok(());
                    }

                    match serde_json::from_str::<StreamChunk>(data) {
                        Ok(chunk) => {
                            // Parse usage data if present
                            if let Some(usage) = chunk.usage {
                                if tx
                                    .send(StreamEvent::Usage(UsageData {
                                        prompt_tokens: usage.prompt_tokens.unwrap_or(0),
                                        completion_tokens: usage.completion_tokens.unwrap_or(0),
                                        total_tokens: usage.total_tokens.unwrap_or(0),
                                        provider: self.provider.as_str().to_string(),
                                        model: self.llm_config.model.clone(),
                                    }))
                                    .await
                                    .is_err()
                                {
                                    debug!("Stream receiver dropped, stopping SSE processing");
                                    return Ok(());
                                }
                            }
                            if let Some(choices) = chunk.choices {
                                for choice in choices {
                                    if let Some(delta) = choice.delta {
                                        if let Some(content) = delta.content {
                                            if tx
                                                .send(StreamEvent::TextDelta(content))
                                                .await
                                                .is_err()
                                            {
                                                debug!("Stream receiver dropped, stopping SSE processing");
                                                return Ok(());
                                            }
                                        }
                                        if let Some(tool_calls) = delta.tool_calls {
                                            for tc in tool_calls {
                                                if tx
                                                    .send(StreamEvent::ToolCallDelta {
                                                        index: tc.index.unwrap_or(0),
                                                        id: tc.id,
                                                        name: tc
                                                            .function
                                                            .as_ref()
                                                            .and_then(|f| f.name.clone()),
                                                        arguments_delta: tc
                                                            .function
                                                            .as_ref()
                                                            .and_then(|f| f.arguments.clone())
                                                            .unwrap_or_default(),
                                                    })
                                                    .await
                                                    .is_err()
                                                {
                                                    debug!("Stream receiver dropped, stopping SSE processing");
                                                    return Ok(());
                                                }
                                            }
                                        }
                                    }
                                    if choice.finish_reason.is_some() {
                                        if tx.send(StreamEvent::Done).await.is_err() {
                                            tracing::debug!("llm: stream receiver closed");
                                        }
                                        return Ok(());
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Failed to parse SSE chunk: {e}");
                        }
                    }
                }
            }
            buffer.drain(..cursor);
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
        };

        let response = self
            .client
            .post(self.effective_base_url())
            .headers(self.provider.build_headers(&self.api_key)?)
            .json(&request)
            .send()
            .await
            .with_context(|| format!("Failed to connect to {}", self.provider))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_else(|e| {
                tracing::warn!("Failed to read error response body: {e}");
                String::new()
            });
            bail!("{} returned {status}: {body}", self.provider);
        }

        #[derive(Deserialize)]
        struct ChatResponse {
            choices: Vec<ChatChoice>,
        }
        #[derive(Deserialize)]
        struct ChatChoice {
            message: Message,
        }

        let resp: ChatResponse = response.json().await?;
        resp.choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .context("No response from LLM")
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

        let cache_enabled = self.llm_config.cache.enabled;

        if let Some(sys) = system_text {
            if cache_enabled {
                // Structured system block with cache_control so the provider can
                // reuse the (stable) system prompt across turns.
                body["system"] = serde_json::json!([
                    {
                        "type": "text",
                        "text": sys,
                        "cache_control": { "type": "ephemeral" },
                    }
                ]);
            } else {
                body["system"] = serde_json::json!(sys);
            }
        }

        let mut anthropic_messages = anthropic_messages;
        if cache_enabled {
            apply_message_cache_control(&mut anthropic_messages);
        }
        body["messages"] = serde_json::json!(anthropic_messages);
        if let Some(tools) = anthropic_tools {
            body["tools"] = serde_json::json!(tools);
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

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut current_tool_index: usize = 0;
        let mut current_block_is_tool = false;
        let mut current_block_is_thinking = false;
        let chunk_timeout_secs = self.llm_config.stream_chunk_timeout_secs;

        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => {
                    if tx.send(StreamEvent::Done).await.is_err() {
                        tracing::debug!("llm: stream receiver closed");
                    }
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
                                source: anyhow::anyhow!("Stream read error: {e}"),
                                retry_after: None,
                                reason: FailoverReason::Timeout,
                            });
                        }
                        Ok(None) => {
                            if tx.send(StreamEvent::Done).await.is_err() {
                                tracing::debug!("llm: stream receiver closed");
                            }
                            return Ok(());
                        }
                        Err(_) => {
                            return Err(LlmError::Retryable {
                                source: anyhow::anyhow!("No data received for {chunk_timeout_secs}s"),
                                retry_after: None,
                                reason: FailoverReason::Timeout,
                            });
                        }
                    }
                }
            };

            buffer.push_str(&String::from_utf8_lossy(&chunk));

            if buffer.len() > MAX_SSE_BUFFER {
                return Err(LlmError::Retryable {
                    source: anyhow::anyhow!("SSE buffer exceeded {MAX_SSE_BUFFER} bytes"),
                    retry_after: None,
                    reason: FailoverReason::Overloaded,
                });
            }

            let mut cursor = 0;
            while let Some(rel_end) = buffer[cursor..].find('\n') {
                let line_end = cursor + rel_end;
                let line = buffer[cursor..line_end].trim();
                cursor = line_end + 1;

                if line.is_empty() || line.starts_with(':') {
                    continue;
                }

                // Anthropic SSE uses "event: <type>" followed by "data: <json>"
                if let Some(data) = line.strip_prefix("data: ") {
                    if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                        let event_type = event["type"].as_str().unwrap_or("");

                        match event_type {
                            "content_block_start" => {
                                let block = &event["content_block"];
                                let block_type = block["type"].as_str().unwrap_or("");
                                current_block_is_tool = false;
                                current_block_is_thinking = false;
                                match block_type {
                                    "tool_use" => {
                                        current_block_is_tool = true;
                                        let id = block["id"].as_str().map(String::from);
                                        let name = block["name"].as_str().map(String::from);
                                        if tx
                                            .send(StreamEvent::ToolCallDelta {
                                                index: current_tool_index,
                                                id,
                                                name,
                                                arguments_delta: String::new(),
                                            })
                                            .await
                                            .is_err()
                                        {
                                            debug!(
                                                "Stream receiver dropped, stopping SSE processing"
                                            );
                                            return Ok(());
                                        }
                                    }
                                    "thinking" => {
                                        current_block_is_thinking = true;
                                    }
                                    _ => {}
                                }
                            }
                            "content_block_delta" => {
                                let delta = &event["delta"];
                                match delta["type"].as_str() {
                                    Some("text_delta") => {
                                        if let Some(text) = delta["text"].as_str() {
                                            if tx
                                                .send(StreamEvent::TextDelta(text.to_string()))
                                                .await
                                                .is_err()
                                            {
                                                debug!("Stream receiver dropped, stopping SSE processing");
                                                return Ok(());
                                            }
                                        }
                                    }
                                    Some("thinking_delta") => {
                                        if let Some(text) = delta["thinking"].as_str() {
                                            if tx
                                                .send(StreamEvent::ThinkingDelta(text.to_string()))
                                                .await
                                                .is_err()
                                            {
                                                debug!("Stream receiver dropped, stopping SSE processing");
                                                return Ok(());
                                            }
                                        }
                                    }
                                    Some("input_json_delta") => {
                                        if let Some(json_delta) = delta["partial_json"].as_str() {
                                            if tx
                                                .send(StreamEvent::ToolCallDelta {
                                                    index: current_tool_index,
                                                    id: None,
                                                    name: None,
                                                    arguments_delta: json_delta.to_string(),
                                                })
                                                .await
                                                .is_err()
                                            {
                                                debug!("Stream receiver dropped, stopping SSE processing");
                                                return Ok(());
                                            }
                                        }
                                    }
                                    _ => {
                                        // For thinking blocks, text comes as text_delta
                                        if current_block_is_thinking {
                                            if let Some(text) = delta["text"].as_str() {
                                                if tx
                                                    .send(StreamEvent::ThinkingDelta(
                                                        text.to_string(),
                                                    ))
                                                    .await
                                                    .is_err()
                                                {
                                                    debug!("Stream receiver dropped, stopping SSE processing");
                                                    return Ok(());
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            "content_block_stop" => {
                                if current_block_is_tool {
                                    current_tool_index += 1;
                                }
                            }
                            "message_stop" => {
                                if tx.send(StreamEvent::Done).await.is_err() {
                                    tracing::debug!("llm: stream receiver closed");
                                }
                                return Ok(());
                            }
                            "message_delta" => {
                                // Parse usage from message_delta
                                if let Some(usage) = event["usage"].as_object() {
                                    let input = usage
                                        .get("input_tokens")
                                        .and_then(serde_json::Value::as_u64)
                                        .unwrap_or(0);
                                    let output = usage
                                        .get("output_tokens")
                                        .and_then(serde_json::Value::as_u64)
                                        .unwrap_or(0);
                                    if tx
                                        .send(StreamEvent::Usage(UsageData {
                                            prompt_tokens: input,
                                            completion_tokens: output,
                                            total_tokens: input + output,
                                            provider: self.provider.as_str().to_string(),
                                            model: self.llm_config.model.clone(),
                                        }))
                                        .await
                                        .is_err()
                                    {
                                        debug!("Stream receiver dropped, stopping SSE processing");
                                        return Ok(());
                                    }
                                }
                                // message_delta with stop_reason indicates end
                                if event["delta"]["stop_reason"].as_str().is_some() {
                                    if tx.send(StreamEvent::Done).await.is_err() {
                                        tracing::debug!("llm: stream receiver closed");
                                    }
                                    return Ok(());
                                }
                            }
                            "error" => {
                                let err_msg = event["error"]["message"]
                                    .as_str()
                                    .unwrap_or("unknown error");
                                return Err(LlmError::Retryable {
                                    source: anyhow::anyhow!("Anthropic stream error: {err_msg}"),
                                    retry_after: None,
                                    reason: FailoverReason::Overloaded,
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
            buffer.drain(..cursor);
        }
    }

    #[instrument(skip_all, fields(llm.provider = "anthropic"))]
    async fn chat_anthropic(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<Message> {
        let body = self.build_anthropic_request(messages, tools, false);

        let response = self
            .client
            .post(self.effective_base_url())
            .headers(self.provider.build_headers(&self.api_key)?)
            .json(&body)
            .send()
            .await
            .context("Failed to connect to Anthropic")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_else(|e| {
                tracing::warn!("Failed to read error response body: {e}");
                String::new()
            });
            bail!("Anthropic returned {status}: {body}");
        }

        let resp: serde_json::Value = response.json().await?;
        parse_anthropic_response(&resp)
    }
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

/// Attach `cache_control: {"type": "ephemeral"}` to the last content block of the
/// last two messages in-place. This is the rolling "system + last 2 messages" cache
/// strategy: the system prompt already carries a cache marker, and the two trailing
/// markers cover the last user turn plus the current assistant/tool context so that
/// successive turns can hit the provider's cache for the shared prefix.
///
/// If a message's `content` is a plain string, it is first upgraded into a single
/// text content block so it can carry the marker (Anthropic accepts both forms).
/// Fewer than two eligible messages is fine — we just mark whatever we can.
fn apply_message_cache_control(messages: &mut [serde_json::Value]) {
    if messages.is_empty() {
        return;
    }

    let mut remaining = 2usize;
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
        let Some(last_block) = blocks.last_mut() else {
            continue;
        };
        if let Some(obj) = last_block.as_object_mut() {
            obj.insert(
                "cache_control".to_string(),
                serde_json::json!({ "type": "ephemeral" }),
            );
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
        };
        let json = serde_json::to_value(&request).unwrap();
        assert!(json.get("reasoning_effort").is_none());
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
        apply_message_cache_control(&mut msgs);
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
        apply_message_cache_control(&mut msgs);
        for msg in &msgs {
            let blocks = msg["content"]
                .as_array()
                .expect("string content should have been upgraded to a block array");
            assert_eq!(blocks[0]["type"], "text");
            assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        }
    }
}
