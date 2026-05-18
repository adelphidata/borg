//! OpenAI Chat Completions API request/response shapes.
//!
//! Modeled to be compatible with the `openai` Python SDK and ChatBox /
//! Open WebUI defaults. We accept what they send and emit what they
//! expect; fields the agent loop doesn't use (logit_bias, frequency
//! penalty, presence_penalty, etc.) are deserialized into a catch-all
//! `extra` to avoid breaking forward-compat.

use serde::{Deserialize, Serialize};

/// One message in an OpenAI chat conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Role: `system`, `user`, `assistant`, or `tool`.
    pub role: String,
    /// Text content. May be `null` when message is a tool-call carrier.
    #[serde(default)]
    pub content: Option<String>,
    /// Tool-call id this message is responding to (when `role = tool`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool calls the assistant made (when `role = assistant`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// One tool call entry in an assistant message or chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned id stable across the same call's deltas.
    pub id: String,
    /// Always `"function"` in v1 — OpenAI reserves the type field for
    /// future tool kinds.
    #[serde(rename = "type")]
    pub kind: String,
    /// Function name + serialized JSON arguments.
    pub function: ToolCallFunction,
}

/// Function payload of a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    /// Function name.
    pub name: String,
    /// JSON-encoded arguments. OpenAI streams this as multiple deltas
    /// in chunks; we emit a single complete delta per call (the agent
    /// has already validated args by the time `ToolExecuting` fires).
    pub arguments: String,
}

/// One streaming-chunk tool-call delta. Distinct from [`ToolCall`]
/// because the streaming format permits partial fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDelta {
    /// Index of this call within the assistant message's `tool_calls`.
    pub index: u32,
    /// Provider-assigned id (set on the first delta).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Always `"function"` when present.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Function name/arguments delta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<ToolCallFunction>,
}

/// `POST /v1/chat/completions` request body.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionRequest {
    /// Conversation so far. Must be non-empty.
    pub messages: Vec<ChatMessage>,
    /// Model alias — maps to a configured Borg gateway binding.
    pub model: String,
    /// `true` to receive Server-Sent Events; `false` for a single
    /// JSON response. Defaults to `false`.
    #[serde(default)]
    pub stream: bool,
    /// Optional max output tokens — passed through to the agent for
    /// budget enforcement.
    #[serde(default, rename = "max_tokens")]
    pub max_tokens: Option<u32>,
    /// Optional sampling temperature — surfaced for completeness but
    /// effective routing is via the gateway binding for the chosen
    /// `model`.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Tools the client wants the model to be able to call. v1 does
    /// not bridge client tools into the agent loop; documented as
    /// out-of-scope until tool-loop unification lands.
    #[serde(default, skip_deserializing)]
    pub tools: Option<serde_json::Value>,
}

/// Non-streaming `chat.completion` response.
#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionResponse {
    /// Stable id for this response — `chatcmpl-<uuid>`.
    pub id: String,
    /// Always `"chat.completion"`.
    pub object: &'static str,
    /// Unix seconds the response was assembled.
    pub created: i64,
    /// Echo of the requested model alias.
    pub model: String,
    /// Single-element vec — borg returns one choice in v1.
    pub choices: Vec<ChatCompletionChoice>,
    /// Token usage. v1 reports zeros until budget plumbing lands.
    pub usage: Usage,
}

/// One choice in a non-streaming response.
#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionChoice {
    /// Always `0` — single choice.
    pub index: u32,
    /// The full assistant message.
    pub message: ChatMessage,
    /// `"stop"` on success, `"length"` on truncation, `"tool_calls"`
    /// when the agent stopped mid-loop (rare in v1).
    pub finish_reason: String,
}

/// Streaming chunk: `data: {…}\n\n` payload.
#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionChunk {
    /// Same id as the eventual response — clients use it to correlate.
    pub id: String,
    /// Always `"chat.completion.chunk"`.
    pub object: &'static str,
    /// Unix seconds.
    pub created: i64,
    /// Echo of the requested model alias.
    pub model: String,
    /// One delta per chunk.
    pub choices: Vec<ChatCompletionChunkChoice>,
}

/// One choice in a streaming chunk.
#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionChunkChoice {
    /// Always `0`.
    pub index: u32,
    /// Incremental field updates.
    pub delta: ChatCompletionDelta,
    /// `Some("stop")` on the final chunk; `None` for intermediates.
    pub finish_reason: Option<String>,
}

/// Incremental fields in a streaming chunk.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ChatCompletionDelta {
    /// Role — only set on the first chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Text delta — empty string on chunks that carry only tool calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Tool-call deltas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

