//! Native Gemini AI Studio adapter (generateContent / streamGenerateContent).
//!
//! This file lands in three milestones:
//!
//! - **M2 (this commit)** — request builder, non-streaming response parser,
//!   error mapping, schema sanitisation, role/part conversion. The streaming
//!   methods are placeholders that fail loudly with `LlmError::Fatal` so a
//!   premature dispatch is obvious in logs.
//! - **M3** — fills `parse_stream_line` and `new_stream_state` for SSE,
//!   driven by `GeminiStreamState`.
//! - **M4** — flips `Provider::Gemini.is_openai_compatible()` to `false` and
//!   wires `transport.endpoint()` into the dispatcher for Gemini.
//!
//! Vertex AI's Gemini variant is intentionally *not* served by this adapter
//! — Vertex auth (service-account OAuth, regionalised endpoints) lives in
//! `crate::llm::vertex`. This file is for the AI Studio API-key endpoint at
//! `generativelanguage.googleapis.com` only.

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::transport::{Transport, TransportRequest, TransportResponse};
use super::{LlmError, Message, Role, StreamEvent, ToolDefinition, UsageData};
use crate::sse::SseAction;
use crate::types::{ContentPart, FunctionCall, MessageContent, ToolCall};

/// Default AI Studio base URL. Override via `BORG_GEMINI_BASE_URL` (for tests
/// pointing at `wiremock`) or via `llm.base_url` for self-hosted relays.
const DEFAULT_GEMINI_BASE: &str = "https://generativelanguage.googleapis.com";

/// Env var name read by [`GeminiTransport::resolve_base`] to override the
/// default endpoint at runtime. Documented so the test harness has a single
/// place to look.
pub(crate) const GEMINI_BASE_URL_ENV: &str = "BORG_GEMINI_BASE_URL";

pub(crate) struct GeminiTransport;

/// SSE accumulator for Gemini's `streamGenerateContent?alt=sse` endpoint.
#[derive(Default)]
pub(crate) struct GeminiStreamState {
    /// Reserved scratch buffer for partial-frame reassembly. The SSE
    /// processor today hands us complete lines, so the field stays inert
    /// behind the public surface — present for adapter symmetry with
    /// transports that need cross-chunk buffering.
    #[allow(dead_code)]
    pub(crate) buf: String,
    /// Accumulator for the assistant message under construction.
    pub(crate) accumulator: AssistantAccumulator,
}

/// Per-stream accumulator built from incremental SSE events.
#[derive(Default)]
pub(crate) struct AssistantAccumulator {
    pub(crate) text: String,
    pub(crate) pending_function_calls: Vec<PendingFunctionCall>,
    pub(crate) usage: Option<UsageData>,
    pub(crate) finish_reason: Option<String>,
}

/// One in-flight function call. Gemini doesn't emit a synthetic call id, so
/// we mint one of the form `gem_call_<name>_<index>` at finalisation time.
/// The id stays stable for the next user turn's `functionResponse` matching.
///
/// Fields are populated for accumulator parity with other transports;
/// today the values are emitted directly via `StreamEvent::ToolCallDelta`
/// rather than being re-read here, so the struct sits at the boundary.
#[derive(Default)]
#[allow(dead_code)]
pub(crate) struct PendingFunctionCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) args_text: String,
}

impl GeminiTransport {
    /// Honour `BORG_GEMINI_BASE_URL` first, then explicit `llm.base_url`,
    /// then the default AI Studio host. Trailing slashes are trimmed so
    /// the path we append doesn't accidentally double-slash.
    fn resolve_base(request: &TransportRequest<'_>) -> String {
        if let Ok(env) = std::env::var(GEMINI_BASE_URL_ENV) {
            if !env.is_empty() {
                return env.trim_end_matches('/').to_string();
            }
        }
        if let Some(b) = request.llm_config.base_url.as_deref() {
            if !b.is_empty() {
                return b.trim_end_matches('/').to_string();
            }
        }
        DEFAULT_GEMINI_BASE.to_string()
    }
}

impl Transport for GeminiTransport {
    fn protocol_name(&self) -> &'static str {
        "gemini-native"
    }

    fn debug_request_label(&self) -> &'static str {
        "request_gemini_native"
    }

    fn endpoint(&self, request: &TransportRequest<'_>, stream: bool) -> Result<String> {
        let base = Self::resolve_base(request);
        let model = request.model;
        let verb = if stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        // M2 deliberately does NOT include `?key=...` in the URL. The caller
        // layer threads credentials in via its existing auth path (mirroring
        // every other transport). M4's dispatch wiring will route Gemini to
        // either the API-key header (`x-goog-api-key`) or the query param
        // depending on operator config — both are accepted by AI Studio. We
        // keep the endpoint pure here so URL-shape tests don't depend on a
        // secret being present.
        if stream {
            Ok(format!("{base}/v1beta/models/{model}:{verb}?alt=sse"))
        } else {
            Ok(format!("{base}/v1beta/models/{model}:{verb}"))
        }
    }

    fn build_chat_request(&self, request: &TransportRequest<'_>) -> Result<Value> {
        Ok(build_gemini_request(request))
    }

    fn build_streaming_request(
        &self,
        request: &TransportRequest<'_>,
    ) -> std::result::Result<Value, LlmError> {
        Ok(build_gemini_request(request))
    }

    fn new_stream_state(&self) -> super::transport::StreamState {
        super::transport::StreamState::Gemini(GeminiStreamState::default())
    }

    fn parse_stream_line(
        &self,
        line: &str,
        provider: &str,
        model: &str,
        state: &mut super::transport::StreamState,
    ) -> SseAction {
        let state = match state {
            super::transport::StreamState::Gemini(s) => s,
            // Defensive: this can only fire if the dispatcher wires a
            // non-Gemini provider through this transport. M4 routes only
            // Gemini here, so the only realistic trigger is a stray test
            // wiring. Continuing keeps the stream alive instead of crashing
            // the agent loop on a misconfiguration.
            _ => return SseAction::Continue,
        };
        parse_gemini_sse_line(line, provider, model, state)
    }

    fn parse_chat_response(
        &self,
        response: Value,
        provider: &str,
        model: &str,
    ) -> Result<TransportResponse> {
        parse_gemini_response(response, provider, model)
    }
}

