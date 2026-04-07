use serde::{Deserialize, Serialize};

// ── SSE Envelope (inbound from signal-cli daemon) ──

/// Top-level SSE event from signal-cli daemon.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalEnvelope {
    pub envelope: EnvelopeData,
    /// The bot's own phone number (e.g., "+1234567890").
    #[serde(default)]
    pub account: Option<String>,
}

/// Inner envelope data from a Signal event.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvelopeData {
    /// Source identifier (phone number or UUID).
    #[serde(default)]
    pub source: Option<String>,
    /// Source phone number.
    #[serde(default)]
    pub source_number: Option<String>,
    /// Source UUID.
    #[serde(default)]
    pub source_uuid: Option<String>,
    /// Source device ID.
    #[serde(default)]
    pub source_device: Option<i32>,
    /// Unix timestamp in milliseconds.
    #[serde(default)]
    pub timestamp: i64,
    /// Incoming data message (text, media, reaction, etc.).
    #[serde(default)]
    pub data_message: Option<DataMessage>,
    /// Sync message from another linked device.
    #[serde(default)]
    pub sync_message: Option<SyncMessage>,
    /// An edit to a previously sent message.
    #[serde(default)]
    pub edit_message: Option<EditMessage>,
    /// Delivery/read receipt message.
    #[serde(default)]
    pub receipt_message: Option<serde_json::Value>,
    /// Typing indicator message.
    #[serde(default)]
    pub typing_message: Option<TypingMessage>,
}

/// A Signal data message containing text, media, or reactions.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataMessage {
    /// Unix timestamp in milliseconds.
    #[serde(default)]
    pub timestamp: i64,
    /// Text body of the message.
    #[serde(default)]
    pub message: Option<String>,
    /// Group info if the message was sent to a group.
    #[serde(default)]
    pub group_info: Option<GroupInfo>,
    /// File attachments.
    #[serde(default)]
    pub attachments: Option<Vec<Attachment>>,
    /// Reaction to another message.
    #[serde(default)]
    pub reaction: Option<Reaction>,
    /// Quoted/replied-to message.
    #[serde(default)]
    pub quote: Option<Quote>,
    /// User mentions within the message.
    #[serde(default)]
    pub mentions: Option<Vec<Mention>>,
    /// Disappearing message timer in seconds.
    #[serde(default)]
    pub expires_in_seconds: Option<i64>,
}

/// Signal group information.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupInfo {
    /// Base64-encoded group identifier.
    pub group_id: String,
    /// Group message type (e.g. "DELIVER").
    #[serde(default, rename = "type")]
    pub group_type: Option<String>,
}

/// A file attachment on a Signal message.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    /// MIME type of the attachment.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Original filename.
    #[serde(default)]
    pub filename: Option<String>,
    /// Attachment identifier.
    #[serde(default)]
    pub id: Option<String>,
    /// File size in bytes.
    #[serde(default)]
    pub size: Option<u64>,
}

/// A reaction to a Signal message.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reaction {
    /// Emoji character for the reaction.
    pub emoji: String,
    /// Author of the message being reacted to.
    #[serde(default)]
    pub target_author: Option<String>,
    /// Timestamp of the message being reacted to.
    #[serde(default)]
    pub target_sent_timestamp: Option<i64>,
    /// Whether this removes a previous reaction.
    #[serde(default)]
    pub is_remove: bool,
}

/// A quoted (replied-to) Signal message.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    /// Timestamp ID of the quoted message.
    #[serde(default)]
    pub id: Option<i64>,
    /// Author of the quoted message.
    #[serde(default)]
    pub author: Option<String>,
    /// Text content of the quoted message.
    #[serde(default)]
    pub text: Option<String>,
}

/// A user mention within a Signal message.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mention {
    /// Start position of the mention in the text.
    #[serde(default)]
    pub start: Option<i32>,
    /// Length of the mention placeholder in the text.
    #[serde(default)]
    pub length: Option<i32>,
    /// UUID of the mentioned user.
    #[serde(default)]
    pub uuid: Option<String>,
    /// Phone number of the mentioned user.
    #[serde(default)]
    pub number: Option<String>,
}

/// A Signal edit message wrapper.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditMessage {
    /// Timestamp of the original message being edited.
    #[serde(default)]
    pub target_sent_timestamp: Option<i64>,
    /// The replacement data message.
    #[serde(default)]
    pub data_message: Option<DataMessage>,
}