/// Token usage block. Filled in from agent `UsageData` events when the
/// provider reports them; surfaces that don't yet wire usage emit zeros.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Usage {
    /// Tokens in the prompt.
    pub prompt_tokens: u32,
    /// Tokens in the completion.
    pub completion_tokens: u32,
    /// Sum of the above.
    pub total_tokens: u32,
}

/// `POST /v1/embeddings` request body.
#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddingsRequest {
    /// Model alias. Echoed in the response so clients key cache lines
    /// on it; provider selection itself is governed by Borg's
    /// configured embedding provider, not this field.
    pub model: String,
    /// Either a single string or an array of strings. OpenAI's API
    /// also accepts numeric token-id arrays; we reject those with 400
    /// (see `EmbeddingInput` deserialization).
    pub input: EmbeddingInput,
    /// `"float"` (default) — clients send this; we only emit float.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<String>,
    /// Optional client identifier — accepted for compat, unused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

/// Either a single string or a list of strings. Numeric token-id
/// inputs (which OpenAI's wire format also accepts) are intentionally
/// not modeled — a numeric input deserializes as `serde_json::Value`
/// failure and the route returns 400.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    /// One string of text to embed.
    Single(String),
    /// Many strings to embed in one request.
    Batch(Vec<String>),
}

/// `POST /v1/embeddings` response body. OpenAI shape: `object: "list"`
/// with one `embedding` entry per input, in input order.
#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingsResponse {
    /// Always `"list"`.
    pub object: &'static str,
    /// One entry per input, in input order.
    pub data: Vec<EmbeddingData>,
    /// Echo of the requested model alias.
    pub model: String,
    /// Token usage. v1 reports zeros until usage plumbing lands —
    /// mirrors how `ChatCompletionResponse` reports `Usage::default()`.
    pub usage: EmbeddingsUsage,
}

/// One embedding result.
#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingData {
    /// Always `"embedding"`.
    pub object: &'static str,
    /// Position in the input array — clients index responses by this.
    pub index: u32,
    /// f32 vector. Length depends on the upstream provider/model.
    pub embedding: Vec<f32>,
}

/// Token usage for an embeddings request.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EmbeddingsUsage {
    /// Tokens in the prompt. Zero until usage plumbing lands.
    pub prompt_tokens: u32,
    /// Same as `prompt_tokens` for embeddings (no completion tokens).
    pub total_tokens: u32,
}

/// `GET /v1/models` response.
#[derive(Debug, Clone, Serialize)]
pub struct ModelsList {
    /// Always `"list"`.
    pub object: &'static str,
    /// One entry per configured gateway binding plus a
    /// `borg-default` alias.
    pub data: Vec<ModelInfo>,
}

