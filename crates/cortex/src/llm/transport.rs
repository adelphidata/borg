use anyhow::{Context, Result};

use super::{
    apply_message_cache_control, apply_openai_cache_markers, bedrock::url_for_model,
    build_anthropic_messages, build_cache_control_marker, classify_anthropic_stream_error,
    normalize_cache_tokens, parse_anthropic_response, repair_message_sequence,
    sanitize_openai_messages, supports_anthropic_cache_control, ChatRequest, LlmConfig, LlmError,
    Message, Provider, Role, StreamChunk, StreamEvent, StreamOptions, StreamUsage, ToolDefinition,
    UsageData, UsageOptions,
};
use crate::llm::llm_error::FailoverReason;

pub(crate) struct TransportRequest<'a> {
    pub(crate) provider: Provider,
    pub(crate) model: &'a str,
    pub(crate) messages: &'a [Message],
    pub(crate) tools: Option<&'a [ToolDefinition]>,
    pub(crate) llm_config: &'a LlmConfig,
    pub(crate) prompt_cache_key: Option<&'a str>,
    /// The byte-stable head of this turn's system prompt, when the caller knows
    /// it. Providers with explicit cache breakpoints put the system marker at
    /// the end of *this* rather than at the end of the whole system prompt.
    ///
    /// Carried as the prefix text — not an offset — so a value left over from an
    /// earlier turn is harmless: it is honored only while it still genuinely
    /// prefixes the system message in hand, and if it does, it is still a
    /// correct stable prefix. A mismatched or absent value costs the split, not
    /// correctness.
    pub(crate) system_stable_prefix: Option<&'a str>,
    pub(crate) cost_events_enabled: bool,
}

pub(crate) struct TransportResponse {
    pub(crate) message: Message,
    pub(crate) usage: Option<UsageData>,
}

pub(crate) enum StreamState {
    OpenAiCompat,
    Anthropic(AnthropicStreamState),
    Bedrock(BedrockStreamState),
    /// Native Gemini SSE accumulator. `Provider::Gemini` routes through
    /// `gemini_transport()`, which produces this variant from
    /// `new_stream_state()`.
    Gemini(crate::llm::gemini::GeminiStreamState),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamProtocol {
    Sse,
    AwsEventStream,
}

pub(crate) trait Transport: Sync {
    fn protocol_name(&self) -> &'static str;

    fn debug_request_label(&self) -> &'static str;

    fn stream_protocol(&self) -> StreamProtocol {
        StreamProtocol::Sse
    }

    fn endpoint(&self, request: &TransportRequest<'_>, stream: bool) -> Result<String>;

    fn build_streaming_request(
        &self,
        request: &TransportRequest<'_>,
    ) -> std::result::Result<serde_json::Value, LlmError>;

    fn build_chat_request(&self, request: &TransportRequest<'_>) -> Result<serde_json::Value>;

    fn new_stream_state(&self) -> StreamState;

    fn parse_stream_line(
        &self,
        line: &str,
        provider: &str,
        model: &str,
        state: &mut StreamState,
    ) -> crate::sse::SseAction;

    fn parse_stream_payload(
        &self,
        _payload: &[u8],
        _provider: &str,
        _model: &str,
        _state: &mut StreamState,
    ) -> crate::sse::SseAction {
        crate::sse::SseAction::Continue
    }

    fn parse_chat_response(
        &self,
        response: serde_json::Value,
        provider: &str,
        model: &str,
    ) -> Result<TransportResponse>;
}

static OPENAI_COMPAT_TRANSPORT: OpenAiCompatTransport = OpenAiCompatTransport;
static ANTHROPIC_TRANSPORT: AnthropicTransport = AnthropicTransport;
static BEDROCK_TRANSPORT: BedrockTransport = BedrockTransport;
static VERTEX_ANTHROPIC_TRANSPORT: VertexAnthropicTransport = VertexAnthropicTransport;
static GEMINI_TRANSPORT: crate::llm::gemini::GeminiTransport = crate::llm::gemini::GeminiTransport;

pub(crate) fn openai_compat_transport() -> &'static dyn Transport {
    &OPENAI_COMPAT_TRANSPORT
}

pub(crate) fn anthropic_transport() -> &'static dyn Transport {
    &ANTHROPIC_TRANSPORT
}

pub(crate) fn bedrock_transport() -> &'static dyn Transport {
    &BEDROCK_TRANSPORT
}

pub(crate) fn vertex_anthropic_transport() -> &'static dyn Transport {
    &VERTEX_ANTHROPIC_TRANSPORT
}

/// Native Gemini AI Studio transport. `Provider::Gemini` routes through
/// this transport rather than the OpenAI-compat façade so video parts
/// and Gemini-specific request fields survive the round-trip.
pub(crate) fn gemini_transport() -> &'static dyn Transport {
    &GEMINI_TRANSPORT
}

pub(crate) fn extract_provider_cost_usd(usage: &StreamUsage) -> Option<String> {
    if let Some(c) = usage.cost.as_ref().and_then(parse_usage_cost) {
        return Some(c);
    }
    let details = usage.cost_details.as_ref()?;
    if let Some(c) = details.total_cost.as_ref().and_then(parse_usage_cost) {
        return Some(c);
    }
    details
        .upstream_inference_cost
        .as_ref()
        .and_then(parse_usage_cost)
}

/// Render the Anthropic top-level `system` field as cacheable text blocks.
///
/// The cache breakpoint goes at the end of the *stable prefix*, not the end of
/// the whole prompt. Marking the whole prompt is what the naive shape does, and
/// it caches nothing: the prompt's dynamic suffix ends with a second-resolution
/// `Current Time`, so the marked block is byte-different on every turn and never
/// hits. Splitting moves the marker back to content that actually repeats.
///
/// This does **not** spend an extra breakpoint — there is still exactly one
/// `cache_control` here, so the default layout (tools + system + 2 rolling)
/// stays inside Anthropic's budget of 4.
///
/// Falls back to a single marked block when `stable_prefix` is absent, doesn't
/// match, or would leave either side empty. Those cases lose the split, not
/// correctness.
fn anthropic_system_blocks(
    system: &str,
    stable_prefix: Option<&str>,
    cache_marker: &serde_json::Value,
) -> serde_json::Value {
    let split = stable_prefix
        .filter(|prefix| !prefix.is_empty() && prefix.len() < system.len())
        .filter(|prefix| system.starts_with(*prefix))
        .map(str::len);

    // `starts_with` above proves bytes [0..at) are exactly the prefix's bytes,
    // and the prefix is a valid `&str`, so `at` is necessarily a char boundary.
    // Assert it rather than leaving the argument in a comment.
    debug_assert!(split.is_none_or(|at| system.is_char_boundary(at)));

    match split {
        Some(at) => serde_json::json!([
            { "type": "text", "text": &system[..at], "cache_control": cache_marker },
            { "type": "text", "text": &system[at..] },
        ]),
        None => serde_json::json!([
            { "type": "text", "text": system, "cache_control": cache_marker },
        ]),
    }
}