/// A Signal typing indicator message.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypingMessage {
    /// Typing action (e.g. "STARTED", "STOPPED").
    #[serde(default)]
    pub action: Option<String>,
    /// Unix timestamp in milliseconds.
    #[serde(default)]
    pub timestamp: Option<i64>,
    /// Group ID if typing in a group.
    #[serde(default)]
    pub group_id: Option<String>,
}

/// A sync message relayed from another linked Signal device.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncMessage {
    /// A message sent from another linked device.
    #[serde(default)]
    pub sent_message: Option<SentMessage>,
}

/// A message sent from another linked Signal device (via sync).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SentMessage {
    /// Recipient phone number.
    #[serde(default)]
    pub destination: Option<String>,
    /// Recipient UUID.
    #[serde(default)]
    pub destination_uuid: Option<String>,
    /// Unix timestamp in milliseconds.
    #[serde(default)]
    pub timestamp: Option<i64>,
    /// Text body of the sent message.
    #[serde(default)]
    pub message: Option<String>,
    /// Group info if sent to a group.
    #[serde(default)]
    pub group_info: Option<GroupInfo>,
}

// ── JSON-RPC 2.0 (outbound to signal-cli daemon) ──

/// A JSON-RPC 2.0 request sent to the signal-cli daemon.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcRequest {
    /// JSON-RPC protocol version (always "2.0").
    pub jsonrpc: String,
    /// Method name to invoke.
    pub method: String,
    /// Optional request ID for correlating responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Method parameters.
    pub params: serde_json::Value,
}

impl JsonRpcRequest {
    /// Create a new JSON-RPC request with an auto-generated ID.
    pub fn new(method: &str, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            id: Some(super::api::next_request_id()),
            params,
        }
    }
}

