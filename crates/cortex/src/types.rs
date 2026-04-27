use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};

/// Message sender role in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// System prompt or instruction.
    System,
    /// Human user input.
    User,
    /// LLM-generated response.
    Assistant,
    /// Tool execution result.
    Tool,
}

/// Media attachment data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MediaData {
    /// MIME type of the media (e.g. "image/png").
    pub mime_type: String,
    /// Base64-encoded binary content.
    pub data: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional original filename of the media.
    pub filename: Option<String>,
}

/// A single content part in a multi-modal message.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentPart {
    /// Plain text segment.
    Text(String),
    /// Base64-encoded image with metadata.
    ImageBase64 {
        /// The image media data.
        media: MediaData,
    },
    /// Image referenced by URL.
    ImageUrl {
        /// The image URL.
        url: String,
    },
    /// Base64-encoded audio with metadata.
    AudioBase64 {
        /// The audio media data.
        media: MediaData,
    },
}

impl Serialize for ContentPart {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            ContentPart::Text(t) => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "text")?;
                map.serialize_entry("text", t)?;
                map.end()
            }
            ContentPart::ImageBase64 { media } => {
                let data_uri = format!("data:{};base64,{}", media.mime_type, media.data);
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "image_url")?;
                map.serialize_entry("image_url", &serde_json::json!({"url": data_uri}))?;
                map.end()
            }
            ContentPart::ImageUrl { url } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "image_url")?;
                map.serialize_entry("image_url", &serde_json::json!({"url": url}))?;
                map.end()
            }
            ContentPart::AudioBase64 { media } => {
                let format = media
                    .mime_type
                    .strip_prefix("audio/")
                    .unwrap_or("mp3")
                    .to_string();
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "input_audio")?;
                map.serialize_entry(
                    "input_audio",
                    &serde_json::json!({"data": media.data, "format": format}),
                )?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ContentPart {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let v: serde_json::Value = serde_json::Value::deserialize(deserializer)?;
        let obj = v
            .as_object()
            .ok_or_else(|| de::Error::custom("expected object"))?;
        let typ = obj
            .get("type")
            .and_then(|t| t.as_str())
            .ok_or_else(|| de::Error::custom("missing 'type'"))?;
        match typ {
            "text" => {
                let text = obj
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(ContentPart::Text(text))
            }
            "image_url" => {
                let url = obj
                    .get("image_url")
                    .and_then(|o| o.get("url"))
                    .and_then(|u| u.as_str())
                    .unwrap_or("")
                    .to_string();
                if url.is_empty() {
                    return Err(de::Error::custom("image_url has empty url"));
                }
                if url.starts_with("data:") {
                    match crate::media::parse_data_uri(&url) {
                        Ok(media) => Ok(ContentPart::ImageBase64 { media }),
                        Err(_) => Ok(ContentPart::ImageUrl { url }),
                    }
                } else {
                    Ok(ContentPart::ImageUrl { url })
                }
            }
            "input_audio" => {
                let audio_obj = obj.get("input_audio");
                let data = audio_obj
                    .and_then(|o| o.get("data"))
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                let format = audio_obj
                    .and_then(|o| o.get("format"))
                    .and_then(|f| f.as_str())
                    .unwrap_or("mp3");
                if data.is_empty() {
                    Ok(ContentPart::Text("[audio: empty]".to_string()))
                } else {
                    Ok(ContentPart::AudioBase64 {
                        media: MediaData {
                            mime_type: format!("audio/{format}"),
                            data,
                            filename: None,
                        },
                    })
                }
            }
            _ => Ok(ContentPart::Text(format!("[unknown content type: {typ}]"))),
        }
    }
}

/// Message content: plain text or structured parts.
/// Serializes as string when text-only (backward compat), array when multimodal.
#[derive(Debug, Clone, PartialEq)]
pub enum MessageContent {
    /// Plain text content.
    Text(String),
    /// Structured multi-modal content parts.
    Parts(Vec<ContentPart>),
}

impl MessageContent {
    /// Returns the first text part. For full text across all parts, use `full_text()`.
    pub fn text(&self) -> Option<&str> {
        match self {
            MessageContent::Text(s) => Some(s),
            MessageContent::Parts(parts) => parts.iter().find_map(|p| match p {
                ContentPart::Text(t) => Some(t.as_str()),
                _ => None,
            }),
        }
    }