pub(crate) fn build_anthropic_request(
    request: &TransportRequest<'_>,
    stream: bool,
) -> serde_json::Value {
    let system_text: Option<String> = request
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .and_then(|m| m.text_content().map(String::from));

    // Repair before converting, exactly as the OpenAI-compatible path does.
    // Anthropic requires every `tool_use` id to be unique *within one
    // request*, and history no longer guarantees that: a server that reuses
    // one id for every call now keeps both pairings (which is what lets the
    // model see its own results at all), so the uniqueness the wire needs has
    // to be restored here. `repair_message_sequence` renames the later
    // occurrence and its paired result rather than dropping either.
    let repaired = repair_message_sequence(request.messages);
    let anthropic_messages = build_anthropic_messages(&repaired);
    let anthropic_tools: Option<Vec<serde_json::Value>> = request.tools.map(|ts| {
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

    let cache_cfg = &request.llm_config.cache;
    let cache_enabled = cache_cfg.enabled;
    let cache_marker = build_cache_control_marker(cache_cfg.ttl);
    let (cache_system_eff, cache_tools_eff, rolling_eff) = cache_cfg.effective_layout();

    let effective_thinking = request.llm_config.effective_thinking(request.model);
    let mut body = if let Some(budget) = effective_thinking.anthropic_budget_tokens() {
        let max_tokens = request.llm_config.max_tokens.max(budget + 1024);
        serde_json::json!({
            "model": request.model,
            "max_tokens": max_tokens,
            "stream": stream,
            "thinking": {
                "type": "enabled",
                "budget_tokens": budget
            }
        })
    } else {
        serde_json::json!({
            "model": request.model,
            "max_tokens": request.llm_config.max_tokens,
            "temperature": request.llm_config.temperature,
            "stream": stream,
        })
    };

    if let Some(sys) = system_text {
        if cache_enabled && cache_system_eff {
            body["system"] =
                anthropic_system_blocks(&sys, request.system_stable_prefix, &cache_marker);
        } else {
            body["system"] = serde_json::json!(sys);
        }
    }

    let mut anthropic_messages = anthropic_messages;
    if cache_enabled {
        apply_message_cache_control(&mut anthropic_messages, rolling_eff, &cache_marker, false);
    }
    body["messages"] = serde_json::json!(anthropic_messages);
    if let Some(mut tools_json) = anthropic_tools {
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

pub(crate) fn parse_openai_sse_line(
    line: &str,
    provider: &str,
    model: &str,
) -> crate::sse::SseAction {
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
                let cached_input_tokens = usage.cached_read_tokens();
                let provider_cost_usd = extract_provider_cost_usd(&usage);
                if provider == "openrouter" && provider_cost_usd.is_none() {
                    let raw_usage = serde_json::from_str::<serde_json::Value>(data)
                        .ok()
                        .and_then(|v| v.get("usage").cloned())
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "<unparseable>".to_string());
                    tracing::warn!(
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
                    cache_creation_tokens: usage.cache_creation_tokens(),
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
                    if choice.finish_reason.as_deref() == Some("length") {
                        events.push(StreamEvent::Truncated);
                    }
                    if choice.finish_reason.as_deref() == Some("tool_calls") {
                        events.push(StreamEvent::ToolIntent);
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
            tracing::warn!("Failed to parse SSE chunk: {e}");
            SseAction::Continue
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn parse_anthropic_sse_line(
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
                if stop_reason == "tool_use" {
                    events.push(StreamEvent::ToolIntent);
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
            let err_type = event["error"]["type"].as_str().unwrap_or("");
            let err_msg = event["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            let reason = classify_anthropic_stream_error(err_type, err_msg);
            let source = anyhow::anyhow!("Anthropic stream error ({err_type}): {err_msg}");
            let llm_err = match reason {
                FailoverReason::Auth
                | FailoverReason::AuthPermanent
                | FailoverReason::Billing
                | FailoverReason::ModelNotFound
                | FailoverReason::PayloadTooLarge
                | FailoverReason::Format => LlmError::Fatal {
                    source,
                    reason,
                    status: None,
                },
                _ => LlmError::Retryable {
                    source,
                    retry_after: None,
                    reason,
                    status: None,
                },
            };
            SseAction::Error(llm_err)
        }
        _ => SseAction::Continue,
    }
}

#[derive(Default)]
pub(crate) struct AnthropicStreamState {
    current_tool_index: usize,
    current_block_is_tool: bool,
    current_block_is_thinking: bool,
    pending_input_tokens: u64,
    pending_cached_input_tokens: u64,
    pending_cache_creation_tokens: u64,
}

#[derive(Default)]
pub(crate) struct BedrockStreamState {
    current_tool_index: usize,
    current_block_is_tool: bool,
}

struct OpenAiCompatTransport;

impl Transport for OpenAiCompatTransport {
    fn protocol_name(&self) -> &'static str {
        "openai-compat"
    }

    fn debug_request_label(&self) -> &'static str {
        "request_openai"
    }

    fn endpoint(&self, request: &TransportRequest<'_>, _stream: bool) -> Result<String> {
        Ok(match request.provider {
            Provider::Gemini => super::normalize_gemini_base(
                request
                    .llm_config
                    .base_url
                    .as_deref()
                    .unwrap_or(request.provider.base_url()),
            )
            .into_owned(),
            _ => request
                .llm_config
                .base_url
                .clone()
                .unwrap_or_else(|| request.provider.base_url().to_string()),
        })
    }

    fn build_streaming_request(
        &self,
        request: &TransportRequest<'_>,
    ) -> std::result::Result<serde_json::Value, LlmError> {
        build_openai_request(request, true).map_err(|e| LlmError::Fatal {
            source: anyhow::anyhow!("serialize OpenAI ChatRequest for cache injection: {e}"),
            reason: FailoverReason::Unknown,
            status: None,
        })
    }

    fn build_chat_request(&self, request: &TransportRequest<'_>) -> Result<serde_json::Value> {
        build_openai_request(request, false)
            .context("serialize OpenAI ChatRequest for cache injection")
    }

    fn new_stream_state(&self) -> StreamState {
        StreamState::OpenAiCompat
    }

    fn parse_stream_line(
        &self,
        line: &str,
        provider: &str,
        model: &str,
        _state: &mut StreamState,
    ) -> crate::sse::SseAction {
        parse_openai_sse_line(line, provider, model)
    }

    fn parse_chat_response(
        &self,
        response: serde_json::Value,
        provider: &str,
        model: &str,
    ) -> Result<TransportResponse> {
        #[derive(serde::Deserialize)]
        struct ChatResponse {
            #[serde(default)]
            choices: Vec<ChatChoice>,
            #[serde(default)]
            usage: Option<StreamUsage>,
        }

        #[derive(serde::Deserialize)]
        struct ChatChoice {
            message: Message,
        }

        let resp: ChatResponse = serde_json::from_value(response)?;
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
            cached_input_tokens: usage.cached_read_tokens(),
            cache_creation_tokens: usage.cache_creation_tokens(),
            provider: provider.to_string(),
            model: model.to_string(),
            provider_cost_usd: extract_provider_cost_usd(&usage),
        });

        Ok(TransportResponse { message, usage })
    }
}

struct AnthropicTransport;

impl Transport for AnthropicTransport {
    fn protocol_name(&self) -> &'static str {
        "anthropic"
    }

    fn debug_request_label(&self) -> &'static str {
        "request_anthropic"
    }

    fn endpoint(&self, request: &TransportRequest<'_>, _stream: bool) -> Result<String> {
        Ok(request
            .llm_config
            .base_url
            .clone()
            .unwrap_or_else(|| request.provider.base_url().to_string()))
    }

    fn build_streaming_request(
        &self,
        request: &TransportRequest<'_>,
    ) -> std::result::Result<serde_json::Value, LlmError> {
        Ok(build_anthropic_request(request, true))
    }

    fn build_chat_request(&self, request: &TransportRequest<'_>) -> Result<serde_json::Value> {
        Ok(build_anthropic_request(request, false))
    }

    fn new_stream_state(&self) -> StreamState {
        StreamState::Anthropic(AnthropicStreamState::default())
    }

    fn parse_stream_line(
        &self,
        line: &str,
        provider: &str,
        model: &str,
        state: &mut StreamState,
    ) -> crate::sse::SseAction {
        let StreamState::Anthropic(state) = state else {
            return crate::sse::SseAction::Continue;
        };

        parse_anthropic_sse_line(
            line,
            provider,
            model,
            &mut state.current_tool_index,
            &mut state.current_block_is_tool,
            &mut state.current_block_is_thinking,
            &mut state.pending_input_tokens,
            &mut state.pending_cached_input_tokens,
            &mut state.pending_cache_creation_tokens,
        )
    }

    fn parse_chat_response(
        &self,
        response: serde_json::Value,
        provider: &str,
        model: &str,
    ) -> Result<TransportResponse> {
        let message = parse_anthropic_response(&response)?;
        let usage = response["usage"].as_object().map(|usage| {
            let usage_value = serde_json::Value::Object(usage.clone());
            let (cached_input_tokens, cache_creation_tokens) = normalize_cache_tokens(&usage_value);
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
                provider: provider.to_string(),
                model: model.to_string(),
                provider_cost_usd: None,
            }
        });

        Ok(TransportResponse { message, usage })
    }
}

struct BedrockTransport;

impl Transport for BedrockTransport {
    fn protocol_name(&self) -> &'static str {
        "bedrock"
    }

    fn debug_request_label(&self) -> &'static str {
        "request_bedrock"
    }

    fn stream_protocol(&self) -> StreamProtocol {
        StreamProtocol::AwsEventStream
    }

    fn endpoint(&self, request: &TransportRequest<'_>, stream: bool) -> Result<String> {
        let region =
            crate::llm::bedrock::resolve_region(request.llm_config.bedrock_region.as_deref())
                .unwrap_or_else(|| "region-unresolved".to_string());
        let base = crate::llm::bedrock::resolve_endpoint_base(
            &region,
            request.llm_config.bedrock_endpoint_url.as_deref(),
            request.llm_config.bedrock_use_fips,
        )?;
        Ok(url_for_model(&base, request.model, stream))
    }

    fn build_streaming_request(
        &self,
        request: &TransportRequest<'_>,
    ) -> std::result::Result<serde_json::Value, LlmError> {
        Ok(build_bedrock_request(request))
    }

    fn build_chat_request(&self, request: &TransportRequest<'_>) -> Result<serde_json::Value> {
        Ok(build_bedrock_request(request))
    }

    fn new_stream_state(&self) -> StreamState {
        StreamState::Bedrock(BedrockStreamState::default())
    }

    fn parse_stream_line(
        &self,
        _line: &str,
        _provider: &str,
        _model: &str,
        _state: &mut StreamState,
    ) -> crate::sse::SseAction {
        crate::sse::SseAction::Continue
    }

    fn parse_stream_payload(
        &self,
        payload: &[u8],
        provider: &str,
        model: &str,
        state: &mut StreamState,
    ) -> crate::sse::SseAction {
        let StreamState::Bedrock(state) = state else {
            return crate::sse::SseAction::Continue;
        };
        parse_bedrock_event_payload(payload, provider, model, state)
    }

    fn parse_chat_response(
        &self,
        response: serde_json::Value,
        provider: &str,
        model: &str,
    ) -> Result<TransportResponse> {
        parse_bedrock_chat_response(response, provider, model)
    }
}

