//! Native Gemini AI Studio adapter (generateContent / streamGenerateContent).
//!
//! Vertex AI's Gemini variant is intentionally *not* served by this adapter
//! — Vertex auth (service-account OAuth, regionalised endpoints) lives in
//! `crate::llm::vertex`. This file is for the AI Studio API-key endpoint at
//! `generativelanguage.googleapis.com` only.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::transport::{Transport, TransportRequest, TransportResponse};
use super::{FailoverReason, LlmError, Message, Role, StreamEvent, ToolDefinition, UsageData};
use crate::sse::SseAction;
use crate::types::{ContentPart, FunctionCall, MessageContent, ToolCall};

/// Placeholder Google documents for a replayed Gemini 3+ function call whose
/// real `thoughtSignature` is unavailable (history produced by another model,
/// or a signature this process never saw). Without *some* signature on every
/// replayed `functionCall` the API answers 400 INVALID_ARGUMENT.
pub(crate) const SKIP_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

/// Upper bound on remembered thought signatures. Each one is a few hundred
/// bytes to a few KB; 4096 covers far more tool calls than any live context
/// window can replay, and the oldest fall off first.
const THOUGHT_SIGNATURE_CAP: usize = 4096;

/// Thought signatures Gemini 3+ attaches to `functionCall` parts, keyed by
/// the synthetic call id Borg minted for the call.
///
/// The signature must travel back verbatim when the call is replayed in a
/// later turn, but `ToolCall` carries only id/name/arguments and is built in
/// every provider adapter and dozens of call sites. Rather than widen that
/// type, the adapter keeps the signature beside the id here. The call id is
/// what survives in the `Message` (and in the DB), so replay looks it up by
/// id. The map is process-local: after a daemon restart a replayed call
/// falls back to [`SKIP_THOUGHT_SIGNATURE`], which Google accepts.
struct ThoughtSignatures {
    order: VecDeque<String>,
    by_call_id: HashMap<String, String>,
}

static THOUGHT_SIGNATURES: LazyLock<Mutex<ThoughtSignatures>> = LazyLock::new(|| {
    Mutex::new(ThoughtSignatures {
        order: VecDeque::new(),
        by_call_id: HashMap::new(),
    })
});

fn remember_thought_signature(call_id: &str, signature: &str) {
    if signature.is_empty() {
        return;
    }
    let mut store = THOUGHT_SIGNATURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if store
        .by_call_id
        .insert(call_id.to_string(), signature.to_string())
        .is_none()
    {
        store.order.push_back(call_id.to_string());
    }
    while store.order.len() > THOUGHT_SIGNATURE_CAP {
        if let Some(evicted) = store.order.pop_front() {
            store.by_call_id.remove(&evicted);
        }
    }
}

fn thought_signature_for(call_id: &str) -> Option<String> {
    THOUGHT_SIGNATURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .by_call_id
        .get(call_id)
        .cloned()
}

/// Monotonic component of [`response_nonce`]; the wall clock supplies the
/// cross-restart part.
static CALL_NONCE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A short token unique to one Gemini response, mixed into every synthetic
/// call id that response produces. Gemini mints no call ids of its own; a
/// bare `name_index` scheme repeats every turn, and the Anthropic adapter's
/// history repair treats a repeated id as a replayed call after a fallback.
fn response_nonce() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let counter = CALL_NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{millis:x}{counter:x}")
}

