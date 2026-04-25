use serde::{Deserialize, Serialize};

/// Telegram Bot API Update object.
#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    /// Unique identifier for the update.
    pub update_id: i64,
    /// New incoming message.
    pub message: Option<TelegramMessage>,
    /// Edited version of an existing message.
    pub edited_message: Option<TelegramMessage>,
    /// Incoming callback query from an inline keyboard button.
    pub callback_query: Option<CallbackQuery>,
}

/// A Telegram message.
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramMessage {
    /// Unique message identifier within the chat.
    pub message_id: i64,
    /// Sender of the message.
    pub from: Option<User>,
    /// Chat the message belongs to.
    pub chat: Chat,
    /// Unix timestamp when the message was sent.
    pub date: i64,
    /// Text content of the message.
    pub text: Option<String>,
    /// Unix timestamp of the last edit.
    pub edit_date: Option<i64>,
    /// Identifier for a group of related media messages.
    pub media_group_id: Option<String>,
    /// Forum topic thread identifier.
    pub message_thread_id: Option<i64>,
    /// Available sizes of an attached photo.
    pub photo: Option<Vec<PhotoSize>>,
    /// Caption for media messages.
    pub caption: Option<String>,
    /// Attached document file.
    pub document: Option<Document>,
    /// Attached video.
    pub video: Option<Video>,
    /// Attached audio file.
    pub audio: Option<Audio>,
    /// Attached voice message.
    pub voice: Option<Voice>,
    /// Attached sticker.
    pub sticker: Option<Sticker>,
    /// The original message this message is a reply to.
    pub reply_to_message: Option<Box<TelegramMessage>>,
    /// Unix timestamp of the original forwarded message.
    pub forward_date: Option<i64>,
    /// Forum topic creation event data.
    pub forum_topic_created: Option<serde_json::Value>,
}

/// A Telegram chat.
#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    /// Unique chat identifier.
    pub id: i64,
    /// Type of chat (e.g. "private", "group", "supergroup", "channel").
    #[serde(rename = "type")]
    pub chat_type: String,
}

/// A Telegram user.
#[derive(Debug, Clone, Deserialize)]
pub struct User {
    /// Unique user identifier.
    pub id: i64,
    /// User's first name.
    pub first_name: String,
    /// User's username (without leading @).
    pub username: Option<String>,
    /// Whether the user is a bot.
    #[serde(default)]
    pub is_bot: bool,
}

/// A Telegram photo size variant.
#[derive(Debug, Clone, Deserialize)]
pub struct PhotoSize {
    /// Identifier for downloading the file.
    pub file_id: String,
    /// Unique file identifier that stays the same across re-uploads.
    pub file_unique_id: String,
    /// Photo width in pixels.
    pub width: i32,
    /// Photo height in pixels.
    pub height: i32,
    /// File size in bytes.
    pub file_size: Option<i64>,
}

/// A Telegram document (file attachment).
#[derive(Debug, Clone, Deserialize)]
pub struct Document {
    /// Identifier for downloading the file.
    pub file_id: String,
    /// Unique file identifier that stays the same across re-uploads.
    pub file_unique_id: String,
    /// Original filename.
    pub file_name: Option<String>,
    /// MIME type of the document.
    pub mime_type: Option<String>,
    /// File size in bytes.
    pub file_size: Option<i64>,
}

/// A Telegram video.
#[derive(Debug, Clone, Deserialize)]
pub struct Video {
    /// Identifier for downloading the file.
    pub file_id: String,
    /// Unique file identifier that stays the same across re-uploads.
    pub file_unique_id: String,
    /// Video width in pixels.
    pub width: i32,
    /// Video height in pixels.
    pub height: i32,
    /// Duration in seconds.
    pub duration: i32,
    /// File size in bytes.
    pub file_size: Option<i64>,
}

/// A Telegram audio file.
#[derive(Debug, Clone, Deserialize)]
pub struct Audio {
    /// Identifier for downloading the file.
    pub file_id: String,
    /// Unique file identifier that stays the same across re-uploads.
    pub file_unique_id: String,
    /// Duration in seconds.
    pub duration: i32,
    /// Title of the audio track.
    pub title: Option<String>,
    /// File size in bytes.
    pub file_size: Option<i64>,
}