// ── Request building ─────────────────────────────────────────────────────

/// Build the full Gemini `generateContent` request body from a Borg
/// `TransportRequest`. Pure function — every shape decision is in one place
/// so the unit tests can exercise it end-to-end without spawning the
/// transport.
fn build_gemini_request(request: &TransportRequest<'_>) -> Value {
    let (contents, system_instruction) = build_contents_and_system(request.messages);

    let mut body = json!({
        "contents": contents,
        "safetySettings": safety_settings_block(),
        "generationConfig": generation_config(request),
    });

    if let Some(system_text) = system_instruction {
        body["systemInstruction"] = json!({
            "parts": [{"text": system_text}],
        });
    }

    if let Some(tools) = request.tools {
        if !tools.is_empty() {
            body["tools"] = json!([{
                "functionDeclarations": tools
                    .iter()
                    .map(tool_definition_to_function_declaration)
                    .collect::<Vec<_>>(),
            }]);
            // Default AUTO — let the model decide whether to call a tool.
            body["toolConfig"] = json!({
                "functionCallingConfig": {"mode": "AUTO"},
            });
        }
    }

    body
}

/// Split Borg messages into a Gemini `contents` array and a single optional
/// `systemInstruction` string. All `Role::System` messages are concatenated
/// (Gemini accepts only one); the rest map per the role table in the M2
/// spec doc.
fn build_contents_and_system(messages: &[Message]) -> (Vec<Value>, Option<String>) {
    let mut system_chunks: Vec<String> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    // We need the previous assistant turn's function-call names so a
    // following `Role::Tool` can fill in `functionResponse.name` (Gemini
    // matches function-call ↔ response by name + ordinal, not by id).
    let mut last_assistant_call_names: Vec<String> = Vec::new();
    let mut pending_tool_responses: Vec<Value> = Vec::new();

    let flush_tool_responses = |contents: &mut Vec<Value>, pending: &mut Vec<Value>| {
        if !pending.is_empty() {
            contents.push(json!({
                "role": "user",
                "parts": std::mem::take(pending),
            }));
        }
    };

    for msg in messages {
        match msg.role {
            Role::System => {
                if let Some(text) = msg.text_content() {
                    if !text.is_empty() {
                        system_chunks.push(text.to_string());
                    }
                }
            }
            Role::User => {
                flush_tool_responses(&mut contents, &mut pending_tool_responses);
                let parts = message_to_user_parts(msg);
                if !parts.is_empty() {
                    contents.push(json!({"role": "user", "parts": parts}));
                }
            }
            Role::Assistant => {
                flush_tool_responses(&mut contents, &mut pending_tool_responses);
                let (parts, call_names) = message_to_model_parts(msg);
                last_assistant_call_names = call_names;
                if !parts.is_empty() {
                    contents.push(json!({"role": "model", "parts": parts}));
                }
            }
            Role::Tool => {
                // Match this tool result to the corresponding prior call by
                // ordinal: the first tool message after an assistant turn
                // fills slot 0, the second fills slot 1, etc.
                let name = last_assistant_call_names
                    .get(pending_tool_responses.len())
                    .cloned()
                    .unwrap_or_else(|| {
                        // No matching call — log + fall back to a synthetic
                        // name so the request is still well-formed. Silently
                        // dropping the result would mask the agent's progress.
                        tracing::warn!(
                            "gemini adapter: tool result has no matching prior functionCall — using placeholder name"
                        );
                        "unknown_tool".to_string()
                    });
                let content_text = msg.text_content().unwrap_or("").to_string();
                pending_tool_responses.push(json!({
                    "functionResponse": {
                        "name": name,
                        "response": {"content": content_text},
                    }
                }));
            }
        }
    }
    flush_tool_responses(&mut contents, &mut pending_tool_responses);

    let system = if system_chunks.is_empty() {
        None
    } else {
        Some(system_chunks.join("\n\n"))
    };
    (contents, system)
}

fn message_to_user_parts(msg: &Message) -> Vec<Value> {
    match &msg.content {
        Some(MessageContent::Text(t)) if !t.is_empty() => vec![json!({"text": t})],
        Some(MessageContent::Parts(parts)) => parts.iter().map(content_part_to_value).collect(),
        _ => Vec::new(),
    }
}

/// Returns (parts, function_call_names_in_order). The names list lets the
/// following `Role::Tool` messages fill in `functionResponse.name`.
fn message_to_model_parts(msg: &Message) -> (Vec<Value>, Vec<String>) {
    let mut parts: Vec<Value> = Vec::new();
    let mut call_names: Vec<String> = Vec::new();

    match &msg.content {
        Some(MessageContent::Text(t)) if !t.is_empty() => {
            parts.push(json!({"text": t}));
        }
        Some(MessageContent::Parts(content_parts)) => {
            for part in content_parts {
                parts.push(content_part_to_value(part));
            }
        }
        _ => {}
    }

    if let Some(tool_calls) = &msg.tool_calls {
        for tc in tool_calls {
            let args_value: Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or_else(|e| {
                    tracing::warn!(
                        "gemini adapter: tool call args not valid JSON ({}); wrapping in _raw",
                        e
                    );
                    json!({"_raw": tc.function.arguments})
                });
            parts.push(json!({
                "functionCall": {
                    "name": tc.function.name,
                    "args": args_value,
                }
            }));
            call_names.push(tc.function.name.clone());
        }
    }

    (parts, call_names)
}

fn content_part_to_value(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text(t) => json!({"text": t}),
        ContentPart::ImageBase64 { media } => json!({
            "inline_data": {
                "mime_type": media.mime_type,
                "data": media.data,
            }
        }),
        ContentPart::ImageUrl { url } => {
            // M2 surfaces this as a text placeholder; SSRF-safe URL fetch
            // belongs in the vision_analyze path (M5), not the LLM adapter.
            json!({"text": format!("[image: {url}]")})
        }
        ContentPart::AudioBase64 { media } => json!({
            "inline_data": {
                "mime_type": media.mime_type,
                "data": media.data,
            }
        }),
        ContentPart::VideoBase64 { media } => json!({
            "inline_data": {
                "mime_type": media.mime_type,
                "data": media.data,
            }
        }),
    }
}