/// `(major, minor)` of a Gemini model id, or `None` for ids that carry no
/// version (`gemini-embedding-001`, non-Gemini slugs). A compat host prefixes
/// the vendor (`google/gemini-3-pro`), so the scan starts at `gemini-`
/// wherever it sits. Undated `gemini-<tier>-latest` aliases always resolve
/// to Google's newest generation and report [`GEMINI_LATEST_ALIAS_VERSION`].
pub(crate) fn gemini_version(model: &str) -> Option<(u32, u32)> {
    let lc = model.to_ascii_lowercase();
    let start = lc.find("gemini-")? + "gemini-".len();
    let rest = &lc[start..];
    let digits_end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(rest.len());
    let version = &rest[..digits_end];
    if version.is_empty() {
        return rest
            .ends_with("-latest")
            .then_some(GEMINI_LATEST_ALIAS_VERSION);
    }
    let mut it = version.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

/// Version reported for `gemini-flash-latest`-style aliases: whatever Google
/// serves there is the newest generation, so every ">= 3" gate must apply.
pub(crate) const GEMINI_LATEST_ALIAS_VERSION: (u32, u32) = (u32::MAX, 0);

/// Gemini 3 and every later generation: steers thinking with `thinkingLevel`,
/// requires `id` on replayed `functionCall`/`functionResponse` parts, and
/// validates `thoughtSignature` on replayed function calls.
pub(crate) fn gemini_is_v3_or_newer(model: &str) -> bool {
    gemini_version(model).is_some_and(|v| v >= (3, 0))
}

/// Default AI Studio base URL. Override via `BORG_GEMINI_BASE_URL` (for tests
/// pointing at `wiremock`) or via `llm.base_url` for self-hosted relays.
const DEFAULT_GEMINI_BASE: &str = "https://generativelanguage.googleapis.com";

/// Env var name read by [`GeminiTransport::resolve_base`] to override the
/// default endpoint at runtime. Documented so the test harness has a single
/// place to look.
pub(crate) const GEMINI_BASE_URL_ENV: &str = "BORG_GEMINI_BASE_URL";

pub(crate) struct GeminiTransport;

/// SSE accumulator for Gemini's `streamGenerateContent?alt=sse` endpoint.
pub(crate) struct GeminiStreamState {
    /// Accumulator for the assistant message under construction.
    pub(crate) accumulator: AssistantAccumulator,
    /// Per-response token mixed into every synthetic call id this stream
    /// mints (see [`response_nonce`]).
    nonce: String,
}

impl Default for GeminiStreamState {
    fn default() -> Self {
        Self {
            accumulator: AssistantAccumulator::default(),
            nonce: response_nonce(),
        }
    }
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
/// we mint one of the form `gem_call_<name>_<index>_<nonce>` at finalisation
/// time. The id stays stable for the next user turn's `functionResponse`
/// matching.
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
        // Deliberately does NOT include `?key=...` in the URL. The caller
        // layer threads credentials in via its existing auth path (mirroring
        // every other transport) — either the API-key header (`x-goog-api-key`)
        // or the query param depending on operator config; both are accepted by
        // AI Studio. We keep the endpoint pure here so URL-shape tests don't depend on a
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
            // non-Gemini provider through this transport (realistically only a
            // stray test wiring). Continuing keeps the stream alive instead of
            // crashing the agent loop on a misconfiguration.
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

/// Build the full Gemini `generateContent` request body from a Borg
/// `TransportRequest`. Pure function — every shape decision is in one place
/// so the unit tests can exercise it end-to-end without spawning the
/// transport.
fn build_gemini_request(request: &TransportRequest<'_>) -> Value {
    let (contents, system_instruction) =
        build_contents_and_system(request.messages, ReplayShape::for_model(request.model));

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
/// (Gemini accepts only one); `Role::User`/`Assistant` map to `user`/`model`
/// turns and `Role::Tool` results fold into a `functionResponse` user turn.
fn build_contents_and_system(
    messages: &[Message],
    replay: ReplayShape,
) -> (Vec<Value>, Option<String>) {
    let mut system_chunks: Vec<String> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    // We need the previous assistant turn's function calls so a following
    // `Role::Tool` can fill in `functionResponse.name` (and, on Gemini 3+,
    // `id`). Pre-3 Gemini matches function-call ↔ response by name +
    // ordinal, not by id.
    let mut last_assistant_calls: Vec<ReplayedCall> = Vec::new();
    // Tool *messages* answered since the last assistant turn — the ordinal
    // fallback when a result carries no id the assistant turn knows.
    let mut tool_results_seen: usize = 0;
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
                let (parts, calls) = message_to_model_parts(msg, replay);
                last_assistant_calls = calls;
                tool_results_seen = 0;
                if !parts.is_empty() {
                    contents.push(json!({"role": "model", "parts": parts}));
                }
            }
            Role::Tool => {
                // Match this tool result to its prior call by id first: the
                // agent persists results for rejected calls ahead of the
                // executed ones, so result order is not call order. A result
                // with no usable id falls back to its ordinal among the tool
                // *messages* since the assistant turn (a multimodal result
                // expands to several parts, so the part count is not it).
                let by_id = msg
                    .tool_call_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .and_then(|id| last_assistant_calls.iter().find(|c| c.id == *id));
                let call = by_id
                    .or_else(|| last_assistant_calls.get(tool_results_seen))
                    .cloned()
                    .unwrap_or_else(|| {
                        // No matching call — log + fall back to a synthetic
                        // name so the request is still well-formed. Silently
                        // dropping the result would mask the agent's progress.
                        tracing::warn!(
                            "gemini adapter: tool result has no matching prior functionCall — using placeholder name"
                        );
                        ReplayedCall {
                            name: "unknown_tool".to_string(),
                            id: msg.tool_call_id.clone().unwrap_or_default(),
                        }
                    });
                tool_results_seen += 1;
                pending_tool_responses.extend(message_to_tool_response_parts(msg, &call, replay));
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

/// How replayed function calls are shaped on the wire, decided once per
/// request from the model id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplayShape {
    /// Gemini ≤ 2.x: `functionCall`/`functionResponse` carry no `id`, and the
    /// API rejects an unknown `thoughtSignature` field.
    Legacy,
    /// Gemini 3+: every replayed `functionCall` carries its `id` and a
    /// `thoughtSignature` (the real one when known, else the documented
    /// placeholder); each `functionResponse` echoes the `id`.
    Versioned,
}

impl ReplayShape {
    fn for_model(model: &str) -> Self {
        if gemini_is_v3_or_newer(model) {
            Self::Versioned
        } else {
            Self::Legacy
        }
    }
}

/// A prior assistant function call, remembered so the matching tool result
/// can be addressed back to it.
#[derive(Clone, Debug)]
struct ReplayedCall {
    name: String,
    id: String,
}

fn message_to_tool_response_parts(
    msg: &Message,
    call: &ReplayedCall,
    replay: ReplayShape,
) -> Vec<Value> {
    let content_text = msg.text_content().unwrap_or("").to_string();
    let mut function_response = json!({
        "name": call.name,
        "response": {"content": content_text},
    });
    if replay == ReplayShape::Versioned && !call.id.is_empty() {
        function_response["id"] = json!(call.id);
    }
    let mut parts = vec![json!({ "functionResponse": function_response })];

    // Multimodal tool outputs need both the Gemini functionResponse and the
    // media parts. `video_analyze` relies on this path to make the video bytes
    // visible to the next Gemini turn.
    if let Some(MessageContent::Parts(content_parts)) = &msg.content {
        parts.extend(
            content_parts
                .iter()
                .filter(|part| !matches!(part, ContentPart::Text(_)))
                .map(content_part_to_value),
        );
    }

    parts
}

/// Returns (parts, function calls in order). The call list lets the following
/// `Role::Tool` messages fill in `functionResponse.name` (and `id`).
fn message_to_model_parts(msg: &Message, replay: ReplayShape) -> (Vec<Value>, Vec<ReplayedCall>) {
    let mut parts: Vec<Value> = Vec::new();
    let mut calls: Vec<ReplayedCall> = Vec::new();

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
            let mut function_call = json!({
                "name": tc.function.name,
                "args": args_value,
            });
            let mut part = serde_json::Map::new();
            if replay == ReplayShape::Versioned {
                function_call["id"] = json!(tc.id);
                // Gemini 3+ validates the thought signature on every replayed
                // call. The real one was captured when the call was parsed;
                // a call this process never saw (daemon restart, history from
                // another model) gets Google's documented skip marker.
                let signature =
                    thought_signature_for(&tc.id).unwrap_or_else(|| SKIP_THOUGHT_SIGNATURE.into());
                part.insert("thoughtSignature".into(), json!(signature));
            }
            part.insert("functionCall".into(), function_call);
            parts.push(Value::Object(part));
            calls.push(ReplayedCall {
                name: tc.function.name.clone(),
                id: tc.id.clone(),
            });
        }
    }

    (parts, calls)
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
            // Surfaced as a text placeholder; SSRF-safe URL fetch belongs in
            // the vision_analyze path, not the LLM adapter.
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

/// Collapse a JSON-Schema array `type` (`"type": ["string", "null"]`) into the
/// single-type form Gemini's subset accepts, carrying the `"null"` member over
/// to `nullable: true`.
///
/// Draft 2020-12 lets `type` be a list, and MCP tool schemas use it routinely
/// for optional fields. Gemini rejects it with an INVALID_ARGUMENT that kills
/// the *whole* request — every tool in the catalog goes down with the one
/// offending parameter — so the union is narrowed rather than passed through.
///
/// A genuine multi-type union (`["string", "number"]`) cannot be expressed:
/// `anyOf` is stripped a few lines above as another key Gemini's subset
/// rejects. The first non-null member wins and the drop is logged, matching
/// the surrounding trade-off — lose one constraint, not the turn. Every exit
/// leaves a single-type `type` behind: a list must never reach the wire.
///
/// Like the rest of the sanitizer this runs on every object the recursion
/// visits, so a non-schema payload that happens to carry a `type` key holding
/// a list (inside `default` or `examples`) is rewritten too. That is the
/// existing shape of the whole pass, not a new hazard.
fn collapse_type_array(map: &mut serde_json::Map<String, Value>) {
    let Some(Value::Array(members)) = map.get("type") else {
        return;
    };
    let mut nullable = false;
    let mut concrete: Vec<String> = Vec::new();
    for member in members {
        match member.as_str() {
            Some("null") => nullable = true,
            Some(other) => concrete.push(other.to_string()),
            // A non-string member is not a type name. Returning here would
            // leave the list on the wire and 400 the whole request — the exact
            // failure this exists to prevent — so drop the member and narrow
            // to what is left.
            None => tracing::warn!(
                "gemini adapter: ignoring non-type-name member {} in a `type` array",
                member
            ),
        }
    }
    if concrete.is_empty() {
        tracing::warn!(
            "gemini adapter: `type` array carried no usable type name; defaulting to `string`"
        );
    }
    if concrete.len() > 1 {
        tracing::warn!(
            "gemini adapter: narrowed union type {:?} to `{}` — Gemini's schema subset has no \
             union form",
            concrete,
            concrete[0]
        );
    }
    // `["null"]` alone still needs a type Gemini accepts; `string` is the
    // widest scalar and the value is nullable anyway.
    let narrowed = concrete
        .first()
        .cloned()
        .unwrap_or_else(|| "string".to_string());
    map.insert("type".to_string(), Value::String(narrowed));
    if nullable {
        // The `null` member is the authoritative signal: it beats a
        // `nullable: false` the producer may have emitted alongside it.
        map.insert("nullable".to_string(), Value::Bool(true));
    }
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
        // Gemini requires `enum` members to be strings even when the declared
        // parameter type is numeric or boolean — `"enum": [1, 2]` is an
        // INVALID_ARGUMENT that kills the whole request (schemas arriving via
        // MCP tools commonly use typed enums).
        if let Some(Value::Array(members)) = map.get_mut("enum") {
            for member in members.iter_mut() {
                match member {
                    Value::String(_) => {}
                    Value::Number(n) => *member = Value::String(n.to_string()),
                    Value::Bool(b) => *member = Value::String(b.to_string()),
                    other => *other = Value::String(other.to_string()),
                }
            }
        }
        collapse_type_array(map);
        for (_, v) in map.iter_mut() {
            sanitize_schema_for_gemini(v);
        }
    } else if let Value::Array(arr) = value {
        for v in arr.iter_mut() {
            sanitize_schema_for_gemini(v);
        }
    }
}

/// Largest `maxOutputTokens` the thinking-capable Gemini models accept.
const GEMINI_MAX_OUTPUT_TOKENS: u32 = 65_535;

/// Room left for visible content above a known thinking budget, matching the
/// headroom the Anthropic path already reserves.
const GEMINI_THINKING_HEADROOM_TOKENS: u32 = 1_024;

/// `maxOutputTokens` for a request, raised so thinking cannot consume the whole
/// budget.
///
/// Gemini bills thought tokens against `maxOutputTokens`, unlike Anthropic
/// where the thinking budget sits under a separate ceiling. A configured cap of
/// 16K with `thinking = High` therefore asks the model to spend its entire
/// output allowance on thoughts and return nothing — the turn comes back empty
/// with no error. The cap is only ever raised, never lowered, so an operator
/// who deliberately asked for a larger one keeps it.
///
/// The trade-off is deliberate: with thinking on, `llm.max_tokens` stops being
/// a cost lever, because the two settings are in direct conflict and returning
/// nothing is the worse failure. Raising costs nothing by itself —
/// `maxOutputTokens` is a limit, not a reservation, and only 2.5/3 models reach
/// this path, both of which accept the ceiling. The override is logged so it is
/// findable under `RUST_LOG=debug` rather than silent.
fn max_output_tokens(configured: u32, thinking_budget: Option<i32>, thinking_on: bool) -> u32 {
    if !thinking_on {
        return configured;
    }
    // A known positive budget tells us exactly how much room the thoughts need.
    // A dynamic budget (-1) and Gemini 3's opaque `thinkingLevel` do not, so
    // fall back to the model ceiling.
    let needed = match thinking_budget {
        Some(budget) if budget > 0 => u32::try_from(budget)
            .unwrap_or(GEMINI_MAX_OUTPUT_TOKENS)
            .saturating_add(GEMINI_THINKING_HEADROOM_TOKENS)
            .min(GEMINI_MAX_OUTPUT_TOKENS),
        _ => GEMINI_MAX_OUTPUT_TOKENS,
    };
    if needed > configured {
        tracing::debug!(
            configured,
            raised_to = needed,
            thinking_budget,
            "raising Gemini maxOutputTokens: thoughts bill against it, so llm.max_tokens \
             would leave no room for a visible reply"
        );
    }
    configured.max(needed)
}

/// The `thinkingConfig` a request sends, plus what it implies for the output
/// cap.
struct ThinkingWire {
    /// Wire value for `generationConfig.thinkingConfig`.
    config: Value,
    /// Explicit 2.5 `thinkingBudget` when one is known; `None` for the opaque
    /// `thinkingLevel` enum.
    budget: Option<i32>,
    /// True when the operator asked for thoughts, so `maxOutputTokens` must
    /// leave room for them. False when the config only pins a model that
    /// cannot stop thinking to its floor — the operator said Off, and the
    /// configured cap stays theirs.
    raises_cap: bool,
}

/// Lowest `thinkingLevel` every Gemini 3+ model accepts. There is no "off":
/// with no `thinkingConfig` at all these models think at HIGH and spend the
/// output cap on thoughts, so Off maps to this floor instead of silence.
const GEMINI_3_THINKING_FLOOR: &str = "LOW";

/// Resolve the `thinkingConfig` a model wants.
///
/// `None` means the request carries no `thinkingConfig` — either thinking is
/// off on a family that can turn it off (2.5), or the family takes no
/// `thinkingConfig` at all (1.5 and friends, which Google rejects the field
/// on). Gemini 3 and later always get one, keyed on a parsed version number
/// rather than a `gemini-3` substring so Gemini 4 and the `-latest` aliases
/// are not silently treated as legacy.
///
/// Kept separate from [`generation_config`] because the OpenAI-compatible wire
/// needs the same *cap* answer without the native request shape: a Gemini slug
/// routed through OpenRouter or a compat relay bills thoughts against
/// `max_tokens` exactly the same way, and answering that question twice is how
/// the two paths drift apart. The thinking *level* is the one place the two
/// wires still differ — see [`openai_compat_max_tokens`].
fn thinking_config(model: &str, cfg: &crate::config::LlmConfig) -> Option<ThinkingWire> {
    let level = cfg.effective_thinking(model);
    let version = gemini_version(model)?;
    if version >= (3, 0) {
        // Gemini 3+ takes an opaque `thinkingLevel` ("LOW"/"MEDIUM"/"HIGH")
        // — no numeric budget to read, and no way to disable it.
        return Some(match level.gemini_3_level() {
            Some(wire_level) => ThinkingWire {
                config: json!({ "thinkingLevel": wire_level }),
                budget: None,
                raises_cap: true,
            },
            None => ThinkingWire {
                config: json!({ "thinkingLevel": GEMINI_3_THINKING_FLOOR }),
                budget: None,
                raises_cap: false,
            },
        });
    }
    if !level.is_enabled() || version < (2, 5) {
        return None;
    }
    // Gemini 2.5 takes `thinkingBudget` (integer; -1 = dynamic).
    let budget = level.gemini_2_5_budget()?;
    Some(ThinkingWire {
        config: json!({ "thinkingBudget": budget }),
        budget: Some(budget),
        raises_cap: true,
    })
}

/// The output-token cap to send on the *OpenAI-compatible* wire for `model`.
///
/// `Provider::Gemini` takes the native path, but the same models arrive through
/// OpenRouter and OpenAI-compatible relays, and Google bills thought tokens
/// against the output cap on that surface too. Without this the compat path
/// keeps the configured 16K, thinking eats it, and the turn returns empty —
/// the identical failure the native path already fixed. Non-Gemini models are
/// returned untouched.
///
/// Known drift: the compat body steers thinking through `reasoning_effort`,
/// which `build_openai_request` derives from the configured level alone and
/// omits for Off. The native path pins Gemini 3+ to `thinkingLevel: LOW` on
/// Off (those models cannot stop thinking); the compat path sends nothing, so
/// OpenRouter runs the model at its own default level. Closing it means
/// mapping Off to `reasoning_effort: "low"` when [`gemini_is_v3_or_newer`]
/// holds, at the `reasoning_effort` site in the compat request builder — this
/// adapter only owns the cap.
pub(crate) fn openai_compat_max_tokens(model: &str, cfg: &crate::config::LlmConfig) -> u32 {
    match thinking_config(model, cfg) {
        Some(wire) if wire.raises_cap => max_output_tokens(cfg.max_tokens, wire.budget, true),
        _ => cfg.max_tokens,
    }
}

fn generation_config(request: &TransportRequest<'_>) -> Value {
    let cfg = &request.llm_config;
    let mut obj = serde_json::Map::new();
    obj.insert("temperature".into(), json!(cfg.temperature));

    let resolved = thinking_config(request.model, cfg);
    let thinking_budget = resolved.as_ref().and_then(|wire| wire.budget);
    let thinking_on = resolved.as_ref().is_some_and(|wire| wire.raises_cap);
    if let Some(wire) = resolved {
        obj.insert("thinkingConfig".into(), wire.config);
    }
    obj.insert(
        "maxOutputTokens".into(),
        json!(max_output_tokens(
            cfg.max_tokens,
            thinking_budget,
            thinking_on
        )),
    );

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

    // The non-streaming wire has no event channel, so a filter stop or a
    // malformed call can only surface as an error here. Both are typed: the
    // caller downcasts to `LlmError`, and a bare `anyhow` error would be
    // classified `Format` — a content refusal must not bench the credential
    // as a malformed request. `MAX_TOKENS` and `CONTINUATION` keep whatever
    // text arrived, as every other provider's non-streaming path does.
    if let Some(finish) = candidate.get("finishReason").and_then(Value::as_str) {
        match classify_finish_reason(finish) {
            FinishOutcome::Filtered => {
                return Err(anyhow::Error::new(LlmError::Fatal {
                    source: anyhow::anyhow!("gemini candidate blocked: {finish}"),
                    reason: FailoverReason::ContentPolicy,
                    status: None,
                }));
            }
            FinishOutcome::MalformedFunctionCall => {
                return Err(anyhow::Error::new(LlmError::Fatal {
                    source: anyhow::anyhow!(
                        "gemini returned a malformed function call ({finish}); the model's tool call could not be parsed"
                    ),
                    reason: FailoverReason::MalformedToolCall,
                    status: None,
                }));
            }
            FinishOutcome::Stop | FinishOutcome::Truncated => {}
            FinishOutcome::Unknown => {
                tracing::warn!(
                    provider,
                    model,
                    "gemini: unknown finishReason `{finish}` — treating as STOP"
                );
            }
        }
    }

    let parts = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let nonce = response_nonce();
    let mut text_accum = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    for part in &parts {
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
            let id = synth_call_id(&name, tool_calls.len(), &nonce);
            if let Some(signature) = part.get("thoughtSignature").and_then(Value::as_str) {
                remember_thought_signature(&id, signature);
            }
            tool_calls.push(ToolCall {
                id,
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
        seq: None,
        reasoning_blocks: None,
        reasoning_content: None,
        reasoning_model: None,
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
            response_id: None,
            upstream: None,
        }
    });

    Ok(TransportResponse { message, usage })
}

/// Synthesize a stable id for a Gemini function call so subsequent tool
/// results can be matched back to it. The streaming path uses the same
/// scheme to stay consistent across streaming/non-streaming. `nonce` is
/// unique per response (see [`response_nonce`]) so two turns that call the
/// same tool never share an id.
fn synth_call_id(name: &str, idx: usize, nonce: &str) -> String {
    format!("gem_call_{name}_{idx}_{nonce}")
}

/// What a Gemini `finishReason` means for the turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FinishOutcome {
    /// `STOP`: a complete reply.
    Stop,
    /// `MAX_TOKENS`, or `CONTINUATION` (a long decode cut at the per-request
    /// cap): the output is incomplete.
    Truncated,
    /// A safety or policy filter stopped the candidate. The text that arrived
    /// is kept; the stop is reported, not treated as a normal end.
    Filtered,
    /// `MALFORMED_FUNCTION_CALL`: the model emitted a tool call the API could
    /// not parse. An error, never a silent empty turn.
    MalformedFunctionCall,
    /// Anything else: logged and treated as `STOP`.
    Unknown,
}