    /// Concatenate all text parts, with placeholders for non-text content.
    pub fn full_text(&self) -> String {
        match self {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Parts(parts) => {
                let mut out = String::new();
                for part in parts {
                    match part {
                        ContentPart::Text(t) => {
                            if !out.is_empty() && !out.ends_with('\n') {
                                out.push(' ');
                            }
                            out.push_str(t);
                        }
                        ContentPart::ImageBase64 { media } => {
                            out.push_str(&format!(
                                " [image: {}]",
                                media.filename.as_deref().unwrap_or("attached")
                            ));
                        }
                        ContentPart::ImageUrl { url } => {
                            out.push_str(&format!(" [image: {url}]"));
                        }
                        ContentPart::AudioBase64 { media } => {
                            out.push_str(&format!(
                                " [audio: {}]",
                                media.filename.as_deref().unwrap_or("attached")
                            ));
                        }
                    }
                }
                out
            }
        }
    }

    /// Returns true if the content is empty or contains only empty text parts.
    pub fn is_empty(&self) -> bool {
        match self {
            MessageContent::Text(s) => s.is_empty(),
            MessageContent::Parts(parts) => {
                parts.is_empty()
                    || parts
                        .iter()
                        .all(|p| matches!(p, ContentPart::Text(t) if t.is_empty()))
            }
        }
    }
}

impl Serialize for MessageContent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            MessageContent::Text(s) => serializer.serialize_str(s),
            MessageContent::Parts(parts) => parts.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for MessageContent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ContentVisitor;
        impl<'de> Visitor<'de> for ContentVisitor {
            type Value = MessageContent;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "a string or array of content parts")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(MessageContent::Text(v.to_string()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(MessageContent::Text(v))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
                let parts =
                    Vec::<ContentPart>::deserialize(de::value::SeqAccessDeserializer::new(seq))?;
                Ok(MessageContent::Parts(parts))
            }
        }
        deserializer.deserialize_any(ContentVisitor)
    }
}

/// A single message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// The sender role (system, user, assistant, or tool).
    pub role: Role,
    /// Message body (text or multimodal parts).
    pub content: Option<MessageContent>,
    /// Tool calls requested by the assistant in this message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// ID of the tool call this message is a response to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// RFC 3339 timestamp of when this message was created.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

/// An LLM-requested tool invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique identifier for this tool call.
    pub id: String,
    /// Call type (typically "function").
    #[serde(rename = "type")]
    pub call_type: String,
    /// The function name and arguments to invoke.
    pub function: FunctionCall,
}

/// Function name and serialized arguments for a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    /// Function name to invoke.
    pub name: String,
    /// JSON-serialized arguments for the function.
    pub arguments: String,
}

/// Result returned from executing a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// ID of the tool call this result corresponds to.
    pub tool_call_id: String,
    /// Output content from the tool execution.
    pub content: String,
    /// Whether the tool execution failed.
    pub is_error: bool,
}

/// Schema definition for a tool, sent to the LLM each turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool type (typically "function").
    #[serde(rename = "type")]
    pub tool_type: String,
    /// The function schema for this tool.
    pub function: FunctionDefinition,
}

/// JSON Schema definition of a tool's function signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
    /// Function name exposed to the LLM.
    pub name: String,
    /// Human-readable description of what the function does.
    pub description: String,
    /// JSON Schema describing the function's parameters.
    pub parameters: serde_json::Value,
}

impl Message {
    fn now_rfc3339() -> String {
        chrono::Local::now().to_rfc3339()
    }

    /// Create a system message (no timestamp).
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(MessageContent::Text(content.into())),
            tool_calls: None,
            tool_call_id: None,
            timestamp: None,
        }
    }

    /// Create a user message with current timestamp.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(MessageContent::Text(content.into())),
            tool_calls: None,
            tool_call_id: None,
            timestamp: Some(Self::now_rfc3339()),
        }
    }

    /// Create an assistant message with current timestamp.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(MessageContent::Text(content.into())),
            tool_calls: None,
            tool_call_id: None,
            timestamp: Some(Self::now_rfc3339()),
        }
    }

    /// Create a tool result message with current timestamp.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(MessageContent::Text(content.into())),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            timestamp: Some(Self::now_rfc3339()),
        }
    }

    /// Create a multimodal user message (text + images/audio).
    pub fn user_multimodal(parts: Vec<ContentPart>) -> Self {
        Self {
            role: Role::User,
            content: Some(MessageContent::Parts(parts)),
            tool_calls: None,
            tool_call_id: None,
            timestamp: Some(Self::now_rfc3339()),
        }
    }

    /// Extract the first text content from this message.
    pub fn text_content(&self) -> Option<&str> {
        self.content.as_ref().and_then(|c| c.text())
    }
}

/// A step in a structured plan tracked by the `update_plan` tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanStep {
    /// Short description of this plan step.
    pub title: String,
    /// Current execution status.
    pub status: PlanStepStatus,
}

/// Status of a plan step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    /// Not yet started.
    Pending,
    /// Currently being executed.
    InProgress,
    /// Successfully finished.
    Completed,
}

