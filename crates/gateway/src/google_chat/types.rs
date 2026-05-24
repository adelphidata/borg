use serde::{Deserialize, Deserializer, Serialize};

/// Google Chat event types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventType {
    /// A new message was received.
    Message,
    /// The bot was added to a space.
    AddedToSpace,
    /// The bot was removed from a space.
    RemovedFromSpace,
    /// A card button or action was clicked.
    CardClicked,
    /// Unrecognized event type.
    Unknown(String),
}

impl<'de> Deserialize<'de> for EventType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "MESSAGE" => EventType::Message,
            "ADDED_TO_SPACE" => EventType::AddedToSpace,
            "REMOVED_FROM_SPACE" => EventType::RemovedFromSpace,
            "CARD_CLICKED" => EventType::CardClicked,
            _ => EventType::Unknown(s),
        })
    }
}

/// Top-level Google Chat webhook event.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatEvent {
    /// The event type (deserialized from "type").
    #[serde(rename = "type")]
    pub event_type: EventType,
    /// Timestamp of the event.
    #[serde(default)]
    pub event_time: Option<String>,
    /// Verification token (deprecated but still sent by some configurations).
    #[serde(default)]
    pub token: Option<String>,
    /// The message that triggered the event.
    #[serde(default)]
    pub message: Option<ChatMessage>,
    /// The user who triggered the event.
    #[serde(default)]
    pub user: Option<ChatUser>,
    /// The space in which the event occurred.
    #[serde(default)]
    pub space: Option<Space>,
}

/// A Google Chat message.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    /// Resource name of the message (e.g. "spaces/SPACE_ID/messages/MSG_ID").
    #[serde(default)]
    pub name: Option<String>,
    /// The user who sent the message.
    #[serde(default)]
    pub sender: Option<ChatUser>,
    /// Plain-text body of the message.
    #[serde(default)]
    pub text: Option<String>,
    /// Text with @mentions stripped (preferred over `text`).
    #[serde(default)]
    pub argument_text: Option<String>,
    /// Thread the message belongs to.
    #[serde(default)]
    pub thread: Option<Thread>,
    /// Creation time of the message.
    #[serde(default)]
    pub create_time: Option<String>,
    /// File attachments (audio/image/etc.) — voice messages arrive here
    /// with `contentType: audio/*` and a `resourceName` we fetch via
    /// `media.download`.
    #[serde(default)]
    pub attachment: Option<Vec<ChatAttachment>>,
}

/// File attachment on a Google Chat message.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatAttachment {
    /// Display name (user-visible).
    #[serde(default)]
    pub content_name: Option<String>,
    /// MIME type — `audio/*` for voice messages.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Reference to the binary content.
    #[serde(default)]
    pub attachment_data_ref: Option<AttachmentDataRef>,
}

/// Reference to attachment binary data, used by the media.download endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentDataRef {
    /// Opaque resource name like `ChatDataMedia/...` passed to media.download.
    #[serde(default)]
    pub resource_name: Option<String>,
}

/// A Google Chat user.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatUser {
    /// Resource name of the user (e.g. "users/USER_ID").
    #[serde(default)]
    pub name: Option<String>,
    /// Display name of the user.
    #[serde(default)]
    pub display_name: Option<String>,
    /// User type (e.g. "HUMAN", "BOT").
    #[serde(rename = "type", default)]
    pub user_type: Option<String>,
}

/// A Google Chat space (room or DM).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Space {
    /// Resource name of the space (e.g. "spaces/SPACE_ID").
    #[serde(default)]
    pub name: Option<String>,
    /// Display name of the space.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Space type (e.g. "DM", "ROOM").
    #[serde(rename = "type", default)]
    pub space_type: Option<String>,
}

/// A Google Chat message thread.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    /// Resource name of the thread (e.g. "spaces/SPACE_ID/threads/THREAD_ID").
    #[serde(default)]
    pub name: Option<String>,
}

/// A simple text response to a Google Chat event (synchronous reply).
#[derive(Debug, Clone, Serialize)]
pub struct ChatResponse {
    /// Response message text.
    pub text: String,
}

