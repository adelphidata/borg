use serde::{Deserialize, Deserializer, Serialize};

/// Discord interaction types.
///
/// Uses a custom deserializer to map unknown u8 values to `Unknown(u8)`.
#[derive(Debug, Clone, PartialEq)]
pub enum InteractionType {
    /// Ping from Discord to verify the endpoint.
    Ping,
    /// Slash command invocation.
    ApplicationCommand,
    /// Button or select menu interaction.
    MessageComponent,
    /// Autocomplete request for a command option.
    ApplicationCommandAutocomplete,
    /// Modal form submission.
    ModalSubmit,
    /// Unrecognized interaction type.
    Unknown(u8),
}

impl<'de> Deserialize<'de> for InteractionType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        Ok(match value {
            1 => InteractionType::Ping,
            2 => InteractionType::ApplicationCommand,
            3 => InteractionType::MessageComponent,
            4 => InteractionType::ApplicationCommandAutocomplete,
            5 => InteractionType::ModalSubmit,
            other => InteractionType::Unknown(other),
        })
    }
}

/// A Discord interaction received via webhook.
#[derive(Debug, Clone, Deserialize)]
pub struct Interaction {
    /// Unique interaction ID.
    pub id: String,
    /// Type of interaction received.
    #[serde(rename = "type")]
    pub interaction_type: InteractionType,
    /// ID of the application that owns the interaction.
    pub application_id: Option<String>,
    /// Command or component data payload.
    pub data: Option<InteractionData>,
    /// Guild member who triggered the interaction (in guilds).
    pub member: Option<GuildMember>,
    /// User who triggered the interaction (in DMs).
    pub user: Option<DiscordUser>,
    /// Continuation token for sending follow-up messages.
    pub token: String,
    /// Guild ID where the interaction occurred.
    pub guild_id: Option<String>,
    /// Channel ID where the interaction occurred.
    pub channel_id: Option<String>,
}

/// Data payload for application commands and components.
#[derive(Debug, Clone, Deserialize)]
pub struct InteractionData {
    /// Command ID (for application commands).
    pub id: Option<String>,
    /// Command name (for application commands).
    pub name: Option<String>,
    /// Command options provided by the user.
    pub options: Option<Vec<CommandOption>>,
    /// Custom ID (for message components and modals).
    pub custom_id: Option<String>,
}

/// A single command option (name + value).
#[derive(Debug, Clone, Deserialize)]
pub struct CommandOption {
    /// Option name.
    pub name: String,
    /// Option value provided by the user.
    pub value: serde_json::Value,
}

/// A Discord guild member (may contain a nested user).
#[derive(Debug, Clone, Deserialize)]
pub struct GuildMember {
    /// The user object for this guild member.
    pub user: Option<DiscordUser>,
}

/// A Discord user.
#[derive(Debug, Clone, Deserialize)]
pub struct DiscordUser {
    /// Unique user ID.
    pub id: String,
    /// Username.
    pub username: String,
    /// Whether the user is a bot.
    pub bot: Option<bool>,
}

/// Response sent back to Discord for an interaction.
#[derive(Debug, Clone, Serialize)]
pub struct InteractionResponse {
    /// Response type code (1=Pong, 4=Message, 5=Deferred).
    #[serde(rename = "type")]
    pub response_type: u8,
    /// Response data payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<InteractionResponseData>,
}

/// Data within an interaction response.
#[derive(Debug, Clone, Serialize)]
pub struct InteractionResponseData {
    /// Text content of the response message.
    pub content: String,
    /// Rich embed objects attached to the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embeds: Option<Vec<Embed>>,
    /// Action row components (buttons, select menus).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub components: Option<Vec<ActionRow>>,
}

/// A Discord embed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Embed {
    /// Embed title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Embed description text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Color code of the embed sidebar (decimal integer).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<u32>,
    /// Embed fields displayed in a grid layout.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<EmbedField>,
    /// Image attached to the embed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<EmbedImage>,
    /// Footer text at the bottom of the embed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer: Option<EmbedFooter>,
}

