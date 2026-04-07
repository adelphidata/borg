use serde::{Deserialize, Serialize};

/// A Bot Framework Activity received from Microsoft Teams.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    /// Unique activity ID.
    pub id: String,
    /// Activity type (e.g. "message", "conversationUpdate").
    #[serde(rename = "type")]
    pub activity_type: String,
    /// Text content of the message.
    #[serde(default)]
    pub text: Option<String>,
    /// Sender of the activity.
    #[serde(default)]
    pub from: Option<ChannelAccount>,
    /// Conversation the activity belongs to.
    #[serde(default)]
    pub conversation: Option<ConversationAccount>,
    /// Intended recipient of the activity.
    #[serde(default)]
    pub recipient: Option<ChannelAccount>,
    /// Bot Framework service URL for sending replies.
    #[serde(default)]
    pub service_url: Option<String>,
    /// ID of the message being replied to.
    #[serde(default)]
    pub reply_to_id: Option<String>,
    /// Entities attached to the activity (e.g. @mentions).
    #[serde(default)]
    pub entities: Option<Vec<Entity>>,
    /// Invoke activity payload (e.g. Adaptive Card submit action).
    #[serde(default)]
    pub value: Option<serde_json::Value>,
    /// ISO 8601 timestamp of the activity.
    #[serde(default)]
    pub timestamp: Option<String>,
    /// Members added in a conversationUpdate activity.
    #[serde(default)]
    pub members_added: Option<Vec<ChannelAccount>>,
    /// Members removed in a conversationUpdate activity.
    #[serde(default)]
    pub members_removed: Option<Vec<ChannelAccount>>,
}

/// A user or bot account in a Teams conversation.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChannelAccount {
    /// Account ID (user or bot).
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: Option<String>,
}

/// A conversation reference in a Teams activity.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationAccount {
    /// Conversation ID.
    pub id: String,
    /// Conversation display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether this is a group conversation.
    #[serde(default)]
    pub is_group: Option<bool>,
}

/// An entity attached to an activity (e.g. an @mention).
#[derive(Debug, Clone, Deserialize)]
pub struct Entity {
    /// Entity type (e.g. "mention", "clientInfo").
    #[serde(rename = "type")]
    pub entity_type: String,
    /// The mentioned user or bot account.
    #[serde(default)]
    pub mentioned: Option<ChannelAccount>,
}

/// An outbound reply activity sent back to Teams.
#[derive(Debug, Clone, Serialize)]
pub struct ReplyActivity {
    /// Activity type (e.g. "message", "typing").
    #[serde(rename = "type")]
    pub activity_type: String,
    /// Text content of the reply.
    pub text: String,
    /// Adaptive Card attachments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<AdaptiveCardAttachment>>,
}

impl ReplyActivity {
    /// Create a new message reply activity.
    pub fn message(text: impl Into<String>) -> Self {
        Self {
            activity_type: "message".to_string(),
            text: text.into(),
            attachments: None,
        }
    }

    /// Create a typing indicator activity.
    pub fn typing() -> Self {
        Self {
            activity_type: "typing".to_string(),
            text: String::new(),
            attachments: None,
        }
    }

    /// Create a message with an Adaptive Card attachment.
    pub fn with_adaptive_card(text: impl Into<String>, card: serde_json::Value) -> Self {
        Self {
            activity_type: "message".to_string(),
            text: text.into(),
            attachments: Some(vec![AdaptiveCardAttachment {
                content_type: "application/vnd.microsoft.card.adaptive".to_string(),
                content: card,
            }]),
        }
    }
}

/// An Adaptive Card attachment for Teams.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdaptiveCardAttachment {
    /// Content type (always "application/vnd.microsoft.card.adaptive").
    pub content_type: String,
    /// Adaptive Card JSON body.
    pub content: serde_json::Value,
}

/// Members added/removed in a conversationUpdate activity.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationUpdateData {
    /// Members added to the conversation.
    #[serde(default)]
    pub members_added: Option<Vec<ChannelAccount>>,
    /// Members removed from the conversation.
    #[serde(default)]
    pub members_removed: Option<Vec<ChannelAccount>>,
}