impl ChatResponse {
    /// Create a new text response.
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// Request body for creating a message via the Google Chat API.
#[derive(Debug, Clone, Serialize)]
pub struct CreateMessageRequest {
    /// Message text content.
    pub text: String,
    /// Thread to post the message in (for threaded replies).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadRequest>,
}

/// Thread reference for creating a threaded reply.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadRequest {
    /// Resource name of the thread to reply in.
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_message_event() {
        let json = r#"{
            "type": "MESSAGE",
            "eventTime": "2024-01-01T00:00:00Z",
            "token": "test-token",
            "message": {
                "name": "spaces/SPACE1/messages/MSG1",
                "sender": {
                    "name": "users/USER1",
                    "displayName": "Alice",
                    "type": "HUMAN"
                },
                "text": "Hello bot",
                "argumentText": "Hello bot",
                "thread": {
                    "name": "spaces/SPACE1/threads/THREAD1"
                },
                "createTime": "2024-01-01T00:00:00Z"
            },
            "user": {
                "name": "users/USER1",
                "displayName": "Alice",
                "type": "HUMAN"
            },
            "space": {
                "name": "spaces/SPACE1",
                "displayName": "General",
                "type": "ROOM"
            }
        }"#;

        let event: ChatEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type, EventType::Message);
        assert_eq!(event.event_time.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert_eq!(event.token.as_deref(), Some("test-token"));

        let msg = event.message.as_ref().unwrap();
        assert_eq!(msg.name.as_deref(), Some("spaces/SPACE1/messages/MSG1"));
        assert_eq!(msg.text.as_deref(), Some("Hello bot"));
        assert_eq!(msg.argument_text.as_deref(), Some("Hello bot"));

        let sender = msg.sender.as_ref().unwrap();
        assert_eq!(sender.name.as_deref(), Some("users/USER1"));
        assert_eq!(sender.display_name.as_deref(), Some("Alice"));
        assert_eq!(sender.user_type.as_deref(), Some("HUMAN"));

        let thread = msg.thread.as_ref().unwrap();
        assert_eq!(
            thread.name.as_deref(),
            Some("spaces/SPACE1/threads/THREAD1")
        );

        let space = event.space.as_ref().unwrap();
        assert_eq!(space.name.as_deref(), Some("spaces/SPACE1"));
        assert_eq!(space.display_name.as_deref(), Some("General"));
        assert_eq!(space.space_type.as_deref(), Some("ROOM"));
    }

    /// EventType is deserialized from a string tag via a hand-rolled
    /// `Deserialize` impl. The Google Chat API spec defines the four
    /// well-known tags below; everything else MUST fall back to
    /// `Unknown(original_string)` so unhandled events surface in logs
    /// rather than failing the webhook with a 400.
    #[test]
    fn event_type_tag_mapping_with_unknown_fallback() {
        let cases = [
            ("MESSAGE", EventType::Message),
            ("ADDED_TO_SPACE", EventType::AddedToSpace),
            ("REMOVED_FROM_SPACE", EventType::RemovedFromSpace),
            ("CARD_CLICKED", EventType::CardClicked),
            (
                "WIDGET_UPDATED",
                EventType::Unknown("WIDGET_UPDATED".into()),
            ),
            ("", EventType::Unknown(String::new())),
        ];
        for (tag, expected) in cases {
            let json = format!(r#"{{"type": "{tag}"}}"#);
            let event: ChatEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(event.event_type, expected, "tag={tag:?}");
        }
    }

    /// `thread` MUST be omitted entirely (not `"thread": null`) when absent
    /// — Google Chat rejects null thread refs. Verified at the JSON-key
    /// level via `skip_serializing_if`.
    #[test]
    fn create_message_request_omits_thread_when_none() {
        let with_thread = CreateMessageRequest {
            text: "reply".into(),
            thread: Some(ThreadRequest {
                name: "spaces/SPACE1/threads/THREAD1".into(),
            }),
        };
        let v = serde_json::to_value(&with_thread).unwrap();
        assert_eq!(v["text"], "reply");
        assert_eq!(v["thread"]["name"], "spaces/SPACE1/threads/THREAD1");

        let without_thread = CreateMessageRequest {
            text: "hello".into(),
            thread: None,
        };
        let v = serde_json::to_value(&without_thread).unwrap();
        assert_eq!(v["text"], "hello");
        assert!(
            v.get("thread").is_none(),
            "thread must be omitted (not null) when None"
        );
    }
}
