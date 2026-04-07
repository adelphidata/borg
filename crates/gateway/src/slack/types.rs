use serde::{Deserialize, Serialize};

/// Top-level Slack Event API envelope.
/// Slack sends either a `url_verification` challenge or an `event_callback`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum SlackEnvelope {
    #[serde(rename = "url_verification")]
    UrlVerification { challenge: String },
    #[serde(rename = "event_callback")]
    EventCallback(Box<EventCallback>),
}

/// An `event_callback` envelope containing a Slack event.
#[derive(Debug, Clone, Deserialize)]
pub struct EventCallback {
    /// Verification token (deprecated; use signing secret instead).
    pub token: Option<String>,
    /// Workspace identifier.
    pub team_id: Option<String>,
    /// Unique event identifier for deduplication.
    pub event_id: Option<String>,
    /// The inner event payload.
    pub event: SlackEvent,
}

/// Inner Slack event (message or app_mention).
#[derive(Debug, Clone, Deserialize)]
pub struct SlackEvent {
    /// Event type identifier (e.g. "message", "app_mention").
    #[serde(rename = "type")]
    pub event_type: String,
    /// Message subtype (e.g. "bot_message", "file_share").
    pub subtype: Option<String>,
    /// User ID of the sender.
    pub user: Option<String>,
    /// Text content of the message.
    pub text: Option<String>,
    /// Message timestamp (serves as unique message ID).
    pub ts: Option<String>,
    /// Parent thread timestamp for threaded replies.
    pub thread_ts: Option<String>,
    /// Channel ID where the event occurred.
    pub channel: Option<String>,
    /// Channel type (e.g. "channel", "im", "mpim").
    pub channel_type: Option<String>,
    /// Bot ID if the message was sent by a bot.
    pub bot_id: Option<String>,
    /// Files attached to the message.
    pub files: Option<Vec<SlackFile>>,
}

/// File metadata attached to a Slack message event.
#[derive(Debug, Clone, Deserialize)]
pub struct SlackFile {
    /// Unique file identifier.
    pub id: String,
    /// Original filename.
    pub name: Option<String>,
    /// MIME type of the file.
    pub mimetype: Option<String>,
    /// Authenticated URL to download the file.
    pub url_private: Option<String>,
    /// File size in bytes.
    pub size: Option<u64>,
    /// Slack file type identifier (e.g. "pdf", "png").
    pub filetype: Option<String>,
}

/// Response from Slack `auth.test` API.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthTestResponse {
    /// Whether the authentication test succeeded.
    pub ok: bool,
    /// Authenticated user ID.
    pub user_id: Option<String>,
    /// Authenticated user name.
    pub user: Option<String>,
    /// Workspace name.
    pub team: Option<String>,
    /// Workspace ID.
    pub team_id: Option<String>,
    /// Error code on failure.
    pub error: Option<String>,
}

/// Request body for `chat.postMessage`.
#[derive(Debug, Serialize)]
pub struct PostMessageRequest {
    /// Target channel ID.
    pub channel: String,
    /// Message text (used as fallback when blocks are present).
    pub text: String,
    /// Thread timestamp to reply in a thread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    /// Block Kit layout blocks for rich formatting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<Vec<serde_json::Value>>,
}

/// Slack slash command payload (application/x-www-form-urlencoded).
#[derive(Debug, Clone, Deserialize)]
pub struct SlashCommandPayload {
    /// The slash command name (e.g. "/borg").
    pub command: String,
    /// Text following the command.
    pub text: Option<String>,
    /// ID of the user who invoked the command.
    pub user_id: String,
    /// Username of the invoking user.
    pub user_name: Option<String>,
    /// Channel ID where the command was invoked.
    pub channel_id: String,
    /// Channel name where the command was invoked.
    pub channel_name: Option<String>,
    /// Workspace ID.
    pub team_id: Option<String>,
    /// URL for posting delayed responses.
    pub response_url: Option<String>,
    /// Trigger ID for opening modals.
    pub trigger_id: Option<String>,
}