/// Finish reasons that mean a filter, not the model, ended the candidate.
const GEMINI_FILTERED_FINISH_REASONS: &[&str] = &[
    "SAFETY",
    "RECITATION",
    "PROHIBITED_CONTENT",
    "BLOCKLIST",
    "SPII",
    "IMAGE_SAFETY",
];

fn classify_finish_reason(reason: &str) -> FinishOutcome {
    match reason {
        "STOP" => FinishOutcome::Stop,
        "MAX_TOKENS" | "CONTINUATION" => FinishOutcome::Truncated,
        "MALFORMED_FUNCTION_CALL" => FinishOutcome::MalformedFunctionCall,
        r if GEMINI_FILTERED_FINISH_REASONS.contains(&r) => FinishOutcome::Filtered,
        _ => FinishOutcome::Unknown,
    }
}

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

    if let Some(parts) = value
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
    {
        for part in parts {
            // Gemini 2.5 Thinking surfaces reasoning as `{"thought": "..."}`
            // or `{"thought": true, "text": "..."}`. Surface it on its own
            // channel so the TUI can dim it the way it does Anthropic thinking.
            if let Some(thought_value) = part.get("thought") {
                let thought = match thought_value {
                    Value::String(s) => Some(s.as_str()),
                    Value::Bool(true) => part.get("text").and_then(Value::as_str),
                    _ => None,
                };
                if let Some(thought) = thought {
                    if !thought.is_empty() {
                        events.push(StreamEvent::ThinkingDelta(thought.to_string()));
                    }
                }
                continue;
            }
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    state.accumulator.text.push_str(text);
                    events.push(StreamEvent::TextDelta(text.to_string()));
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
                let id = synth_call_id(&name, idx, &state.nonce);
                if let Some(signature) = part.get("thoughtSignature").and_then(Value::as_str) {
                    remember_thought_signature(&id, signature);
                }
                state
                    .accumulator
                    .pending_function_calls
                    .push(PendingFunctionCall {
                        id: id.clone(),
                        name: name.clone(),
                        args_text: args_text.clone(),
                    });
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

    if let Some(reason) = value
        .pointer("/candidates/0/finishReason")
        .and_then(Value::as_str)
    {
        state.accumulator.finish_reason = Some(reason.to_string());
        match classify_finish_reason(reason) {
            FinishOutcome::Stop => {}
            FinishOutcome::Truncated => {
                events.push(StreamEvent::Truncated);
            }
            FinishOutcome::Filtered => {
                // Mid-stream candidate-level block — keep the text that
                // arrived, report the filter, and stop.
                events.push(StreamEvent::ContentFiltered {
                    reason: reason.to_string(),
                });
                done_after = true;
            }
            FinishOutcome::MalformedFunctionCall => {
                events.push(StreamEvent::Error(format!(
                    "gemini returned a malformed function call ({reason}); the model's tool call could not be parsed"
                )));
                done_after = true;
            }
            FinishOutcome::Unknown => {
                tracing::warn!(
                    provider,
                    model,
                    "gemini SSE: unknown finishReason `{reason}` — treating as STOP"
                );
            }
        }
    }

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
            response_id: None,
            upstream: None,
        };
        state.accumulator.usage = Some(usage.clone());
        events.push(StreamEvent::Usage(usage));
    }

    if state.accumulator.finish_reason.is_some() && state.accumulator.usage.is_some() {
        done_after = true;
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
            system_stable_prefix: None,
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
            seq: None,
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
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

    fn assistant_call(id: &str, name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: id.into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: name.into(),
                    arguments: "{}".into(),
                },
            }]),
            tool_call_id: None,
            timestamp: None,
            seq: None,
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
        }
    }

    /// The model-turn part carrying the replayed `functionCall`, and the
    /// following user-turn `functionResponse`, for a one-call history.
    fn replayed_call_and_response(model: &str, assistant: Message) -> (Value, Value) {
        let msgs = vec![
            Message::user("go"),
            assistant.clone(),
            Message::tool_result(&assistant.tool_calls.as_ref().unwrap()[0].id, "ok"),
        ];
        let cfg = default_cfg();
        let body = build_gemini_request(&make_request_for_model(model, &msgs, &cfg));
        let contents = body["contents"].as_array().unwrap();
        let call_part = contents
            .iter()
            .flat_map(|c| c["parts"].as_array().unwrap().iter())
            .find(|p| p.get("functionCall").is_some())
            .expect("replayed functionCall part")
            .clone();
        let response = contents
            .iter()
            .flat_map(|c| c["parts"].as_array().unwrap().iter())
            .find_map(|p| p.get("functionResponse"))
            .expect("functionResponse part")
            .clone();
        (call_part, response)
    }

    /// Gemini 3+ rejects replayed history whose `functionCall` parts lack a
    /// `thoughtSignature` (400 INVALID_ARGUMENT). The signature captured when
    /// the call was parsed must travel back verbatim, and the call id must be
    /// echoed on both the call and its response.
    #[test]
    fn replay_on_gemini_3_sends_the_captured_thought_signature_and_ids() {
        for (label, parsed_call) in [
            (
                "non-streaming",
                parse_gemini_response(
                    json!({"candidates": [{"content": {"parts": [
                        {"functionCall": {"name": "fs_read", "args": {"path": "a"}},
                         "thoughtSignature": "sig-from-model"}
                    ]}, "finishReason": "STOP"}]}),
                    "gemini",
                    "gemini-3-pro",
                )
                .unwrap()
                .message
                .tool_calls
                .unwrap()
                .remove(0),
            ),
            ("streaming", {
                let line = r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"fs_read","args":{"path":"a"}},"thoughtSignature":"sig-from-model"}]}}]}"#;
                let (_, emitted, _) = feed(&[line]);
                let id = emitted
                    .iter()
                    .find_map(|e| match e {
                        StreamEvent::ToolCallDelta { id, .. } => id.clone(),
                        _ => None,
                    })
                    .expect("tool call delta");
                ToolCall {
                    id,
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "fs_read".into(),
                        arguments: "{}".into(),
                    },
                }
            }),
        ] {
            let id = parsed_call.id.clone();
            let (call_part, response) =
                replayed_call_and_response("gemini-3-pro", assistant_call(&id, "fs_read"));
            assert_eq!(
                call_part["thoughtSignature"], "sig-from-model",
                "{label}: the real signature must be replayed"
            );
            assert_eq!(call_part["functionCall"]["id"], id, "{label}");
            assert_eq!(
                response["id"], id,
                "{label}: functionResponse must echo the id"
            );
            assert_eq!(response["name"], "fs_read", "{label}");
        }
    }

    /// A call this process never saw (history from another model, daemon
    /// restart) still needs *a* signature on Gemini 3+: Google documents a
    /// placeholder that skips validation. Gemini 2.5 rejects the field, and
    /// takes no ids, so it must receive neither.
    #[test]
    fn replay_without_a_signature_sends_placeholder_on_gemini_3_and_nothing_on_2_5() {
        let cases = [
            ("gemini-3-pro", true),
            ("gemini-4-flash", true),
            ("gemini-flash-latest", true),
            ("gemini-2.5-flash", false),
            ("gemini-2.5-pro", false),
        ];
        for (model, versioned) in cases {
            let (call_part, response) =
                replayed_call_and_response(model, assistant_call("call_from_claude", "fs_read"));
            if versioned {
                assert_eq!(
                    call_part["thoughtSignature"], SKIP_THOUGHT_SIGNATURE,
                    "{model}: an unknown signature must fall back to the placeholder"
                );
                assert_eq!(
                    call_part["functionCall"]["id"], "call_from_claude",
                    "{model}"
                );
                assert_eq!(response["id"], "call_from_claude", "{model}");
            } else {
                assert!(
                    call_part.get("thoughtSignature").is_none(),
                    "{model}: pre-3 models reject thoughtSignature"
                );
                assert!(
                    call_part["functionCall"].get("id").is_none(),
                    "{model}: pre-3 models take no functionCall.id"
                );
                assert!(
                    response.get("id").is_none(),
                    "{model}: pre-3 models take no functionResponse.id"
                );
            }
            assert_eq!(call_part["functionCall"]["name"], "fs_read", "{model}");
        }
    }

    fn assistant_calls(calls: &[(&str, &str)]) -> Message {
        Message {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(
                calls
                    .iter()
                    .map(|(id, name)| ToolCall {
                        id: (*id).into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: (*name).into(),
                            arguments: "{}".into(),
                        },
                    })
                    .collect(),
            ),
            tool_call_id: None,
            timestamp: None,
            seq: None,
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
        }
    }

    /// Every `functionResponse` in the request, in wire order, as
    /// `(name, id, content)`.
    fn function_responses(body: &Value) -> Vec<(String, Option<String>, String)> {
        body["contents"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|c| c["parts"].as_array().unwrap().iter())
            .filter_map(|p| p.get("functionResponse"))
            .map(|fr| {
                (
                    fr["name"].as_str().unwrap().to_string(),
                    fr.get("id").and_then(Value::as_str).map(str::to_string),
                    fr["response"]["content"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// Tool results pair with their call by `tool_call_id`, never by
    /// position: the agent persists rejected-call results ahead of executed
    /// ones, and a multimodal result expands to several parts. Either would
    /// put the wrong name — and on Gemini 3+, the wrong `id` — on the wire.
    #[test]
    fn tool_results_pair_with_calls_by_id_across_multimodal_and_reordered_results() {
        let cfg = default_cfg();
        let image = ContentPart::ImageBase64 {
            media: MediaData {
                mime_type: "image/png".into(),
                data: "AAAA".into(),
                filename: None,
            },
        };
        let calls = &[("call_a", "vision_analyze"), ("call_b", "fs_read")];
        let cases: [(&str, Vec<Message>); 2] = [
            (
                "first result multimodal",
                vec![
                    Message::tool_result_multimodal(
                        "call_a",
                        vec![ContentPart::Text("a cat".into()), image.clone()],
                    ),
                    Message::tool_result("call_b", "file body"),
                ],
            ),
            (
                "results arrive swapped",
                vec![
                    Message::tool_result("call_b", "file body"),
                    Message::tool_result("call_a", "a cat"),
                ],
            ),
        ];
        for (label, results) in cases {
            let mut msgs = vec![Message::user("go"), assistant_calls(calls)];
            msgs.extend(results);
            for model in ["gemini-3-pro", "gemini-2.5-flash"] {
                let body = build_gemini_request(&make_request_for_model(model, &msgs, &cfg));
                let responses = function_responses(&body);
                assert_eq!(responses.len(), 2, "{label} / {model}: {responses:?}");
                let versioned = gemini_is_v3_or_newer(model);
                let expect_id = |id: &str| versioned.then(|| id.to_string());
                let by_content: Vec<_> = responses
                    .iter()
                    .map(|(name, id, content)| (content.as_str(), (name.as_str(), id.clone())))
                    .collect();
                for (content, name, id) in [
                    ("a cat", "vision_analyze", "call_a"),
                    ("file body", "fs_read", "call_b"),
                ] {
                    let found = by_content
                        .iter()
                        .find(|(c, _)| *c == content)
                        .map(|(_, pair)| pair.clone())
                        .unwrap_or_else(|| panic!("{label} / {model}: no response for {content}"));
                    assert_eq!(
                        found,
                        (name, expect_id(id)),
                        "{label} / {model}: result `{content}` must address its own call"
                    );
                }
            }
        }
    }

    #[test]
    fn build_request_tool_result_preserves_multimodal_parts() {
        let assistant_with_call = Message {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_video".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "video_analyze".into(),
                    arguments: r#"{"video_url":"data:video/mp4;base64,AAAA"}"#.into(),
                },
            }]),
            tool_call_id: None,
            timestamp: None,
            seq: None,
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
        };
        let tool_result = Message::tool_result_multimodal(
            "call_video",
            vec![
                ContentPart::Text("Video from data URI (12 bytes).".into()),
                ContentPart::VideoBase64 {
                    media: MediaData {
                        mime_type: "video/mp4".into(),
                        data: "AAAA".into(),
                        filename: Some("clip.mp4".into()),
                    },
                },
            ],
        );
        let cfg = default_cfg();
        let body = build_gemini_request(&make_request(
            &[Message::user("describe"), assistant_with_call, tool_result],
            None,
            &cfg,
        ));

        let tool_turn = body["contents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| {
                c["parts"]
                    .as_array()
                    .map(|parts| parts.iter().any(|p| p.get("functionResponse").is_some()))
                    .unwrap_or(false)
            })
            .expect("tool response turn");
        let parts = tool_turn["parts"].as_array().unwrap();
        assert!(parts.iter().any(|p| p.get("functionResponse").is_some()));
        let inline = parts
            .iter()
            .find_map(|p| p.get("inline_data"))
            .expect("video inline_data must survive tool result conversion");
        assert_eq!(inline["mime_type"], "video/mp4");
        assert_eq!(inline["data"], "AAAA");
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
            seq: None,
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
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
            seq: None,
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
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

    /// Draft 2020-12 array `type` is what MCP tools emit for optional fields.
    /// Gemini rejects it outright, so it must be narrowed before the request
    /// leaves — otherwise the whole tool catalog 400s on one parameter.
    #[test]
    fn sanitize_schema_collapses_array_typed_parameters() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "note": {"type": ["string", "null"]},
                // The `null` member wins over the producer's `nullable: false`.
                "tag": {"nullable": false, "type": ["null", "string"]},
                "count": {"type": ["integer"]},
                // No union form exists in Gemini's subset: first member wins.
                "mixed": {"type": ["number", "string"]},
                "empty": {"type": ["null"]},
                "plain": {"type": "string"},
            }
        });
        sanitize_schema_for_gemini(&mut schema);
        let props = &schema["properties"];

        assert_eq!(props["note"]["type"], "string");
        assert_eq!(props["note"]["nullable"], true);

        assert_eq!(props["tag"]["type"], "string");
        assert_eq!(props["tag"]["nullable"], true);

        assert_eq!(props["count"]["type"], "integer");
        assert!(props["count"].get("nullable").is_none());

        assert_eq!(props["mixed"]["type"], "number");
        assert!(props["mixed"].get("nullable").is_none());

        // `["null"]` alone still needs a type Gemini accepts.
        assert_eq!(props["empty"]["type"], "string");
        assert_eq!(props["empty"]["nullable"], true);

        // A scalar type is untouched.
        assert_eq!(props["plain"]["type"], "string");

        // Nothing anywhere in the emitted schema still carries a list type.
        fn has_array_type(v: &Value) -> bool {
            match v {
                Value::Object(m) => {
                    m.get("type").is_some_and(Value::is_array) || m.values().any(has_array_type)
                }
                Value::Array(a) => a.iter().any(has_array_type),
                _ => false,
            }
        }
        assert!(!has_array_type(&schema));
    }

    /// Array types nested under `items` reach the wire too — the recursion
    /// must cover them, not just top-level properties.
    #[test]
    fn sanitize_schema_collapses_array_types_inside_items() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "rows": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"id": {"type": ["string", "null"]}}
                    }
                }
            }
        });
        sanitize_schema_for_gemini(&mut schema);
        let id = &schema["properties"]["rows"]["items"]["properties"]["id"];
        assert_eq!(id["type"], "string");
        assert_eq!(id["nullable"], true);
    }

    /// A malformed `type` array must still leave a single type behind.
    /// Bailing out would put the list on the wire and 400 the whole request,
    /// which is the failure the collapse exists to prevent.
    #[test]
    fn sanitize_schema_narrows_malformed_type_arrays() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                // A non-type-name member is dropped, not a reason to give up.
                "mixed": {"type": ["string", 5]},
                "only_junk": {"type": [5, true]},
                "empty": {"type": []},
                "nested_null": {"type": [{"a": 1}, "null"]},
            }
        });
        sanitize_schema_for_gemini(&mut schema);
        let props = &schema["properties"];

        assert_eq!(props["mixed"]["type"], "string");
        // Nothing usable is left, so the widest scalar stands in.
        assert_eq!(props["only_junk"]["type"], "string");
        assert_eq!(props["empty"]["type"], "string");
        assert_eq!(props["nested_null"]["type"], "string");
        assert_eq!(props["nested_null"]["nullable"], true);

        for name in ["mixed", "only_junk", "empty", "nested_null"] {
            assert!(
                props[name]["type"].is_string(),
                "{name} must not carry a list type on the wire"
            );
        }
    }

    #[test]
    fn build_request_sanitize_schema_stringifies_typed_enum_members() {
        // Gemini rejects non-string enum members with INVALID_ARGUMENT even
        // when the declared type is numeric/boolean, and MCP-sourced schemas
        // use typed enums freely. Nested sites (properties, items) must be
        // covered by the recursive walk; string enums stay untouched.
        let mut schema = json!({
            "type": "object",
            "properties": {
                "level": {"type": "integer", "enum": [1, 2, 3]},
                "ratio": {"type": "number", "enum": [0.5, 1.5]},
                "active": {"type": "boolean", "enum": [true, false]},
                "mode": {"type": "string", "enum": ["fast", "slow"]},
                "tags": {"type": "array", "items": {"type": "integer", "enum": [10, 20]}},
            }
        });
        sanitize_schema_for_gemini(&mut schema);
        assert_eq!(
            schema["properties"]["level"]["enum"],
            json!(["1", "2", "3"])
        );
        assert_eq!(schema["properties"]["ratio"]["enum"], json!(["0.5", "1.5"]));
        assert_eq!(
            schema["properties"]["active"]["enum"],
            json!(["true", "false"])
        );
        assert_eq!(
            schema["properties"]["mode"]["enum"],
            json!(["fast", "slow"])
        );
        assert_eq!(
            schema["properties"]["tags"]["items"]["enum"],
            json!(["10", "20"])
        );
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

    /// Build a Gemini transport request for an explicit model id (lets the
    /// thinking-config tests probe both 2.5 and 3 branches without spinning
    /// up a whole client).
    fn make_request_for_model<'a>(
        model: &'a str,
        messages: &'a [Message],
        cfg: &'a LlmConfig,
    ) -> TransportRequest<'a> {
        TransportRequest {
            provider: Provider::Gemini,
            model,
            messages,
            tools: None,
            llm_config: cfg,
            prompt_cache_key: None,
            system_stable_prefix: None,
            cost_events_enabled: false,
        }
    }

    #[test]
    fn build_request_omits_thinking_config_when_level_off() {
        // Default config has thinking = Off. The thinkingConfig field MUST
        // NOT appear — Google rejects requests with `thinkingBudget: 0` on
        // some 2.5 model variants, and Gemini 1.5 rejects the field
        // entirely. Off must mean "send nothing".
        let cfg = default_cfg();
        assert_eq!(cfg.thinking, crate::config::ThinkingLevel::Off);
        let body = build_gemini_request(&make_request_for_model(
            "gemini-2.5-pro",
            &[Message::user("hi")],
            &cfg,
        ));
        assert!(
            body["generationConfig"].get("thinkingConfig").is_none(),
            "thinkingConfig must be absent when thinking is Off"
        );
    }

    #[test]
    fn build_request_gemini_2_5_emits_thinking_budget_for_high() {
        // High on Gemini 2.5 must serialize as `thinkingBudget: 16384` and
        // MUST NOT leak the Gemini-3-shaped `thinkingLevel` field.
        let mut cfg = default_cfg();
        cfg.thinking = crate::config::ThinkingLevel::High;
        let body = build_gemini_request(&make_request_for_model(
            "gemini-2.5-pro",
            &[Message::user("hi")],
            &cfg,
        ));
        let tc = &body["generationConfig"]["thinkingConfig"];
        assert_eq!(tc["thinkingBudget"], 16384);
        assert!(
            tc.get("thinkingLevel").is_none(),
            "Gemini 2.5 must not receive the Gemini-3 thinkingLevel key"
        );
    }

    #[test]
    fn build_request_gemini_2_5_adaptive_emits_dynamic_sentinel() {
        // The whole point of the Adaptive level on Gemini 2.5: send
        // `thinkingBudget: -1` so the model decides at runtime. Anyone who
        // "fixes" the negative number to 0 silently disables thinking.
        let mut cfg = default_cfg();
        cfg.thinking = crate::config::ThinkingLevel::Adaptive;
        let body = build_gemini_request(&make_request_for_model(
            "gemini-2.5-flash",
            &[Message::user("hi")],
            &cfg,
        ));
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"], -1,
            "Adaptive must hit Gemini's dynamic-thinking sentinel"
        );
    }

    /// Gemini charges thought tokens against `maxOutputTokens`, so a cap at or
    /// below the thinking budget leaves nothing for the answer and the turn
    /// comes back empty with no error. The cap must clear the budget whenever
    /// thinking is on, and must never shrink what the operator configured.
    #[test]
    fn thinking_raises_the_output_cap_above_the_thought_budget() {
        // (configured cap, thinking budget, thinking on, expected cap)
        let cases: &[(u32, Option<i32>, bool, u32)] = &[
            // Thinking off: the configured cap is sent untouched.
            (4_096, None, false, 4_096),
            (4_096, Some(16_384), false, 4_096),
            // A known budget: room for the thoughts plus content headroom.
            (4_096, Some(16_384), true, 16_384 + 1_024),
            (1_024, Some(512), true, 512 + 1_024),
            // A cap that already clears the budget is left alone.
            (60_000, Some(2_048), true, 60_000),
            (4_096, Some(512), true, 4_096),
            // An unknowable spend (dynamic budget, or Gemini 3's opaque
            // level) has to assume the worst, since guessing low is the bug.
            (4_096, Some(-1), true, GEMINI_MAX_OUTPUT_TOKENS),
            (4_096, None, true, GEMINI_MAX_OUTPUT_TOKENS),
            // Never above what the API accepts.
            (4_096, Some(i32::MAX), true, GEMINI_MAX_OUTPUT_TOKENS),
        ];
        for (configured, budget, on, expected) in cases {
            assert_eq!(
                max_output_tokens(*configured, *budget, *on),
                *expected,
                "cap {configured} with budget {budget:?} (thinking={on})"
            );
        }
    }

    /// The raised cap has to reach the wire, not just the helper: this is the
    /// assertion that fails if `generation_config` ever writes `maxOutputTokens`
    /// before deciding whether thinking is on.
    #[test]
    fn build_request_sends_the_raised_cap_when_thinking_is_on() {
        let mut cfg = default_cfg();
        cfg.max_tokens = 4_096;
        cfg.thinking = crate::config::ThinkingLevel::High;
        let body = build_gemini_request(&make_request_for_model(
            "gemini-2.5-pro",
            &[Message::user("hi")],
            &cfg,
        ));
        let gc = &body["generationConfig"];
        assert_eq!(gc["thinkingConfig"]["thinkingBudget"], 16_384);
        assert_eq!(
            gc["maxOutputTokens"], 17_408,
            "a 4096 cap under a 16384 thought budget leaves no room for an answer"
        );
    }

    #[test]
    fn build_request_gemini_3_emits_thinking_level_not_budget() {
        // Medium on Gemini 3 must serialize as `thinkingLevel: "MEDIUM"`
        // (the only shape 3.x accepts) and never as a numeric budget.
        let mut cfg = default_cfg();
        cfg.thinking = crate::config::ThinkingLevel::Medium;
        let body = build_gemini_request(&make_request_for_model(
            "gemini-3-flash-preview",
            &[Message::user("hi")],
            &cfg,
        ));
        let tc = &body["generationConfig"]["thinkingConfig"];
        assert_eq!(tc["thinkingLevel"], "MEDIUM");
        assert!(
            tc.get("thinkingBudget").is_none(),
            "Gemini 3 must not receive the 2.5 thinkingBudget key"
        );
    }

    #[test]
    fn build_request_per_model_override_routes_through_to_gemini() {
        // End-to-end: global thinking = Off but the per-model override
        // bumps every gemini-* model to High. The Gemini body MUST honor
        // the override, otherwise per-model defaults are decorative.
        let mut cfg = default_cfg();
        cfg.thinking = crate::config::ThinkingLevel::Off;
        cfg.thinking_overrides
            .insert("gemini".into(), crate::config::ThinkingLevel::High);
        let body = build_gemini_request(&make_request_for_model(
            "gemini-2.5-pro",
            &[Message::user("hi")],
            &cfg,
        ));
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"], 16384,
            "per-model override must beat global thinking=Off"
        );
    }

    /// The thinking gate keys on a parsed version, not a `gemini-3` substring,
    /// so a new generation or an undated `-latest` alias is never treated as
    /// legacy, and a compat vendor prefix does not hide the version.
    #[test]
    fn gemini_version_parses_generation_and_latest_aliases() {
        let cases = [
            ("gemini-3-pro", Some((3, 0))),
            ("gemini-3.1-flash-preview", Some((3, 1))),
            ("gemini-4-flash", Some((4, 0))),
            ("google/gemini-4-flash", Some((4, 0))),
            ("gemini-2.5-pro", Some((2, 5))),
            ("gemini-2.0-flash", Some((2, 0))),
            ("gemini-1.5-flash-latest", Some((1, 5))),
            ("gemini-flash-latest", Some(GEMINI_LATEST_ALIAS_VERSION)),
            ("gemini-pro-latest", Some(GEMINI_LATEST_ALIAS_VERSION)),
            ("gemini-embedding-001", None),
            ("claude-sonnet-4-5", None),
        ];
        for (model, expected) in cases {
            assert_eq!(gemini_version(model), expected, "{model}");
        }
        for (model, v3) in [
            ("gemini-3-pro", true),
            ("gemini-4-flash", true),
            ("gemini-flash-latest", true),
            ("gemini-2.5-pro", false),
            ("gemini-1.5-pro", false),
        ] {
            assert_eq!(gemini_is_v3_or_newer(model), v3, "{model}");
        }
    }

    /// Gemini 3+ cannot stop thinking: with no `thinkingConfig` it runs at
    /// HIGH and spends the output cap on thoughts. Borg's default (Off) must
    /// therefore pin the floor level, while 2.5 keeps sending nothing. The
    /// pinned floor is not a request for thoughts, so the configured output
    /// cap must stay as the operator set it.
    #[test]
    fn thinking_off_pins_the_floor_level_on_gemini_3_and_later_only() {
        let cfg = default_cfg();
        assert_eq!(cfg.thinking, crate::config::ThinkingLevel::Off);
        let cases = [
            ("gemini-3-pro", Some("LOW")),
            ("gemini-3.1-flash-preview", Some("LOW")),
            ("gemini-4-flash", Some("LOW")),
            ("google/gemini-4-flash", Some("LOW")),
            ("gemini-flash-latest", Some("LOW")),
            ("gemini-2.5-pro", None),
            ("gemini-2.5-flash", None),
            ("gemini-1.5-pro", None),
        ];
        for (model, expected_level) in cases {
            let body = build_gemini_request(&make_request_for_model(model, &[], &cfg));
            let gc = &body["generationConfig"];
            match expected_level {
                Some(level) => {
                    assert_eq!(gc["thinkingConfig"]["thinkingLevel"], level, "{model}");
                    assert!(
                        gc["thinkingConfig"].get("thinkingBudget").is_none(),
                        "{model}: a 3+ model must never see thinkingBudget"
                    );
                }
                None => assert!(
                    gc.get("thinkingConfig").is_none(),
                    "{model}: thinking Off must send no thinkingConfig"
                ),
            }
            assert_eq!(
                gc["maxOutputTokens"], cfg.max_tokens,
                "{model}: Off must not raise the output cap"
            );
            assert_eq!(
                openai_compat_max_tokens(model, &cfg),
                cfg.max_tokens,
                "{model}: the compat wire must not raise the cap either"
            );
        }
    }

    /// An explicit level on Gemini 4 and the `-latest` aliases takes the same
    /// `thinkingLevel` enum as Gemini 3 and raises the cap like it does.
    #[test]
    fn explicit_thinking_level_reaches_gemini_4_and_latest_aliases() {
        let mut cfg = default_cfg();
        cfg.thinking = crate::config::ThinkingLevel::Medium;
        for model in ["gemini-4-flash", "gemini-flash-latest", "gemini-3-pro"] {
            let body = build_gemini_request(&make_request_for_model(model, &[], &cfg));
            let gc = &body["generationConfig"];
            assert_eq!(gc["thinkingConfig"]["thinkingLevel"], "MEDIUM", "{model}");
            assert!(
                gc["maxOutputTokens"].as_u64().unwrap() > u64::from(cfg.max_tokens),
                "{model}: thinking on must raise the cap"
            );
        }
    }

    #[test]
    fn build_request_gemini_1_5_omits_thinking_config_even_when_level_set() {
        // Gemini 1.5 doesn't support `thinkingConfig` at all — sending it
        // would surface a Google 400. When the operator has thinking
        // enabled globally but the active model is a non-thinking family,
        // we silently drop the field rather than failing the request.
        let mut cfg = default_cfg();
        cfg.thinking = crate::config::ThinkingLevel::High;
        let body = build_gemini_request(&make_request_for_model(
            "gemini-1.5-pro",
            &[Message::user("hi")],
            &cfg,
        ));
        assert!(
            body["generationConfig"].get("thinkingConfig").is_none(),
            "Gemini 1.5 must never receive thinkingConfig"
        );
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
                let id = id.as_deref().expect("synthetic id");
                assert!(
                    id.starts_with("gem_call_vision_analyze_0_"),
                    "id must carry name, ordinal and a response nonce: {id}"
                );
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
        let (state, emitted, _) = feed(lines);
        let ids: Vec<&str> = emitted
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallDelta { id, .. } => id.as_deref(),
                _ => None,
            })
            .collect();
        let nonce = &state.nonce;
        assert_eq!(
            ids,
            vec![
                format!("gem_call_a_0_{nonce}").as_str(),
                format!("gem_call_b_1_{nonce}").as_str()
            ],
            "calls in one response share the nonce and differ by ordinal"
        );
    }

    #[test]
    fn stream_two_responses_mint_distinct_synthetic_ids_for_the_same_call() {
        // Gemini mints no call ids. If two turns that call the same tool
        // reuse `gem_call_<name>_0`, the Anthropic adapter's history repair
        // treats the second as a replay of the first after a fallback and
        // drops it. Every response must get its own nonce.
        let line = r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"fs_read","args":{}}}]}}]}"#;
        let first_id = |emitted: &[StreamEvent]| {
            emitted
                .iter()
                .find_map(|e| match e {
                    StreamEvent::ToolCallDelta { id, .. } => id.clone(),
                    _ => None,
                })
                .expect("tool call delta")
        };
        let (_, first, _) = feed(&[line]);
        let (_, second, _) = feed(&[line]);
        assert_ne!(first_id(&first), first_id(&second));

        let resp = || {
            json!({"candidates": [{"content": {"parts": [
                {"functionCall": {"name": "fs_read", "args": {}}}
            ]}, "finishReason": "STOP"}]})
        };
        let a = parse_gemini_response(resp(), "gemini", "gemini-3-pro").unwrap();
        let b = parse_gemini_response(resp(), "gemini", "gemini-3-pro").unwrap();
        assert_ne!(
            a.message.tool_calls.unwrap()[0].id,
            b.message.tool_calls.unwrap()[0].id
        );
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
        assert!(
            !done,
            "finishReason without usageMetadata should not stop before a later usage frame or EOF"
        );
        assert!(emitted.iter().any(|e| matches!(e, StreamEvent::Truncated)));
    }

    /// Every Gemini `finishReason` maps to one stream outcome. A filter stop
    /// must reach the agent as `ContentFiltered` (the same path OpenAI's
    /// `content_filter` and Anthropic's `refusal` take), a malformed tool
    /// call must be an error rather than a silent empty turn, and a
    /// `CONTINUATION` cut is a truncation like `MAX_TOKENS`.
    #[test]
    fn stream_finish_reasons_map_to_the_documented_outcomes() {
        #[derive(Debug, PartialEq)]
        enum Expect {
            Filtered,
            Error,
            Truncated,
            PlainStop,
        }
        let cases = [
            ("SAFETY", Expect::Filtered),
            ("RECITATION", Expect::Filtered),
            ("PROHIBITED_CONTENT", Expect::Filtered),
            ("BLOCKLIST", Expect::Filtered),
            ("SPII", Expect::Filtered),
            ("IMAGE_SAFETY", Expect::Filtered),
            ("MALFORMED_FUNCTION_CALL", Expect::Error),
            ("MAX_TOKENS", Expect::Truncated),
            ("CONTINUATION", Expect::Truncated),
            ("STOP", Expect::PlainStop),
            ("SOME_FUTURE_REASON", Expect::PlainStop),
        ];
        for (reason, expect) in cases {
            let line = format!(
                r#"data: {{"candidates":[{{"content":{{"parts":[{{"text":"partial"}}]}},"finishReason":"{reason}"}}]}}"#
            );
            let (_, emitted, done) = feed(&[line.as_str()]);
            let filtered = emitted.iter().find_map(|e| match e {
                StreamEvent::ContentFiltered { reason } => Some(reason.as_str()),
                _ => None,
            });
            let errored = emitted
                .iter()
                .any(|e| matches!(e, StreamEvent::Error(m) if m.contains(reason)));
            let truncated = emitted.iter().any(|e| matches!(e, StreamEvent::Truncated));
            assert!(
                emitted
                    .iter()
                    .any(|e| matches!(e, StreamEvent::TextDelta(t) if t == "partial")),
                "{reason}: text that arrived before the stop must be kept"
            );
            match expect {
                Expect::Filtered => {
                    assert_eq!(
                        filtered,
                        Some(reason),
                        "{reason} must surface as ContentFiltered"
                    );
                    assert!(!errored && !truncated, "{reason} is a filter, not an error");
                    assert!(done, "{reason} must end the stream");
                }
                Expect::Error => {
                    assert!(errored, "{reason} must surface as an Error event");
                    assert!(filtered.is_none() && !truncated);
                    assert!(done, "{reason} must end the stream");
                }
                Expect::Truncated => {
                    assert!(truncated, "{reason} must surface as Truncated");
                    assert!(filtered.is_none() && !errored);
                }
                Expect::PlainStop => {
                    assert!(
                        filtered.is_none() && !errored && !truncated,
                        "{reason} must be a plain stop, got {emitted:?}"
                    );
                }
            }
        }
    }

    /// The non-streaming parser has no event channel, so the same reasons
    /// must become errors (filters, malformed calls) or keep the reply
    /// (truncations, stop, unknown). The errors are typed: the client
    /// downcasts to `LlmError`, and a bare error would be classified
    /// `Format`, benching the credential for a content refusal.
    #[test]
    fn parse_response_finish_reasons_map_to_errors_or_replies() {
        let cases = [
            ("SAFETY", Some(("blocked", FailoverReason::ContentPolicy))),
            (
                "RECITATION",
                Some(("blocked", FailoverReason::ContentPolicy)),
            ),
            (
                "PROHIBITED_CONTENT",
                Some(("blocked", FailoverReason::ContentPolicy)),
            ),
            (
                "BLOCKLIST",
                Some(("blocked", FailoverReason::ContentPolicy)),
            ),
            ("SPII", Some(("blocked", FailoverReason::ContentPolicy))),
            (
                "IMAGE_SAFETY",
                Some(("blocked", FailoverReason::ContentPolicy)),
            ),
            (
                "MALFORMED_FUNCTION_CALL",
                Some(("malformed function call", FailoverReason::MalformedToolCall)),
            ),
            ("MAX_TOKENS", None),
            ("CONTINUATION", None),
            ("STOP", None),
            ("SOME_FUTURE_REASON", None),
        ];
        for (reason, expect_err) in cases {
            let resp = json!({
                "candidates": [{"content": {"parts": [{"text": "partial"}]}, "finishReason": reason}]
            });
            let result = parse_gemini_response(resp, "gemini", "gemini-2.5-flash");
            match expect_err {
                Some((needle, failover)) => {
                    let err = result
                        .err()
                        .unwrap_or_else(|| panic!("{reason} must be an error"));
                    let msg = err.to_string();
                    assert!(
                        msg.contains(needle) && msg.contains(reason),
                        "{reason}: error must name the cause, got `{msg}`"
                    );
                    let classified = err
                        .downcast::<LlmError>()
                        .unwrap_or_else(|e| panic!("{reason}: must be a typed LlmError, got {e}"));
                    assert!(
                        matches!(classified, LlmError::Fatal { .. }),
                        "{reason}: a candidate verdict is not retryable"
                    );
                    assert_eq!(classified.reason(), failover, "{reason}");
                }
                None => {
                    let parsed = result.unwrap_or_else(|e| panic!("{reason} must parse: {e}"));
                    assert_eq!(parsed.message.text_content(), Some("partial"), "{reason}");
                }
            }
        }
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
    fn stream_thinking_delta_surfaced_for_legacy_bool_shape() {
        let line = r#"data: {"candidates":[{"content":{"parts":[{"thought":true,"text":"legacy thought"}]}}]}"#;
        let (_, emitted, _) = feed(&[line]);
        assert!(
            emitted
                .iter()
                .any(|e| matches!(e, StreamEvent::ThinkingDelta(t) if t == "legacy thought")),
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
    fn stream_finish_reason_waits_for_later_usage_metadata() {
        let lines = &[
            r#"data: {"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}]}"#,
            r#"data: {"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5}}"#,
        ];
        let (_, emitted, done) = feed(lines);
        assert!(
            done,
            "usage frame after finishReason should terminate the stream"
        );
        let kinds: Vec<&str> = emitted
            .iter()
            .map(|e| match e {
                StreamEvent::TextDelta(_) => "TextDelta",
                StreamEvent::Usage(_) => "Usage",
                StreamEvent::Done => "Done",
                _ => "Other",
            })
            .collect();
        assert_eq!(kinds, vec!["TextDelta", "Usage", "Done"]);
    }

    #[test]
    fn stream_finish_reason_max_tokens_emits_truncated_then_done() {
        // MAX_TOKENS: must emit BOTH `Truncated` (sets the truncation flag
        // for the next-turn hint) AND `Done` (terminates the loop). The
        // order matters — `Done` short-circuits, so it must come last.
        let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"cut"}]},"finishReason":"MAX_TOKENS"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#;
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

    #[test]
    fn gemini_provider_is_not_openai_compatible_after_m4_flip() {
        // Gemini must stop sharing the OpenAI dispatch
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
}
