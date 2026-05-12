use anyhow::{Context, Result};

use super::{
    apply_message_cache_control, apply_openai_cache_markers, build_anthropic_messages,
    build_cache_control_marker, classify_anthropic_stream_error, normalize_cache_tokens,
    parse_anthropic_response, sanitize_openai_messages, supports_anthropic_cache_control,
    ChatRequest, LlmConfig, LlmError, Message, Provider, Role, StreamChunk, StreamEvent,
    StreamOptions, StreamUsage, ToolDefinition, UsageData, UsageOptions,
};
use crate::llm_error::FailoverReason;

pub(crate) struct TransportRequest<'a> {
    pub(crate) provider: Provider,
    pub(crate) model: &'a str,
    pub(crate) messages: &'a [Message],
    pub(crate) tools: Option<&'a [ToolDefinition]>,
    pub(crate) llm_config: &'a LlmConfig,
    pub(crate) prompt_cache_key: Option<&'a str>,
    pub(crate) cost_events_enabled: bool,
}

pub(crate) struct TransportResponse {
    pub(crate) message: Message,
    pub(crate) usage: Option<UsageData>,
}

pub(crate) enum StreamState {
    OpenAiCompat,
    Anthropic(AnthropicStreamState),
}

pub(crate) trait Transport: Sync {
    fn protocol_name(&self) -> &'static str;

    fn debug_request_label(&self) -> &'static str;

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

    fn parse_chat_response(
        &self,
        response: serde_json::Value,
        provider: &str,
        model: &str,
    ) -> Result<TransportResponse>;
}

static OPENAI_COMPAT_TRANSPORT: OpenAiCompatTransport = OpenAiCompatTransport;
static ANTHROPIC_TRANSPORT: AnthropicTransport = AnthropicTransport;

pub(crate) fn openai_compat_transport() -> &'static dyn Transport {
    &OPENAI_COMPAT_TRANSPORT
}

pub(crate) fn anthropic_transport() -> &'static dyn Transport {
    &ANTHROPIC_TRANSPORT
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

pub(crate) fn build_anthropic_request(
    request: &TransportRequest<'_>,
    stream: bool,
) -> serde_json::Value {
    let system_text: Option<String> = request
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .and_then(|m| m.text_content().map(String::from));

    let anthropic_messages = build_anthropic_messages(request.messages);
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

    let mut body = if let Some(budget) = request.llm_config.thinking.budget_tokens() {
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
                let cached_input_tokens = usage
                    .prompt_tokens_details
                    .as_ref()
                    .and_then(|d| d.cached_tokens)
                    .or(usage.cached_tokens)
                    .unwrap_or(0);
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
                    cache_creation_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
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
                | FailoverReason::Format => LlmError::Fatal { source, reason },
                _ => LlmError::Retryable {
                    source,
                    retry_after: None,
                    reason,
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

struct OpenAiCompatTransport;

impl Transport for OpenAiCompatTransport {
    fn protocol_name(&self) -> &'static str {
        "openai-compat"
    }

    fn debug_request_label(&self) -> &'static str {
        "request_openai"
    }

    fn build_streaming_request(
        &self,
        request: &TransportRequest<'_>,
    ) -> std::result::Result<serde_json::Value, LlmError> {
        build_openai_request(request, true).map_err(|e| LlmError::Fatal {
            source: anyhow::anyhow!("serialize OpenAI ChatRequest for cache injection: {e}"),
            reason: FailoverReason::Unknown,
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
            cached_input_tokens: usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens)
                .or(usage.cached_tokens)
                .unwrap_or(0),
            cache_creation_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
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
                provider: provider.to_string(),
                model: model.to_string(),
                provider_cost_usd: None,
            }
        });

        Ok(TransportResponse { message, usage })
    }
}

fn build_openai_request(
    request: &TransportRequest<'_>,
    stream: bool,
) -> serde_json::Result<serde_json::Value> {
    let reasoning_effort = request
        .llm_config
        .thinking
        .openai_reasoning_effort()
        .map(String::from);
    let chat_request = ChatRequest {
        model: request.model.to_string(),
        messages: if stream {
            sanitize_openai_messages(request.messages)
        } else {
            request.messages.to_vec()
        },
        tools: request.tools.map(<[ToolDefinition]>::to_vec),
        temperature: request.llm_config.temperature,
        max_tokens: request.llm_config.max_tokens,
        stream,
        reasoning_effort,
        prompt_cache_key: request.prompt_cache_key.map(str::to_string),
        user: request.prompt_cache_key.map(str::to_string),
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
        apply_openai_cache_markers(&mut body, &request.llm_config.cache);
    }
    Ok(body)
}

fn parse_usage_cost(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    }
}