/// Slack interactive payload (Block Kit buttons, select menus, modals).
/// Sent as `application/x-www-form-urlencoded` with a `payload` JSON field.
#[derive(Debug, Clone, Deserialize)]
pub struct InteractionPayload {
    /// Interaction type (e.g. "block_actions", "view_submission").
    #[serde(rename = "type")]
    pub interaction_type: String,
    /// User who triggered the interaction.
    pub user: InteractionUser,
    /// Channel where the interaction occurred.
    pub channel: Option<InteractionChannel>,
    /// List of actions triggered by the user.
    pub actions: Option<Vec<BlockAction>>,
    /// Trigger ID for opening modals.
    pub trigger_id: Option<String>,
    /// URL for posting delayed responses.
    pub response_url: Option<String>,
    /// View payload for modal submissions.
    pub view: Option<ViewPayload>,
}

/// A user in a Slack interaction payload.
#[derive(Debug, Clone, Deserialize)]
pub struct InteractionUser {
    /// User ID.
    pub id: String,
    /// Username.
    pub username: Option<String>,
}

/// A channel in a Slack interaction payload.
#[derive(Debug, Clone, Deserialize)]
pub struct InteractionChannel {
    /// Channel ID.
    pub id: String,
    /// Channel name.
    pub name: Option<String>,
}

/// A single Block Kit action from an interaction.
#[derive(Debug, Clone, Deserialize)]
pub struct BlockAction {
    /// Unique action identifier defined in the block.
    pub action_id: String,
    /// Action element type (e.g. "button", "static_select").
    #[serde(rename = "type")]
    pub action_type: Option<String>,
    /// Value associated with the action (for buttons).
    pub value: Option<String>,
    /// Block ID containing this action.
    pub block_id: Option<String>,
    /// Selected option (for select menus).
    pub selected_option: Option<SelectedOption>,
}

/// A selected option from a Slack select menu.
#[derive(Debug, Clone, Deserialize)]
pub struct SelectedOption {
    /// Value of the selected option.
    pub value: String,
    /// Display text of the selected option.
    pub text: Option<serde_json::Value>,
}

/// Payload for a Slack modal view submission or interaction.
#[derive(Debug, Clone, Deserialize)]
pub struct ViewPayload {
    /// Callback ID identifying which view was submitted.
    pub callback_id: Option<String>,
    /// Current state of the view's input elements.
    pub state: Option<ViewState>,
}

/// State of input elements in a Slack modal view.
#[derive(Debug, Clone, Deserialize)]
pub struct ViewState {
    /// Map of block_id -> action_id -> action value.
    pub values: std::collections::BTreeMap<String, std::collections::BTreeMap<String, ActionValue>>,
}

/// Value of an input element in a Slack modal view.
#[derive(Debug, Clone, Deserialize)]
pub struct ActionValue {
    /// Input element type (e.g. "plain_text_input").
    #[serde(rename = "type")]
    pub action_type: Option<String>,
    /// Text value entered by the user.
    pub value: Option<String>,
    /// Selected option (for select inputs).
    pub selected_option: Option<SelectedOption>,
}

/// Generic Slack Web API response.
#[derive(Debug, Deserialize)]
pub struct ApiResponse<T> {
    /// Whether the API call succeeded.
    pub ok: bool,
    /// Response data (flattened into the top-level object).
    #[serde(flatten)]
    pub data: Option<T>,
    /// Error code on failure.
    pub error: Option<String>,
    /// Seconds to wait before retrying (rate limit).
    #[serde(default)]
    pub retry_after: Option<u64>,
}

/// Reaction info from `reactions.get`.
#[derive(Debug, Clone, Deserialize)]
pub struct ReactionInfo {
    /// Emoji name (without colons).
    pub name: String,
    /// Number of users who reacted.
    pub count: u32,
    /// User IDs who reacted.
    #[serde(default)]
    pub users: Vec<String>,
}

/// User profile from `users.info`.
#[derive(Debug, Clone, Deserialize)]
pub struct UserInfo {
    /// User ID.
    pub id: String,
    /// Username (handle).
    pub name: String,
    /// Display name.
    pub real_name: Option<String>,
    /// Whether this user is a bot.
    #[serde(default)]
    pub is_bot: bool,
}

