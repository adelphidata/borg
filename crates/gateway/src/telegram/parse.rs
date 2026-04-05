use super::types::Update;
use crate::constants::{PEER_KIND_DIRECT, PEER_KIND_GROUP};
use crate::handler::InboundMessage;

/// Reference to an audio file in a Telegram message (for transcription).
#[derive(Debug, Clone)]
pub struct TelegramAudioRef {
    pub file_id: String,
    pub mime_type: String,
    pub duration: i32,
}

/// Reference to a photo in a Telegram message (for downstream vision download).
#[derive(Debug, Clone)]
pub struct TelegramPhotoRef {
    /// File ID of the largest photo variant to download.
    pub file_id: String,
    /// Filename hint for the downloaded image.
    pub filename: String,
}

/// Parsed Telegram inbound with optional media references to download post-parse.
pub struct TelegramParsed {
    pub inbound: InboundMessage,
    pub audio: Option<TelegramAudioRef>,
    pub photo: Option<TelegramPhotoRef>,
}

/// Extract an `InboundMessage` and optional media references from a Telegram update.
///
/// Handles text messages, media messages (photo, document, video, audio, voice, sticker),
/// edited messages, callback queries, and forum topic routing.
/// Returns `None` for service messages (forum_topic_created, etc.) and empty updates.
///
/// When voice/audio is present, the returned `TelegramParsed.audio` drives
/// downstream transcription. When a photo is present, `TelegramParsed.photo`
/// drives downstream download + base64 attachment so vision-capable models
/// can actually see the image instead of a `"[Photo]"` placeholder.
pub fn parse_update(update: &Update) -> Option<TelegramParsed> {
    // Try regular message first, then edited message
    if let Some(msg) = update.message.as_ref().or(update.edited_message.as_ref()) {
        // Skip service messages
        if msg.forum_topic_created.is_some() {
            return None;
        }

        // Try text first, then caption, then generate placeholder for media
        let mut audio_ref = None;
        let mut photo_ref = None;

        // Extract the largest photo variant (last entry) so vision-capable
        // models can see the image. Applies for both bare photos and photos
        // with captions.
        if let Some(ref photos) = msg.photo {
            if let Some(largest) = photos.last() {
                photo_ref = Some(TelegramPhotoRef {
                    file_id: largest.file_id.clone(),
                    filename: format!("photo_{}.jpg", largest.file_unique_id),
                });
            }
        }

        let text = if let Some(ref t) = msg.text {
            if t.is_empty() {
                return None;
            }
            t.clone()
        } else if let Some(ref caption) = msg.caption {
            caption.clone()
        } else if msg.photo.is_some() {
            "[Photo]".to_string()
        } else if let Some(ref doc) = msg.document {
            match &doc.file_name {
                Some(name) => format!("[Document: {name}]"),
                None => "[Document]".to_string(),
            }
        } else if msg.video.is_some() {
            "[Video]".to_string()
        } else if let Some(ref audio) = msg.audio {
            audio_ref = Some(TelegramAudioRef {
                file_id: audio.file_id.clone(),
                mime_type: "audio/mpeg".to_string(),
                duration: audio.duration,
            });
            "[Audio]".to_string()
        } else if let Some(ref voice) = msg.voice {
            audio_ref = Some(TelegramAudioRef {
                file_id: voice.file_id.clone(),
                mime_type: "audio/ogg".to_string(),
                duration: voice.duration,
            });
            "[Voice message]".to_string()
        } else if let Some(ref sticker) = msg.sticker {
            match &sticker.emoji {
                Some(emoji) => format!("[Sticker: {emoji}]"),
                None => "[Sticker]".to_string(),
            }
        } else {
            return None;
        };

        let sender_id = msg
            .from
            .as_ref()
            .map(|u| u.id.to_string())
            .unwrap_or_else(|| msg.chat.id.to_string());

        let thread_id = msg.message_thread_id.map(|t| t.to_string());
        let message_id = Some(msg.message_id.to_string());

        let peer_kind = match msg.chat.chat_type.as_str() {
            "private" => Some(PEER_KIND_DIRECT.to_string()),
            "group" | "supergroup" | "channel" => Some(PEER_KIND_GROUP.to_string()),
            _ => None,
        };

        return Some(TelegramParsed {
            inbound: InboundMessage {
                sender_id,
                text,
                channel_id: Some(msg.chat.id.to_string()),
                thread_id,
                message_id,
                thread_ts: None,
                attachments: Vec::new(),
                reaction: None,
                metadata: serde_json::Value::Null,
                peer_kind,
            },
            audio: audio_ref,
            photo: photo_ref,
        });
    }

    // Try callback query
    if let Some(cb) = &update.callback_query {
        let data = cb.data.as_deref()?;
        if data.is_empty() {
            return None;
        }

        let chat_id = cb.message.as_ref().map(|m| m.chat.id.to_string());
        let thread_id = cb
            .message
            .as_ref()
            .and_then(|m| m.message_thread_id)
            .map(|t| t.to_string());
        let message_id = cb.message.as_ref().map(|m| m.message_id.to_string());

        let peer_kind = cb
            .message
            .as_ref()
            .map(|m| match m.chat.chat_type.as_str() {
                "private" => PEER_KIND_DIRECT.to_string(),
                _ => PEER_KIND_GROUP.to_string(),
            });

        return Some(TelegramParsed {
            inbound: InboundMessage {
                sender_id: cb.from.id.to_string(),
                text: data.to_string(),
                channel_id: chat_id,
                thread_id,
                message_id,
                thread_ts: None,
                attachments: Vec::new(),
                reaction: None,
                metadata: serde_json::Value::Null,
                peer_kind,
            },
            audio: None,
            photo: None,
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_update(text: &str) -> Update {
        serde_json::from_str(&format!(
            r#"{{
                "update_id": 1,
                "message": {{
                    "message_id": 1,
                    "from": {{ "id": 42, "first_name": "Alice", "is_bot": false }},
                    "chat": {{ "id": 42, "type": "private" }},
                    "date": 1700000000,
                    "text": {text_json}
                }}
            }}"#,
            text_json = serde_json::to_string(text).unwrap()
        ))
        .unwrap()
    }

    #[test]
    fn parse_text_message() {
        let update = text_update("Hello bot");
        let p = parse_update(&update).unwrap(); let (msg, audio) = (p.inbound, p.audio);
        assert_eq!(msg.sender_id, "42");
        assert_eq!(msg.text, "Hello bot");
        assert_eq!(msg.channel_id.as_deref(), Some("42"));
        assert_eq!(msg.message_id.as_deref(), Some("1"));
        assert!(audio.is_none());
    }

    #[test]
    fn parse_edited_message() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 2,
                "edited_message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "edit_date": 1700000060,
                    "text": "edited text"
                }
            }"#,
        )
        .unwrap();

        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(msg.text, "edited text");
    }

    #[test]
    fn parse_callback_query() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 3,
                "callback_query": {
                    "id": "cb1",
                    "from": { "id": 99, "first_name": "Bob", "is_bot": false },
                    "message": {
                        "message_id": 5,
                        "chat": { "id": 42, "type": "private" },
                        "date": 1700000000
                    },
                    "data": "btn_click"
                }
            }"#,
        )
        .unwrap();

        let p = parse_update(&update).unwrap(); let (msg, audio) = (p.inbound, p.audio);
        assert_eq!(msg.sender_id, "99");
        assert_eq!(msg.text, "btn_click");
        assert_eq!(msg.channel_id.as_deref(), Some("42"));
        assert!(audio.is_none());
    }

    #[test]
    fn photo_with_caption() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 4,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "photo": [{ "file_id": "abc", "file_unique_id": "u1", "width": 100, "height": 100 }],
                    "caption": "Look at this!"
                }
            }"#,
        )
        .unwrap();

        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(msg.text, "Look at this!");
    }

    #[test]
    fn photo_without_caption_generates_placeholder() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 5,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "photo": [{ "file_id": "abc", "file_unique_id": "u1", "width": 100, "height": 100 }]
                }
            }"#,
        )
        .unwrap();

        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(msg.text, "[Photo]");
    }

    #[test]
    fn document_with_filename() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 6,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "document": { "file_id": "abc", "file_unique_id": "u1", "file_name": "report.pdf" }
                }
            }"#,
        )
        .unwrap();

        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(msg.text, "[Document: report.pdf]");
    }

    #[test]
    fn voice_message_returns_audio_ref() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 7,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "voice": { "file_id": "abc", "file_unique_id": "u1", "duration": 5 }
                }
            }"#,
        )
        .unwrap();

        let p = parse_update(&update).unwrap(); let (msg, audio) = (p.inbound, p.audio);
        assert_eq!(msg.text, "[Voice message]");
        let audio = audio.unwrap();
        assert_eq!(audio.file_id, "abc");
        assert_eq!(audio.mime_type, "audio/ogg");
        assert_eq!(audio.duration, 5);
    }

    #[test]
    fn sticker_with_emoji() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 8,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "sticker": { "file_id": "abc", "file_unique_id": "u1", "width": 512, "height": 512, "emoji": "\ud83d\ude00" }
                }
            }"#,
        )
        .unwrap();

        let msg = parse_update(&update).unwrap().inbound;
        assert!(msg.text.starts_with("[Sticker:"));
    }

    #[test]
    fn audio_message_returns_audio_ref() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 12,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "audio": { "file_id": "audio123", "file_unique_id": "u1", "duration": 180 }
                }
            }"#,
        )
        .unwrap();

        let p = parse_update(&update).unwrap(); let (msg, audio) = (p.inbound, p.audio);
        assert_eq!(msg.text, "[Audio]");
        let audio = audio.unwrap();
        assert_eq!(audio.file_id, "audio123");
        assert_eq!(audio.mime_type, "audio/mpeg");
        assert_eq!(audio.duration, 180);
    }

    #[test]
    fn forum_topic_created_skipped() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 9,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": -100123, "type": "supergroup" },
                    "date": 1700000000,
                    "forum_topic_created": { "name": "General", "icon_color": 0 }
                }
            }"#,
        )
        .unwrap();

        assert!(parse_update(&update).is_none());
    }

    #[test]
    fn message_thread_id_extracted() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 10,
                "message": {
                    "message_id": 50,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": -100123, "type": "supergroup" },
                    "date": 1700000000,
                    "message_thread_id": 99,
                    "text": "forum message"
                }
            }"#,
        )
        .unwrap();

        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(msg.thread_id.as_deref(), Some("99"));
        assert_eq!(msg.text, "forum message");
    }

    #[test]
    fn empty_text_returns_none() {
        let update = text_update("");
        assert!(parse_update(&update).is_none());
    }

    #[test]
    fn minimal_update_no_message_returns_none() {
        let update: Update = serde_json::from_str(r#"{ "update_id": 5 }"#).unwrap();
        assert!(parse_update(&update).is_none());
    }

    #[test]
    fn callback_without_data_returns_none() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 6,
                "callback_query": {
                    "id": "cb1",
                    "from": { "id": 99, "first_name": "Bob", "is_bot": false }
                }
            }"#,
        )
        .unwrap();

        assert!(parse_update(&update).is_none());
    }

    #[test]
    fn callback_with_data_but_no_message() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 8,
                "callback_query": {
                    "id": "cb2",
                    "from": { "id": 99, "first_name": "Bob", "is_bot": false },
                    "data": "inline_click"
                }
            }"#,
        )
        .unwrap();

        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(msg.sender_id, "99");
        assert_eq!(msg.text, "inline_click");
        assert!(msg.channel_id.is_none());
    }

    #[test]
    fn group_message_uses_sender_not_chat() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 7,
                "message": {
                    "message_id": 10,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": -100123, "type": "supergroup" },
                    "date": 1700000000,
                    "text": "group msg"
                }
            }"#,
        )
        .unwrap();

        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(msg.sender_id, "42");
        assert_eq!(msg.channel_id.as_deref(), Some("-100123"));
    }

    #[test]
    fn private_chat_is_direct_peer_kind() {
        let update = text_update("hello");
        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(msg.peer_kind.as_deref(), Some("direct"));
    }

    #[test]
    fn group_chat_is_group_peer_kind() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 20,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": -100123, "type": "supergroup" },
                    "date": 1700000000,
                    "text": "hello group"
                }
            }"#,
        )
        .unwrap();

        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(msg.peer_kind.as_deref(), Some("group"));
    }

    #[test]
    fn video_placeholder() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 11,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "video": { "file_id": "abc", "file_unique_id": "u1", "width": 1920, "height": 1080, "duration": 30 }
                }
            }"#,
        )
        .unwrap();

        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(msg.text, "[Video]");
    }
}