/// A Telegram voice message.
#[derive(Debug, Clone, Deserialize)]
pub struct Voice {
    /// Identifier for downloading the file.
    pub file_id: String,
    /// Unique file identifier that stays the same across re-uploads.
    pub file_unique_id: String,
    /// Duration in seconds.
    pub duration: i32,
    /// File size in bytes.
    pub file_size: Option<i64>,
}

/// A Telegram sticker.
#[derive(Debug, Clone, Deserialize)]
pub struct Sticker {
    /// Identifier for downloading the file.
    pub file_id: String,
    /// Unique file identifier that stays the same across re-uploads.
    pub file_unique_id: String,
    /// Sticker width in pixels.
    pub width: i32,
    /// Sticker height in pixels.
    pub height: i32,
    /// Emoji associated with the sticker.
    pub emoji: Option<String>,
    /// File size in bytes.
    pub file_size: Option<i64>,
}

/// Telegram file info returned by getFile.
#[derive(Debug, Clone, Deserialize)]
pub struct FileInfo {
    /// Identifier for downloading the file.
    pub file_id: String,
    /// Unique file identifier that stays the same across re-uploads.
    pub file_unique_id: String,
    /// File size in bytes.
    pub file_size: Option<i64>,
    /// File path for downloading via the Bot API.
    pub file_path: Option<String>,
}

/// A Telegram callback query (inline button press).
#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    /// Unique callback query identifier.
    pub id: String,
    /// User who pressed the button.
    pub from: User,
    /// Message containing the inline keyboard that triggered the callback.
    pub message: Option<TelegramMessage>,
    /// Data associated with the callback button.
    pub data: Option<String>,
}

/// Generic Telegram Bot API response wrapper.
#[derive(Debug, Deserialize)]
pub struct ApiResponse<T> {
    /// Whether the request was successful.
    pub ok: bool,
    /// The response payload on success.
    pub result: Option<T>,
    /// Human-readable error description.
    pub description: Option<String>,
    /// HTTP-like error code on failure.
    pub error_code: Option<u16>,
    /// Seconds to wait before retrying (rate limit).
    #[serde(default)]
    pub retry_after: Option<u64>,
}

/// Request body for sendMessage.
#[derive(Debug, Serialize)]
pub struct SendMessageRequest {
    /// Target chat identifier.
    pub chat_id: i64,
    /// Text of the message to send.
    pub text: String,
    /// Parse mode for formatting (e.g. "HTML", "MarkdownV2").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<String>,
    /// ID of the message to reply to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<i64>,
    /// Forum topic thread ID to send into.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_thread_id: Option<i64>,
    /// Inline keyboard markup attached to the message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_markup: Option<InlineKeyboardMarkup>,
    /// Whether to send the message silently.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable_notification: Option<bool>,
}

/// Telegram inline keyboard markup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineKeyboardMarkup {
    /// Rows of inline keyboard buttons.
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

/// A single inline keyboard button.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineKeyboardButton {
    /// Label text on the button.
    pub text: String,
    /// Data sent in a callback query when pressed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_data: Option<String>,
    /// URL to open when pressed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Request body for sendPoll.
#[derive(Debug, Serialize)]
pub struct SendPollRequest {
    /// Target chat identifier.
    pub chat_id: i64,
    /// Poll question text.
    pub question: String,
    /// List of answer options.
    pub options: Vec<String>,
    /// Whether the poll is anonymous.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_anonymous: Option<bool>,
    /// Forum topic thread ID to send into.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_thread_id: Option<i64>,
}

/// Request body for setMessageReaction.
#[derive(Debug, Serialize)]
pub struct SetMessageReactionRequest {
    /// Target chat identifier.
    pub chat_id: i64,
    /// Message to react to.
    pub message_id: i64,
    /// List of reaction types to set.
    pub reaction: Vec<ReactionType>,
}

