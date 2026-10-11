//! Message-format helpers for Anthropic and OpenAI-compatible providers.
//!
//! Extracted from `llm/mod.rs` — these functions convert internal `Message`s
//! into provider-specific JSON, normalize usage/cache token fields across
//! provider dialects, and adapt content (stripping images for non-vision
//! models).

use anyhow::{Context, Result};

use crate::types::{Message, Role, ToolCall};

/// Sanitize messages for OpenAI-compatible providers (OpenRouter, Gemini, DeepSeek, etc.).
///
/// - Replaces `None` content with empty string on assistant tool-call messages
///   (some backends reject `"content": null`).
/// - Drops empty user/assistant messages that carry no semantic value — these
///   shouldn't be in history (agent.rs guards against creating them), but legacy
///   sessions or edge cases may still have them. Gemini rejects messages with no
///   content parts.
pub(super) fn sanitize_openai_messages(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|m| {
            let content_empty = m
                .content
                .as_ref()
                .map(crate::types::MessageContent::is_blank)
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

/// Repair a replayed/resumed message history so it satisfies strict
/// OpenAI-compatible providers (DeepSeek, Kimi, OpenRouter, …). Live turns are
/// already well-formed — the live loop substitutes synthetic tool results at
/// dispatch — so this guards histories reconstructed from the session log or
/// carried across a provider fallback, where three invalid shapes arise and
/// 400 the request:
///
/// 1. **Duplicate `tool_call_id`** — a resumed or merged history can repeat an
///    id across assistant `tool_calls` or `tool` results (DeepSeek: "Duplicate
///    value"). An *exact replay* (same id, same call payload — a re-appended
///    turn) is dropped, keeping the first. A colliding id with a *different*
///    payload (id-reusing backends, long-context degradation) is renamed
///    deterministically instead, together with its matching result, so no
///    call/result pair is silently lost.
/// 2. **Orphaned `tool` result** — a `Role::Tool` message whose `tool_call_id`
///    matches no *preceding* assistant `tool_calls` entry (the assistant turn
///    that requested it was compacted away). Dropped.
/// 3. **Consecutive `assistant` turns** — two adjacent assistant messages with
///    no intervening tool/user message (an interrupted turn re-appended on
///    resume). Coalesced into one: text joined, `tool_calls` unioned.
///
/// A well-formed history is returned unchanged — every pass is a no-op on it.
pub(super) fn repair_message_sequence(messages: &[Message]) -> Vec<Message> {
    let coalesced = coalesce_consecutive_assistants(messages);
    prune_tool_call_ids(&coalesced)
}

/// True if `m`'s content is plain text or empty (not multimodal `Parts`). Only
/// text/empty assistants are coalesced, so multimodal content is never mangled.
fn content_is_text_or_empty(m: &Message) -> bool {
    !matches!(m.content, Some(crate::types::MessageContent::Parts(_)))
}

/// Merge runs of adjacent `Role::Assistant` messages into a single message
/// (see [`repair_message_sequence`] case 3). Only text/empty assistants merge.
fn coalesce_consecutive_assistants(messages: &[Message]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for m in messages {
        let can_merge = m.role == Role::Assistant
            && content_is_text_or_empty(m)
            && out
                .last()
                .is_some_and(|p| p.role == Role::Assistant && content_is_text_or_empty(p));
        // `can_merge` already established the previous message exists, so the
        // `Some` arm always binds when merging; the `_` arm handles the rest.
        match out.last_mut() {
            Some(prev) if can_merge => {
                let prev_text = prev
                    .content
                    .as_ref()
                    .and_then(crate::types::MessageContent::text)
                    .unwrap_or("")
                    .to_string();
                let cur_text = m
                    .content
                    .as_ref()
                    .and_then(crate::types::MessageContent::text)
                    .unwrap_or("");
                let joined = match (prev_text.is_empty(), cur_text.is_empty()) {
                    (true, _) => cur_text.to_string(),
                    (false, true) => prev_text,
                    (false, false) => format!("{prev_text}\n{cur_text}"),
                };
                prev.content = if joined.is_empty() {
                    None
                } else {
                    Some(crate::types::MessageContent::Text(joined))
                };
                match (prev.tool_calls.as_mut(), m.tool_calls.as_ref()) {
                    (Some(pc), Some(cc)) => pc.extend(cc.iter().cloned()),
                    (None, Some(cc)) => prev.tool_calls = Some(cc.clone()),
                    _ => {}
                }
            }
            _ => out.push(m.clone()),
        }
    }
    out
}

/// Repair duplicate `tool_call_id`s and drop orphaned `tool` results (see
/// [`repair_message_sequence`] cases 1 and 2) in a single forward scan.
///
/// A colliding call id splits into two cases:
/// - **Exact replay** — same id *and* same `(name, arguments)` payload as the
///   first occurrence (a resumed/merged history re-appended the turn). The
///   later copy and its duplicate result are dropped; nothing is lost.
/// - **Id reuse** — same id, *different* payload (id-reusing
///   OpenAI-compatible backends, long-context degradation). Dropping here
///   would silently lose a real call's output, so the later call is renamed
///   with a stable occurrence-indexed `_d<n>` suffix and its matching result
///   (results pair by id, in emission order) is renamed the same way. The
///   suffix is deterministic — same input → same renames — so replayed
///   histories stay byte-stable for prompt-cache prefixing.
///
/// A rename only happens when the history actually carries an *unclaimed*
/// result for that id. Renaming a call no result can pair with would trade one
/// invalid shape (duplicate id) for another (an assistant `tool_call` with no
/// `tool` message after it), which the same strict providers also 400 on — so
/// in that case the later call is dropped exactly as before.
fn prune_tool_call_ids(messages: &[Message]) -> Vec<Message> {
    use std::collections::{HashMap, HashSet, VecDeque};
    // How many `tool` results the history carries per id, and how many calls
    // have already claimed one. A rename needs a spare result to pair with.
    let mut results_per_id: HashMap<&str, usize> = HashMap::new();
    for m in messages {
        if m.role == Role::Tool {
            if let Some(id) = m.tool_call_id.as_deref() {
                *results_per_id.entry(id).or_default() += 1;
            }
        }
    }
    let mut claimed_results: HashMap<String, usize> = HashMap::new();
    // Every tool-call id emitted by a *preceding* assistant — a `tool` result
    // is valid only if its id is in here. Accumulates for the whole history
    // since ids are conversation-global.
    let mut seen_call_ids: HashSet<String> = HashSet::new();
    // `(name, arguments)` of the first call kept under each id, to tell an
    // exact replay (safe to drop) from id reuse (must rename).
    let mut first_payload: HashMap<String, (String, String)> = HashMap::new();
    // Renamed ids per original id, FIFO — consumed by later `tool` results
    // still carrying the original id so call/result pairs stay paired.
    let mut pending_result_renames: HashMap<String, VecDeque<String>> = HashMap::new();
    let mut seen_result_ids: HashSet<String> = HashSet::new();
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for m in messages {
        match (&m.role, m.tool_calls.as_ref()) {
            (Role::Assistant, Some(calls)) => {
                let mut kept: Vec<ToolCall> = Vec::new();
                for call in calls {
                    let payload = (call.function.name.clone(), call.function.arguments.clone());
                    if seen_call_ids.insert(call.id.clone()) {
                        first_payload.insert(call.id.clone(), payload);
                        *claimed_results.entry(call.id.clone()).or_default() += 1;
                        kept.push(call.clone());
                        continue;
                    }
                    if first_payload.get(&call.id) == Some(&payload) {
                        // Exact replay of an earlier turn; its duplicate
                        // result is dropped in the Tool arm below.
                        tracing::debug!(
                            id = %call.id,
                            "repair: dropping replayed duplicate tool_call"
                        );
                        continue;
                    }
                    // Id reuse, but no spare result to pair a renamed call
                    // with: keeping it would emit a `tool_call` nothing answers,
                    // which strict providers reject just as hard. Drop it.
                    let available = results_per_id.get(call.id.as_str()).copied().unwrap_or(0);
                    if available <= claimed_results.get(&call.id).copied().unwrap_or(0) {
                        tracing::debug!(
                            id = %call.id,
                            "repair: dropping duplicate tool_call with no result to pair"
                        );
                        continue;
                    }
                    *claimed_results.entry(call.id.clone()).or_default() += 1;
                    // Id reuse: rename deterministically, stepping over any
                    // id that already exists (including a crafted `_d<n>`).
                    let mut n = 1usize;
                    let mut candidate = format!("{}_d{n}", call.id);
                    while !seen_call_ids.insert(candidate.clone()) {
                        n += 1;
                        candidate = format!("{}_d{n}", call.id);
                    }
                    tracing::warn!(
                        id = %call.id,
                        renamed = %candidate,
                        tool = %call.function.name,
                        "repair: duplicate tool_call id with different payload; \
                         renamed call and its result to keep the pair"
                    );
                    first_payload.insert(candidate.clone(), payload);
                    pending_result_renames
                        .entry(call.id.clone())
                        .or_default()
                        .push_back(candidate.clone());
                    kept.push(ToolCall {
                        id: candidate,
                        ..call.clone()
                    });
                }
                let content_empty = m
                    .content
                    .as_ref()
                    .is_none_or(crate::types::MessageContent::is_blank);
                if kept.is_empty() && content_empty {
                    // Assistant reduced to nothing (all calls were exact
                    // replays, no text) — drop it rather than send an empty turn.
                    continue;
                }
                out.push(Message {
                    tool_calls: (!kept.is_empty()).then_some(kept),
                    ..m.clone()
                });
            }
            (Role::Tool, _) => {
                let id = m.tool_call_id.as_deref().unwrap_or("");
                if !seen_call_ids.contains(id) {
                    tracing::debug!(id = %id, "repair: dropping orphaned tool result");
                    continue;
                }
                if seen_result_ids.insert(id.to_string()) {
                    out.push(m.clone());
                    continue;
                }
                // Id already answered: route this result to the next renamed
                // call still waiting for one (results pair by id + order), or
                // drop it as a genuine replayed duplicate.
                let renamed = pending_result_renames.get_mut(id).and_then(|queue| loop {
                    match queue.pop_front() {
                        // Skip renames whose result already arrived under the
                        // renamed id itself (an already-repaired history).
                        Some(r) if seen_result_ids.contains(&r) => {}
                        other => break other,
                    }
                });
                match renamed {
                    Some(renamed_id) => {
                        seen_result_ids.insert(renamed_id.clone());
                        out.push(Message {
                            tool_call_id: Some(renamed_id),
                            ..m.clone()
                        });
                    }
                    None => {
                        tracing::debug!(id = %id, "repair: dropping duplicate tool result");
                    }
                }
            }
            _ => out.push(m.clone()),
        }
    }
    out
}

/// Convert one message's `Parts` content into Anthropic content blocks.
/// Shared by the user-turn and tool-result paths below, which differ only in
/// whether a blank text part is worth keeping (`skip_blank_text`).
fn content_parts_to_anthropic_blocks(
    parts: &[crate::types::ContentPart],
    skip_blank_text: bool,
) -> Vec<serde_json::Value> {
    let mut blocks: Vec<serde_json::Value> = Vec::new();
    for part in parts {
        match part {
            crate::types::ContentPart::Text(t) => {
                if !skip_blank_text || !t.trim().is_empty() {
                    blocks.push(serde_json::json!({
                        "type": "text",
                        "text": t,
                    }));
                }
            }
            crate::types::ContentPart::ImageBase64 { media } => {
                blocks.push(serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media.mime_type,
                        "data": media.data,
                    },
                }));
            }
            crate::types::ContentPart::ImageUrl { url } => {
                blocks.push(serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": "url",
                        "url": url,
                    },
                }));
            }
            crate::types::ContentPart::AudioBase64 { media } => {
                blocks.push(serde_json::json!({
                    "type": "text",
                    "text": format!("[audio: {}]", media.filename.as_deref().unwrap_or("audio")),
                }));
            }
            crate::types::ContentPart::VideoBase64 { media } => {
                blocks.push(serde_json::json!({
                    "type": "text",
                    "text": format!("[video: {}]", media.filename.as_deref().unwrap_or("video")),
                }));
            }
        }
    }
    blocks
}

