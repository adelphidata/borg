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
    /// Emoji reactions added in a `messageReaction` activity.
    #[serde(default)]
    pub reactions_added: Option<Vec<MessageReaction>>,
    /// Emoji reactions removed in a `messageReaction` activity.
    #[serde(default)]
    pub reactions_removed: Option<Vec<MessageReaction>>,
}

/// A single reaction entry inside a `messageReaction` activity.
#[derive(Debug, Clone, Deserialize)]
pub struct MessageReaction {
    /// Reaction type: a well-known name (`like`, `heart`, `laugh`, `surprised`,
    /// `sad`, `angry`) or a Unicode emoji.
    #[serde(rename = "type")]
    pub reaction_type: String,
}

/// Well-known Teams reaction types accepted by Graph `setReaction`.
pub const TEAMS_REACTION_TYPES: &[&str] = &["like", "heart", "laugh", "surprised", "sad", "angry"];

/// Normalize a reaction type string for outbound calls.
///
/// - Trims whitespace.
/// - Lowercases known names (`like`, `heart`, …).
/// - Passes Unicode emoji through unchanged (Graph beta accepts both).
/// - Returns `Err` for empty input.
///
/// Mirrors OpenClaw's `normalizeReactionType` so behavior stays in sync.
pub fn normalize_reaction_type(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!(
            "Reaction type is required. Common types: {}",
            TEAMS_REACTION_TYPES.join(", ")
        );
    }
    let lowered = trimmed.to_lowercase();
    if TEAMS_REACTION_TYPES.iter().any(|t| *t == lowered) {
        Ok(lowered)
    } else {
        Ok(trimmed.to_string())
    }
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
    /// Entities attached to the activity (e.g. streaminfo for streaming).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entities: Option<Vec<serde_json::Value>>,
}

impl ReplyActivity {
    /// Create a new message reply activity.
    pub fn message(text: impl Into<String>) -> Self {
        Self {
            activity_type: "message".to_string(),
            text: text.into(),
            attachments: None,
            entities: None,
        }
    }

    /// Create a typing indicator activity.
    pub fn typing() -> Self {
        Self {
            activity_type: "typing".to_string(),
            text: String::new(),
            attachments: None,
            entities: None,
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
            entities: None,
        }
    }

    /// Create a streaming typing activity with `streaminfo` entity.
    ///
    /// Teams renders these as a progress indicator while the bot is generating.
    pub fn streaming_typing(text: impl Into<String>, sequence: u32) -> Self {
        Self {
            activity_type: "typing".to_string(),
            text: text.into(),
            attachments: None,
            entities: Some(vec![serde_json::json!({
                "type": "streaminfo",
                "streamType": "streaming",
                "streamSequence": sequence,
            })]),
        }
    }

    /// Create a message with embedded @mentions.
    ///
    /// `text` should already contain `<at>Display Name</at>` tags at the
    /// positions where each mention appears. The matching `mention` entities
    /// are attached so Teams renders the mention pill instead of literal markup.
    pub fn message_with_mentions(text: impl Into<String>, mentions: &[Mention]) -> Self {
        let entities = if mentions.is_empty() {
            None
        } else {
            Some(build_mention_entities(mentions))
        };
        Self {
            activity_type: "message".to_string(),
            text: text.into(),
            attachments: None,
            entities,
        }
    }

    /// Create a final streaming message activity.
    ///
    /// Closes the streaming session with `streamType: "final"`.
    pub fn streaming_final(text: impl Into<String>) -> Self {
        Self {
            activity_type: "message".to_string(),
            text: text.into(),
            attachments: None,
            entities: Some(vec![serde_json::json!({
                "type": "streaminfo",
                "streamType": "final",
            })]),
        }
    }
}

/// A user @mention to embed in an outbound Teams message.
///
/// Teams renders mentions only when **both** parts line up: the message body
/// contains an `<at>Display Name</at>` tag AND the activity's `entities`
/// array contains a matching `mention` entity referencing the same name and
/// the user's account ID. Either half alone renders as plain text.
#[derive(Debug, Clone)]
pub struct Mention {
    /// AAD object ID or Bot Framework ID (`28:...` / `29:...`) of the mentioned account.
    pub id: String,
    /// Display name to render inside `<at>...</at>` tags.
    pub name: String,
}

impl Mention {
    /// Construct a mention from id + display name.
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
        }
    }
}