/// OAuth2 token response from the Microsoft identity platform.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    /// Bearer token for authenticating Bot Framework API calls.
    pub access_token: String,
    /// Token lifetime in seconds.
    pub expires_in: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_message_activity() {
        let json = r#"{
            "id": "act-1",
            "type": "message",
            "text": "hello bot",
            "from": {"id": "user-1", "name": "Alice"},
            "conversation": {"id": "conv-1", "name": "General", "isGroup": true},
            "recipient": {"id": "bot-1", "name": "MyBot"},
            "serviceUrl": "https://smba.trafficmanager.net/teams/",
            "replyToId": "parent-1",
            "timestamp": "2026-03-17T12:00:00Z"
        }"#;

        let activity: Activity = serde_json::from_str(json).unwrap();
        assert_eq!(activity.id, "act-1");
        assert_eq!(activity.activity_type, "message");
        assert_eq!(activity.text.as_deref(), Some("hello bot"));
        assert_eq!(activity.from.as_ref().unwrap().id, "user-1");
        assert_eq!(
            activity.from.as_ref().unwrap().name.as_deref(),
            Some("Alice")
        );
        assert_eq!(activity.conversation.as_ref().unwrap().id, "conv-1");
        assert_eq!(activity.conversation.as_ref().unwrap().is_group, Some(true));
        assert_eq!(activity.recipient.as_ref().unwrap().id, "bot-1");
        assert_eq!(
            activity.service_url.as_deref(),
            Some("https://smba.trafficmanager.net/teams/")
        );
        assert_eq!(activity.reply_to_id.as_deref(), Some("parent-1"));
        assert_eq!(activity.timestamp.as_deref(), Some("2026-03-17T12:00:00Z"));
    }

    #[test]
    fn deserialize_minimal_activity() {
        let json = r#"{"id": "act-2", "type": "conversationUpdate"}"#;
        let activity: Activity = serde_json::from_str(json).unwrap();
        assert_eq!(activity.id, "act-2");
        assert_eq!(activity.activity_type, "conversationUpdate");
        assert!(activity.text.is_none());
        assert!(activity.from.is_none());
        assert!(activity.conversation.is_none());
        assert!(activity.recipient.is_none());
        assert!(activity.service_url.is_none());
        assert!(activity.entities.is_none());
    }

    #[test]
    fn deserialize_activity_with_entities() {
        let json = r#"{
            "id": "act-3",
            "type": "message",
            "text": "<at>MyBot</at> help me",
            "entities": [
                {"type": "mention", "mentioned": {"id": "bot-1", "name": "MyBot"}},
                {"type": "clientInfo"}
            ]
        }"#;
        let activity: Activity = serde_json::from_str(json).unwrap();
        let entities = activity.entities.unwrap();
        assert_eq!(entities.len(), 2);
        assert_eq!(entities[0].entity_type, "mention");
        assert_eq!(entities[0].mentioned.as_ref().unwrap().id, "bot-1");
        assert_eq!(entities[1].entity_type, "clientInfo");
        assert!(entities[1].mentioned.is_none());
    }

    #[test]
    fn serialize_reply_activity() {
        let reply = ReplyActivity::message("Hello back!");
        let json = serde_json::to_value(&reply).unwrap();
        assert_eq!(json["type"], "message");
        assert_eq!(json["text"], "Hello back!");
    }

    #[test]
    fn deserialize_token_response() {
        let json = r#"{"access_token": "eyJ0eXAi...", "expires_in": 3600}"#;
        let resp: TokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "eyJ0eXAi...");
        assert_eq!(resp.expires_in, 3600);
    }

    #[test]
    fn deserialize_conversation_account_no_group() {
        let json = r#"{"id": "conv-2"}"#;
        let conv: ConversationAccount = serde_json::from_str(json).unwrap();
        assert_eq!(conv.id, "conv-2");
        assert!(conv.name.is_none());
        assert!(conv.is_group.is_none());
    }

    #[test]
    fn channel_account_serialize_roundtrip() {
        let account = ChannelAccount {
            id: "user-1".to_string(),
            name: Some("Alice".to_string()),
        };
        let json = serde_json::to_string(&account).unwrap();
        let deserialized: ChannelAccount = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "user-1");
        assert_eq!(deserialized.name.as_deref(), Some("Alice"));
    }

    #[test]
    fn serialize_typing_activity() {
        let typing = ReplyActivity::typing();
        let json = serde_json::to_value(&typing).unwrap();
        assert_eq!(json["type"], "typing");
        assert!(json.get("attachments").is_none());
    }

    #[test]
    fn serialize_adaptive_card_attachment() {
        let card_content = serde_json::json!({
            "type": "AdaptiveCard",
            "version": "1.4",
            "body": [{"type": "TextBlock", "text": "Hello"}]
        });
        let reply = ReplyActivity::with_adaptive_card("fallback text", card_content);
        let json = serde_json::to_value(&reply).unwrap();
        assert_eq!(json["type"], "message");
        assert_eq!(json["text"], "fallback text");
        let attachments = json["attachments"].as_array().unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(
            attachments[0]["contentType"],
            "application/vnd.microsoft.card.adaptive"
        );
        assert_eq!(attachments[0]["content"]["type"], "AdaptiveCard");
    }

    #[test]
    fn serialize_reply_no_attachments_skipped() {
        let reply = ReplyActivity::message("hi");
        let json = serde_json::to_value(&reply).unwrap();
        assert!(json.get("attachments").is_none());
    }

    #[test]
    fn deserialize_conversation_update_with_members() {
        let json = r#"{
            "id": "act-cu",
            "type": "conversationUpdate",
            "membersAdded": [
                {"id": "user-new", "name": "NewUser"}
            ],
            "conversation": {"id": "conv-1"},
            "serviceUrl": "https://smba.trafficmanager.net/teams/"
        }"#;
        let activity: Activity = serde_json::from_str(json).unwrap();
        assert_eq!(activity.activity_type, "conversationUpdate");
        let members = activity.members_added.unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].id, "user-new");
    }

    #[test]
    fn deserialize_conversation_update_members_removed() {
        let json = r#"{
            "id": "act-cu2",
            "type": "conversationUpdate",
            "membersRemoved": [
                {"id": "user-gone"}
            ]
        }"#;
        let activity: Activity = serde_json::from_str(json).unwrap();
        let removed = activity.members_removed.unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].id, "user-gone");
    }

    #[test]
    fn deserialize_invoke_activity_with_value() {
        let json = r#"{
            "id": "act-inv",
            "type": "invoke",
            "from": {"id": "user-1", "name": "Alice"},
            "conversation": {"id": "conv-1"},
            "value": {"text": "button clicked", "action": "submit"}
        }"#;
        let activity: Activity = serde_json::from_str(json).unwrap();
        assert_eq!(activity.activity_type, "invoke");
        let value = activity.value.unwrap();
        assert_eq!(value["text"], "button clicked");
        assert_eq!(value["action"], "submit");
    }
}