struct VertexAnthropicTransport;

impl Transport for VertexAnthropicTransport {
    fn protocol_name(&self) -> &'static str {
        "vertex-anthropic"
    }

    fn debug_request_label(&self) -> &'static str {
        "request_vertex_anthropic"
    }

    fn endpoint(&self, request: &TransportRequest<'_>, stream: bool) -> Result<String> {
        if let Some(base_url) = request
            .llm_config
            .base_url
            .as_ref()
            .filter(|s| !s.is_empty())
        {
            return Ok(base_url.clone());
        }
        crate::llm::vertex::compose_anthropic_url(request.llm_config, request.model, stream)
    }

    fn build_streaming_request(
        &self,
        request: &TransportRequest<'_>,
    ) -> std::result::Result<serde_json::Value, LlmError> {
        Ok(build_vertex_anthropic_request(request, true))
    }

    fn build_chat_request(&self, request: &TransportRequest<'_>) -> Result<serde_json::Value> {
        Ok(build_vertex_anthropic_request(request, false))
    }

    fn new_stream_state(&self) -> StreamState {
        StreamState::Anthropic(AnthropicStreamState::default())
    }

    fn parse_stream_line(
        &self,
        line: &str,
        provider: &str,
        model: &str,
        state: &mut StreamState,
    ) -> crate::sse::SseAction {
        let StreamState::Anthropic(state) = state else {
            return crate::sse::SseAction::Continue;
        };

        parse_anthropic_sse_line(
            line,
            provider,
            model,
            &mut state.current_tool_index,
            &mut state.current_block_is_tool,
            &mut state.current_block_is_thinking,
            &mut state.pending_input_tokens,
            &mut state.pending_cached_input_tokens,
            &mut state.pending_cache_creation_tokens,
        )
    }

    fn parse_chat_response(
        &self,
        response: serde_json::Value,
        provider: &str,
        model: &str,
    ) -> Result<TransportResponse> {
        ANTHROPIC_TRANSPORT.parse_chat_response(response, provider, model)
    }
}

pub(crate) fn build_openai_request(
    request: &TransportRequest<'_>,
    stream: bool,
) -> serde_json::Result<serde_json::Value> {
    let reasoning_effort = request
        .llm_config
        .effective_thinking(request.model)
        .openai_reasoning_effort()
        .map(String::from);
    // xAI rejects several JSON-schema keywords that OpenAI/Anthropic accept
    // (`minLength`, `maxLength`, `minItems`, `maxItems`, `minContains`,
    // `maxContains`, and top-level `strict`). Strip them from the per-tool
    // parameter schema before the request hits the wire — otherwise the
    // model returns 400 for any tool that uses string/array bounds.
    let tools = request.tools.map(|ts| {
        let mut owned: Vec<ToolDefinition> = ts.to_vec();
        if matches!(request.provider, Provider::XAi) {
            for tool in &mut owned {
                sanitize_xai_tool_schema(&mut tool.function.parameters);
            }
        }
        owned
    });
    let chat_request = ChatRequest {
        model: request.model.to_string(),
        // Sanitize null-content/empty messages, then repair replayed/resumed
        // histories (duplicate tool_call_ids, orphaned tool results,
        // consecutive assistant turns) that strict providers 400 on. A no-op
        // on well-formed live-loop histories. Both the streaming and
        // non-streaming paths need this: non-streaming carries summarization,
        // title generation, and aux-model calls, and a resumed session with a
        // stub message would otherwise 400 exactly when it tries to compact.
        messages: repair_message_sequence(&sanitize_openai_messages(request.messages)),
        tools,
        temperature: request.llm_config.temperature,
        // Gemini bills thought tokens against the output cap on this wire too,
        // so a Gemini slug reached through OpenRouter or a compat relay needs
        // the same raise the native path applies — otherwise thinking consumes
        // the whole allowance and the turn comes back empty with no error.
        // A no-op for every other model.
        max_tokens: super::gemini::openai_compat_max_tokens(request.model, request.llm_config),
        stream,
        reasoning_effort,
        prompt_cache_key: request.prompt_cache_key.map(str::to_string),
        user: request.prompt_cache_key.map(str::to_string),
        // OpenRouter-only sticky routing key; other OpenAI-compatible hosts
        // could reject or misread an unknown `session_id` field.
        session_id: (request.provider == Provider::OpenRouter)
            .then(|| request.prompt_cache_key.map(str::to_string))
            .flatten(),
        stream_options: stream.then_some(StreamOptions {
            include_usage: true,
        }),
        usage: (request.provider == Provider::OpenRouter && request.cost_events_enabled)
            .then_some(UsageOptions { include: true }),
    };

    let mut body = serde_json::to_value(&chat_request)?;
    if request.llm_config.cache.enabled
        && supports_anthropic_cache_control(request.provider, request.model)
        && request.provider != Provider::Anthropic
    {
        apply_openai_cache_markers(
            &mut body,
            &request.llm_config.cache,
            request.system_stable_prefix,
        );
    }
    // OpenAI's reasoning families (o-series, gpt-5*) replaced `max_tokens` with
    // `max_completion_tokens` on the Chat Completions wire. Rewrite at the JSON
    // boundary so the in-Rust `ChatRequest` struct stays single-named while the
    // wire shape matches the endpoint. Match is model-driven (registry slug),
    // not provider-driven, because the same slug routed through any host that
    // proxies to api.openai.com (OpenRouter `openai/o3`, Azure deployments
    // named after reasoning models) needs the same field name.
    if super::model_registry::uses_max_completion_tokens(request.model) {
        if let Some(obj) = body.as_object_mut() {
            if let Some(v) = obj.remove("max_tokens") {
                obj.insert("max_completion_tokens".to_string(), v);
            }
        }
    }
    Ok(body)
}

/// JSON-schema keywords xAI's tool-call validator rejects with HTTP 400.
/// Captured against `https://api.x.ai/v1/chat/completions` 2026-05-24 — every
/// keyword here has been confirmed to produce a "schema not supported" error
/// when present in a tool's `parameters`. OpenAI/Anthropic schemas that use
/// these keywords reach xAI unchanged via this filter.
const XAI_UNSUPPORTED_SCHEMA_KEYWORDS: &[&str] = &[
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "minContains",
    "maxContains",
];

