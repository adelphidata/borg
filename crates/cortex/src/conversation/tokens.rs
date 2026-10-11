//! Per-message and whole-history token estimation.

use crate::constants::{
    self, AUDIO_TOKEN_ESTIMATE_MIN, IMAGE_TOKEN_ESTIMATE, MESSAGE_ROLE_TOKEN_OVERHEAD,
    TOOL_CALL_ENVELOPE_TOKEN_OVERHEAD,
};
use crate::memory::tokenizer::estimate_tokens;
use crate::types::{ContentPart, Message, MessageContent};

/// Estimate the token count of a single message, including role overhead.
pub(super) fn message_tokens(msg: &Message) -> usize {
    let role_overhead = MESSAGE_ROLE_TOKEN_OVERHEAD;
    let content_tokens = match &msg.content {
        Some(MessageContent::Text(s)) => estimate_tokens(s),
        Some(MessageContent::Parts(parts)) => parts
            .iter()
            .map(|p| match p {
                ContentPart::Text(t) => estimate_tokens(t),
                ContentPart::ImageBase64 { .. } | ContentPart::ImageUrl { .. } => {
                    IMAGE_TOKEN_ESTIMATE
                }
                ContentPart::AudioBase64 { media } => {
                    // Rough estimate: base64 length / 4 * 3 gives decoded bytes,
                    // then ~1 token per 16 bytes of audio data.
                    let decoded_bytes = media.data.len() * 3 / 4;
                    (decoded_bytes / constants::AUDIO_BYTES_PER_TOKEN).max(AUDIO_TOKEN_ESTIMATE_MIN)
                }
                ContentPart::VideoBase64 { media } => {
                    // Gemini tokenises video server-side and the only adapter
                    // that ships bytes today is Gemini; for other providers we
                    // emit a placeholder string, so a coarse audio-shaped
                    // estimate is the conservative budget here.
                    let decoded_bytes = media.data.len() * 3 / 4;
                    (decoded_bytes / constants::AUDIO_BYTES_PER_TOKEN).max(AUDIO_TOKEN_ESTIMATE_MIN)
                }
            })
            .sum(),
        None => 0,
    };
    let tool_call_tokens: usize = msg
        .tool_calls
        .as_ref()
        .map(|tcs| {
            tcs.iter()
                .map(|tc| {
                    // Count the whole call, not just name + arguments: the `id` and
                    // `type` field values plus the JSON envelope framing are real tokens
                    // the model is billed for. Omitting them undercounts multi-call turns
                    // and lets the protected tail blow past the budget before compaction.
                    estimate_tokens(&tc.id)
                        + estimate_tokens(&tc.call_type)
                        + estimate_tokens(&tc.function.name)
                        + estimate_tokens(&tc.function.arguments)
                        + TOOL_CALL_ENVELOPE_TOKEN_OVERHEAD
                })
                .sum()
        })
        .unwrap_or(0);
    // Stored reasoning goes back on the wire for the providers that require
    // the echo, so it is prompt budget like any other field of the turn.
    let reasoning_tokens = msg
        .reasoning_content
        .as_deref()
        .map(estimate_tokens)
        .unwrap_or(0);
    role_overhead + content_tokens + tool_call_tokens + reasoning_tokens
}