/// Output from a tool execution: either plain text or multimodal (text + images).
#[derive(Debug, Clone)]
pub enum ToolOutput {
    /// Plain text result.
    Text(String),
    /// Rich result with text summary and additional content parts.
    Multimodal {
        /// Text summary of the tool output.
        text: String,
        /// Additional content parts (images, audio, etc.).
        parts: Vec<ContentPart>,
    },
}

impl From<String> for ToolOutput {
    fn from(s: String) -> Self {
        ToolOutput::Text(s)
    }
}

impl From<Result<String, anyhow::Error>> for ToolOutput {
    fn from(r: Result<String, anyhow::Error>) -> Self {
        match r {
            Ok(s) => ToolOutput::Text(s),
            Err(e) => ToolOutput::Text(format!("Error: {e}")),
        }
    }
}

impl Message {
    /// Create a multimodal tool result (text + images).
    pub fn tool_result_multimodal(
        tool_call_id: impl Into<String>,
        parts: Vec<ContentPart>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: Some(MessageContent::Parts(parts)),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            timestamp: Some(Self::now_rfc3339()),
        }
    }
}

impl ToolDefinition {
    /// Create a new function-type tool definition with the given JSON Schema parameters.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_system_constructor() {
        let msg = Message::system("You are helpful.");
        assert_eq!(msg.role, Role::System);
        assert_eq!(msg.text_content(), Some("You are helpful."));
        assert!(msg.tool_calls.is_none());
        assert!(msg.tool_call_id.is_none());
    }

    #[test]
    fn message_user_constructor() {
        let msg = Message::user("Hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.text_content(), Some("Hello"));
    }

    #[test]
    fn message_assistant_constructor() {
        let msg = Message::assistant("Hi there");
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.text_content(), Some("Hi there"));
    }

    #[test]
    fn message_tool_result_constructor() {
        let msg = Message::tool_result("call_123", "result text");
        assert_eq!(msg.role, Role::Tool);
        assert_eq!(msg.text_content(), Some("result text"));
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_123"));
        assert!(msg.tool_calls.is_none());
    }

    #[test]
    fn message_constructors_accept_string_types() {
        let owned = String::from("owned string");
        let msg = Message::user(owned);
        assert_eq!(msg.text_content(), Some("owned string"));

        let msg2 = Message::system("static str");
        assert_eq!(msg2.text_content(), Some("static str"));
    }

    #[test]
    fn role_serializes_to_lowercase() {
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"system\"");
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(
            serde_json::to_string(&Role::Assistant).unwrap(),
            "\"assistant\""
        );
        assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), "\"tool\"");
    }

    #[test]
    fn role_deserializes_from_lowercase() {
        let role: Role = serde_json::from_str("\"system\"").unwrap();
        assert_eq!(role, Role::System);
        let role: Role = serde_json::from_str("\"tool\"").unwrap();
        assert_eq!(role, Role::Tool);
    }

    #[test]
    fn message_serialization_skips_none_fields() {
        let msg = Message::user("test");
        let json = serde_json::to_value(&msg).unwrap();
        assert!(!json.as_object().unwrap().contains_key("tool_calls"));
        assert!(!json.as_object().unwrap().contains_key("tool_call_id"));
    }

    #[test]
    fn tool_definition_new_sets_function_type() {
        let td = ToolDefinition::new("my_tool", "A tool", serde_json::json!({"type": "object"}));
        assert_eq!(td.tool_type, "function");
        assert_eq!(td.function.name, "my_tool");
        assert_eq!(td.function.description, "A tool");
        assert_eq!(td.function.parameters["type"], "object");
    }

    #[test]
    fn tool_call_json_round_trip() {
        let tc = ToolCall {
            id: "call_1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "read_memory".to_string(),
                arguments: "{\"filename\":\"test.md\"}".to_string(),
            },
        };
        let json = serde_json::to_string(&tc).unwrap();
        let deserialized: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "call_1");
        assert_eq!(deserialized.function.name, "read_memory");
        assert_eq!(
            deserialized.function.arguments,
            "{\"filename\":\"test.md\"}"
        );
    }

    #[test]
    fn tool_result_serialization() {
        let tr = ToolResult {
            tool_call_id: "call_1".to_string(),
            content: "success".to_string(),
            is_error: false,
        };
        let json = serde_json::to_value(&tr).unwrap();
        assert_eq!(json["tool_call_id"], "call_1");
        assert_eq!(json["content"], "success");
        assert_eq!(json["is_error"], false);
    }

    #[test]
    fn message_content_text_serializes_as_string() {
        let mc = MessageContent::Text("hi".to_string());
        let json = serde_json::to_value(&mc).unwrap();
        assert_eq!(json, serde_json::json!("hi"));
    }

    #[test]
    fn message_content_parts_serializes_as_array() {
        let mc = MessageContent::Parts(vec![
            ContentPart::Text("hello".to_string()),
            ContentPart::ImageUrl {
                url: "https://example.com/img.png".to_string(),
            },
        ]);
        let json = serde_json::to_value(&mc).unwrap();
        assert!(json.is_array());
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[1]["type"], "image_url");
    }

    #[test]
    fn message_content_deserializes_from_string() {
        let mc: MessageContent = serde_json::from_str("\"hello\"").unwrap();
        assert_eq!(mc, MessageContent::Text("hello".to_string()));
    }

    #[test]
    fn message_content_deserializes_from_array() {
        let json = r#"[{"type":"text","text":"hi"}]"#;
        let mc: MessageContent = serde_json::from_str(json).unwrap();
        match mc {
            MessageContent::Parts(parts) => {
                assert_eq!(parts.len(), 1);
                assert!(matches!(&parts[0], ContentPart::Text(t) if t == "hi"));
            }
            other => unreachable!("expected Parts, got {other:?}"),
        }
    }

    #[test]
    fn content_part_image_base64_serializes_openai_format() {
        let part = ContentPart::ImageBase64 {
            media: MediaData {
                mime_type: "image/png".to_string(),
                data: "abc123".to_string(),
                filename: None,
            },
        };
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json["type"], "image_url");
        assert_eq!(json["image_url"]["url"], "data:image/png;base64,abc123");
    }

    #[test]
    fn user_multimodal_constructor() {
        let parts = vec![
            ContentPart::Text("Describe this".to_string()),
            ContentPart::ImageUrl {
                url: "https://example.com/img.png".to_string(),
            },
        ];
        let msg = Message::user_multimodal(parts);
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.text_content(), Some("Describe this"));
        match &msg.content {
            Some(MessageContent::Parts(p)) => assert_eq!(p.len(), 2),
            other => unreachable!("expected Parts, got {other:?}"),
        }
    }

    #[test]
    fn text_only_message_serialization_unchanged() {
        let msg = Message::user("test message");
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["content"], "test message");
        assert_eq!(json["role"], "user");
    }

    #[test]
    fn tool_output_text_from_string() {
        let output: ToolOutput = "hello".to_string().into();
        match output {
            ToolOutput::Text(s) => assert_eq!(s, "hello"),
            other @ ToolOutput::Multimodal { .. } => unreachable!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn tool_output_multimodal_has_parts() {
        let output = ToolOutput::Multimodal {
            text: "screenshot taken".to_string(),
            parts: vec![
                ContentPart::Text("screenshot taken".to_string()),
                ContentPart::ImageBase64 {
                    media: MediaData {
                        mime_type: "image/png".to_string(),
                        data: "abc123".to_string(),
                        filename: None,
                    },
                },
            ],
        };
        match output {
            ToolOutput::Multimodal { text, parts } => {
                assert_eq!(text, "screenshot taken");
                assert_eq!(parts.len(), 2);
            }
            other @ ToolOutput::Text(_) => unreachable!("expected Multimodal, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_multimodal_constructor() {
        let parts = vec![
            ContentPart::Text("result text".to_string()),
            ContentPart::ImageBase64 {
                media: MediaData {
                    mime_type: "image/png".to_string(),
                    data: "abc123".to_string(),
                    filename: None,
                },
            },
        ];
        let msg = Message::tool_result_multimodal("call_1", parts);
        assert_eq!(msg.role, Role::Tool);
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_1"));
        match &msg.content {
            Some(MessageContent::Parts(p)) => assert_eq!(p.len(), 2),
            other => unreachable!("expected Parts, got {other:?}"),
        }
    }

    // -- PlanStep / PlanStepStatus --

    #[test]
    fn plan_step_status_serializes_snake_case() {
        let step = PlanStep {
            title: "Do thing".into(),
            status: PlanStepStatus::InProgress,
        };
        let json = serde_json::to_string(&step).unwrap();
        assert!(json.contains("\"in_progress\""));
    }

    #[test]
    fn plan_step_deserializes_from_json() {
        let json = r#"{"title":"Test","status":"pending"}"#;
        let step: PlanStep = serde_json::from_str(json).unwrap();
        assert_eq!(step.status, PlanStepStatus::Pending);
        assert_eq!(step.title, "Test");
    }

    #[test]
    fn plan_step_status_all_variants_roundtrip() {
        for status in [
            PlanStepStatus::Pending,
            PlanStepStatus::InProgress,
            PlanStepStatus::Completed,
        ] {
            let step = PlanStep {
                title: "x".into(),
                status: status.clone(),
            };
            let json = serde_json::to_string(&step).unwrap();
            let back: PlanStep = serde_json::from_str(&json).unwrap();
            assert_eq!(back.status, step.status);
        }
    }

    #[test]
    fn plan_step_invalid_status_fails() {
        let json = r#"{"title":"X","status":"unknown"}"#;
        let result = serde_json::from_str::<PlanStep>(json);
        assert!(result.is_err());
    }
}