/// Strip JSON-schema keywords that xAI rejects from a tool's parameter
/// schema. Walks the schema recursively (properties, items, anyOf/oneOf/allOf,
/// definitions/$defs). Top-level `strict: true` — emitted by tools targeting
/// OpenAI's structured-outputs flag — is also removed so the same tool
/// definition works on both endpoints.
///
/// Legitimate keywords (`type`, `properties`, `required`, `enum`, `description`,
/// `additionalProperties`, …) are left intact.
fn sanitize_xai_tool_schema(schema: &mut serde_json::Value) {
    match schema {
        serde_json::Value::Object(map) => {
            for kw in XAI_UNSUPPORTED_SCHEMA_KEYWORDS {
                map.remove(*kw);
            }
            // `strict: true` is OpenAI's structured-outputs flag; xAI rejects
            // it at the tool-definition level. Only strip the boolean — leave
            // alternate uses (e.g. a property literally named "strict") alone.
            if matches!(map.get("strict"), Some(serde_json::Value::Bool(_))) {
                map.remove("strict");
            }
            for (_k, v) in map.iter_mut() {
                sanitize_xai_tool_schema(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                sanitize_xai_tool_schema(v);
            }
        }
        _ => {}
    }
}

fn parse_usage_cost(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn build_bedrock_request(request: &TransportRequest<'_>) -> serde_json::Value {
    let mut bedrock_messages = Vec::new();
    let mut system_blocks = Vec::new();

    for message in request.messages {
        match message.role {
            Role::System => {
                if let Some(text) = message.text_content() {
                    // Converse rejects a blank system block just as it does a
                    // blank message block.
                    if !text.trim().is_empty() {
                        system_blocks.push(serde_json::json!({ "text": text }));
                    }
                }
            }
            Role::User => {
                let mut content = Vec::new();
                append_bedrock_content(&mut content, message);
                // Converse rejects a message whose content array is empty.
                // The assistant arm below has always guarded this; the user
                // arm reaches it too once blank text is filtered out.
                if content.is_empty() {
                    content.push(serde_json::json!({
                        "text": crate::llm::anthropic::EMPTY_ASSISTANT_PLACEHOLDER,
                    }));
                }
                bedrock_messages.push(serde_json::json!({
                    "role": "user",
                    "content": content,
                }));
            }
            Role::Assistant => {
                let mut content = Vec::new();
                append_bedrock_content(&mut content, message);
                if content.is_empty() {
                    content.push(serde_json::json!({
                        "text": crate::llm::anthropic::EMPTY_ASSISTANT_PLACEHOLDER,
                    }));
                }
                bedrock_messages.push(serde_json::json!({
                    "role": "assistant",
                    "content": content,
                }));
            }
            Role::Tool => {
                // Bedrock Converse toolResult.content accepts {text:...} and {image:...}
                // blocks. Multimodal tool results (e.g. computer_use screenshots)
                // need the image blocks so they reach the model intact.
                let mut tool_result_content: Vec<serde_json::Value> = Vec::new();
                match &message.content {
                    Some(crate::types::MessageContent::Parts(parts)) => {
                        for part in parts {
                            match part {
                                crate::types::ContentPart::Text(text) => {
                                    if !text.is_empty() {
                                        tool_result_content
                                            .push(serde_json::json!({ "text": text }));
                                    }
                                }
                                crate::types::ContentPart::ImageBase64 { media } => {
                                    // Bedrock Converse only accepts {png,jpeg,gif,webp}.
                                    // Falling through to a text block for anything else
                                    // keeps the request well-formed instead of letting
                                    // the API reject it.
                                    let fmt = match media.mime_type.as_str() {
                                        "image/png" => Some("png"),
                                        "image/jpeg" | "image/jpg" => Some("jpeg"),
                                        "image/gif" => Some("gif"),
                                        "image/webp" => Some("webp"),
                                        _ => None,
                                    };
                                    match fmt {
                                        Some(fmt) => tool_result_content.push(serde_json::json!({
                                            "image": {
                                                "format": fmt,
                                                "source": { "bytes": media.data },
                                            }
                                        })),
                                        None => tool_result_content.push(serde_json::json!({
                                            "text": format!("[image omitted: unsupported format {}]", media.mime_type)
                                        })),
                                    }
                                }
                                crate::types::ContentPart::ImageUrl { url } => {
                                    tool_result_content.push(serde_json::json!({
                                        "text": format!("[image: {url}]")
                                    }));
                                }
                                crate::types::ContentPart::AudioBase64 { media } => {
                                    tool_result_content.push(serde_json::json!({
                                        "text": format!("[audio: {}]", media.filename.as_deref().unwrap_or("audio"))
                                    }));
                                }
                                crate::types::ContentPart::VideoBase64 { media } => {
                                    tool_result_content.push(serde_json::json!({
                                        "text": format!("[video: {}]", media.filename.as_deref().unwrap_or("video"))
                                    }));
                                }
                            }
                        }
                    }
                    _ => {
                        let text = message.text_content().unwrap_or("");
                        if !text.trim().is_empty() {
                            tool_result_content.push(serde_json::json!({ "text": text }));
                        }
                    }
                }
                if tool_result_content.is_empty() {
                    tool_result_content.push(serde_json::json!({
                        "text": crate::llm::anthropic::EMPTY_ASSISTANT_PLACEHOLDER,
                    }));
                }
                bedrock_messages.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "toolResult": {
                            "toolUseId": message.tool_call_id.as_deref().unwrap_or(""),
                            "content": tool_result_content,
                        }
                    }],
                }));
            }
        }
    }

    let mut body = serde_json::json!({
        "messages": bedrock_messages,
        "inferenceConfig": {
            "temperature": request.llm_config.temperature,
            "maxTokens": request.llm_config.max_tokens,
        },
    });
    if !system_blocks.is_empty() {
        body["system"] = serde_json::json!(system_blocks);
    }
    if let Some(tools) = request.tools {
        let tool_specs: Vec<serde_json::Value> = tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "toolSpec": {
                        "name": tool.function.name,
                        "description": tool.function.description,
                        "inputSchema": {
                            "json": tool.function.parameters,
                        },
                    }
                })
            })
            .collect();
        if !tool_specs.is_empty() {
            body["toolConfig"] = serde_json::json!({ "tools": tool_specs });
        }
    }
    body
}

fn build_vertex_anthropic_request(
    request: &TransportRequest<'_>,
    stream: bool,
) -> serde_json::Value {
    let mut body = build_anthropic_request(request, stream);
    if let Some(obj) = body.as_object_mut() {
        obj.remove("model");
        obj.insert(
            "anthropic_version".to_string(),
            serde_json::json!("vertex-2023-10-16"),
        );
    }
    body
}

fn append_bedrock_content(content: &mut Vec<serde_json::Value>, message: &Message) {
    match &message.content {
        Some(crate::types::MessageContent::Text(text)) => {
            if !text.trim().is_empty() {
                content.push(serde_json::json!({ "text": text }));
            }
        }
        Some(crate::types::MessageContent::Parts(parts)) => {
            for part in parts {
                match part {
                    crate::types::ContentPart::Text(text) => {
                        if !text.trim().is_empty() {
                            content.push(serde_json::json!({ "text": text }));
                        }
                    }
                    crate::types::ContentPart::ImageBase64 { media } => {
                        content.push(serde_json::json!({
                            "image": {
                                "format": media.mime_type.split('/').next_back().unwrap_or("png"),
                                "source": { "bytes": media.data },
                            }
                        }));
                    }
                    crate::types::ContentPart::ImageUrl { url } => {
                        content.push(serde_json::json!({ "text": format!("[image: {url}]") }));
                    }
                    crate::types::ContentPart::AudioBase64 { media } => {
                        content.push(serde_json::json!({
                            "text": format!("[audio: {}]", media.filename.as_deref().unwrap_or("audio"))
                        }));
                    }
                    crate::types::ContentPart::VideoBase64 { media } => {
                        content.push(serde_json::json!({
                            "text": format!("[video: {}]", media.filename.as_deref().unwrap_or("video"))
                        }));
                    }
                }
            }
        }
        None => {}
    }
    if let Some(tool_calls) = &message.tool_calls {
        for tool_call in tool_calls {
            let input = serde_json::from_str::<serde_json::Value>(&tool_call.function.arguments)
                .unwrap_or_else(|_| serde_json::json!({}));
            content.push(serde_json::json!({
                "toolUse": {
                    "toolUseId": tool_call.id,
                    "name": tool_call.function.name,
                    "input": input,
                }
            }));
        }
    }
}