/// A field within a Discord embed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedField {
    /// Field name (bold header).
    pub name: String,
    /// Field value text.
    pub value: String,
    /// Whether this field should display inline with others.
    #[serde(default)]
    pub inline: bool,
}

/// An image within a Discord embed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedImage {
    /// Image URL.
    pub url: String,
}

/// Footer text for a Discord embed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedFooter {
    /// Footer text content.
    pub text: String,
}

/// Discord action row container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRow {
    /// Always 1 for ActionRow.
    #[serde(rename = "type")]
    pub component_type: u8,
    /// Child components within this action row.
    pub components: Vec<Component>,
}

/// A Discord message component (button or select menu).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Component {
    /// Component type (2=Button, 3=StringSelect, etc.).
    #[serde(rename = "type")]
    pub component_type: u8,
    /// Button style (1=Primary, 2=Secondary, 3=Success, 4=Danger, 5=Link).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style: Option<u8>,
    /// Display label text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Developer-defined identifier for the component.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_id: Option<String>,
    /// URL for link-style buttons.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl InteractionResponse {
    /// PONG response (type 1) — reply to a Ping interaction.
    pub fn pong() -> Self {
        Self {
            response_type: 1,
            data: None,
        }
    }

    /// Channel message with source response (type 4).
    pub fn message(text: &str) -> Self {
        Self {
            response_type: 4,
            data: Some(InteractionResponseData {
                content: text.to_string(),
                embeds: None,
                components: None,
            }),
        }
    }

    /// Deferred channel message with source response (type 5).
    pub fn deferred() -> Self {
        Self {
            response_type: 5,
            data: None,
        }
    }
}

/// Response from GET /users/@me.
#[derive(Debug, Clone, Deserialize)]
pub struct CurrentUser {
    /// User ID.
    pub id: String,
    /// Username.
    pub username: String,
    /// Whether the user is a bot.
    pub bot: Option<bool>,
}

/// Request body for POST /channels/{id}/messages.
#[derive(Debug, Clone, Serialize)]
pub struct CreateMessageRequest {
    /// Text content of the message.
    pub content: String,
}

/// Query parameters for fetching channel messages with pagination.
#[derive(Debug, Clone, Default)]
pub struct MessageQuery {
    /// Maximum number of messages to return (1-100).
    pub limit: u32,
    /// Get messages before this message ID.
    pub before: Option<String>,
    /// Get messages after this message ID.
    pub after: Option<String>,
    /// Get messages around this message ID.
    pub around: Option<String>,
}

/// A user who reacted to a message.
#[derive(Debug, Clone, Deserialize)]
pub struct ReactionUser {
    /// User ID.
    pub id: String,
    /// Username.
    pub username: String,
}

/// Info about a Discord thread channel.
#[derive(Debug, Clone, Deserialize)]
pub struct ThreadInfo {
    /// Thread channel ID.
    pub id: String,
    /// Thread name.
    pub name: Option<String>,
}

/// Info about a Discord guild member.
#[derive(Debug, Clone, Deserialize)]
pub struct MemberInfo {
    /// The user object for this member.
    pub user: Option<DiscordUser>,
    /// Server-specific nickname.
    pub nick: Option<String>,
    /// Role IDs assigned to this member.
    #[serde(default)]
    pub roles: Vec<String>,
}

/// Info about a Discord channel.
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelInfo {
    /// Channel ID.
    pub id: String,
    /// Channel name.
    pub name: Option<String>,
    /// Channel type (0=text, 2=voice, 4=category, 5=announcement, 10/11/12=thread, 13=stage, 15=forum).
    #[serde(rename = "type")]
    pub channel_type: u8,
    /// Channel topic.
    pub topic: Option<String>,
    /// Guild ID this channel belongs to.
    pub guild_id: Option<String>,
}

/// A message from Discord's channel messages endpoint.
#[derive(Debug, Clone)]
pub struct ChannelMessage {
    /// Author user ID.
    pub author_id: String,
    /// Author username.
    pub author_username: String,
    /// Message text content.
    pub content: String,
    /// Message ID.
    pub id: String,
}