/// A reaction type for setMessageReaction.
#[derive(Debug, Clone, Serialize)]
pub struct ReactionType {
    /// Reaction kind (e.g. "emoji").
    #[serde(rename = "type")]
    pub reaction_type: String,
    /// Emoji character for the reaction.
    pub emoji: String,
}

impl ReactionType {
    /// Create an emoji reaction type.
    pub fn emoji(emoji: impl Into<String>) -> Self {
        Self {
            reaction_type: "emoji".to_string(),
            emoji: emoji.into(),
        }
    }
}

/// Request body for editMessageText.
#[derive(Debug, Serialize)]
pub struct EditMessageTextRequest {
    /// Target chat identifier.
    pub chat_id: i64,
    /// Identifier of the message to edit.
    pub message_id: i64,
    /// New text of the message.
    pub text: String,
    /// Parse mode for formatting (e.g. "HTML", "MarkdownV2").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<String>,
    /// Optional inline keyboard markup to attach to the edited message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_markup: Option<InlineKeyboardMarkup>,
}

/// Request body for deleteMessage.
#[derive(Debug, Serialize)]
pub struct DeleteMessageRequest {
    /// Target chat identifier.
    pub chat_id: i64,
    /// Identifier of the message to delete.
    pub message_id: i64,
}

/// Source of an outbound media file: existing Telegram file_id, public URL, or raw bytes upload.
#[derive(Debug, Clone)]
pub enum MediaSource<'a> {
    /// Reuse a previously uploaded file by its Telegram file_id.
    FileId(&'a str),
    /// Tell Telegram to fetch the media from this URL.
    Url(&'a str),
    /// Upload bytes via multipart/form-data with the given filename.
    Bytes {
        /// Raw file bytes.
        bytes: &'a [u8],
        /// Filename Telegram should display.
        filename: &'a str,
        /// MIME type. If `None`, reqwest's default is used.
        mime: Option<&'a str>,
    },
}

/// Webhook info returned by getWebhookInfo.
#[derive(Debug, Deserialize)]
pub struct WebhookInfo {
    /// Currently registered webhook URL.
    pub url: String,
    /// Whether a custom HTTPS certificate was provided.
    #[serde(default)]
    pub has_custom_certificate: bool,
    /// Number of pending updates awaiting delivery.
    #[serde(default)]
    pub pending_update_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_text_message_update() {
        let json = r#"{
            "update_id": 123456,
            "message": {
                "message_id": 1,
                "from": {
                    "id": 42,
                    "first_name": "Alice",
                    "username": "alice",
                    "is_bot": false
                },
                "chat": { "id": 42, "type": "private" },
                "date": 1700000000,
                "text": "Hello bot"
            }
        }"#;

        let update: Update = serde_json::from_str(json).unwrap();
        assert_eq!(update.update_id, 123456);
        let msg = update.message.unwrap();
        assert_eq!(msg.text.as_deref(), Some("Hello bot"));
        assert_eq!(msg.chat.id, 42);
        assert_eq!(msg.chat.chat_type, "private");
        let from = msg.from.unwrap();
        assert_eq!(from.id, 42);
        assert_eq!(from.username.as_deref(), Some("alice"));
        assert!(!from.is_bot);
    }

    #[test]
    fn deserialize_edited_message_update() {
        let json = r#"{
            "update_id": 123457,
            "edited_message": {
                "message_id": 1,
                "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                "chat": { "id": 42, "type": "private" },
                "date": 1700000000,
                "edit_date": 1700000060,
                "text": "Hello bot (edited)"
            }
        }"#;

        let update: Update = serde_json::from_str(json).unwrap();
        assert!(update.message.is_none());
        let edited = update.edited_message.unwrap();
        assert_eq!(edited.text.as_deref(), Some("Hello bot (edited)"));
        assert_eq!(edited.edit_date, Some(1700000060));
    }

    #[test]
    fn deserialize_callback_query_update() {
        let json = r#"{
            "update_id": 123458,
            "callback_query": {
                "id": "cb_1",
                "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                "message": {
                    "message_id": 5,
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000
                },
                "data": "button_click"
            }
        }"#;

        let update: Update = serde_json::from_str(json).unwrap();
        let cb = update.callback_query.unwrap();
        assert_eq!(cb.id, "cb_1");
        assert_eq!(cb.data.as_deref(), Some("button_click"));
        assert_eq!(cb.from.id, 42);
    }

    #[test]
    fn deserialize_api_response_success() {
        let json = r#"{
            "ok": true,
            "result": { "id": 123, "first_name": "TestBot", "is_bot": true }
        }"#;

        let resp: ApiResponse<User> = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        let user = resp.result.unwrap();
        assert_eq!(user.id, 123);
        assert!(user.is_bot);
    }

    #[test]
    fn deserialize_api_response_error() {
        let json = r#"{
            "ok": false,
            "error_code": 429,
            "description": "Too Many Requests",
            "retry_after": 30
        }"#;

        let resp: ApiResponse<()> = serde_json::from_str(json).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error_code, Some(429));
        assert_eq!(resp.retry_after, Some(30));
    }

    #[test]
    fn deserialize_webhook_info() {
        let json = r#"{
            "url": "https://example.com/webhook/telegram",
            "has_custom_certificate": false,
            "pending_update_count": 3
        }"#;

        let info: WebhookInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.url, "https://example.com/webhook/telegram");
        assert_eq!(info.pending_update_count, 3);
    }

    #[test]
    fn deserialize_minimal_update() {
        let json = r#"{ "update_id": 999 }"#;
        let update: Update = serde_json::from_str(json).unwrap();
        assert_eq!(update.update_id, 999);
        assert!(update.message.is_none());
        assert!(update.edited_message.is_none());
        assert!(update.callback_query.is_none());
    }

    #[test]
    fn deserialize_photo_message() {
        let json = r#"{
            "update_id": 100,
            "message": {
                "message_id": 1,
                "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                "chat": { "id": 42, "type": "private" },
                "date": 1700000000,
                "photo": [
                    { "file_id": "small", "file_unique_id": "u1", "width": 90, "height": 90, "file_size": 1024 },
                    { "file_id": "large", "file_unique_id": "u2", "width": 800, "height": 600, "file_size": 50000 }
                ],
                "caption": "My photo"
            }
        }"#;

        let update: Update = serde_json::from_str(json).unwrap();
        let msg = update.message.unwrap();
        let photos = msg.photo.unwrap();
        assert_eq!(photos.len(), 2);
        assert_eq!(photos[0].file_id, "small");
        assert_eq!(photos[1].width, 800);
        assert_eq!(msg.caption.as_deref(), Some("My photo"));
    }

    #[test]
    fn deserialize_document_message() {
        let json = r#"{
            "update_id": 101,
            "message": {
                "message_id": 2,
                "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                "chat": { "id": 42, "type": "private" },
                "date": 1700000000,
                "document": {
                    "file_id": "doc1",
                    "file_unique_id": "ud1",
                    "file_name": "report.pdf",
                    "mime_type": "application/pdf",
                    "file_size": 123456
                }
            }
        }"#;

        let update: Update = serde_json::from_str(json).unwrap();
        let doc = update.message.unwrap().document.unwrap();
        assert_eq!(doc.file_name.as_deref(), Some("report.pdf"));
        assert_eq!(doc.mime_type.as_deref(), Some("application/pdf"));
    }

    #[test]
    fn deserialize_forum_message() {
        let json = r#"{
            "update_id": 102,
            "message": {
                "message_id": 50,
                "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                "chat": { "id": -100123, "type": "supergroup" },
                "date": 1700000000,
                "message_thread_id": 99,
                "text": "Forum topic message"
            }
        }"#;

        let update: Update = serde_json::from_str(json).unwrap();
        let msg = update.message.unwrap();
        assert_eq!(msg.message_thread_id, Some(99));
        assert_eq!(msg.text.as_deref(), Some("Forum topic message"));
    }

    #[test]
    fn deserialize_voice_message() {
        let json = r#"{
            "update_id": 103,
            "message": {
                "message_id": 3,
                "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                "chat": { "id": 42, "type": "private" },
                "date": 1700000000,
                "voice": { "file_id": "v1", "file_unique_id": "uv1", "duration": 5, "file_size": 8000 }
            }
        }"#;

        let update: Update = serde_json::from_str(json).unwrap();
        let voice = update.message.unwrap().voice.unwrap();
        assert_eq!(voice.duration, 5);
    }

    #[test]
    fn deserialize_sticker_message() {
        let json = r#"{
            "update_id": 104,
            "message": {
                "message_id": 4,
                "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                "chat": { "id": 42, "type": "private" },
                "date": 1700000000,
                "sticker": { "file_id": "s1", "file_unique_id": "us1", "width": 512, "height": 512, "emoji": "\ud83d\ude00" }
            }
        }"#;

        let update: Update = serde_json::from_str(json).unwrap();
        let sticker = update.message.unwrap().sticker.unwrap();
        assert_eq!(sticker.width, 512);
        assert!(sticker.emoji.is_some());
    }

    #[test]
    fn deserialize_reply_to_message() {
        let json = r#"{
            "update_id": 105,
            "message": {
                "message_id": 10,
                "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                "chat": { "id": 42, "type": "private" },
                "date": 1700000000,
                "text": "Reply text",
                "reply_to_message": {
                    "message_id": 5,
                    "chat": { "id": 42, "type": "private" },
                    "date": 1699999000,
                    "text": "Original"
                }
            }
        }"#;

        let update: Update = serde_json::from_str(json).unwrap();
        let msg = update.message.unwrap();
        let reply = msg.reply_to_message.unwrap();
        assert_eq!(reply.message_id, 5);
        assert_eq!(reply.text.as_deref(), Some("Original"));
    }

    #[test]
    fn deserialize_forwarded_message() {
        let json = r#"{
            "update_id": 106,
            "message": {
                "message_id": 11,
                "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                "chat": { "id": 42, "type": "private" },
                "date": 1700000000,
                "forward_date": 1699000000,
                "text": "Forwarded text"
            }
        }"#;

        let update: Update = serde_json::from_str(json).unwrap();
        let msg = update.message.unwrap();
        assert_eq!(msg.forward_date, Some(1699000000));
    }

    #[test]
    fn deserialize_send_message_with_thread() {
        let req = SendMessageRequest {
            chat_id: 42,
            text: "hello".into(),
            parse_mode: Some("HTML".into()),
            reply_to_message_id: Some(10),
            message_thread_id: Some(99),
            reply_markup: None,
            disable_notification: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["message_thread_id"], 99);
        assert_eq!(json["reply_to_message_id"], 10);
        assert!(json.get("reply_markup").is_none());
        assert!(json.get("disable_notification").is_none());
    }

    #[test]
    fn deserialize_file_info() {
        let json = r#"{
            "file_id": "abc123",
            "file_unique_id": "unique1",
            "file_size": 12345,
            "file_path": "photos/file_1.jpg"
        }"#;

        let info: FileInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.file_id, "abc123");
        assert_eq!(info.file_path.as_deref(), Some("photos/file_1.jpg"));
    }

    #[test]
    fn serialize_inline_keyboard_markup() {
        let markup = InlineKeyboardMarkup {
            inline_keyboard: vec![vec![
                InlineKeyboardButton {
                    text: "Yes".into(),
                    callback_data: Some("yes".into()),
                    url: None,
                },
                InlineKeyboardButton {
                    text: "No".into(),
                    callback_data: Some("no".into()),
                    url: None,
                },
            ]],
        };
        let json = serde_json::to_value(&markup).unwrap();
        let rows = json["inline_keyboard"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        let btns = rows[0].as_array().unwrap();
        assert_eq!(btns.len(), 2);
        assert_eq!(btns[0]["text"], "Yes");
        assert_eq!(btns[0]["callback_data"], "yes");
        assert!(btns[0].get("url").is_none());
    }

    #[test]
    fn serialize_inline_keyboard_url_button() {
        let btn = InlineKeyboardButton {
            text: "Open".into(),
            callback_data: None,
            url: Some("https://example.com".into()),
        };
        let json = serde_json::to_value(&btn).unwrap();
        assert_eq!(json["text"], "Open");
        assert_eq!(json["url"], "https://example.com");
        assert!(json.get("callback_data").is_none());
    }

    #[test]
    fn serialize_send_message_with_keyboard() {
        let req = SendMessageRequest {
            chat_id: 42,
            text: "Choose:".into(),
            parse_mode: None,
            reply_to_message_id: None,
            message_thread_id: None,
            reply_markup: Some(InlineKeyboardMarkup {
                inline_keyboard: vec![vec![InlineKeyboardButton {
                    text: "OK".into(),
                    callback_data: Some("ok".into()),
                    url: None,
                }]],
            }),
            disable_notification: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("reply_markup").is_some());
        assert_eq!(json["reply_markup"]["inline_keyboard"][0][0]["text"], "OK");
    }

    #[test]
    fn serialize_send_message_silent() {
        let req = SendMessageRequest {
            chat_id: 42,
            text: "silent".into(),
            parse_mode: None,
            reply_to_message_id: None,
            message_thread_id: None,
            reply_markup: None,
            disable_notification: Some(true),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["disable_notification"], true);
    }

    #[test]
    fn serialize_send_poll_request() {
        let req = SendPollRequest {
            chat_id: 42,
            question: "Lunch?".into(),
            options: vec!["Pizza".into(), "Sushi".into(), "Tacos".into()],
            is_anonymous: Some(true),
            message_thread_id: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["chat_id"], 42);
        assert_eq!(json["question"], "Lunch?");
        assert_eq!(json["options"].as_array().unwrap().len(), 3);
        assert_eq!(json["is_anonymous"], true);
    }

    #[test]
    fn serialize_set_message_reaction_request() {
        let req = SetMessageReactionRequest {
            chat_id: 42,
            message_id: 100,
            reaction: vec![ReactionType::emoji("👍")],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["chat_id"], 42);
        assert_eq!(json["message_id"], 100);
        let reactions = json["reaction"].as_array().unwrap();
        assert_eq!(reactions[0]["type"], "emoji");
        assert_eq!(reactions[0]["emoji"], "👍");
    }

    #[test]
    fn serialize_edit_message_text_request() {
        let req = EditMessageTextRequest {
            chat_id: 42,
            message_id: 7,
            text: "updated".into(),
            parse_mode: Some("HTML".into()),
            reply_markup: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["chat_id"], 42);
        assert_eq!(json["message_id"], 7);
        assert_eq!(json["text"], "updated");
        assert_eq!(json["parse_mode"], "HTML");
        assert!(json.get("reply_markup").is_none());
    }

    #[test]
    fn serialize_edit_message_text_omits_optional_fields() {
        let req = EditMessageTextRequest {
            chat_id: 42,
            message_id: 7,
            text: "x".into(),
            parse_mode: None,
            reply_markup: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("parse_mode").is_none());
        assert!(json.get("reply_markup").is_none());
    }

    #[test]
    fn serialize_delete_message_request_minimal_shape() {
        let req = DeleteMessageRequest {
            chat_id: 42,
            message_id: 100,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["chat_id"], 42);
        assert_eq!(json["message_id"], 100);
        let obj = json.as_object().expect("body is an object");
        assert_eq!(
            obj.len(),
            2,
            "deleteMessage body must contain exactly chat_id and message_id"
        );
    }

    #[test]
    fn deserialize_inline_keyboard_markup() {
        let json = r#"{
            "inline_keyboard": [[
                {"text": "A", "callback_data": "a"},
                {"text": "B", "url": "https://b.com"}
            ]]
        }"#;
        let markup: InlineKeyboardMarkup = serde_json::from_str(json).unwrap();
        assert_eq!(markup.inline_keyboard.len(), 1);
        assert_eq!(markup.inline_keyboard[0].len(), 2);
        assert_eq!(
            markup.inline_keyboard[0][0].callback_data.as_deref(),
            Some("a")
        );
        assert_eq!(
            markup.inline_keyboard[0][1].url.as_deref(),
            Some("https://b.com")
        );
    }
}