/// A JSON-RPC 2.0 response from the signal-cli daemon.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcResponse {
    /// JSON-RPC protocol version.
    #[serde(default)]
    pub jsonrpc: Option<String>,
    /// Result payload on success.
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    /// Error object on failure.
    #[serde(default)]
    pub error: Option<JsonRpcError>,
    /// Request ID this response correlates to.
    #[serde(default)]
    pub id: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcError {
    /// Numeric error code.
    pub code: i64,
    /// Human-readable error message.
    pub message: String,
    /// Additional error data.
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_dm_text_envelope() {
        let json = r#"{
            "envelope": {
                "source": "+15551234567",
                "sourceUuid": "abc-def-123",
                "sourceDevice": 1,
                "timestamp": 1700000000000,
                "dataMessage": {
                    "timestamp": 1700000000000,
                    "message": "Hello from Signal"
                }
            },
            "account": "+15559876543"
        }"#;
        let env: SignalEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.account.as_deref(), Some("+15559876543"));
        assert_eq!(env.envelope.source.as_deref(), Some("+15551234567"));
        assert_eq!(env.envelope.source_uuid.as_deref(), Some("abc-def-123"));
        let dm = env.envelope.data_message.unwrap();
        assert_eq!(dm.message.as_deref(), Some("Hello from Signal"));
        assert!(dm.group_info.is_none());
    }

    #[test]
    fn deserialize_group_text_envelope() {
        let json = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1700000000000,
                "dataMessage": {
                    "timestamp": 1700000000000,
                    "message": "Hello group",
                    "groupInfo": {
                        "groupId": "group123abc",
                        "type": "DELIVER"
                    }
                }
            }
        }"#;
        let env: SignalEnvelope = serde_json::from_str(json).unwrap();
        let dm = env.envelope.data_message.unwrap();
        let group = dm.group_info.unwrap();
        assert_eq!(group.group_id, "group123abc");
        assert_eq!(group.group_type.as_deref(), Some("DELIVER"));
    }

    #[test]
    fn deserialize_attachment_envelope() {
        let json = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1700000000000,
                "dataMessage": {
                    "timestamp": 1700000000000,
                    "message": "Check this out",
                    "attachments": [
                        {
                            "contentType": "image/png",
                            "filename": "photo.png",
                            "id": "att-123",
                            "size": 102400
                        }
                    ]
                }
            }
        }"#;
        let env: SignalEnvelope = serde_json::from_str(json).unwrap();
        let dm = env.envelope.data_message.unwrap();
        let attachments = dm.attachments.unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].content_type.as_deref(), Some("image/png"));
        assert_eq!(attachments[0].filename.as_deref(), Some("photo.png"));
        assert_eq!(attachments[0].size, Some(102400));
    }

    #[test]
    fn deserialize_reaction_envelope() {
        let json = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1700000000000,
                "dataMessage": {
                    "timestamp": 1700000000000,
                    "reaction": {
                        "emoji": "👍",
                        "targetAuthor": "+15559876543",
                        "targetSentTimestamp": 1699999999000,
                        "isRemove": false
                    }
                }
            }
        }"#;
        let env: SignalEnvelope = serde_json::from_str(json).unwrap();
        let dm = env.envelope.data_message.unwrap();
        let reaction = dm.reaction.unwrap();
        assert_eq!(reaction.emoji, "👍");
        assert_eq!(reaction.target_author.as_deref(), Some("+15559876543"));
        assert!(!reaction.is_remove);
    }

    #[test]
    fn deserialize_receipt_only_envelope() {
        let json = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1700000000000,
                "receiptMessage": {
                    "when": 1700000000000,
                    "isDelivery": true,
                    "timestamps": [1699999999000]
                }
            }
        }"#;
        let env: SignalEnvelope = serde_json::from_str(json).unwrap();
        assert!(env.envelope.data_message.is_none());
        assert!(env.envelope.receipt_message.is_some());
    }

    #[test]
    fn deserialize_sync_sent_message() {
        let json = r#"{
            "envelope": {
                "source": "+15559876543",
                "timestamp": 1700000000000,
                "syncMessage": {
                    "sentMessage": {
                        "destination": "+15551234567",
                        "timestamp": 1700000000000,
                        "message": "Sent from another device"
                    }
                }
            }
        }"#;
        let env: SignalEnvelope = serde_json::from_str(json).unwrap();
        let sync = env.envelope.sync_message.unwrap();
        let sent = sync.sent_message.unwrap();
        assert_eq!(sent.destination.as_deref(), Some("+15551234567"));
        assert_eq!(sent.message.as_deref(), Some("Sent from another device"));
    }

    #[test]
    fn deserialize_minimal_envelope() {
        let json = r#"{
            "envelope": {
                "timestamp": 0
            }
        }"#;
        let env: SignalEnvelope = serde_json::from_str(json).unwrap();
        assert!(env.envelope.source.is_none());
        assert!(env.envelope.data_message.is_none());
        assert!(env.envelope.sync_message.is_none());
        assert!(env.account.is_none());
    }

    #[test]
    fn deserialize_typing_message() {
        let json = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1700000000000,
                "typingMessage": {
                    "action": "STARTED",
                    "timestamp": 1700000000000
                }
            }
        }"#;
        let env: SignalEnvelope = serde_json::from_str(json).unwrap();
        assert!(env.envelope.data_message.is_none());
        let typing = env.envelope.typing_message.unwrap();
        assert_eq!(typing.action.as_deref(), Some("STARTED"));
    }

    #[test]
    fn jsonrpc_response_success() {
        let json = r#"{
            "jsonrpc": "2.0",
            "result": { "version": "0.13.2" },
            "id": "1"
        }"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn jsonrpc_response_error() {
        let json = r#"{
            "jsonrpc": "2.0",
            "error": {
                "code": -32601,
                "message": "Method not found"
            },
            "id": "1"
        }"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.result.is_none());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "Method not found");
    }

    #[test]
    fn deserialize_edit_message_envelope() {
        let json = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1700000001000,
                "editMessage": {
                    "targetSentTimestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000001000,
                        "message": "corrected text"
                    }
                }
            }
        }"#;
        let env: SignalEnvelope = serde_json::from_str(json).unwrap();
        assert!(env.envelope.data_message.is_none());
        let edit = env.envelope.edit_message.unwrap();
        assert_eq!(edit.target_sent_timestamp, Some(1700000000000));
        let dm = edit.data_message.unwrap();
        assert_eq!(dm.message.as_deref(), Some("corrected text"));
    }

    #[test]
    fn jsonrpc_request_serialization() {
        let req = JsonRpcRequest::new(
            "send",
            serde_json::json!({ "recipient": "+15551234567", "message": "hi" }),
        );
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.method, "send");
        assert!(req.id.is_some());
        let serialized = serde_json::to_string(&req).unwrap();
        assert!(serialized.contains("\"jsonrpc\":\"2.0\""));
        assert!(serialized.contains("\"method\":\"send\""));
    }
}