/// Convert a Borg `ToolDefinition` into a Gemini `functionDeclarations`
/// entry, stripping JSON-Schema constructs Gemini doesn't accept. We mutate
/// a clone, never the original (the same `ToolDefinition` may be sent to
/// other providers in the same turn under fallback).
fn tool_definition_to_function_declaration(tool: &ToolDefinition) -> Value {
    let mut params = tool.function.parameters.clone();
    sanitize_schema_for_gemini(&mut params);
    json!({
        "name": tool.function.name,
        "description": tool.function.description,
        "parameters": params,
    })
}

/// Recursively strip JSON-Schema keys that Gemini's subset rejects. Drift
/// to `oneOf` / `anyOf` / `allOf` etc. would make the whole request 400 —
/// safer to drop the constraint and let the model misbehave once than to
/// fail every turn.
pub(crate) fn sanitize_schema_for_gemini(value: &mut Value) {
    if let Value::Object(map) = value {
        // Hard-banned keys. `additionalProperties` would surface as an
        // INVALID_ARGUMENT and kill the request.
        for k in &[
            "additionalProperties",
            "$schema",
            "$ref",
            "definitions",
            "oneOf",
            "anyOf",
            "allOf",
        ] {
            if map.remove(*k).is_some() {
                tracing::warn!(
                    "gemini adapter: stripped unsupported schema key `{}` from tool parameters",
                    k
                );
            }
        }
        // `format` is conditionally accepted (Gemini supports date-time and
        // enum). Drop anything else so the request is well-formed; the
        // model still sees the field by name.
        if let Some(format) = map.get("format").and_then(Value::as_str) {
            if !matches!(format, "date-time" | "enum") {
                map.remove("format");
            }
        }
        for (_, v) in map.iter_mut() {
            sanitize_schema_for_gemini(v);
        }
    } else if let Value::Array(arr) = value {
        for v in arr.iter_mut() {
            sanitize_schema_for_gemini(v);
        }
    }
}

fn generation_config(request: &TransportRequest<'_>) -> Value {
    let cfg = &request.llm_config;
    let mut obj = serde_json::Map::new();
    obj.insert("temperature".into(), json!(cfg.temperature));
    obj.insert("maxOutputTokens".into(), json!(cfg.max_tokens));
    Value::Object(obj)
}

/// Borg is an agent loop; the default Gemini safety thresholds block
/// legitimate discussion of e.g. shell commands. The four categories are
/// deliberately set to `BLOCK_NONE`, matching the no-safety-prompt stance
/// the agent already takes with Anthropic and OpenAI.
fn safety_settings_block() -> Value {
    json!([
        {"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE"},
        {"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_NONE"},
        {"category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "BLOCK_NONE"},
        {"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "BLOCK_NONE"},
    ])
}

// ── Response parsing ─────────────────────────────────────────────────────

fn parse_gemini_response(
    response: Value,
    provider: &str,
    model: &str,
) -> Result<TransportResponse> {
    // Prompt-level safety block: the request never reached the model. Map
    // to `LlmError::Fatal { reason: Unknown }` via `anyhow::bail!` — surfacing
    // this as success would mislead the agent into thinking it got a reply.
    if let Some(block_reason) = response
        .get("promptFeedback")
        .and_then(|f| f.get("blockReason"))
        .and_then(Value::as_str)
    {
        anyhow::bail!("gemini blocked prompt: {block_reason}");
    }

    let candidates = response
        .get("candidates")
        .and_then(Value::as_array)
        .context("gemini response missing `candidates`")?;
    let candidate = candidates
        .first()
        .context("gemini response had empty `candidates`")?;

    // Candidate-level safety/recitation blocks: also surface as errors.
    if let Some(finish) = candidate.get("finishReason").and_then(Value::as_str) {
        if matches!(finish, "SAFETY" | "RECITATION") {
            anyhow::bail!("gemini candidate blocked: {finish}");
        }
    }

    let parts = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut text_accum = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    for (idx, part) in parts.iter().enumerate() {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            text_accum.push_str(text);
        } else if let Some(fc) = part.get("functionCall") {
            let name = fc
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
            let arguments = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
            tool_calls.push(ToolCall {
                id: synth_call_id(&name, idx),
                call_type: "function".into(),
                function: FunctionCall { name, arguments },
            });
        }
    }

    let content = if text_accum.is_empty() {
        None
    } else {
        Some(MessageContent::Text(text_accum))
    };

    let tool_calls = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };

    let message = Message {
        role: Role::Assistant,
        content,
        tool_calls,
        tool_call_id: None,
        timestamp: None,
    };

    let usage = response.get("usageMetadata").map(|u| {
        let pick = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        UsageData {
            prompt_tokens: pick("promptTokenCount"),
            completion_tokens: pick("candidatesTokenCount"),
            total_tokens: pick("totalTokenCount"),
            cached_input_tokens: pick("cachedContentTokenCount"),
            cache_creation_tokens: 0,
            provider: provider.to_string(),
            model: model.to_string(),
            provider_cost_usd: None,
        }
    });

    Ok(TransportResponse { message, usage })
}

/// Synthesize a stable id for a Gemini function call so subsequent tool
/// results can be matched back to it. M3's streaming path uses the same
/// scheme to stay consistent across streaming/non-streaming.
fn synth_call_id(name: &str, idx: usize) -> String {
    format!("gem_call_{name}_{idx}")
}

/// Append-or-push semantics for the SSE function-call accumulator. AI Studio
/// today ships the full `args` object in a single frame, so the `push` branch
/// is the live path. The `append` branch is the defensive symmetry with the
/// Anthropic accumulator (`stream_handler::ToolCallDelta`'s
/// `arguments.push_str`): if a proxy or future SDK ever splits a single call
/// across multiple frames with a stable id, the accumulator concatenates
/// rather than silently discarding the prior bytes.
fn accumulate_function_call(
    pending: &mut Vec<PendingFunctionCall>,
    id: &str,
    name: &str,
    args_text: &str,
) {
    if let Some(existing) = pending.iter_mut().find(|p| p.id == id) {
        existing.args_text.push_str(args_text);
    } else {
        pending.push(PendingFunctionCall {
            id: id.to_string(),
            name: name.to_string(),
            args_text: args_text.to_string(),
        });
    }
}