/// Raw Discord message for deserialization.
#[derive(Debug, Deserialize)]
pub struct RawChannelMessage {
    /// Message ID.
    pub id: String,
    /// Message text content.
    pub content: String,
    /// Message author.
    pub author: RawAuthor,
}

/// Raw Discord author for deserialization.
#[derive(Debug, Deserialize)]
pub struct RawAuthor {
    /// User ID.
    pub id: String,
    /// Username.
    pub username: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_ping_interaction() {
        let json = r#"{
            "id": "123",
            "type": 1,
            "token": "tok",
            "application_id": "app1"
        }"#;
        let interaction: Interaction = serde_json::from_str(json).unwrap();
        assert_eq!(interaction.interaction_type, InteractionType::Ping);
        assert_eq!(interaction.id, "123");
        assert_eq!(interaction.application_id.as_deref(), Some("app1"));
    }

    #[test]
    fn deserialize_application_command() {
        let json = r#"{
            "id": "456",
            "type": 2,
            "token": "tok",
            "application_id": "app1",
            "data": {
                "id": "cmd1",
                "name": "ask",
                "options": [
                    { "name": "question", "value": "What is Rust?" }
                ]
            },
            "member": {
                "user": { "id": "u1", "username": "alice", "bot": false }
            },
            "channel_id": "ch1",
            "guild_id": "g1"
        }"#;
        let interaction: Interaction = serde_json::from_str(json).unwrap();
        assert_eq!(
            interaction.interaction_type,
            InteractionType::ApplicationCommand
        );
        let data = interaction.data.as_ref().unwrap();
        assert_eq!(data.name.as_deref(), Some("ask"));
        let opts = data.options.as_ref().unwrap();
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0].name, "question");
        assert_eq!(opts[0].value.as_str(), Some("What is Rust?"));
        let member = interaction.member.as_ref().unwrap();
        let user = member.user.as_ref().unwrap();
        assert_eq!(user.id, "u1");
        assert_eq!(user.bot, Some(false));
    }

    #[test]
    fn deserialize_message_component() {
        let json = r#"{
            "id": "789",
            "type": 3,
            "token": "tok",
            "data": { "custom_id": "btn_confirm" },
            "user": { "id": "u2", "username": "bob" }
        }"#;
        let interaction: Interaction = serde_json::from_str(json).unwrap();
        assert_eq!(
            interaction.interaction_type,
            InteractionType::MessageComponent
        );
        assert_eq!(
            interaction.data.as_ref().unwrap().custom_id.as_deref(),
            Some("btn_confirm")
        );
    }

    #[test]
    fn deserialize_unknown_interaction_type() {
        let json = r#"{
            "id": "999",
            "type": 99,
            "token": "tok"
        }"#;
        let interaction: Interaction = serde_json::from_str(json).unwrap();
        assert_eq!(interaction.interaction_type, InteractionType::Unknown(99));
    }

    #[test]
    fn interaction_response_pong_serialization() {
        let resp = InteractionResponse::pong();
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["type"], 1);
        assert!(json.get("data").is_none());
    }

    #[test]
    fn interaction_response_message_serialization() {
        let resp = InteractionResponse::message("Hello!");
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["type"], 4);
        assert_eq!(json["data"]["content"], "Hello!");
    }

    #[test]
    fn interaction_response_deferred_serialization() {
        let resp = InteractionResponse::deferred();
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["type"], 5);
        assert!(json.get("data").is_none());
    }

    #[test]
    fn deserialize_autocomplete_interaction() {
        let json = r#"{
            "id": "100",
            "type": 4,
            "token": "tok",
            "data": { "name": "search" }
        }"#;
        let interaction: Interaction = serde_json::from_str(json).unwrap();
        assert_eq!(
            interaction.interaction_type,
            InteractionType::ApplicationCommandAutocomplete
        );
    }

    #[test]
    fn deserialize_modal_submit() {
        let json = r#"{
            "id": "200",
            "type": 5,
            "token": "tok",
            "data": { "custom_id": "feedback_form" },
            "user": { "id": "u3", "username": "carol" }
        }"#;
        let interaction: Interaction = serde_json::from_str(json).unwrap();
        assert_eq!(interaction.interaction_type, InteractionType::ModalSubmit);
    }

    #[test]
    fn deserialize_bot_user() {
        let json = r#"{
            "id": "300",
            "type": 2,
            "token": "tok",
            "user": { "id": "bot1", "username": "mybot", "bot": true }
        }"#;
        let interaction: Interaction = serde_json::from_str(json).unwrap();
        assert_eq!(interaction.user.as_ref().unwrap().bot, Some(true));
    }

    #[test]
    fn create_message_request_serialization() {
        let req = CreateMessageRequest {
            content: "test message".into(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["content"], "test message");
    }

    #[test]
    fn deserialize_current_user() {
        let json = r#"{ "id": "u1", "username": "borgbot", "bot": true }"#;
        let user: CurrentUser = serde_json::from_str(json).unwrap();
        assert_eq!(user.id, "u1");
        assert_eq!(user.username, "borgbot");
        assert_eq!(user.bot, Some(true));
    }

    #[test]
    fn embed_serialization_minimal() {
        let embed = Embed {
            title: None,
            description: Some("Hello".into()),
            color: None,
            fields: vec![],
            image: None,
            footer: None,
        };
        let json = serde_json::to_value(&embed).unwrap();
        assert_eq!(json["description"], "Hello");
        assert!(json.get("title").is_none());
        assert!(json.get("fields").is_none());
        assert!(json.get("color").is_none());
    }

    #[test]
    fn embed_serialization_full() {
        let embed = Embed {
            title: Some("Title".into()),
            description: Some("Desc".into()),
            color: Some(0xFF0000),
            fields: vec![EmbedField {
                name: "Field1".into(),
                value: "Val1".into(),
                inline: true,
            }],
            image: Some(EmbedImage {
                url: "https://example.com/img.png".into(),
            }),
            footer: Some(EmbedFooter {
                text: "footer text".into(),
            }),
        };
        let json = serde_json::to_value(&embed).unwrap();
        assert_eq!(json["title"], "Title");
        assert_eq!(json["color"], 0xFF0000);
        assert_eq!(json["fields"][0]["name"], "Field1");
        assert_eq!(json["fields"][0]["inline"], true);
        assert_eq!(json["image"]["url"], "https://example.com/img.png");
        assert_eq!(json["footer"]["text"], "footer text");
    }

    #[test]
    fn embed_deserialization() {
        let json = r#"{ "title": "T", "description": "D", "color": 255 }"#;
        let embed: Embed = serde_json::from_str(json).unwrap();
        assert_eq!(embed.title.as_deref(), Some("T"));
        assert_eq!(embed.color, Some(255));
        assert!(embed.fields.is_empty());
    }

    #[test]
    fn action_row_serialization() {
        let row = ActionRow {
            component_type: 1,
            components: vec![Component {
                component_type: 2,
                style: Some(1),
                label: Some("Click me".into()),
                custom_id: Some("btn1".into()),
                url: None,
            }],
        };
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["type"], 1);
        assert_eq!(json["components"][0]["type"], 2);
        assert_eq!(json["components"][0]["style"], 1);
        assert_eq!(json["components"][0]["label"], "Click me");
        assert_eq!(json["components"][0]["custom_id"], "btn1");
        assert!(json["components"][0].get("url").is_none());
    }

    #[test]
    fn component_link_button() {
        let comp = Component {
            component_type: 2,
            style: Some(5),
            label: Some("Visit".into()),
            custom_id: None,
            url: Some("https://example.com".into()),
        };
        let json = serde_json::to_value(&comp).unwrap();
        assert_eq!(json["style"], 5);
        assert_eq!(json["url"], "https://example.com");
        assert!(json.get("custom_id").is_none());
    }

    #[test]
    fn interaction_response_with_embeds_and_components() {
        let resp = InteractionResponse {
            response_type: 4,
            data: Some(InteractionResponseData {
                content: "text".into(),
                embeds: Some(vec![Embed {
                    title: Some("E".into()),
                    description: None,
                    color: None,
                    fields: vec![],
                    image: None,
                    footer: None,
                }]),
                components: Some(vec![ActionRow {
                    component_type: 1,
                    components: vec![],
                }]),
            }),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["data"]["content"], "text");
        assert_eq!(json["data"]["embeds"][0]["title"], "E");
        assert_eq!(json["data"]["components"][0]["type"], 1);
    }

    #[test]
    fn interaction_response_data_without_embeds_or_components() {
        let data = InteractionResponseData {
            content: "hello".into(),
            embeds: None,
            components: None,
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["content"], "hello");
        assert!(json.get("embeds").is_none());
        assert!(json.get("components").is_none());
    }

    // ── New feature-parity types ──

    #[test]
    fn message_query_default() {
        let q = MessageQuery::default();
        assert_eq!(q.limit, 0);
        assert!(q.before.is_none());
        assert!(q.after.is_none());
        assert!(q.around.is_none());
    }

    #[test]
    fn deserialize_reaction_user() {
        let json = r#"{"id": "123", "username": "alice"}"#;
        let user: ReactionUser = serde_json::from_str(json).unwrap();
        assert_eq!(user.id, "123");
        assert_eq!(user.username, "alice");
    }

    #[test]
    fn deserialize_reaction_user_list() {
        let json = r#"[
            {"id": "1", "username": "alice"},
            {"id": "2", "username": "bob"}
        ]"#;
        let users: Vec<ReactionUser> = serde_json::from_str(json).unwrap();
        assert_eq!(users.len(), 2);
    }

    #[test]
    fn deserialize_thread_info() {
        let json = r#"{"id": "999", "name": "bug-report"}"#;
        let info: ThreadInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.id, "999");
        assert_eq!(info.name.as_deref(), Some("bug-report"));
    }

    #[test]
    fn deserialize_thread_info_no_name() {
        let json = r#"{"id": "999"}"#;
        let info: ThreadInfo = serde_json::from_str(json).unwrap();
        assert!(info.name.is_none());
    }

    #[test]
    fn deserialize_member_info() {
        let json = r#"{
            "user": {"id": "u1", "username": "alice"},
            "nick": "Ali",
            "roles": ["r1", "r2"]
        }"#;
        let info: MemberInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.user.as_ref().unwrap().id, "u1");
        assert_eq!(info.nick.as_deref(), Some("Ali"));
        assert_eq!(info.roles, vec!["r1", "r2"]);
    }

    #[test]
    fn deserialize_member_info_minimal() {
        let json = r#"{}"#;
        let info: MemberInfo = serde_json::from_str(json).unwrap();
        assert!(info.user.is_none());
        assert!(info.nick.is_none());
        assert!(info.roles.is_empty());
    }

    #[test]
    fn deserialize_channel_info() {
        let json = r#"{
            "id": "ch1",
            "name": "general",
            "type": 0,
            "topic": "Welcome!",
            "guild_id": "g1"
        }"#;
        let info: ChannelInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.id, "ch1");
        assert_eq!(info.name.as_deref(), Some("general"));
        assert_eq!(info.channel_type, 0);
        assert_eq!(info.topic.as_deref(), Some("Welcome!"));
        assert_eq!(info.guild_id.as_deref(), Some("g1"));
    }

    #[test]
    fn deserialize_channel_info_thread() {
        let json = r#"{"id": "t1", "type": 11}"#;
        let info: ChannelInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.channel_type, 11);
        assert!(info.name.is_none());
        assert!(info.topic.is_none());
    }

    #[test]
    fn deserialize_raw_channel_message() {
        let json =
            r#"{"id": "m1", "content": "hello", "author": {"id": "u1", "username": "alice"}}"#;
        let msg: RawChannelMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.id, "m1");
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.author.id, "u1");
    }
}
