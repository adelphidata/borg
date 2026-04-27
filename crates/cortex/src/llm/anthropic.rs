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
                .map(crate::types::MessageContent::is_empty)
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
pub(super) fn build_anthropic_messages(messages: &[Message]) -> Vec<serde_json::Value> {
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
/// Accepts the many field-name variants we've seen in the wild:
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
pub(super) fn apply_message_cache_control(
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
pub(super) fn parse_anthropic_response(resp: &serde_json::Value) -> Result<Message> {
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
pub(super) fn strip_images(messages: &[Message]) -> Vec<Message> {
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