// ── SSE streaming parser ─────────────────────────────────────────────────

/// Parse one SSE line from `streamGenerateContent?alt=sse`. The driver in
/// `crate::sse` strips empty lines and `:`-prefixed comments before
/// dispatch, but we still defensively skip anything that isn't a `data:`
/// payload — Gemini occasionally emits `event:` lines for keepalive that
/// must not be mis-parsed.
///
/// Returns `Done` only when `finishReason` is terminal and we have nothing
/// further to surface. Otherwise we emit deltas/usage and rely on the SSE
/// driver to send the `Done` event when the upstream socket closes.
pub(crate) fn parse_gemini_sse_line(
    line: &str,
    provider: &str,
    model: &str,
    state: &mut GeminiStreamState,
) -> SseAction {
    let Some(payload) = line.strip_prefix("data:") else {
        return SseAction::Continue;
    };
    let payload = payload.trim();
    if payload.is_empty() {
        return SseAction::Continue;
    }
    // Gemini doesn't emit `[DONE]`; the socket just closes. We guard
    // anyway because relays sometimes inject it. Always emit a terminal
    // `StreamEvent::Done` so the agent loop's `received_terminal` flag
    // flips — `SseAction::Done(vec![])` would suppress the synthetic
    // EOF Done in `sse.rs` and the turn would log "stream ended
    // unexpectedly".
    if payload == "[DONE]" {
        return SseAction::Done(vec![StreamEvent::Done]);
    }

    let value: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(provider, model, error = %e, "gemini SSE: failed to parse data line");
            return SseAction::Continue;
        }
    };

    // Prompt-level safety block: the model didn't run. Surface as Error
    // and stop the stream so the agent doesn't treat empty as success.
    if let Some(reason) = value
        .pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
    {
        return SseAction::Done(vec![StreamEvent::Error(format!(
            "gemini blocked prompt: {reason}"
        ))]);
    }

    let mut events: Vec<StreamEvent> = Vec::new();
    let mut done_after = false;

    // ── candidates[0].content.parts → text / thought / functionCall ──
    if let Some(parts) = value
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
    {
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    state.accumulator.text.push_str(text);
                    events.push(StreamEvent::TextDelta(text.to_string()));
                }
                continue;
            }
            // Gemini 2.5 Thinking surfaces reasoning as `{"thought": "..."}`
            // (separate from `text`). Surface it on its own channel so the
            // TUI can dim it the way it does Anthropic thinking.
            if let Some(thought) = part.get("thought").and_then(Value::as_str) {
                if !thought.is_empty() {
                    events.push(StreamEvent::ThinkingDelta(thought.to_string()));
                }
                continue;
            }
            if let Some(fc) = part.get("functionCall") {
                let name = fc
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                let args_text = serde_json::to_string(&args).unwrap_or_else(|_| "{}".into());
                let idx = state.accumulator.pending_function_calls.len();
                let id = synth_call_id(&name, idx);
                accumulate_function_call(
                    &mut state.accumulator.pending_function_calls,
                    &id,
                    &name,
                    &args_text,
                );
                events.push(StreamEvent::ToolCallDelta {
                    index: idx,
                    id: Some(id),
                    name: Some(name),
                    arguments_delta: args_text,
                });
                continue;
            }
        }
    }

    // ── finishReason (candidate-level) ──
    if let Some(reason) = value
        .pointer("/candidates/0/finishReason")
        .and_then(Value::as_str)
    {
        state.accumulator.finish_reason = Some(reason.to_string());
        match reason {
            "STOP" => {
                done_after = true;
            }
            "MAX_TOKENS" => {
                events.push(StreamEvent::Truncated);
                done_after = true;
            }
            "SAFETY" | "RECITATION" => {
                // Mid-stream candidate-level block — surface and stop.
                events.push(StreamEvent::Error(format!(
                    "gemini candidate blocked: {reason}"
                )));
                done_after = true;
            }
            other => {
                tracing::warn!(
                    provider,
                    model,
                    "gemini SSE: unknown finishReason `{other}` — treating as STOP"
                );
                done_after = true;
            }
        }
    }

    // ── usageMetadata (last event) ──
    if let Some(u) = value.get("usageMetadata") {
        let pick = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let usage = UsageData {
            prompt_tokens: pick("promptTokenCount"),
            completion_tokens: pick("candidatesTokenCount"),
            total_tokens: pick("totalTokenCount"),
            cached_input_tokens: pick("cachedContentTokenCount"),
            cache_creation_tokens: 0,
            provider: provider.to_string(),
            model: model.to_string(),
            provider_cost_usd: None,
        };
        state.accumulator.usage = Some(usage.clone());
        events.push(StreamEvent::Usage(usage));
    }

    if events.is_empty() && !done_after {
        SseAction::Continue
    } else if done_after {
        events.push(StreamEvent::Done);
        SseAction::Done(events)
    } else {
        SseAction::Emit(events)
    }
}