/// One model entry.
#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    /// Model alias clients pass in `model` field.
    pub id: String,
    /// Always `"model"`.
    pub object: &'static str,
    /// Unix seconds the model was registered.
    pub created: i64,
    /// Always `"borg"`.
    pub owned_by: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_minimal_chat_request() {
        // The minimum every OpenAI SDK sends: messages + model. The
        // optional fields must not be required, or ChatBox / openai-py
        // basic usage would 400.
        let json = r#"{"messages":[{"role":"user","content":"hi"}],"model":"borg"}"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
        assert_eq!(req.messages[0].content.as_deref(), Some("hi"));
        assert_eq!(req.model, "borg");
        assert!(!req.stream, "stream defaults to false");
    }

    #[test]
    fn ignores_unknown_request_fields() {
        // ChatBox sends `presence_penalty`, `frequency_penalty`,
        // `top_p`, `user`, etc. The deserializer must not reject those —
        // `serde(deny_unknown_fields)` would break the entire surface.
        let json = r#"{"messages":[{"role":"user","content":"x"}],"model":"m",
                       "presence_penalty":0.5,"frequency_penalty":0.1,
                       "user":"u","top_p":0.9}"#;
        let req: ChatCompletionRequest = serde_json::from_str(json).expect("parse with extras");
        assert_eq!(req.model, "m");
    }

    #[test]
    fn assistant_message_with_tool_calls_serializes_round_trip() {
        // Tool-call passthrough is the central feature for agent-driven
        // clients. A round-trip serialization guard ensures we don't
        // accidentally drop the `id` or `arguments` fields a future
        // tool-result message would need to correlate.
        let msg = ChatMessage {
            role: "assistant".into(),
            content: None,
            tool_call_id: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_abc".into(),
                kind: "function".into(),
                function: ToolCallFunction {
                    name: "search".into(),
                    arguments: r#"{"q":"ferris"}"#.into(),
                },
            }]),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: ChatMessage = serde_json::from_str(&json).expect("deserialize");
        let calls = back.tool_calls.expect("tool_calls preserved");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].function.name, "search");
        assert_eq!(calls[0].function.arguments, r#"{"q":"ferris"}"#);
    }

    #[test]
    fn embeddings_request_accepts_single_string_input() {
        // Cursor's lightweight callers send `input` as a bare string;
        // narrowing the type to `Vec<String>` would 400 that traffic.
        let json = r#"{"model":"text-embedding-3-small","input":"hello"}"#;
        let req: EmbeddingsRequest = serde_json::from_str(json).expect("parse single");
        match req.input {
            EmbeddingInput::Single(s) => assert_eq!(s, "hello"),
            EmbeddingInput::Batch(_) => panic!("expected Single, got Batch"),
        }
        assert_eq!(req.model, "text-embedding-3-small");
    }

    #[test]
    fn embeddings_request_accepts_array_input() {
        // Continue and Open WebUI batch embeddings in one call; the
        // array form must parse so those flows aren't forced to fan
        // out into N HTTP requests.
        let json = r#"{"model":"m","input":["a","b","c"]}"#;
        let req: EmbeddingsRequest = serde_json::from_str(json).expect("parse batch");
        match req.input {
            EmbeddingInput::Batch(v) => assert_eq!(v, vec!["a", "b", "c"]),
            EmbeddingInput::Single(_) => panic!("expected Batch, got Single"),
        }
    }

    #[test]
    fn embeddings_request_ignores_unknown_fields() {
        // Open WebUI sends `encoding_format: "float"` unconditionally
        // and some clients send `dimensions`. `serde(deny_unknown_fields)`
        // here would 400 those requests; the test guards against a
        // future regression that adds that attribute.
        let json = r#"{"model":"m","input":"x","encoding_format":"float",
                       "dimensions":1536,"user":"alice"}"#;
        let req: EmbeddingsRequest = serde_json::from_str(json).expect("parse extras");
        assert_eq!(req.encoding_format.as_deref(), Some("float"));
        assert_eq!(req.user.as_deref(), Some("alice"));
    }

    #[test]
    fn embeddings_request_rejects_numeric_token_input() {
        // OpenAI's wire format also accepts arrays of token ids
        // (integers). We model strings-only so a numeric input must
        // fail to deserialize — the route layer surfaces this as 400,
        // matching what `openai-py` callers expect to see.
        let json = r#"{"model":"m","input":[1,2,3]}"#;
        let err = serde_json::from_str::<EmbeddingsRequest>(json);
        assert!(
            err.is_err(),
            "numeric token-id input must not deserialize, got: {err:?}"
        );
    }

    #[test]
    fn embeddings_response_serializes_with_index_per_entry() {
        // Clients zip `response.data[i].embedding` back against
        // `request.input[i]` — the `index` field must round-trip and
        // match insertion order.
        let resp = EmbeddingsResponse {
            object: "list",
            data: vec![
                EmbeddingData {
                    object: "embedding",
                    index: 0,
                    embedding: vec![0.1, 0.2],
                },
                EmbeddingData {
                    object: "embedding",
                    index: 1,
                    embedding: vec![0.3, 0.4],
                },
            ],
            model: "text-embedding-3-small".into(),
            usage: EmbeddingsUsage::default(),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).expect("ser")).expect("de");
        assert_eq!(v["object"], "list");
        assert_eq!(v["data"][0]["index"], 0);
        assert_eq!(v["data"][1]["index"], 1);
        assert_eq!(v["data"][0]["embedding"][0], 0.1);
        assert_eq!(v["data"][1]["embedding"][1], 0.4);
        assert_eq!(v["model"], "text-embedding-3-small");
    }

    #[test]
    fn chunk_delta_omits_unset_fields_for_minimal_wire_size() {
        // Streaming responses are token-frequency hot — every chunk
        // sent over the wire counts. Optional delta fields must
        // serialize as absent (not `null`) when empty so we don't
        // double the chunk size for plain text streams.
        let chunk = ChatCompletionChunk {
            id: "chatcmpl-x".into(),
            object: "chat.completion.chunk",
            created: 0,
            model: "m".into(),
            choices: vec![ChatCompletionChunkChoice {
                index: 0,
                delta: ChatCompletionDelta {
                    role: None,
                    content: Some("hi".into()),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
        };
        let json = serde_json::to_string(&chunk).expect("serialize");
        assert!(
            !json.contains("\"role\""),
            "unset role must be absent, got: {json}"
        );
        assert!(
            !json.contains("\"tool_calls\""),
            "unset tool_calls must be absent, got: {json}"
        );
        assert!(
            json.contains("\"content\":\"hi\""),
            "set content must serialize, got: {json}"
        );
    }
}