/// Apply mentions to a body of text, returning a tuple of
/// `(rewritten_body, entities_json)`.
///
/// The body is left **unchanged**: callers should already have inserted the
/// `<at>Name</at>` tags where they want the mentions to appear (mirrors how
/// Teams clients author messages). The returned entities array is what gets
/// attached to `ReplyActivity.entities`.
///
/// Returns an empty vec when `mentions` is empty so the activity stays
/// entity-free (Teams treats empty `entities: []` as noise on some surfaces).
pub fn build_mention_entities(mentions: &[Mention]) -> Vec<serde_json::Value> {
    mentions
        .iter()
        .map(|m| {
            serde_json::json!({
                "type": "mention",
                "text": format!("<at>{}</at>", m.name),
                "mentioned": {
                    "id": m.id,
                    "name": m.name,
                },
            })
        })
        .collect()
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
    fn message_with_mentions_emits_at_tag_and_entity() {
        let mentions = [Mention::new("aad-id-1", "Alice")];
        let activity = ReplyActivity::message_with_mentions("Hi <at>Alice</at>!", &mentions);
        let json = serde_json::to_value(&activity).unwrap();
        assert_eq!(json["type"], "message");
        assert_eq!(json["text"], "Hi <at>Alice</at>!");
        let entities = json["entities"].as_array().expect("entities present");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0]["type"], "mention");
        assert_eq!(entities[0]["text"], "<at>Alice</at>");
        assert_eq!(entities[0]["mentioned"]["id"], "aad-id-1");
        assert_eq!(entities[0]["mentioned"]["name"], "Alice");
    }

    #[test]
    fn message_with_multiple_mentions_preserves_order() {
        let mentions = [Mention::new("id-1", "Alice"), Mention::new("id-2", "Bob")];
        let activity =
            ReplyActivity::message_with_mentions("<at>Alice</at> and <at>Bob</at>", &mentions);
        let json = serde_json::to_value(&activity).unwrap();
        let entities = json["entities"].as_array().unwrap();
        assert_eq!(entities.len(), 2);
        assert_eq!(entities[0]["mentioned"]["id"], "id-1");
        assert_eq!(entities[1]["mentioned"]["id"], "id-2");
    }

    #[test]
    fn message_with_no_mentions_omits_entities_array() {
        let activity = ReplyActivity::message_with_mentions("plain text", &[]);
        let json = serde_json::to_value(&activity).unwrap();
        assert!(
            json.get("entities").is_none(),
            "empty mentions should omit entities entirely, got {json}"
        );
    }

    #[test]
    fn build_mention_entities_keyed_by_aad_id_not_name() {
        // Two mentions with the same display name but different IDs must
        // produce two distinct entities — Teams routes the @ping by id.
        let mentions = [Mention::new("id-A", "Alex"), Mention::new("id-B", "Alex")];
        let entities = build_mention_entities(&mentions);
        assert_eq!(entities.len(), 2);
        assert_ne!(
            entities[0]["mentioned"]["id"],
            entities[1]["mentioned"]["id"]
        );
    }

    #[test]
    fn normalize_reaction_type_lowercases_known_names() {
        for (input, expected) in [
            ("Like", "like"),
            ("HEART", "heart"),
            ("laugh", "laugh"),
            ("  Surprised  ", "surprised"),
            ("Sad", "sad"),
            ("ANGRY", "angry"),
        ] {
            assert_eq!(normalize_reaction_type(input).unwrap(), expected);
        }
    }

    #[test]
    fn normalize_reaction_type_passes_emoji_through() {
        // Unicode emoji must not be lowercased / mangled.
        assert_eq!(normalize_reaction_type("👍").unwrap(), "👍");
        assert_eq!(normalize_reaction_type("🎉").unwrap(), "🎉");
    }

    #[test]
    fn normalize_reaction_type_passes_unknown_strings_through_untrimmed_case() {
        // Unknown strings are trimmed but case-preserved (Graph treats unknowns as custom).
        assert_eq!(
            normalize_reaction_type(" CustomEmoji ").unwrap(),
            "CustomEmoji"
        );
    }

    #[test]
    fn normalize_reaction_type_empty_errors() {
        assert!(normalize_reaction_type("").is_err());
        assert!(normalize_reaction_type("   ").is_err());
    }

    #[test]
    fn deserialize_message_reaction_activity() {
        let json = r#"{
            "id": "act-r",
            "type": "messageReaction",
            "from": {"id": "user-1"},
            "recipient": {"id": "bot-1"},
            "replyToId": "parent-1",
            "reactionsAdded": [{"type": "like"}]
        }"#;
        let activity: Activity = serde_json::from_str(json).unwrap();
        assert_eq!(activity.activity_type, "messageReaction");
        let added = activity.reactions_added.expect("reactionsAdded present");
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].reaction_type, "like");
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