/// Channel info from `conversations.info` / `conversations.list`.
#[derive(Debug, Clone, Deserialize)]
pub struct SlackChannelInfo {
    /// Channel ID.
    pub id: String,
    /// Channel name.
    pub name: Option<String>,
    /// Whether the channel is private.
    #[serde(default)]
    pub is_private: bool,
    /// Channel topic.
    pub topic: Option<ChannelTopic>,
}

/// Channel topic metadata.
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelTopic {
    /// Topic text value.
    pub value: String,
}

/// A pinned item from `pins.list`.
#[derive(Debug, Clone, Deserialize)]
pub struct PinnedItem {
    /// The pinned message (if type is "message").
    pub message: Option<PinnedMessage>,
}

/// A pinned message.
#[derive(Debug, Clone, Deserialize)]
pub struct PinnedMessage {
    /// Message text content.
    pub text: Option<String>,
    /// User who sent the message.
    pub user: Option<String>,
    /// Message timestamp.
    pub ts: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_url_verification() {
        let json = r#"{
            "type": "url_verification",
            "challenge": "abc123",
            "token": "xyz"
        }"#;

        let envelope: SlackEnvelope = serde_json::from_str(json).unwrap();
        match envelope {
            SlackEnvelope::UrlVerification { challenge } => {
                assert_eq!(challenge, "abc123");
            }
            _ => panic!("expected UrlVerification"),
        }
    }

    #[test]
    fn deserialize_event_callback_message() {
        let json = r#"{
            "type": "event_callback",
            "token": "tok",
            "team_id": "T123",
            "event_id": "Ev123",
            "event": {
                "type": "message",
                "user": "U456",
                "text": "hello",
                "ts": "1234567890.123456",
                "channel": "C789",
                "channel_type": "channel"
            }
        }"#;

        let envelope: SlackEnvelope = serde_json::from_str(json).unwrap();
        match envelope {
            SlackEnvelope::EventCallback(cb) => {
                assert_eq!(cb.team_id.as_deref(), Some("T123"));
                assert_eq!(cb.event.event_type, "message");
                assert_eq!(cb.event.user.as_deref(), Some("U456"));
                assert_eq!(cb.event.text.as_deref(), Some("hello"));
                assert_eq!(cb.event.channel.as_deref(), Some("C789"));
                assert!(cb.event.bot_id.is_none());
            }
            _ => panic!("expected EventCallback"),
        }
    }

    #[test]
    fn deserialize_event_callback_app_mention() {
        let json = r#"{
            "type": "event_callback",
            "token": "tok",
            "team_id": "T123",
            "event_id": "Ev456",
            "event": {
                "type": "app_mention",
                "user": "U456",
                "text": "<@U789> help",
                "ts": "1234567890.654321",
                "channel": "C789",
                "channel_type": "channel"
            }
        }"#;

        let envelope: SlackEnvelope = serde_json::from_str(json).unwrap();
        match envelope {
            SlackEnvelope::EventCallback(cb) => {
                assert_eq!(cb.event.event_type, "app_mention");
                assert_eq!(cb.event.text.as_deref(), Some("<@U789> help"));
            }
            _ => panic!("expected EventCallback"),
        }
    }

    #[test]
    fn deserialize_bot_message() {
        let json = r#"{
            "type": "event_callback",
            "token": "tok",
            "team_id": "T123",
            "event_id": "Ev789",
            "event": {
                "type": "message",
                "text": "bot says hi",
                "ts": "1234567890.111111",
                "channel": "C789",
                "bot_id": "B123"
            }
        }"#;

        let envelope: SlackEnvelope = serde_json::from_str(json).unwrap();
        match envelope {
            SlackEnvelope::EventCallback(cb) => {
                assert!(cb.event.bot_id.is_some());
                assert!(cb.event.user.is_none());
            }
            _ => panic!("expected EventCallback"),
        }
    }

    #[test]
    fn deserialize_threaded_message() {
        let json = r#"{
            "type": "event_callback",
            "token": "tok",
            "team_id": "T123",
            "event_id": "Ev101",
            "event": {
                "type": "message",
                "user": "U456",
                "text": "thread reply",
                "ts": "1234567890.222222",
                "thread_ts": "1234567890.111111",
                "channel": "C789"
            }
        }"#;

        let envelope: SlackEnvelope = serde_json::from_str(json).unwrap();
        match envelope {
            SlackEnvelope::EventCallback(cb) => {
                assert_eq!(cb.event.thread_ts.as_deref(), Some("1234567890.111111"));
            }
            _ => panic!("expected EventCallback"),
        }
    }

    #[test]
    fn deserialize_auth_test_response() {
        let json = r#"{
            "ok": true,
            "user_id": "U123",
            "user": "bot",
            "team": "My Team",
            "team_id": "T123"
        }"#;

        let resp: AuthTestResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.user_id.as_deref(), Some("U123"));
        assert_eq!(resp.team.as_deref(), Some("My Team"));
    }

    #[test]
    fn deserialize_auth_test_error() {
        let json = r#"{
            "ok": false,
            "error": "invalid_auth"
        }"#;

        let resp: AuthTestResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error.as_deref(), Some("invalid_auth"));
    }

    #[test]
    fn serialize_post_message_request() {
        let req = PostMessageRequest {
            channel: "C789".into(),
            text: "hello".into(),
            thread_ts: Some("1234567890.111111".into()),
            blocks: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["channel"], "C789");
        assert_eq!(json["text"], "hello");
        assert_eq!(json["thread_ts"], "1234567890.111111");
    }

    #[test]
    fn serialize_post_message_no_thread() {
        let req = PostMessageRequest {
            channel: "C789".into(),
            text: "hello".into(),
            thread_ts: None,
            blocks: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("thread_ts").is_none());
    }

    #[test]
    fn deserialize_minimal_event_callback() {
        let json = r#"{
            "type": "event_callback",
            "event": {
                "type": "message"
            }
        }"#;

        let envelope: SlackEnvelope = serde_json::from_str(json).unwrap();
        match envelope {
            SlackEnvelope::EventCallback(cb) => {
                assert!(cb.token.is_none());
                assert!(cb.team_id.is_none());
                assert!(cb.event.user.is_none());
                assert!(cb.event.text.is_none());
            }
            _ => panic!("expected EventCallback"),
        }
    }

    #[test]
    fn serialize_post_message_with_blocks() {
        let req = PostMessageRequest {
            channel: "C789".into(),
            text: "fallback".into(),
            thread_ts: None,
            blocks: Some(vec![serde_json::json!({
                "type": "section",
                "text": { "type": "mrkdwn", "text": "*Hello*" }
            })]),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["text"], "fallback");
        let blocks = json["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "section");
    }

    #[test]
    fn serialize_post_message_without_blocks() {
        let req = PostMessageRequest {
            channel: "C789".into(),
            text: "hello".into(),
            thread_ts: None,
            blocks: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("blocks").is_none());
    }

    #[test]
    fn deserialize_event_with_files() {
        let json = r#"{
            "type": "event_callback",
            "token": "tok",
            "team_id": "T123",
            "event_id": "Ev123",
            "event": {
                "type": "message",
                "user": "U456",
                "text": "check this file",
                "ts": "1234567890.123456",
                "channel": "C789",
                "files": [
                    {
                        "id": "F123",
                        "name": "report.pdf",
                        "mimetype": "application/pdf",
                        "url_private": "https://files.slack.com/files-pri/T123-F123/report.pdf",
                        "size": 2048,
                        "filetype": "pdf"
                    }
                ]
            }
        }"#;

        let envelope: SlackEnvelope = serde_json::from_str(json).unwrap();
        match envelope {
            SlackEnvelope::EventCallback(cb) => {
                let files = cb.event.files.unwrap();
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].id, "F123");
                assert_eq!(files[0].name.as_deref(), Some("report.pdf"));
                assert_eq!(files[0].mimetype.as_deref(), Some("application/pdf"));
                assert_eq!(files[0].size, Some(2048));
            }
            _ => panic!("expected EventCallback"),
        }
    }

    #[test]
    fn deserialize_event_without_files() {
        let json = r#"{
            "type": "event_callback",
            "token": "tok",
            "team_id": "T123",
            "event_id": "Ev123",
            "event": {
                "type": "message",
                "user": "U456",
                "text": "hello",
                "ts": "1234567890.123456",
                "channel": "C789"
            }
        }"#;

        let envelope: SlackEnvelope = serde_json::from_str(json).unwrap();
        match envelope {
            SlackEnvelope::EventCallback(cb) => {
                assert!(cb.event.files.is_none());
            }
            _ => panic!("expected EventCallback"),
        }
    }

    #[test]
    fn deserialize_slash_command_payload() {
        let form = "command=%2Fborg&text=hello+world&user_id=U123&channel_id=C456";
        let payload: SlashCommandPayload = serde_urlencoded::from_str(form).unwrap();
        assert_eq!(payload.command, "/borg");
        assert_eq!(payload.text.as_deref(), Some("hello world"));
        assert_eq!(payload.user_id, "U123");
        assert_eq!(payload.channel_id, "C456");
        assert!(payload.user_name.is_none());
        assert!(payload.team_id.is_none());
        assert!(payload.response_url.is_none());
        assert!(payload.trigger_id.is_none());
    }

    #[test]
    fn deserialize_slash_command_full() {
        let form = "command=%2Fborg&text=deploy&user_id=U123&user_name=alice\
            &channel_id=C456&channel_name=general&team_id=T789\
            &response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2Fxyz\
            &trigger_id=tr1";
        let payload: SlashCommandPayload = serde_urlencoded::from_str(form).unwrap();
        assert_eq!(payload.command, "/borg");
        assert_eq!(payload.text.as_deref(), Some("deploy"));
        assert_eq!(payload.user_name.as_deref(), Some("alice"));
        assert_eq!(payload.channel_name.as_deref(), Some("general"));
        assert_eq!(payload.team_id.as_deref(), Some("T789"));
        assert!(payload.response_url.is_some());
        assert_eq!(payload.trigger_id.as_deref(), Some("tr1"));
    }

    // ── Interactive payload tests ──

    #[test]
    fn deserialize_block_actions_payload() {
        let json = r#"{
            "type": "block_actions",
            "user": { "id": "U123", "username": "alice" },
            "channel": { "id": "C456", "name": "general" },
            "trigger_id": "tr1",
            "response_url": "https://hooks.slack.com/actions/T123/456/abc",
            "actions": [
                {
                    "action_id": "approve_btn",
                    "type": "button",
                    "value": "approved",
                    "block_id": "block_1"
                }
            ]
        }"#;

        let payload: InteractionPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.interaction_type, "block_actions");
        assert_eq!(payload.user.id, "U123");
        assert_eq!(payload.user.username.as_deref(), Some("alice"));
        assert_eq!(payload.channel.as_ref().unwrap().id, "C456");
        assert!(payload.response_url.is_some());
        let actions = payload.actions.unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_id, "approve_btn");
        assert_eq!(actions[0].value.as_deref(), Some("approved"));
    }

    #[test]
    fn deserialize_select_menu_action() {
        let json = r#"{
            "type": "block_actions",
            "user": { "id": "U123" },
            "actions": [
                {
                    "action_id": "priority_select",
                    "type": "static_select",
                    "block_id": "block_2",
                    "selected_option": {
                        "value": "high",
                        "text": { "type": "plain_text", "text": "High" }
                    }
                }
            ]
        }"#;

        let payload: InteractionPayload = serde_json::from_str(json).unwrap();
        let actions = payload.actions.unwrap();
        assert!(actions[0].value.is_none());
        let opt = actions[0].selected_option.as_ref().unwrap();
        assert_eq!(opt.value, "high");
    }

    #[test]
    fn deserialize_view_submission() {
        let json = r#"{
            "type": "view_submission",
            "user": { "id": "U123" },
            "view": {
                "callback_id": "feedback_form",
                "state": {
                    "values": {
                        "block_1": {
                            "input_1": {
                                "type": "plain_text_input",
                                "value": "Great product!"
                            }
                        }
                    }
                }
            }
        }"#;

        let payload: InteractionPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.interaction_type, "view_submission");
        let view = payload.view.unwrap();
        assert_eq!(view.callback_id.as_deref(), Some("feedback_form"));
        let state = view.state.unwrap();
        let val = &state.values["block_1"]["input_1"];
        assert_eq!(val.value.as_deref(), Some("Great product!"));
    }

    #[test]
    fn deserialize_interaction_no_channel() {
        let json = r#"{
            "type": "view_submission",
            "user": { "id": "U123" },
            "view": { "callback_id": "test" }
        }"#;

        let payload: InteractionPayload = serde_json::from_str(json).unwrap();
        assert!(payload.channel.is_none());
        assert!(payload.actions.is_none());
    }

    // ── New feature-parity types ──

    #[test]
    fn deserialize_reaction_info() {
        let json = r#"{"name": "thumbsup", "count": 3, "users": ["U1", "U2", "U3"]}"#;
        let info: ReactionInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.name, "thumbsup");
        assert_eq!(info.count, 3);
        assert_eq!(info.users.len(), 3);
    }

    #[test]
    fn deserialize_reaction_info_no_users() {
        let json = r#"{"name": "wave", "count": 1}"#;
        let info: ReactionInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.name, "wave");
        assert!(info.users.is_empty());
    }

    #[test]
    fn deserialize_user_info() {
        let json =
            r#"{"id": "U123", "name": "alice", "real_name": "Alice Smith", "is_bot": false}"#;
        let info: UserInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.id, "U123");
        assert_eq!(info.name, "alice");
        assert_eq!(info.real_name.as_deref(), Some("Alice Smith"));
        assert!(!info.is_bot);
    }

    #[test]
    fn deserialize_user_info_bot() {
        let json = r#"{"id": "U456", "name": "botuser", "is_bot": true}"#;
        let info: UserInfo = serde_json::from_str(json).unwrap();
        assert!(info.is_bot);
        assert!(info.real_name.is_none());
    }

    #[test]
    fn deserialize_slack_channel_info() {
        let json = r#"{
            "id": "C123",
            "name": "general",
            "is_private": false,
            "topic": {"value": "All the general stuff"}
        }"#;
        let info: SlackChannelInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.id, "C123");
        assert_eq!(info.name.as_deref(), Some("general"));
        assert!(!info.is_private);
        assert_eq!(info.topic.as_ref().unwrap().value, "All the general stuff");
    }

    #[test]
    fn deserialize_slack_channel_info_private() {
        let json = r#"{"id": "G123", "name": "secret", "is_private": true}"#;
        let info: SlackChannelInfo = serde_json::from_str(json).unwrap();
        assert!(info.is_private);
        assert!(info.topic.is_none());
    }

    #[test]
    fn deserialize_channel_topic() {
        let json = r#"{"value": "Welcome to the channel"}"#;
        let topic: ChannelTopic = serde_json::from_str(json).unwrap();
        assert_eq!(topic.value, "Welcome to the channel");
    }

    #[test]
    fn deserialize_pinned_item() {
        let json = r#"{
            "message": {
                "text": "Important announcement",
                "user": "U123",
                "ts": "1234567890.123456"
            }
        }"#;
        let item: PinnedItem = serde_json::from_str(json).unwrap();
        let msg = item.message.unwrap();
        assert_eq!(msg.text.as_deref(), Some("Important announcement"));
        assert_eq!(msg.user.as_deref(), Some("U123"));
        assert_eq!(msg.ts.as_deref(), Some("1234567890.123456"));
    }

    #[test]
    fn deserialize_pinned_item_no_message() {
        let json = r#"{}"#;
        let item: PinnedItem = serde_json::from_str(json).unwrap();
        assert!(item.message.is_none());
    }
}