/// Total estimated tokens for a conversation history.
pub fn history_tokens(history: &[Message]) -> usize {
    history.iter().map(message_tokens).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FunctionCall, MessageContent, Role, ToolCall};

    fn make_user(text: &str) -> Message {
        Message::user(text)
    }

    fn make_assistant(text: &str) -> Message {
        Message::assistant(text)
    }

    fn make_tool_call_msg(text: &str, call_id: &str, tool_name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: if text.is_empty() {
                None
            } else {
                Some(MessageContent::Text(text.to_string()))
            },
            tool_calls: Some(vec![ToolCall {
                id: call_id.to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: tool_name.to_string(),
                    arguments: "{}".to_string(),
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

    fn make_tool_result(call_id: &str, result: &str) -> Message {
        Message::tool_result(call_id, result)
    }

    #[test]
    fn message_tokens_text_only() {
        let msg = make_user("hello world!");
        let tokens = message_tokens(&msg);
        // 4 overhead + content tokens from BPE tokenizer
        assert!(tokens > 4, "should include overhead + content tokens");
    }

    #[test]
    fn message_tokens_with_tool_calls() {
        let msg = make_tool_call_msg("", "id1", "read_memory");
        let tokens = message_tokens(&msg);
        // 4 overhead + tool name + args tokens
        assert!(tokens > 4, "should include overhead + tool call tokens");
    }

    /// Stored `reasoning_content` is echoed to the provider on the next
    /// request of the loop, so it counts toward the turn's prompt budget by
    /// its own text size.
    #[test]
    fn message_tokens_count_stored_reasoning_content() {
        let without = make_tool_call_msg("", "id1", "read_memory");
        let reasoning = "I should read the file before I change it. ".repeat(20);
        let mut with = without.clone();
        with.reasoning_content = Some(reasoning.clone());
        assert_eq!(
            message_tokens(&with),
            message_tokens(&without) + estimate_tokens(&reasoning)
        );
    }

    /// Build an assistant message carrying a single tool call with explicit
    /// id / type / arguments, so tests can probe which fields the estimator counts.
    fn tool_call_with(id: &str, call_type: &str, name: &str, args: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: id.to_string(),
                call_type: call_type.to_string(),
                function: FunctionCall {
                    name: name.to_string(),
                    arguments: args.to_string(),
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

    #[test]
    fn tool_call_token_estimate_counts_envelope_not_just_name_and_args() {
        // Two calls identical except for the `id` length. The model is billed for
        // the id token(s), so a longer id MUST raise the estimate. The old estimator
        // counted only name + arguments and would report these as equal — this test
        // fails against that regression.
        let short_id = tool_call_with("a", "function", "read_memory", "{}");
        let long_id = tool_call_with(
            "call_0123456789abcdef0123456789abcdef",
            "function",
            "read_memory",
            "{}",
        );
        assert!(
            message_tokens(&long_id) > message_tokens(&short_id),
            "tool-call id must be counted toward the token estimate"
        );

        // The estimate must also clear the bare name+args+role floor by at least the
        // envelope overhead, proving the structural framing is accounted for.
        let name_arg_floor =
            estimate_tokens("read_memory") + estimate_tokens("{}") + MESSAGE_ROLE_TOKEN_OVERHEAD;
        assert!(
            message_tokens(&short_id) >= name_arg_floor + TOOL_CALL_ENVELOPE_TOKEN_OVERHEAD,
            "tool-call estimate must include the JSON envelope overhead"
        );
    }

    #[test]
    fn tool_call_token_estimate_scales_with_parallel_calls() {
        // A turn that fans out N parallel tool calls must cost ~N× a single call's
        // envelope, not a flat name+args sum. Three calls must exceed one.
        let one = tool_call_with("id_aaaa", "function", "search_files", "{\"q\":\"x\"}");
        let mut three = one.clone();
        if let Some(calls) = three.tool_calls.as_mut() {
            calls.push(calls[0].clone());
            calls.push(calls[0].clone());
        }
        assert!(
            message_tokens(&three) > message_tokens(&one) * 2,
            "three parallel tool calls should cost well over twice a single call"
        );
    }

    #[test]
    fn message_tokens_empty_content() {
        let msg = Message {
            role: Role::Assistant,
            content: None,
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
            seq: None,
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
        };
        assert_eq!(message_tokens(&msg), 4); // just overhead
    }

    #[test]
    fn message_tokens_with_parts_content() {
        let msg = Message {
            role: Role::User,
            content: Some(MessageContent::Parts(vec![
                ContentPart::Text("hello".to_string()),
                ContentPart::ImageBase64 {
                    media: crate::types::MediaData {
                        mime_type: "image/png".to_string(),
                        data: "data".to_string(),
                        filename: None,
                    },
                },
            ])),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
            seq: None,
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
        };
        let tokens = message_tokens(&msg);
        // Should include overhead + text tokens + IMAGE_TOKEN_ESTIMATE
        assert!(tokens >= IMAGE_TOKEN_ESTIMATE);
    }

    #[test]
    fn message_tokens_audio_estimate_scales_with_size() {
        let small_audio = Message {
            role: Role::User,
            content: Some(MessageContent::Parts(vec![ContentPart::AudioBase64 {
                media: crate::types::MediaData {
                    mime_type: "audio/wav".to_string(),
                    data: "a".repeat(100),
                    filename: None,
                },
            }])),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
            seq: None,
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
        };
        let large_audio = Message {
            role: Role::User,
            content: Some(MessageContent::Parts(vec![ContentPart::AudioBase64 {
                media: crate::types::MediaData {
                    mime_type: "audio/wav".to_string(),
                    data: "a".repeat(100_000),
                    filename: None,
                },
            }])),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
            seq: None,
            reasoning_blocks: None,
            reasoning_content: None,
            reasoning_model: None,
        };
        let small_tokens = message_tokens(&small_audio);
        let large_tokens = message_tokens(&large_audio);
        assert!(large_tokens > small_tokens);
    }

    #[test]
    fn history_tokens_empty() {
        assert_eq!(history_tokens(&[]), 0);
    }

    #[test]
    fn history_tokens_sums_messages() {
        let history = vec![make_user("hello"), make_assistant("world")];
        let total = history_tokens(&history);
        assert_eq!(
            total,
            message_tokens(&history[0]) + message_tokens(&history[1])
        );
    }

    #[test]
    fn history_tokens_with_tool_calls() {
        let history = vec![
            make_user("test"),
            make_tool_call_msg("thinking", "c1", "run_shell"),
            make_tool_result("c1", "output data here"),
        ];
        let total = history_tokens(&history);
        assert!(total > 0);
        assert_eq!(
            total,
            message_tokens(&history[0]) + message_tokens(&history[1]) + message_tokens(&history[2])
        );
    }
}