// Error classification lives in `llm_error::classify_status` for all
// providers, including Gemini. The Google-specific `RetryInfo.retryDelay`
// shape is parsed there via `parse_retry_after` so 429s honour the
// server-supplied cooldown.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LlmConfig;
    use crate::llm::Provider;
    use crate::types::{FunctionDefinition, MediaData};

    fn make_request<'a>(
        messages: &'a [Message],
        tools: Option<&'a [ToolDefinition]>,
        cfg: &'a LlmConfig,
    ) -> TransportRequest<'a> {
        TransportRequest {
            provider: Provider::Gemini,
            model: "gemini-2.5-flash",
            messages,
            tools,
            llm_config: cfg,
            prompt_cache_key: None,
            cost_events_enabled: false,
        }
    }

    fn default_cfg() -> LlmConfig {
        let mut cfg = LlmConfig::default();
        cfg.temperature = 0.7;
        cfg.max_tokens = 4096;
        cfg
    }

    #[test]
    fn build_request_text_only_user_message() {
        let msgs = vec![Message::user("hi")];
        let cfg = default_cfg();
        let body = build_gemini_request(&make_request(&msgs, None, &cfg));
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "hi");
    }

    #[test]
    fn build_request_collapses_multiple_system_messages_into_system_instruction() {
        // Gemini allows only one systemInstruction — if Borg passes more than
        // one System message, joining with `\n\n` keeps every instruction
        // intact instead of silently dropping all but the first.
        let msgs = vec![
            Message::system("rule one"),
            Message::system("rule two"),
            Message::user("ok"),
        ];
        let cfg = default_cfg();
        let body = build_gemini_request(&make_request(&msgs, None, &cfg));
        let sys = body["systemInstruction"]["parts"][0]["text"]
            .as_str()
            .unwrap();
        assert!(sys.contains("rule one"));
        assert!(sys.contains("rule two"));
        assert!(sys.contains("\n\n"), "expected double-newline join");
    }

    #[test]
    fn build_request_assistant_message_uses_model_role() {
        // The single rejection case Gemini will not forgive: a turn labelled
        // `"assistant"` instead of `"model"` 400s every time.
        let msgs = vec![Message::assistant("hello"), Message::user("hi")];
        let cfg = default_cfg();
        let body = build_gemini_request(&make_request(&msgs, None, &cfg));
        let roles: Vec<&str> = body["contents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, vec!["model", "user"]);
        assert!(!roles.contains(&"assistant"));
    }

    #[test]
    fn build_request_tool_role_becomes_function_response_on_user_turn() {
        // Gemini matches tool result ↔ prior call by name + ordinal, not id.
        // The assistant's `functionCall.name` must flow into the next user
        // turn's `functionResponse.name` — if it doesn't, the model has no
        // way to correlate the result to the request.
        let assistant_with_call = Message {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_abc".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "vision_analyze".into(),
                    arguments: r#"{"image_url":"x"}"#.into(),
                },
            }]),
            tool_call_id: None,
            timestamp: None,
        };
        let tool_result = Message::tool_result("call_abc", "the image shows a cat");
        let msgs = vec![Message::user("look"), assistant_with_call, tool_result];
        let cfg = default_cfg();
        let body = build_gemini_request(&make_request(&msgs, None, &cfg));

        // Find the user turn carrying the functionResponse.
        let contents = body["contents"].as_array().unwrap();
        let tool_user_turn = contents
            .iter()
            .find(|c| {
                c["parts"]
                    .as_array()
                    .map(|p| p.iter().any(|x| x.get("functionResponse").is_some()))
                    .unwrap_or(false)
            })
            .expect("expected a tool-result user turn");
        let fr = &tool_user_turn["parts"][0]["functionResponse"];
        assert_eq!(fr["name"], "vision_analyze");
        assert_eq!(fr["response"]["content"], "the image shows a cat");
    }

    #[test]
    fn build_request_image_base64_maps_to_inline_data() {
        let msg = Message::user_multimodal(vec![ContentPart::ImageBase64 {
            media: MediaData {
                mime_type: "image/png".into(),
                data: "Zm9v".into(),
                filename: None,
            },
        }]);
        let cfg = default_cfg();
        let body = build_gemini_request(&make_request(&[msg], None, &cfg));
        let part = &body["contents"][0]["parts"][0];
        assert_eq!(part["inline_data"]["mime_type"], "image/png");
        assert_eq!(part["inline_data"]["data"], "Zm9v");
    }

    #[test]
    fn build_request_video_base64_maps_to_inline_data() {
        // The whole reason this adapter exists. A regression here means
        // `video_analyze` is silently broken on Gemini.
        let msg = Message::user_multimodal(vec![ContentPart::VideoBase64 {
            media: MediaData {
                mime_type: "video/mp4".into(),
                data: "AAAA".into(),
                filename: Some("clip.mp4".into()),
            },
        }]);
        let cfg = default_cfg();
        let body = build_gemini_request(&make_request(&[msg], None, &cfg));
        let part = &body["contents"][0]["parts"][0];
        assert_eq!(part["inline_data"]["mime_type"], "video/mp4");
        assert_eq!(part["inline_data"]["data"], "AAAA");
    }

    #[test]
    fn build_request_tool_call_args_parsed_to_object() {
        // Gemini requires `args` as a JSON object, not a stringified JSON.
        // Borg stores OpenAI-shaped string arguments; the conversion has to
        // happen in the adapter or the model sees `{"x": "{\"y\":1}"}`.
        let assistant = Message {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "x".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "fn".into(),
                    arguments: r#"{"y":1}"#.into(),
                },
            }]),
            tool_call_id: None,
            timestamp: None,
        };
        let cfg = default_cfg();
        let body = build_gemini_request(&make_request(&[assistant], None, &cfg));
        let fc = &body["contents"][0]["parts"][0]["functionCall"];
        assert_eq!(fc["name"], "fn");
        assert!(
            fc["args"].is_object(),
            "expected args to be a JSON object, got {}",
            fc["args"]
        );
        assert_eq!(fc["args"]["y"], 1);
    }

    #[test]
    fn build_request_tool_call_args_invalid_json_falls_back_to_raw_wrapper() {
        // A malformed arguments string should never crash request building —
        // wrap it in `{"_raw": ...}` so the model at least sees that
        // something was attempted, and emit a warn for observability.
        let assistant = Message {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "x".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "fn".into(),
                    arguments: "not-json".into(),
                },
            }]),
            tool_call_id: None,
            timestamp: None,
        };
        let cfg = default_cfg();
        let body = build_gemini_request(&make_request(&[assistant], None, &cfg));
        let args = &body["contents"][0]["parts"][0]["functionCall"]["args"];
        assert!(args.is_object());
        assert_eq!(args["_raw"], "not-json");
    }

    #[test]
    fn build_request_sanitize_schema_strips_unsupported_keys() {
        let mut schema = json!({
            "type": "object",
            "additionalProperties": false,
            "oneOf": [{"type": "string"}],
            "properties": {
                "name": {"type": "string", "format": "email"},
                "ts": {"type": "string", "format": "date-time"},
            }
        });
        sanitize_schema_for_gemini(&mut schema);
        assert!(schema.get("additionalProperties").is_none());
        assert!(schema.get("oneOf").is_none());
        // `format: email` is unsupported -> stripped; `format: date-time`
        // is in Gemini's accepted set -> preserved.
        assert!(schema["properties"]["name"].get("format").is_none());
        assert_eq!(schema["properties"]["ts"]["format"], "date-time");
    }

    #[test]
    fn build_request_includes_tool_declarations() {
        let tool = ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "vision_analyze".into(),
                description: "look at an image".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
        };
        let cfg = default_cfg();
        let body = build_gemini_request(&make_request(&[Message::user("hi")], Some(&[tool]), &cfg));
        let decls = body["tools"][0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0]["name"], "vision_analyze");
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");
    }

    #[test]
    fn build_request_sets_safety_settings_to_block_none() {
        // Borg deliberately disables Gemini's default safety filters
        // (matching its stance on Anthropic and OpenAI) so the agent can
        // discuss shell commands, security topics, etc. without surprise
        // block-by-default rejections.
        let cfg = default_cfg();
        let body = build_gemini_request(&make_request(&[Message::user("hi")], None, &cfg));
        let settings = body["safetySettings"].as_array().unwrap();
        assert_eq!(settings.len(), 4);
        for s in settings {
            assert_eq!(s["threshold"], "BLOCK_NONE");
        }
    }

    #[test]
    fn build_request_generation_config_maps_temperature_and_max_tokens() {
        let mut cfg = default_cfg();
        cfg.temperature = 0.3;
        cfg.max_tokens = 1234;
        let body = build_gemini_request(&make_request(&[Message::user("hi")], None, &cfg));
        let gc = &body["generationConfig"];
        // serde JSON renders f32 as a number; compare via f64.
        assert!((gc["temperature"].as_f64().unwrap() - 0.3).abs() < 1e-6);
        assert_eq!(gc["maxOutputTokens"], 1234);
    }

    #[test]
    fn parse_response_with_function_call_returns_tool_call() {
        let resp = json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"text": "Let me check."},
                        {"functionCall": {"name": "vision_analyze", "args": {"url": "x"}}}
                    ]
                },
                "finishReason": "STOP"
            }]
        });
        let parsed = parse_gemini_response(resp, "gemini", "gemini-2.5-flash").unwrap();
        let msg = parsed.message;
        assert_eq!(msg.role, Role::Assistant);
        let tcs = msg.tool_calls.as_ref().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].function.name, "vision_analyze");
        // arguments round-trip back to the original JSON object.
        let args: Value = serde_json::from_str(&tcs[0].function.arguments).unwrap();
        assert_eq!(args["url"], "x");
        assert!(msg.content.is_some());
    }

    #[test]
    fn parse_response_safety_block_returns_error() {
        // promptFeedback.blockReason means the *prompt* was refused before
        // the model ran. Surfacing this as `Ok(empty)` would silently lie
        // to the agent loop; bail loudly instead.
        let resp = json!({
            "promptFeedback": {"blockReason": "SAFETY"}
        });
        let err = match parse_gemini_response(resp, "gemini", "gemini-2.5-flash") {
            Err(e) => e,
            Ok(_) => panic!("expected safety-block error, got Ok"),
        };
        assert!(err.to_string().contains("SAFETY"));
    }

    #[test]
    fn parse_response_candidate_safety_block_returns_error() {
        let resp = json!({
            "candidates": [{"finishReason": "SAFETY", "content": {"parts": []}}]
        });
        let err = match parse_gemini_response(resp, "gemini", "gemini-2.5-flash") {
            Err(e) => e,
            Ok(_) => panic!("expected candidate-block error, got Ok"),
        };
        assert!(err.to_string().contains("SAFETY"));
    }

    #[test]
    fn parse_response_usage_metadata_maps_correctly() {
        let resp = json!({
            "candidates": [{"content": {"parts": [{"text": "ok"}]}, "finishReason": "STOP"}],
            "usageMetadata": {
                "promptTokenCount": 42,
                "candidatesTokenCount": 17,
                "totalTokenCount": 59,
                "cachedContentTokenCount": 8
            }
        });
        let parsed = parse_gemini_response(resp, "gemini", "gemini-2.5-flash").unwrap();
        let usage = parsed.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 42);
        assert_eq!(usage.completion_tokens, 17);
        assert_eq!(usage.total_tokens, 59);
        assert_eq!(usage.cached_input_tokens, 8);
    }

    /// Serialises the three endpoint tests so their `BORG_GEMINI_BASE_URL`
    /// mutations don't race. Cargo's default parallel runner would
    /// otherwise let test A `set_var` and test B `remove_var` interleave.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn endpoint_non_streaming_uses_generate_content_path() {
        let _g = ENV_LOCK.lock().unwrap();
        let cfg = default_cfg();
        let msgs: Vec<Message> = Vec::new();
        let req = make_request(&msgs, None, &cfg);
        // SAFETY: the lock guards every test that touches this env var.
        unsafe {
            std::env::remove_var(GEMINI_BASE_URL_ENV);
        }
        let url = GeminiTransport.endpoint(&req, false).unwrap();
        assert!(url.starts_with(DEFAULT_GEMINI_BASE));
        assert!(url.ends_with(":generateContent"));
        assert!(url.contains("/models/gemini-2.5-flash:"));
    }

    #[test]
    fn endpoint_streaming_uses_stream_path_with_alt_sse() {
        let _g = ENV_LOCK.lock().unwrap();
        let cfg = default_cfg();
        let msgs: Vec<Message> = Vec::new();
        let req = make_request(&msgs, None, &cfg);
        unsafe {
            std::env::remove_var(GEMINI_BASE_URL_ENV);
        }
        let url = GeminiTransport.endpoint(&req, true).unwrap();
        assert!(url.ends_with("streamGenerateContent?alt=sse"));
    }

    #[test]
    fn endpoint_respects_borg_gemini_base_url_env_override() {
        let _g = ENV_LOCK.lock().unwrap();
        let cfg = default_cfg();
        let msgs: Vec<Message> = Vec::new();
        let req = make_request(&msgs, None, &cfg);
        // SAFETY: env mutation guarded by ENV_LOCK above.
        unsafe {
            std::env::set_var(GEMINI_BASE_URL_ENV, "http://127.0.0.1:9999");
        }
        let url = GeminiTransport.endpoint(&req, false).unwrap();
        unsafe {
            std::env::remove_var(GEMINI_BASE_URL_ENV);
        }
        assert!(
            url.starts_with("http://127.0.0.1:9999/"),
            "expected override to win, got {url}"
        );
    }

    // ── M3 SSE streaming tests ───────────────────────────────────────────

    /// Feed an SSE byte sequence one line at a time and collect every event
    /// emitted along the way. Mirrors how `crate::sse` drives the parser.
    fn feed(events: &[&str]) -> (GeminiStreamState, Vec<StreamEvent>, bool) {
        let mut state = GeminiStreamState::default();
        let mut emitted: Vec<StreamEvent> = Vec::new();
        let mut done = false;
        for line in events {
            match parse_gemini_sse_line(line, "gemini", "gemini-2.5-flash", &mut state) {
                SseAction::Continue => {}
                SseAction::Emit(evs) => emitted.extend(evs),
                SseAction::Done(evs) => {
                    emitted.extend(evs);
                    done = true;
                    break;
                }
                SseAction::Error(e) => panic!("unexpected SseAction::Error in test: {e}"),
            }
        }
        (state, emitted, done)
    }

    #[test]
    fn stream_text_deltas_accumulate_to_full_text() {
        let events = &[
            r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]}}]}"#,
            r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":" "}]}}]}"#,
            r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"world"}]}}]}"#,
        ];
        let (state, emitted, _) = feed(events);
        let texts: Vec<&str> = emitted
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Hello", " ", "world"]);
        assert_eq!(state.accumulator.text, "Hello world");
    }

    #[test]
    fn stream_function_call_event_emitted_with_synth_id() {
        let line = r#"data: {"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"vision_analyze","args":{"x":1}}}]}}]}"#;
        let (state, emitted, _) = feed(&[line]);
        assert_eq!(emitted.len(), 1);
        match &emitted[0] {
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                assert_eq!(*index, 0);
                assert_eq!(id.as_deref(), Some("gem_call_vision_analyze_0"));
                assert_eq!(name.as_deref(), Some("vision_analyze"));
                // Borg's wire is a string; assert it round-trips back to {x:1}.
                let args: Value = serde_json::from_str(arguments_delta).unwrap();
                assert_eq!(args["x"], 1);
            }
            other => panic!("expected ToolCallDelta, got {other:?}"),
        }
        assert_eq!(state.accumulator.pending_function_calls.len(), 1);
    }

    #[test]
    fn stream_multiple_function_calls_in_sequence_get_distinct_synth_ids() {
        let lines = &[
            r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"a","args":{}}}]}}]}"#,
            r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"b","args":{}}}]}}]}"#,
        ];
        let (_, emitted, _) = feed(lines);
        let ids: Vec<&str> = emitted
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallDelta { id, .. } => id.as_deref(),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["gem_call_a_0", "gem_call_b_1"]);
    }

    #[test]
    fn stream_usage_metadata_emits_usage_event() {
        let line = r#"data: {"candidates":[{"content":{"parts":[]}}],"usageMetadata":{"promptTokenCount":15,"candidatesTokenCount":12,"totalTokenCount":27}}"#;
        let (state, emitted, _) = feed(&[line]);
        let usage = emitted
            .iter()
            .find_map(|e| match e {
                StreamEvent::Usage(u) => Some(u),
                _ => None,
            })
            .expect("expected Usage event");
        assert_eq!(usage.prompt_tokens, 15);
        assert_eq!(usage.completion_tokens, 12);
        assert_eq!(usage.total_tokens, 27);
        assert_eq!(state.accumulator.usage.as_ref().unwrap().prompt_tokens, 15);
    }

    #[test]
    fn stream_safety_block_emits_error_and_stops() {
        // promptFeedback.blockReason in the SSE stream is just as fatal as
        // in the non-streaming path. Surfacing as success would let the
        // agent think it got an empty reply on purpose.
        let line = r#"data: {"promptFeedback":{"blockReason":"SAFETY"}}"#;
        let (_, emitted, done) = feed(&[line]);
        assert!(done, "stream should terminate on safety block");
        let err = emitted
            .iter()
            .find_map(|e| match e {
                StreamEvent::Error(msg) => Some(msg.as_str()),
                _ => None,
            })
            .expect("expected Error event");
        assert!(err.contains("SAFETY"));
    }

    #[test]
    fn stream_finish_reason_max_tokens_emits_truncated() {
        let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"cut"}]},"finishReason":"MAX_TOKENS"}]}"#;
        let (_, emitted, done) = feed(&[line]);
        assert!(done);
        assert!(emitted.iter().any(|e| matches!(e, StreamEvent::Truncated)));
    }

    #[test]
    fn stream_finish_reason_recitation_emits_error() {
        let line = r#"data: {"candidates":[{"content":{"parts":[]},"finishReason":"RECITATION"}]}"#;
        let (_, emitted, done) = feed(&[line]);
        assert!(done);
        assert!(emitted
            .iter()
            .any(|e| matches!(e, StreamEvent::Error(msg) if msg.contains("RECITATION"))));
    }

    #[test]
    fn stream_thinking_delta_surfaced_for_gemini_2_5_thinking() {
        // Gemini 2.5 Thinking emits `{"thought": "..."}` parts that the TUI
        // needs to render on a separate channel.
        let line = r#"data: {"candidates":[{"content":{"parts":[{"thought":"reasoning..."}]}}]}"#;
        let (_, emitted, _) = feed(&[line]);
        assert!(
            emitted
                .iter()
                .any(|e| matches!(e, StreamEvent::ThinkingDelta(t) if t == "reasoning...")),
            "expected ThinkingDelta, got {emitted:?}"
        );
    }

    #[test]
    fn stream_invalid_json_warns_and_continues() {
        // The agent loop must not crash on a malformed line — log a warn
        // and keep reading. (Tracing capture is wired via `tracing_test`
        // elsewhere; here we just assert no panic + no event.)
        let (_, emitted, done) = feed(&[r#"data: not-json"#]);
        assert!(emitted.is_empty());
        assert!(!done);
    }

    #[test]
    fn stream_empty_lines_and_event_prefixes_ignored() {
        // The SSE driver typically strips these before calling us, but the
        // parser still has to be robust — a comment or `event:` line must
        // not be misinterpreted as data.
        let (_, emitted, done) = feed(&["", ": heartbeat", "event: ping"]);
        assert!(emitted.is_empty());
        assert!(!done);
    }

    #[test]
    fn stream_finish_reason_stop_terminates_stream() {
        let lines = &[
            r#"data: {"candidates":[{"content":{"parts":[{"text":"ok"}]}}]}"#,
            r#"data: {"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#,
        ];
        let (state, _, done) = feed(lines);
        assert!(done, "STOP must terminate the stream");
        assert_eq!(state.accumulator.finish_reason.as_deref(), Some("STOP"));
    }

    #[test]
    fn stream_done_sentinel_terminates_even_though_gemini_does_not_send_it() {
        // Defensive: some relays inject `data: [DONE]`. We must still close
        // out the stream cleanly rather than try to JSON-parse it — AND
        // emit `StreamEvent::Done` so the agent loop's terminal flag flips.
        // If we drop the Done event here, `sse.rs` returns on `SseAction::Done`
        // *without* synthesizing one (only EOF does that), and the agent
        // logs "stream ended unexpectedly" on every turn.
        let (_, emitted, done) = feed(&["data: [DONE]"]);
        assert!(done);
        assert!(
            emitted.iter().any(|e| matches!(e, StreamEvent::Done)),
            "expected StreamEvent::Done so stream_handler reaches Terminal, got {emitted:?}"
        );
    }

    #[test]
    fn stream_finish_reason_stop_emits_terminal_done_event() {
        // The full happy-path SSE termination: STOP must produce a
        // `StreamEvent::Done` in the emitted events so the agent loop's
        // `received_terminal` flips. Without this, the SSE driver ships
        // the events and returns without `sse.rs` injecting an EOF-Done,
        // because `parse_gemini_sse_line` returned `SseAction::Done(events)`
        // — the `Ok(None)` branch in `sse.rs:94` is only reached when the
        // upstream socket actually EOFs *before* a terminal `SseAction::Done`.
        let line = r#"data: {"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#;
        let (_, emitted, done) = feed(&[line]);
        assert!(done, "STOP must terminate the stream");
        assert!(
            emitted.iter().any(|e| matches!(e, StreamEvent::Done)),
            "STOP must emit StreamEvent::Done so stream_handler reaches Terminal, got {emitted:?}"
        );
    }

    #[test]
    fn stream_finish_reason_max_tokens_emits_truncated_then_done() {
        // MAX_TOKENS: must emit BOTH `Truncated` (sets the truncation flag
        // for the next-turn hint) AND `Done` (terminates the loop). The
        // order matters — `Done` short-circuits, so it must come last.
        let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"cut"}]},"finishReason":"MAX_TOKENS"}]}"#;
        let (_, emitted, done) = feed(&[line]);
        assert!(done);
        let kinds: Vec<&str> = emitted
            .iter()
            .map(|e| match e {
                StreamEvent::Truncated => "Truncated",
                StreamEvent::Done => "Done",
                StreamEvent::TextDelta(_) => "TextDelta",
                _ => "Other",
            })
            .collect();
        let trunc_idx = kinds.iter().position(|k| *k == "Truncated");
        let done_idx = kinds.iter().position(|k| *k == "Done");
        assert!(trunc_idx.is_some(), "expected Truncated, got {kinds:?}");
        assert!(done_idx.is_some(), "expected Done, got {kinds:?}");
        assert!(
            trunc_idx < done_idx,
            "Truncated must precede Done so the flag is set before terminal"
        );
    }

    // ── M4 dispatch wiring regression guards ─────────────────────────────

    #[test]
    fn gemini_provider_is_not_openai_compatible_after_m4_flip() {
        // The whole point of M4: Gemini stops sharing the OpenAI dispatch
        // path so the native transport (and inline_data video parts) reach
        // the model. If someone reverts `is_openai_compatible()` to true,
        // Gemini silently goes back through the lossy façade.
        assert!(!crate::llm::provider::Provider::Gemini.is_openai_compatible());
    }

    #[test]
    fn gemini_native_transport_protocol_label_is_distinct() {
        // The dispatcher selects transports by protocol/identity; the
        // native transport's label must not collide with `openai-compat`
        // or `anthropic`, otherwise debug logging and lifecycle telemetry
        // misattribute Gemini requests.
        let t = super::super::transport::gemini_transport();
        assert_eq!(t.protocol_name(), "gemini-native");
    }

    #[test]
    fn accumulate_function_call_pushes_then_appends_on_same_id() {
        // Today AI Studio ships the full `args` object in one SSE frame, so
        // the live path always pushes a fresh entry. This guards the
        // append branch for any future proxy/SDK that splits a single call
        // across frames with a stable id — the prior bytes must concat,
        // not get silently discarded.
        let mut pending: Vec<PendingFunctionCall> = Vec::new();
        accumulate_function_call(&mut pending, "gem_call_foo_0", "foo", r#"{"x":"#);
        accumulate_function_call(&mut pending, "gem_call_foo_0", "foo", r#""bar"}"#);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "gem_call_foo_0");
        assert_eq!(pending[0].name, "foo");
        assert_eq!(pending[0].args_text, r#"{"x":"bar"}"#);

        // Distinct ids — including the legitimate "same tool twice in one
        // turn" case from the spec — still produce separate entries.
        accumulate_function_call(&mut pending, "gem_call_foo_1", "foo", r#"{"y":1}"#);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[1].args_text, r#"{"y":1}"#);
    }
}