fn parse_bedrock_event_payload(
    payload: &[u8],
    provider: &str,
    model: &str,
    state: &mut BedrockStreamState,
) -> crate::sse::SseAction {
    use crate::sse::SseAction;

    let event = match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(event) => event,
        Err(_) => return SseAction::Continue,
    };

    if let Some(start) = event.get("contentBlockStart").and_then(|v| v.get("start")) {
        if let Some(tool_use) = start.get("toolUse") {
            state.current_block_is_tool = true;
            return SseAction::Emit(vec![StreamEvent::ToolCallDelta {
                index: state.current_tool_index,
                id: tool_use
                    .get("toolUseId")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                name: tool_use
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                arguments_delta: String::new(),
            }]);
        }
        state.current_block_is_tool = false;
    }

    if let Some(delta) = event.get("contentBlockDelta").and_then(|v| v.get("delta")) {
        if let Some(text) = delta.get("text").and_then(serde_json::Value::as_str) {
            return SseAction::Emit(vec![StreamEvent::TextDelta(text.to_string())]);
        }
        if let Some(tool_use) = delta.get("toolUse") {
            let arguments_delta = match tool_use.get("input") {
                Some(serde_json::Value::String(text)) => text.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            if !arguments_delta.is_empty() {
                return SseAction::Emit(vec![StreamEvent::ToolCallDelta {
                    index: state.current_tool_index,
                    id: None,
                    name: None,
                    arguments_delta,
                }]);
            }
        }
    }

    if event.get("contentBlockStop").is_some() && state.current_block_is_tool {
        state.current_tool_index += 1;
        state.current_block_is_tool = false;
    }

    if let Some(metadata) = event.get("metadata") {
        let usage = metadata.get("usage").cloned().unwrap_or_default();
        let input = usage
            .get("inputTokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let output = usage
            .get("outputTokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if input > 0 || output > 0 {
            let (cached_input_tokens, cache_creation_tokens) = normalize_cache_tokens(&usage);
            return SseAction::Emit(vec![StreamEvent::Usage(UsageData {
                prompt_tokens: input,
                completion_tokens: output,
                total_tokens: usage
                    .get("totalTokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(input + output),
                cached_input_tokens,
                cache_creation_tokens,
                provider: provider.to_string(),
                model: model.to_string(),
                provider_cost_usd: None,
            })]);
        }
    }

    if let Some(stop) = event.get("messageStop") {
        let mut events = Vec::new();
        if stop.get("stopReason").and_then(serde_json::Value::as_str) == Some("max_tokens") {
            events.push(StreamEvent::Truncated);
        }
        events.push(StreamEvent::Done);
        return SseAction::Done(events);
    }

    if let Some(error) = parse_bedrock_event_error(&event) {
        return SseAction::Error(error);
    }

    SseAction::Continue
}

fn parse_bedrock_event_error(event: &serde_json::Value) -> Option<LlmError> {
    let object = event.as_object()?;
    let (error_type, payload) = object.iter().find(|(key, _)| key.ends_with("Exception"))?;
    let message = payload
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown Bedrock stream error");
    let reason = match error_type.as_str() {
        "accessDeniedException" => FailoverReason::AuthPermanent,
        "resourceNotFoundException" => FailoverReason::ModelNotFound,
        "validationException" => FailoverReason::Format,
        "throttlingException" => FailoverReason::RateLimit,
        "serviceUnavailableException" | "internalServerException" => FailoverReason::Overloaded,
        "modelTimeoutException" => FailoverReason::Timeout,
        _ => FailoverReason::Unknown,
    };
    let source = anyhow::anyhow!("Bedrock stream error ({error_type}): {message}");
    Some(match reason {
        FailoverReason::Auth
        | FailoverReason::AuthPermanent
        | FailoverReason::Billing
        | FailoverReason::ModelNotFound
        | FailoverReason::PayloadTooLarge
        | FailoverReason::Format => LlmError::Fatal {
            source,
            reason,
            status: None,
        },
        _ => LlmError::Retryable {
            source,
            retry_after: None,
            reason,
            status: None,
        },
    })
}

fn parse_bedrock_chat_response(
    response: serde_json::Value,
    provider: &str,
    model: &str,
) -> Result<TransportResponse> {
    let message_json = response
        .get("output")
        .and_then(|v| v.get("message"))
        .cloned()
        .context("Bedrock response missing output.message")?;
    let message = parse_bedrock_message(&message_json)?;
    let usage_json = response.get("usage").cloned().unwrap_or_default();
    let input = usage_json
        .get("inputTokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let output = usage_json
        .get("outputTokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let (cached_input_tokens, cache_creation_tokens) = normalize_cache_tokens(&usage_json);
    let usage = (input > 0 || output > 0).then_some(UsageData {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: usage_json
            .get("totalTokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(input + output),
        cached_input_tokens,
        cache_creation_tokens,
        provider: provider.to_string(),
        model: model.to_string(),
        provider_cost_usd: None,
    });

    Ok(TransportResponse { message, usage })
}

fn parse_bedrock_message(message_json: &serde_json::Value) -> Result<Message> {
    let role = match message_json
        .get("role")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("assistant")
    {
        "user" => Role::User,
        "tool" => Role::Tool,
        "system" => Role::System,
        _ => Role::Assistant,
    };
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    for block in message_json
        .get("content")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(text) = block.get("text").and_then(serde_json::Value::as_str) {
            text_parts.push(text.to_string());
        }
        if let Some(tool_use) = block.get("toolUse") {
            let id = tool_use
                .get("toolUseId")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = tool_use
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let input = tool_use
                .get("input")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            tool_calls.push(crate::types::ToolCall {
                id,
                call_type: "function".to_string(),
                function: crate::types::FunctionCall {
                    name,
                    arguments: input.to_string(),
                },
            });
        }
    }

    Ok(Message {
        role,
        content: Some(crate::types::MessageContent::Text(text_parts.join(""))),
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        tool_call_id: None,
        timestamp: None,
        seq: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_llm_config() -> &'static LlmConfig {
        static CFG: std::sync::LazyLock<LlmConfig> = std::sync::LazyLock::new(LlmConfig::default);
        &CFG
    }

    fn bedrock_request<'a>(messages: &'a [Message]) -> TransportRequest<'a> {
        TransportRequest {
            provider: Provider::Bedrock,
            model: "anthropic.claude-3-7-sonnet-20250219-v1:0",
            messages,
            tools: None,
            llm_config: default_llm_config(),
            prompt_cache_key: None,
            system_stable_prefix: None,
            cost_events_enabled: false,
        }
    }

    /// An Anthropic request whose system prompt is `stable` + `dynamic`, with
    /// `prefix` declared as the cacheable head.
    fn anthropic_system_request<'a>(
        messages: &'a [Message],
        prefix: Option<&'a str>,
    ) -> TransportRequest<'a> {
        TransportRequest {
            provider: Provider::Anthropic,
            model: "claude-opus-5",
            messages,
            tools: None,
            llm_config: default_llm_config(),
            prompt_cache_key: None,
            system_stable_prefix: prefix,
            cost_events_enabled: false,
        }
    }

    fn system_blocks(body: &serde_json::Value) -> &Vec<serde_json::Value> {
        body["system"]
            .as_array()
            .expect("system must be an array of text blocks")
    }

    /// Count every `cache_control` in the request. Anthropic's hard limit is 4,
    /// and the default layout already spends all of them (tools + system + 2
    /// rolling) — so splitting the system block must MOVE its breakpoint, never
    /// add one.
    fn breakpoint_count(body: &serde_json::Value) -> usize {
        fn walk(v: &serde_json::Value) -> usize {
            match v {
                serde_json::Value::Object(map) => map
                    .iter()
                    .map(|(k, v)| usize::from(k == "cache_control") + walk(v))
                    .sum(),
                serde_json::Value::Array(items) => items.iter().map(walk).sum(),
                _ => 0,
            }
        }
        walk(body)
    }

    /// The bug this split exists to fix: the system prompt's dynamic tail
    /// carries a second-resolution clock, so marking the whole prompt produces a
    /// block that is byte-different every turn and can never hit the cache. The
    /// marked block must be the stable head only.
    #[test]
    fn anthropic_system_cache_block_is_stable_across_a_clock_tick() {
        let stable = "<system_instructions>\nidentity\n</system_instructions>\n\n";
        let turn_one = format!("{stable}<environment>\nCurrent Time: 12:00:01\n</environment>\n");
        let turn_two = format!("{stable}<environment>\nCurrent Time: 12:00:02\n</environment>\n");

        let m1 = vec![Message::system(&turn_one), Message::user("hi")];
        let m2 = vec![Message::system(&turn_two), Message::user("hi")];

        let b1 = build_anthropic_request(&anthropic_system_request(&m1, Some(stable)), false);
        let b2 = build_anthropic_request(&anthropic_system_request(&m2, Some(stable)), false);

        let (s1, s2) = (system_blocks(&b1), system_blocks(&b2));
        assert_eq!(s1.len(), 2, "system must split into prefix + suffix");
        assert_eq!(
            s1[0], s2[0],
            "the cached block must be byte-identical across turns"
        );
        assert_eq!(s1[0]["text"], stable);
        assert!(s1[0].get("cache_control").is_some());

        // The suffix carries the volatile content and must NOT be marked.
        assert!(s1[1].get("cache_control").is_none());
        assert_ne!(s1[1], s2[1], "the clock lives in the unmarked suffix");

        // Reassembling the blocks must reproduce the prompt exactly — a split
        // that drops or duplicates a byte would silently corrupt the prompt.
        let rejoined = format!(
            "{}{}",
            s1[0]["text"].as_str().unwrap(),
            s1[1]["text"].as_str().unwrap()
        );
        assert_eq!(rejoined, turn_one);
    }

    /// Splitting relocates the system breakpoint; it must not spend a second one.
    #[test]
    fn anthropic_system_split_does_not_spend_an_extra_breakpoint() {
        let stable = "stable head\n";
        let system = format!("{stable}volatile tail");
        let messages = vec![Message::system(&system), Message::user("hi")];

        let unsplit = build_anthropic_request(&anthropic_system_request(&messages, None), false);
        let split =
            build_anthropic_request(&anthropic_system_request(&messages, Some(stable)), false);

        assert_eq!(system_blocks(&unsplit).len(), 1);
        assert_eq!(system_blocks(&split).len(), 2);
        assert_eq!(
            breakpoint_count(&split),
            breakpoint_count(&unsplit),
            "split must move the system breakpoint, not add one"
        );
        assert!(
            breakpoint_count(&split) <= 4,
            "Anthropic allows at most 4 cache breakpoints"
        );
    }

    /// The split slices at `prefix.len()`, a byte offset. Multi-byte content
    /// either side of the boundary must not panic or corrupt the text — and a
    /// *mismatched* prefix whose byte length lands mid-codepoint in the system
    /// prompt must be rejected by `starts_with` before any slicing happens.
    #[test]
    fn anthropic_system_split_is_safe_across_multibyte_boundaries() {
        // Prefix ends with a 4-byte emoji; suffix opens with 3-byte CJK.
        let stable = "identity 🙂\n";
        let system = format!("{stable}環境 Current Time: 12:00:01");
        let messages = vec![Message::system(&system), Message::user("hi")];

        let body =
            build_anthropic_request(&anthropic_system_request(&messages, Some(stable)), false);
        let blocks = system_blocks(&body);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"], stable);
        assert_eq!(
            format!(
                "{}{}",
                blocks[0]["text"].as_str().unwrap(),
                blocks[1]["text"].as_str().unwrap()
            ),
            system,
            "multi-byte split must round-trip exactly"
        );

        // A non-matching prefix whose length (10 bytes) falls inside the leading
        // 4-byte emoji of `system`. Slicing there would panic; `starts_with`
        // must reject it first.
        let emoji_system = "🙂🙂🙂 rest of prompt";
        let emoji_messages = vec![Message::system(emoji_system), Message::user("hi")];
        let mismatched = "0123456789";
        assert!(!emoji_system.is_char_boundary(mismatched.len()));

        let body = build_anthropic_request(
            &anthropic_system_request(&emoji_messages, Some(mismatched)),
            false,
        );
        let blocks = system_blocks(&body);
        assert_eq!(blocks.len(), 1, "mismatched prefix must not split");
        assert_eq!(blocks[0]["text"], emoji_system);
    }

    /// A prefix that no longer matches the prompt (stale client, reused aux
    /// client) must degrade to the single-block shape rather than slicing at a
    /// meaningless offset.
    #[test]
    fn anthropic_system_falls_back_to_one_block_on_prefix_mismatch() {
        let system = "a completely different system prompt";
        let messages = vec![Message::system(system), Message::user("hi")];

        for prefix in [
            Some("prefix from an earlier turn"), // no longer matches
            Some(""),                            // empty
            Some(system),                        // whole prompt: suffix would be empty
            None,                                // never declared
        ] {
            let body = build_anthropic_request(&anthropic_system_request(&messages, prefix), false);
            let blocks = system_blocks(&body);
            assert_eq!(blocks.len(), 1, "prefix {prefix:?} must not split");
            assert_eq!(blocks[0]["text"], system);
            assert!(blocks[0].get("cache_control").is_some());
        }
    }

    /// Collect every `{"text": …}` string value anywhere in the body, so the
    /// assertion can't miss a blank block by only checking the shapes the
    /// test author happened to think of.
    fn collect_text_values(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, val) in map {
                    if k == "text" {
                        if let Some(s) = val.as_str() {
                            out.push(s.to_string());
                        }
                    }
                    collect_text_values(val, out);
                }
            }
            serde_json::Value::Array(arr) => {
                for item in arr {
                    collect_text_values(item, out);
                }
            }
            _ => {}
        }
    }

    /// Every `content` array in the body, so "message with zero content
    /// blocks" — the other shape Converse rejects — is also covered.
    fn collect_content_arrays<'a>(
        v: &'a serde_json::Value,
        out: &mut Vec<&'a Vec<serde_json::Value>>,
    ) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, val) in map {
                    if k == "content" {
                        if let Some(arr) = val.as_array() {
                            out.push(arr);
                        }
                    }
                    collect_content_arrays(val, out);
                }
            }
            serde_json::Value::Array(arr) => {
                for item in arr {
                    collect_content_arrays(item, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn bedrock_request_never_emits_blank_text_block() {
        // Converse rejects a text block that is present but whitespace-only,
        // and rejects a message whose content array is empty. Both shapes
        // were reachable here: blank user/assistant/tool text passed the
        // `is_empty` guard, and the empty-content repair pushed a literal "".
        let mut assistant_with_calls = Message::assistant("   ");
        assistant_with_calls.tool_calls = Some(vec![crate::types::ToolCall {
            id: "call_1".to_string(),
            call_type: "function".to_string(),
            function: crate::types::FunctionCall {
                name: "run_shell".to_string(),
                arguments: "{}".to_string(),
            },
        }]);

        let messages = vec![
            Message::system("  \n "),
            Message::user("   "),
            Message::assistant("\t\n"),
            assistant_with_calls,
            Message::tool_result("call_1", "  "),
        ];
        let body = build_bedrock_request(&bedrock_request(&messages));

        let mut texts = Vec::new();
        collect_text_values(&body, &mut texts);
        for t in &texts {
            assert!(
                !t.trim().is_empty(),
                "blank text block would 400 at Converse; body: {body}"
            );
        }

        let mut arrays = Vec::new();
        collect_content_arrays(&body, &mut arrays);
        for arr in arrays {
            assert!(
                !arr.is_empty(),
                "empty content array would 400 at Converse; body: {body}"
            );
        }
    }

    #[test]
    fn bedrock_assistant_with_blank_text_still_carries_tool_use() {
        // The blank text is dropped, but the tool call must survive — and no
        // placeholder should be injected, because the tool block already
        // makes the content array non-empty.
        let mut msg = Message::assistant("   ");
        msg.tool_calls = Some(vec![crate::types::ToolCall {
            id: "call_7".to_string(),
            call_type: "function".to_string(),
            function: crate::types::FunctionCall {
                name: "run_shell".to_string(),
                arguments: r#"{"command":"pwd"}"#.to_string(),
            },
        }]);
        let messages = vec![Message::user("go"), msg];
        let body = build_bedrock_request(&bedrock_request(&messages));

        let content = body["messages"][1]["content"]
            .as_array()
            .expect("assistant content array");
        assert_eq!(
            content.len(),
            1,
            "expected only the toolUse block, got: {content:?}"
        );
        assert_eq!(content[0]["toolUse"]["name"], "run_shell");
    }

    #[test]
    fn build_bedrock_request_maps_system_and_messages() {
        let messages = vec![
            Message::system("system prompt"),
            Message::user("hello"),
            Message::assistant("world"),
        ];
        let body = build_bedrock_request(&bedrock_request(&messages));
        assert_eq!(body["system"][0]["text"], "system prompt");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
        assert_eq!(body["messages"][1]["role"], "assistant");
    }

    #[test]
    fn build_bedrock_request_includes_tool_config_and_tool_result() {
        let messages = vec![
            Message::user("run pwd"),
            Message::tool_result("call_42", "/tmp"),
        ];
        let tools = vec![crate::types::ToolDefinition::new(
            "run_shell",
            "Execute a shell command",
            serde_json::json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
            }),
        )];
        let mut req = bedrock_request(&messages);
        req.tools = Some(&tools);

        let body = build_bedrock_request(&req);

        // Tool definitions map onto `toolConfig.tools[].toolSpec.inputSchema.json`.
        let spec = &body["toolConfig"]["tools"][0]["toolSpec"];
        assert_eq!(spec["name"], "run_shell");
        assert_eq!(spec["description"], "Execute a shell command");
        assert_eq!(spec["inputSchema"]["json"]["type"], "object");

        // Tool results travel under the `user` role with a `toolResult` block
        // referencing the originating tool-use id — Converse's contract.
        let tool_result_msg = &body["messages"][1];
        assert_eq!(tool_result_msg["role"], "user");
        let tool_result = &tool_result_msg["content"][0]["toolResult"];
        assert_eq!(tool_result["toolUseId"], "call_42");
        assert_eq!(tool_result["content"][0]["text"], "/tmp");
    }

    #[test]
    fn build_bedrock_request_carries_multimodal_tool_result_image_block() {
        // Invariant: a tool result with image parts must reach Bedrock as
        // `toolResult.content` with both text and image blocks. Flattening
        // to a single text block would lose the screenshot bytes that
        // computer_use/vision_analyze just paid to capture.
        use crate::types::{ContentPart, MediaData};

        let parts = vec![
            ContentPart::Text("captured".to_string()),
            ContentPart::ImageBase64 {
                media: MediaData {
                    mime_type: "image/png".to_string(),
                    data: "iVBORw0KGgo=".to_string(),
                    filename: None,
                },
            },
        ];
        let messages = vec![
            Message::user("screenshot"),
            Message::tool_result_multimodal("call_cap", parts),
        ];
        let body = build_bedrock_request(&bedrock_request(&messages));
        let tool_result = &body["messages"][1]["content"][0]["toolResult"];
        assert_eq!(tool_result["toolUseId"], "call_cap");
        let inner = tool_result["content"]
            .as_array()
            .expect("multimodal toolResult.content must be a block array");
        assert_eq!(inner.len(), 2);
        assert_eq!(inner[0]["text"], "captured");
        assert_eq!(inner[1]["image"]["format"], "png");
        assert_eq!(inner[1]["image"]["source"]["bytes"], "iVBORw0KGgo=");
    }

    #[test]
    fn bedrock_transport_endpoint_uses_setting_then_env_then_stream_suffix() {
        use crate::config::LlmConfig;
        // Setting wins; toggling `stream` flips the suffix.
        let mut cfg = LlmConfig::default();
        cfg.bedrock_region = Some("ap-southeast-2".to_string());
        let cfg_box: &'static LlmConfig = Box::leak(Box::new(cfg));
        let req = TransportRequest {
            provider: Provider::Bedrock,
            model: "anthropic.claude-3-5-haiku-20241022-v1:0",
            messages: &[],
            tools: None,
            llm_config: cfg_box,
            prompt_cache_key: None,
            system_stable_prefix: None,
            cost_events_enabled: false,
        };
        let transport = BedrockTransport;
        let stream_url = transport.endpoint(&req, true).expect("endpoint");
        assert!(
            stream_url.contains("bedrock-runtime.ap-southeast-2.amazonaws.com"),
            "setting region must win: {stream_url}"
        );
        assert!(
            stream_url.ends_with("/converse-stream"),
            "stream=true must select converse-stream: {stream_url}"
        );
        let unary_url = transport.endpoint(&req, false).expect("endpoint");
        assert!(
            unary_url.ends_with("/converse"),
            "stream=false must select converse: {unary_url}"
        );
    }

    #[test]
    fn bedrock_transport_endpoint_honors_fips_and_custom_override() {
        use crate::config::LlmConfig;
        // FIPS toggle selects the -fips host for a GovCloud region.
        let mut fips_cfg = LlmConfig::default();
        fips_cfg.bedrock_region = Some("us-gov-west-1".to_string());
        fips_cfg.bedrock_use_fips = true;
        let fips_box: &'static LlmConfig = Box::leak(Box::new(fips_cfg));
        let fips_req = TransportRequest {
            provider: Provider::Bedrock,
            model: "anthropic.claude-3-5-sonnet-20241022-v2:0",
            messages: &[],
            tools: None,
            llm_config: fips_box,
            prompt_cache_key: None,
            system_stable_prefix: None,
            cost_events_enabled: false,
        };
        let fips_url = BedrockTransport
            .endpoint(&fips_req, false)
            .expect("endpoint");
        assert_eq!(
            fips_url,
            "https://bedrock-runtime-fips.us-gov-west-1.amazonaws.com/model/anthropic.claude-3-5-sonnet-20241022-v2:0/converse",
            "FIPS toggle must select the -fips host"
        );

        // An explicit PrivateLink endpoint wins over FIPS derivation.
        let mut vpce_cfg = LlmConfig::default();
        vpce_cfg.bedrock_region = Some("us-gov-west-1".to_string());
        vpce_cfg.bedrock_use_fips = true;
        vpce_cfg.bedrock_endpoint_url =
            Some("vpce-0abc.bedrock-runtime.us-gov-west-1.vpce.amazonaws.com".to_string());
        let vpce_box: &'static LlmConfig = Box::leak(Box::new(vpce_cfg));
        let vpce_req = TransportRequest {
            provider: Provider::Bedrock,
            model: "anthropic.claude-3-5-sonnet-20241022-v2:0",
            messages: &[],
            tools: None,
            llm_config: vpce_box,
            prompt_cache_key: None,
            system_stable_prefix: None,
            cost_events_enabled: false,
        };
        let vpce_url = BedrockTransport
            .endpoint(&vpce_req, true)
            .expect("endpoint");
        assert_eq!(
            vpce_url,
            "https://vpce-0abc.bedrock-runtime.us-gov-west-1.vpce.amazonaws.com/model/anthropic.claude-3-5-sonnet-20241022-v2:0/converse-stream",
            "explicit endpoint override must win over FIPS derivation"
        );
    }

    #[test]
    fn parse_bedrock_chat_response_normalizes_usage() {
        let response = serde_json::json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{ "text": "done" }]
                }
            },
            "usage": {
                "inputTokens": 12,
                "outputTokens": 3,
                "totalTokens": 15,
                "cache_write_input_tokens": 2
            }
        });
        let parsed =
            parse_bedrock_chat_response(response, "bedrock", "anthropic.claude").expect("parse");
        assert_eq!(parsed.message.text_content(), Some("done"));
        let usage = parsed.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, 12);
        assert_eq!(usage.completion_tokens, 3);
        assert_eq!(usage.cache_creation_tokens, 2);
    }

    #[test]
    fn parse_bedrock_event_payload_emits_tool_and_usage() {
        let mut state = BedrockStreamState::default();
        let start = serde_json::json!({
            "contentBlockStart": {
                "start": {
                    "toolUse": {
                        "toolUseId": "tool_1",
                        "name": "run_shell"
                    }
                }
            }
        });
        let action = parse_bedrock_event_payload(
            start.to_string().as_bytes(),
            "bedrock",
            "anthropic.claude",
            &mut state,
        );
        match action {
            crate::sse::SseAction::Emit(events) => {
                assert!(matches!(
                    &events[0],
                    StreamEvent::ToolCallDelta { name: Some(name), .. } if name == "run_shell"
                ));
            }
            _ => panic!("expected tool event"),
        }

        let metadata = serde_json::json!({
            "metadata": {
                "usage": {
                    "inputTokens": 10,
                    "outputTokens": 4,
                    "totalTokens": 14
                }
            }
        });
        let action = parse_bedrock_event_payload(
            metadata.to_string().as_bytes(),
            "bedrock",
            "anthropic.claude",
            &mut state,
        );
        match action {
            crate::sse::SseAction::Emit(events) => {
                assert!(matches!(
                    &events[0],
                    StreamEvent::Usage(usage) if usage.total_tokens == 14
                ));
            }
            _ => panic!("expected usage event"),
        }
    }

    #[test]
    fn parse_bedrock_event_payload_classifies_stream_exceptions() {
        let mut state = BedrockStreamState::default();
        let throttled = serde_json::json!({
            "throttlingException": {
                "message": "rate limited"
            }
        });
        let action = parse_bedrock_event_payload(
            throttled.to_string().as_bytes(),
            "bedrock",
            "anthropic.claude",
            &mut state,
        );
        match action {
            crate::sse::SseAction::Error(LlmError::Retryable { reason, .. }) => {
                assert_eq!(reason, FailoverReason::RateLimit);
            }
            _ => panic!("expected retryable rate-limit error"),
        }

        let denied = serde_json::json!({
            "accessDeniedException": {
                "message": "not allowed"
            }
        });
        let action = parse_bedrock_event_payload(
            denied.to_string().as_bytes(),
            "bedrock",
            "anthropic.claude",
            &mut state,
        );
        match action {
            crate::sse::SseAction::Error(LlmError::Fatal { reason, .. }) => {
                assert_eq!(reason, FailoverReason::AuthPermanent);
            }
            _ => panic!("expected fatal auth-permanent error"),
        }
    }

    #[test]
    fn sanitize_xai_tool_schema_strips_unsupported_keywords_recursively() {
        // Fixture covers every shape xAI rejects: each unsupported keyword
        // present at top-level, inside `properties`, and
        // inside nested `items`/`anyOf`. After sanitization, every banned
        // keyword must be gone everywhere AND every legitimate keyword must
        // survive — otherwise tools either 400 on xAI or stop describing
        // themselves correctly to every other provider that reuses this
        // schema.
        let mut schema = serde_json::json!({
            "type": "object",
            "strict": true,
            "properties": {
                "name": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 64,
                    "description": "user name",
                },
                "tags": {
                    "type": "array",
                    "minItems": 0,
                    "maxItems": 10,
                    "minContains": 1,
                    "maxContains": 5,
                    "items": {
                        "type": "string",
                        "minLength": 2,
                        "enum": ["a", "b", "c"],
                    },
                },
                "kind": {
                    "anyOf": [
                        { "type": "string", "maxLength": 8 },
                        { "type": "null" },
                    ]
                }
            },
            "required": ["name"],
        });
        sanitize_xai_tool_schema(&mut schema);

        // Banned keywords are removed at every nesting depth.
        for path in [
            "/strict",
            "/properties/name/minLength",
            "/properties/name/maxLength",
            "/properties/tags/minItems",
            "/properties/tags/maxItems",
            "/properties/tags/minContains",
            "/properties/tags/maxContains",
            "/properties/tags/items/minLength",
            "/properties/kind/anyOf/0/maxLength",
        ] {
            assert!(
                schema.pointer(path).is_none(),
                "expected {path} to be stripped but it remained: {schema}"
            );
        }
        // Legitimate keywords survive — drift here would break tools on every
        // other provider while fixing them on xAI.
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["name"]["type"], "string");
        assert_eq!(schema["properties"]["name"]["description"], "user name");
        assert_eq!(schema["properties"]["tags"]["type"], "array");
        assert_eq!(schema["properties"]["tags"]["items"]["type"], "string");
        assert_eq!(
            schema["properties"]["tags"]["items"]["enum"],
            serde_json::json!(["a", "b", "c"])
        );
        assert_eq!(schema["required"], serde_json::json!(["name"]));
        assert_eq!(schema["properties"]["kind"]["anyOf"][1]["type"], "null");
    }

    #[test]
    fn sanitize_xai_tool_schema_preserves_property_named_strict() {
        // A property literally named "strict" (as opposed to the top-level
        // boolean `strict: true` flag) is a real schema; stripping it would
        // silently delete a user-defined field from tool inputs.
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "strict": { "type": "boolean", "description": "should we enforce" }
            }
        });
        sanitize_xai_tool_schema(&mut schema);
        assert_eq!(schema["properties"]["strict"]["type"], "boolean");
        assert_eq!(
            schema["properties"]["strict"]["description"],
            "should we enforce"
        );
    }

    #[test]
    fn build_openai_request_strips_xai_unsupported_tool_schema() {
        // End-to-end: build_openai_request must run the xAI sanitizer when
        // (and only when) the provider is XAi. Without this, every tool whose
        // schema uses string/array bounds 400s on xAI — a high-impact silent
        // regression.
        let messages = vec![Message::user("hi")];
        let tools = vec![crate::types::ToolDefinition::new(
            "lookup",
            "Look up a record",
            serde_json::json!({
                "type": "object",
                "strict": true,
                "properties": {
                    "id": { "type": "string", "minLength": 1, "maxLength": 36 },
                    "tags": { "type": "array", "items": { "type": "string" }, "maxItems": 4 }
                },
                "required": ["id"]
            }),
        )];

        let xai_req = TransportRequest {
            provider: Provider::XAi,
            model: "grok-4",
            messages: &messages,
            tools: Some(&tools),
            llm_config: default_llm_config(),
            prompt_cache_key: None,
            system_stable_prefix: None,
            cost_events_enabled: false,
        };
        let body = build_openai_request(&xai_req, false).expect("xai body");
        let params = &body["tools"][0]["function"]["parameters"];
        assert!(
            params.pointer("/strict").is_none(),
            "xAI body still carries top-level strict: {body}"
        );
        assert!(
            params.pointer("/properties/id/minLength").is_none(),
            "xAI body still carries minLength: {body}"
        );
        assert!(
            params.pointer("/properties/id/maxLength").is_none(),
            "xAI body still carries maxLength: {body}"
        );
        assert!(
            params.pointer("/properties/tags/maxItems").is_none(),
            "xAI body still carries maxItems: {body}"
        );
        // `required` / nested `type` / property surface must survive.
        assert_eq!(params["properties"]["id"]["type"], "string");
        assert_eq!(params["required"], serde_json::json!(["id"]));

        // The sanitizer is xAI-only: pointing the same tool at another
        // OpenAI-compat provider must leave the schema intact. Otherwise tool
        // validation on OpenAI / Groq / DeepSeek silently weakens for every
        // user who happens to share a tool catalog with an xAI session.
        let openai_req = TransportRequest {
            provider: Provider::OpenAi,
            model: "gpt-4.1",
            messages: &messages,
            tools: Some(&tools),
            llm_config: default_llm_config(),
            prompt_cache_key: None,
            system_stable_prefix: None,
            cost_events_enabled: false,
        };
        let body = build_openai_request(&openai_req, false).expect("openai body");
        let params = &body["tools"][0]["function"]["parameters"];
        assert_eq!(params["strict"], true);
        assert_eq!(params["properties"]["id"]["minLength"], 1);
        assert_eq!(params["properties"]["id"]["maxLength"], 36);
        assert_eq!(params["properties"]["tags"]["maxItems"], 4);
    }

    /// Gemini bills thought tokens against the output cap, so a thinking-on
    /// Gemini slug reached through an OpenAI-compatible host (OpenRouter, a
    /// compat relay) must carry the raised cap the native path already sends.
    /// Left at the configured 16K, high thinking consumes the whole allowance
    /// and the turn returns empty with no error — a silent failure, not a 400.
    #[test]
    fn build_openai_request_raises_gemini_output_cap_when_thinking_is_on() {
        let messages = vec![Message::user("hi")];
        let mut thinking_cfg = LlmConfig::default();
        thinking_cfg.thinking = crate::config::ThinkingLevel::High;
        let configured = thinking_cfg.max_tokens;

        let gemini = TransportRequest {
            provider: Provider::OpenRouter,
            model: "google/gemini-3-pro-preview",
            messages: &messages,
            tools: None,
            llm_config: &thinking_cfg,
            prompt_cache_key: None,
            system_stable_prefix: None,
            cost_events_enabled: false,
        };
        let body = build_openai_request(&gemini, false).expect("gemini body");
        let sent = body["max_tokens"].as_u64().expect("max_tokens on the wire");
        assert!(
            sent > u64::from(configured),
            "thinking Gemini must get headroom above the configured {configured}, got {sent}"
        );

        // The raise is Gemini-specific: every other model keeps the operator's
        // cap, which is a real cost lever there.
        let other = TransportRequest {
            provider: Provider::OpenRouter,
            model: "anthropic/claude-sonnet-4",
            messages: &messages,
            tools: None,
            llm_config: &thinking_cfg,
            prompt_cache_key: None,
            system_stable_prefix: None,
            cost_events_enabled: false,
        };
        let body = build_openai_request(&other, false).expect("anthropic body");
        assert_eq!(
            body["max_tokens"].as_u64(),
            Some(u64::from(configured)),
            "a non-Gemini model must keep the configured cap: {body}"
        );

        // Thinking off is the same: nothing bills against the cap, so nothing
        // is raised even for Gemini.
        let off = LlmConfig::default();
        let gemini_off = TransportRequest {
            provider: Provider::OpenRouter,
            model: "google/gemini-3-pro-preview",
            messages: &messages,
            tools: None,
            llm_config: &off,
            prompt_cache_key: None,
            system_stable_prefix: None,
            cost_events_enabled: false,
        };
        let body = build_openai_request(&gemini_off, false).expect("gemini-off body");
        assert_eq!(
            body["max_tokens"].as_u64(),
            Some(u64::from(off.max_tokens)),
            "thinking-off Gemini must keep the configured cap: {body}"
        );
    }

    #[test]
    fn build_openai_request_sanitizes_empty_messages_on_both_stream_modes() {
        // A resumed session can carry an empty non-final assistant/user stub.
        // Strict providers 400 on it, and the non-streaming path is exactly
        // the one summarization/compaction and title generation use — the
        // sanitizer must run there too, not only when streaming.
        let messages = vec![
            Message::user("hi"),
            Message::assistant(""), // empty non-final stub — must be dropped
            Message::user("continue"),
        ];
        let req = TransportRequest {
            provider: Provider::OpenAi,
            model: "gpt-4.1",
            messages: &messages,
            tools: None,
            llm_config: default_llm_config(),
            prompt_cache_key: None,
            system_stable_prefix: None,
            cost_events_enabled: false,
        };

        for stream in [false, true] {
            let body = build_openai_request(&req, stream).expect("body");
            let sent = body["messages"].as_array().expect("messages array");
            assert_eq!(
                sent.len(),
                2,
                "stream={stream}: empty stub not dropped: {body}"
            );
            assert_eq!(sent[0]["content"], "hi");
            assert_eq!(sent[1]["content"], "continue");
        }
    }

    #[test]
    fn build_vertex_anthropic_request_moves_model_to_url_contract() {
        let messages = vec![Message::system("sys"), Message::user("hello")];
        let mut req = bedrock_request(&messages);
        req.provider = Provider::VertexAi;
        req.model = "claude-sonnet-4";

        let body = build_vertex_anthropic_request(&req, true);

        assert!(
            body.get("model").is_none(),
            "Vertex Anthropic model lives in the URL path, not the JSON body"
        );
        assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "user");
    }
}