/// Text pushed when a message would otherwise serialize to no content at all.
///
/// Anthropic and Bedrock Converse both reject an empty *and* a whitespace-only
/// text block, and also reject a message whose content array is empty — so
/// neither `""` nor omission is a legal repair. A short inert token is the only
/// shape both accept. Dropping the message instead is not an option here:
/// `build_anthropic_messages` is a positional mapper, and removing an element
/// mid-array strands a following `tool_result` that merges into
/// `result.last_mut()`.
pub(super) const EMPTY_ASSISTANT_PLACEHOLDER: &str = "(no content)";

/// Convert internal Messages to Anthropic format.
///
/// `replay_model` is the model this request goes to on a native Anthropic
/// route, or `None` when stored reasoning must not be replayed (a
/// third-party endpoint). An assistant turn's signed reasoning blocks lead
/// its content only when they were produced by that same model: the
/// signature is bound to it, and a failover or model switch leaves the
/// reasoning behind.
pub(super) fn build_anthropic_messages(
    messages: &[Message],
    replay_model: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut result: Vec<serde_json::Value> = Vec::new();

    for msg in messages {
        if msg.role == Role::System {
            continue;
        }

        match msg.role {
            Role::User => match &msg.content {
                Some(crate::types::MessageContent::Parts(parts)) => {
                    let mut content_blocks = content_parts_to_anthropic_blocks(parts, true);
                    if content_blocks.is_empty() {
                        content_blocks.push(serde_json::json!({
                            "type": "text",
                            "text": EMPTY_ASSISTANT_PLACEHOLDER,
                        }));
                    }
                    result.push(serde_json::json!({
                        "role": "user",
                        "content": content_blocks,
                    }));
                }
                _ => {
                    let text = msg.text_content().unwrap_or("");
                    let text = if text.trim().is_empty() {
                        EMPTY_ASSISTANT_PLACEHOLDER
                    } else {
                        text
                    };
                    result.push(serde_json::json!({
                        "role": "user",
                        "content": text,
                    }));
                }
            },
            Role::Assistant => {
                let mut content_blocks: Vec<serde_json::Value> = Vec::new();

                let replay_ok = replay_model.is_some()
                    && msg.reasoning_model.as_deref() == replay_model
                    && msg.tool_calls.is_some();
                if let (true, Some(blocks)) = (replay_ok, &msg.reasoning_blocks) {
                    content_blocks.extend(blocks.iter().cloned());
                } else if let (Some(request_model), Some(stored_model), Some(blocks)) = (
                    replay_model,
                    msg.reasoning_model.as_deref(),
                    msg.reasoning_blocks.as_ref(),
                ) {
                    // A native route with signed blocks that cannot go back:
                    // the model changed under the tool loop (failover, a
                    // switch mid-loop). The provider may answer 400 for the
                    // missing thinking ahead of the `tool_use`; name both
                    // models so that 400 is explainable from the log.
                    if msg.tool_calls.is_some() && stored_model != request_model {
                        tracing::warn!(
                            target: "cortex::llm",
                            stored_model,
                            request_model,
                            blocks = blocks.len(),
                            "dropping signed thinking blocks: they were produced by another model and will not verify on this request"
                        );
                    }
                }

                if let Some(text) = msg.text_content() {
                    if !text.trim().is_empty() {
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
                        "text": EMPTY_ASSISTANT_PLACEHOLDER,
                    }));
                }

                result.push(serde_json::json!({
                    "role": "assistant",
                    "content": content_blocks,
                }));
            }
            Role::Tool => {
                // Anthropic tool_result.content accepts either a plain string or an
                // array of {text, image} blocks. Multimodal tool results
                // (e.g. computer_use screenshots) need the array form so images
                // reach the model instead of being flattened.
                let content_value = match &msg.content {
                    Some(crate::types::MessageContent::Parts(parts)) => {
                        let blocks = content_parts_to_anthropic_blocks(parts, true);
                        if blocks.is_empty() {
                            serde_json::json!(EMPTY_ASSISTANT_PLACEHOLDER)
                        } else {
                            serde_json::Value::Array(blocks)
                        }
                    }
                    _ => {
                        let text = msg.text_content().unwrap_or("");
                        if text.trim().is_empty() {
                            serde_json::json!(EMPTY_ASSISTANT_PLACEHOLDER)
                        } else {
                            serde_json::json!(text)
                        }
                    }
                };

                let tool_result_block = serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": msg.tool_call_id.as_deref().unwrap_or(""),
                    "content": content_value,
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
/// Accepts the many field-name variants we've seen in the wild:
///
/// - **Anthropic**: `cache_read_input_tokens`, `cache_creation_input_tokens`
/// - **OpenAI / Responses API**: `prompt_tokens_details.cached_tokens`
/// - **DeepSeek / Kimi / Moonshot**: top-level `cached_tokens`
/// - **DeepSeek direct API**: top-level `prompt_cache_hit_tokens`
/// - **Bedrock alt**: already covered by the Anthropic names
///
/// Missing fields default to 0. Never panics.
pub(crate) fn normalize_cache_tokens(usage: &serde_json::Value) -> (u64, u64) {
    let as_u64 = |v: &serde_json::Value| v.as_u64().unwrap_or(0);
    let cached_read = [
        usage.get("cache_read_input_tokens"),
        usage.get("cached_input_tokens"),
        usage.get("cached_tokens"),
        usage.get("prompt_cache_hit_tokens"),
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
pub(super) fn build_cache_control_marker(ttl: crate::config::CacheTtl) -> serde_json::Value {
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
///
/// `skip_tool_messages` is the OpenRouter-compat escape hatch: marking a
/// `role: tool` message with `cache_control` causes OpenRouter to silently
/// hang the request. Native Anthropic flattens tool-results into user
/// messages, so this never fires there — pass `false` from the Anthropic
/// path. The OpenAI-compat path (used for OpenRouter) keeps `role: tool`
/// messages around and must pass `true`.
pub(super) fn apply_message_cache_control(
    messages: &mut [serde_json::Value],
    rolling_messages: usize,
    marker: &serde_json::Value,
    skip_tool_messages: bool,
) {
    if messages.is_empty() || rolling_messages == 0 {
        return;
    }

    let mut remaining = rolling_messages;
    for msg in messages.iter_mut().rev() {
        if remaining == 0 {
            break;
        }

        if skip_tool_messages && msg.get("role").and_then(serde_json::Value::as_str) == Some("tool")
        {
            // OpenRouter rejects `cache_control` on tool-role messages by
            // silently hanging — skip without consuming a breakpoint so the
            // rolling window keeps walking back.
            continue;
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

/// Attach a `cache_control` marker to a single message's content in place.
/// Used by the OpenAI-compat path to mark the system message (which lives
/// in the messages array, unlike on the native Anthropic path where the
/// system prompt is a top-level field).
///
/// Mirrors the upgrade-string-to-block logic of `apply_message_cache_control`
/// so a plain-string system content gets wrapped before the marker lands.
pub(super) fn apply_cache_control_to_message(
    msg: &mut serde_json::Value,
    marker: &serde_json::Value,
) {
    let Some(content) = msg.get_mut("content") else {
        return;
    };
    if let Some(s) = content.as_str() {
        *content = serde_json::json!([{ "type": "text", "text": s }]);
    }
    let Some(blocks) = content.as_array_mut() else {
        return;
    };
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
        return;
    }
}

/// Parse Anthropic non-streaming response into internal Message.
pub(super) fn parse_anthropic_response(resp: &serde_json::Value) -> Result<Message> {
    let content_blocks = resp["content"]
        .as_array()
        .with_context(|| format!("Missing 'content' array in Anthropic response: {resp}"))?;

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut reasoning_blocks: Vec<serde_json::Value> = Vec::new();

    for block in content_blocks {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(text) = block["text"].as_str() {
                    text_parts.push(text.to_string());
                }
            }
            // Signed reasoning is kept whole: the signature only verifies
            // on the exact bytes the wire returned.
            Some("thinking" | "redacted_thinking") => reasoning_blocks.push(block.clone()),
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
        seq: None,
        reasoning_blocks: (!reasoning_blocks.is_empty()).then_some(reasoning_blocks),
        reasoning_content: None,
        // Stamped by the transport, which knows the model that answered.
        reasoning_model: None,
    })
}

/// Placeholder left where an image was removed. Silence would leave the model
/// believing it had looked at something; naming the omission lets it say so and
/// pick a text-only path (`get_text`, `snapshot`, `capture mode: "ax"`).
///
/// Deliberately worded so it does *not* trip
/// [`crate::llm::llm_error::classify_error_text`]'s image-rejection needles:
/// the notice travels inside the request body, and proxies that echo the
/// offending prompt back in a 4xx would otherwise make an unrelated error look
/// like a fresh image rejection.
pub(super) const IMAGE_OMITTED_NOTICE: &str = "[image omitted: this model reads text only]";

/// Strip image content parts from messages when model doesn't support vision.
/// Each removed image leaves [`IMAGE_OMITTED_NOTICE`] in its place.
/// Simplifies `Parts([Text])` → `Text` after stripping.
pub(super) fn strip_images(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|msg| {
            let content = match &msg.content {
                Some(crate::types::MessageContent::Parts(parts)) => {
                    let filtered: Vec<crate::types::ContentPart> = parts
                        .iter()
                        .map(|p| match p {
                            crate::types::ContentPart::ImageBase64 { .. }
                            | crate::types::ContentPart::ImageUrl { .. } => {
                                crate::types::ContentPart::Text(IMAGE_OMITTED_NOTICE.to_string())
                            }
                            other => other.clone(),
                        })
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
                seq: msg.seq,
                reasoning_blocks: msg.reasoning_blocks.clone(),
                reasoning_content: msg.reasoning_content.clone(),
                reasoning_model: msg.reasoning_model.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod strip_images_tests {
    use super::*;
    use crate::types::{ContentPart, MediaData, MessageContent};

    fn image_part() -> ContentPart {
        ContentPart::ImageBase64 {
            media: MediaData {
                mime_type: "image/png".into(),
                data: "aGk=".into(),
                filename: Some("screenshot.png".into()),
            },
        }
    }

    #[test]
    fn image_parts_become_a_named_omission_not_silence() {
        // Dropping the pixels without a word leaves the model answering as if
        // it had looked at the page.
        let msg = Message {
            role: Role::User,
            content: Some(MessageContent::Parts(vec![
                ContentPart::Text("here is the page".into()),
                image_part(),
            ])),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
            seq: None,
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
        };
        let out = strip_images(&[msg]);
        let Some(MessageContent::Parts(parts)) = &out[0].content else {
            panic!("expected parts, got {:?}", out[0].content);
        };
        assert_eq!(parts.len(), 2, "notice must replace the image: {parts:?}");
        assert!(
            !parts
                .iter()
                .any(|p| matches!(p, ContentPart::ImageBase64 { .. })),
            "image survived: {parts:?}"
        );
        assert!(
            matches!(&parts[1], ContentPart::Text(t) if t == IMAGE_OMITTED_NOTICE),
            "expected the omission notice, got {:?}",
            parts[1]
        );
    }

    #[test]
    fn image_only_message_collapses_to_the_notice_text() {
        let msg = Message {
            role: Role::User,
            content: Some(MessageContent::Parts(vec![image_part()])),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
            seq: None,
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
        };
        let out = strip_images(&[msg]);
        assert!(
            matches!(&out[0].content, Some(MessageContent::Text(t)) if t == IMAGE_OMITTED_NOTICE),
            "got {:?}",
            out[0].content
        );
    }

    #[test]
    fn text_messages_and_tool_calls_pass_through_untouched() {
        let msg = Message {
            role: Role::Assistant,
            content: Some(MessageContent::Text("no pixels here".into())),
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            timestamp: Some("t".into()),
            seq: Some(7),
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
        };
        let out = strip_images(std::slice::from_ref(&msg));
        assert!(
            matches!(&out[0].content, Some(MessageContent::Text(t)) if t == "no pixels here"),
            "got {:?}",
            out[0].content
        );
        assert_eq!(out[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(out[0].seq, Some(7));
    }
}
