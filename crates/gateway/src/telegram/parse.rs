use super::types::{CallbackQuery, MessageEntity, TelegramMessage, Update};
use crate::constants::{PEER_KIND_DIRECT, PEER_KIND_GROUP};
use crate::handler::InboundMessage;

/// The Telegram input type relevant to voice reply policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelegramInputKind {
    Text,
    Audio,
    Voice,
    Other,
}

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
    pub input_kind: TelegramInputKind,
}

/// Detect non-printable control characters that indicate raw binary payload
/// leaked into a text field. Binary documents (.mobi, .epub, PDFs) can spill
/// into `message.caption` or `document.file_name`; left unsanitized they cause
/// catastrophic token inflation (~460K tokens) when forwarded to the LLM.
///
/// Returns true when the string contains any byte in `0x00..=0x08` or
/// `0x0E..=0x1F`. Tab (`0x09`), newline (`0x0A`), and carriage return (`0x0D`)
/// are allowed so legitimate multiline captions pass through.
fn is_binary_content(s: &str) -> bool {
    s.bytes().any(|b| matches!(b, 0x00..=0x08 | 0x0E..=0x1F))
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
    if let Some(msg) = update.message.as_ref().or(update.edited_message.as_ref()) {
        return parse_message(msg);
    }
    if let Some(cb) = update.callback_query.as_ref() {
        return parse_callback(cb);
    }
    None
}

fn parse_message(msg: &TelegramMessage) -> Option<TelegramParsed> {
    // Skip service messages
    if msg.forum_topic_created.is_some() {
        return None;
    }

    // Extract the largest photo variant (last entry) so vision-capable
    // models can see the image. Applies for both bare photos and photos
    // with captions.
    let photo_ref = msg
        .photo
        .as_ref()
        .and_then(|photos| photos.last())
        .map(|largest| TelegramPhotoRef {
            file_id: largest.file_id.clone(),
            filename: format!("photo_{}.jpg", largest.file_unique_id),
        });

    let (text, audio_ref, input_kind) = derive_message_text(msg)?;

    let sender_id = msg
        .from
        .as_ref()
        .map(|u| u.id.to_string())
        .unwrap_or_else(|| msg.chat.id.to_string());

    let thread_id = effective_thread_id(msg).map(|t| t.to_string());
    let message_id = Some(msg.message_id.to_string());
    let peer_kind = peer_kind_for_chat(&msg.chat.chat_type);

    Some(TelegramParsed {
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
        input_kind,
    })
}

/// Derive the session-relevant thread id for a Telegram message.
///
/// Telegram overloads `message_thread_id`: in forum supergroups it is the
/// topic id (a real thread, flagged by `is_topic_message`), but on an
/// ordinary reply it is a reply-UI anchor equal to the replied-to message id.
/// Keying sessions on the raw value forks a fresh session for every reply in
/// a plain group. Only explicit forum-topic messages keep their thread id;
/// everything else — plain-group reply anchors, forum General-topic messages
/// and their replies (which carry an anchor but no `is_topic_message`), DMs —
/// normalizes to `None`, i.e. the chat-level session.
fn effective_thread_id(msg: &TelegramMessage) -> Option<i64> {
    if msg.is_topic_message {
        msg.message_thread_id
    } else {
        None
    }
}

/// Make the hidden URL of each `text_link` entity visible in the text.
///
/// A formatted link shows only its label; the URL travels in the entity.
/// Without this step the agent gets the label and cannot see the target.
/// Each URL is inserted as ` (<url>)` directly after its label. The URL is
/// sender-controlled and stays part of the message text, so it takes the same
/// sanitize and trust-boundary path as the rest of the message.
///
/// Entity offsets are UTF-16 code units. An entity that is out of range or
/// that does not sit on character boundaries is skipped.
fn expand_text_links(text: &str, entities: &[MessageEntity]) -> String {
    if !entities.iter().any(|e| e.kind == "text_link") {
        return text.to_string();
    }

    // UTF-16 offset of each char boundary -> byte index, plus the end of text.
    let mut boundaries = std::collections::HashMap::new();
    let mut utf16_len = 0usize;
    for (byte_idx, ch) in text.char_indices() {
        boundaries.insert(utf16_len, byte_idx);
        utf16_len += ch.len_utf16();
    }
    boundaries.insert(utf16_len, text.len());

    // (byte index of the label end, url)
    let mut inserts: Vec<(usize, &str)> = entities
        .iter()
        .filter(|e| e.kind == "text_link")
        .filter_map(|e| {
            let url = e.url.as_deref().map(str::trim).filter(|u| !u.is_empty())?;
            let start = usize::try_from(e.offset).ok()?;
            let length = usize::try_from(e.length).ok().filter(|l| *l > 0)?;
            let end = start.checked_add(length)?;
            boundaries.get(&start)?;
            Some((*boundaries.get(&end)?, url))
        })
        .collect();

    // Last insert first, so the earlier byte indexes stay valid.
    inserts.sort_by_key(|insert| std::cmp::Reverse(insert.0));
    let mut out = text.to_string();
    for (at, url) in inserts {
        let suffix = format!(" ({url})");
        if !out[at..].starts_with(&suffix) {
            out.insert_str(at, &suffix);
        }
    }
    out
}

/// Derive display text (and any audio ref needed for transcription) from a
/// Telegram message. Prefers text, then caption, then a per-media-type
/// placeholder. Returns `None` if there's nothing routable.
fn derive_message_text(
    msg: &TelegramMessage,
) -> Option<(String, Option<TelegramAudioRef>, TelegramInputKind)> {
    if let Some(t) = msg.text.as_ref() {
        if t.is_empty() || is_binary_content(t) {
            return None;
        }
        let text = expand_text_links(t, msg.entities.as_deref().unwrap_or_default());
        return Some((text, None, TelegramInputKind::Text));
    }
    if let Some(caption) = msg.caption.as_deref().filter(|c| !is_binary_content(c)) {
        let text = expand_text_links(caption, msg.caption_entities.as_deref().unwrap_or_default());
        return Some((text, None, TelegramInputKind::Other));
    }
    if msg.photo.is_some() {
        return Some(("[Photo]".to_string(), None, TelegramInputKind::Other));
    }
    if let Some(doc) = msg.document.as_ref() {
        let text = match doc.file_name.as_deref() {
            Some(name) if !is_binary_content(name) => format!("[Document: {name}]"),
            _ => "[Document]".to_string(),
        };
        return Some((text, None, TelegramInputKind::Other));
    }
    if msg.video.is_some() {
        return Some(("[Video]".to_string(), None, TelegramInputKind::Other));
    }
    if let Some(audio) = msg.audio.as_ref() {
        let audio_ref = TelegramAudioRef {
            file_id: audio.file_id.clone(),
            mime_type: "audio/mpeg".to_string(),
            duration: audio.duration,
        };
        return Some((
            "[Audio]".to_string(),
            Some(audio_ref),
            TelegramInputKind::Audio,
        ));
    }
    if let Some(voice) = msg.voice.as_ref() {
        let audio_ref = TelegramAudioRef {
            file_id: voice.file_id.clone(),
            mime_type: "audio/ogg".to_string(),
            duration: voice.duration,
        };
        return Some((
            "[Voice message]".to_string(),
            Some(audio_ref),
            TelegramInputKind::Voice,
        ));
    }
    if let Some(sticker) = msg.sticker.as_ref() {
        let text = match &sticker.emoji {
            Some(emoji) => format!("[Sticker: {emoji}]"),
            None => "[Sticker]".to_string(),
        };
        return Some((text, None, TelegramInputKind::Other));
    }
    None
}

fn parse_callback(cb: &CallbackQuery) -> Option<TelegramParsed> {
    let data = cb.data.as_deref()?;
    if data.is_empty() {
        return None;
    }

    let chat_id = cb.message.as_ref().map(|m| m.chat.id.to_string());
    let thread_id = cb
        .message
        .as_ref()
        .and_then(effective_thread_id)
        .map(|t| t.to_string());
    let message_id = cb.message.as_ref().map(|m| m.message_id.to_string());

    let peer_kind = cb
        .message
        .as_ref()
        .map(|m| match m.chat.chat_type.as_str() {
            "private" => PEER_KIND_DIRECT.to_string(),
            _ => PEER_KIND_GROUP.to_string(),
        });

    Some(TelegramParsed {
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
        input_kind: TelegramInputKind::Other,
    })
}

fn peer_kind_for_chat(chat_type: &str) -> Option<String> {
    match chat_type {
        "private" => Some(PEER_KIND_DIRECT.to_string()),
        "group" | "supergroup" | "channel" => Some(PEER_KIND_GROUP.to_string()),
        _ => None,
    }
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

    fn entity(kind: &str, offset: i64, length: i64, url: Option<&str>) -> MessageEntity {
        MessageEntity {
            kind: kind.to_string(),
            offset,
            length,
            url: url.map(str::to_string),
        }
    }

    #[test]
    fn expand_text_links_table() {
        let link = |o, l, u| entity("text_link", o, l, Some(u));
        let cases: Vec<(&str, &str, Vec<MessageEntity>, &str)> = vec![
            (
                "ascii label",
                "read this now",
                vec![link(5, 4, "https://x.example")],
                "read this (https://x.example) now",
            ),
            (
                "astral emoji before the label counts as 2 UTF-16 units",
                "\u{1F600} read this now",
                vec![link(8, 4, "https://x.example")],
                "\u{1F600} read this (https://x.example) now",
            ),
            (
                "two links, given in document order",
                "one and two",
                vec![
                    link(0, 3, "https://a.example"),
                    link(8, 3, "https://b.example"),
                ],
                "one (https://a.example) and two (https://b.example)",
            ),
            (
                "two links, given in reverse order",
                "one and two",
                vec![
                    link(8, 3, "https://b.example"),
                    link(0, 3, "https://a.example"),
                ],
                "one (https://a.example) and two (https://b.example)",
            ),
            (
                "end past the text",
                "short",
                vec![link(3, 40, "https://x.example")],
                "short",
            ),
            (
                "negative offset",
                "short",
                vec![link(-1, 3, "https://x.example")],
                "short",
            ),
            (
                "zero length",
                "short",
                vec![link(0, 0, "https://x.example")],
                "short",
            ),
            (
                "end inside a surrogate pair",
                "\u{1F600}x",
                vec![link(0, 1, "https://x.example")],
                "\u{1F600}x",
            ),
            ("blank url", "label", vec![link(0, 5, "   ")], "label"),
            (
                "url already written after the label",
                "label (https://x.example) tail",
                vec![link(0, 5, "https://x.example")],
                "label (https://x.example) tail",
            ),
            (
                "plain url, mention and bold entities change nothing",
                "see https://x.example @bob now",
                vec![
                    entity("url", 4, 17, None),
                    entity("mention", 22, 4, None),
                    entity("bold", 27, 3, Some("https://ignored.example")),
                ],
                "see https://x.example @bob now",
            ),
        ];
        for (name, text, entities, expected) in cases {
            assert_eq!(expand_text_links(text, &entities), expected, "{name}");
        }
    }

    #[test]
    fn text_link_entities_reach_the_inbound_text() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 1,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "text": "see the report",
                    "entities": [
                        { "type": "text_link", "offset": 8, "length": 6, "url": "https://x.example/r" }
                    ]
                }
            }"#,
        )
        .unwrap();
        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(msg.text, "see the report (https://x.example/r)");
    }

    #[test]
    fn caption_entities_reach_the_inbound_text() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 4,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "photo": [{ "file_id": "abc", "file_unique_id": "u1", "width": 100, "height": 100 }],
                    "caption": "full story here",
                    "caption_entities": [
                        { "type": "text_link", "offset": 11, "length": 4, "url": "https://x.example/s" }
                    ]
                }
            }"#,
        )
        .unwrap();
        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(msg.text, "full story here (https://x.example/s)");
    }

    #[test]
    fn parse_text_message() {
        let update = text_update("Hello bot");
        let p = parse_update(&update).unwrap();
        let input_kind = p.input_kind;
        let (msg, audio) = (p.inbound, p.audio);
        assert_eq!(msg.sender_id, "42");
        assert_eq!(msg.text, "Hello bot");
        assert_eq!(msg.channel_id.as_deref(), Some("42"));
        assert_eq!(msg.message_id.as_deref(), Some("1"));
        assert!(audio.is_none());
        assert_eq!(input_kind, TelegramInputKind::Text);
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

        let p = parse_update(&update).unwrap();
        let (msg, audio) = (p.inbound, p.audio);
        assert_eq!(msg.sender_id, "99");
        assert_eq!(msg.text, "btn_click");
        assert_eq!(msg.channel_id.as_deref(), Some("42"));
        assert!(audio.is_none());
    }

    /// Callback queries carry the chat's peer kind: a private chat is a
    /// direct conversation, anything else is a group. Session routing keys
    /// off this, so a callback in a DM misclassified as group-scoped would
    /// land in the wrong session.
    #[test]
    fn callback_peer_kind_tracks_chat_type() {
        let cases = [
            ("private", PEER_KIND_DIRECT),
            ("supergroup", PEER_KIND_GROUP),
        ];
        for (chat_type, expected) in cases {
            let update: Update = serde_json::from_str(&format!(
                r#"{{
                    "update_id": 3,
                    "callback_query": {{
                        "id": "cb1",
                        "from": {{ "id": 99, "first_name": "Bob", "is_bot": false }},
                        "message": {{
                            "message_id": 5,
                            "chat": {{ "id": 42, "type": "{chat_type}" }},
                            "date": 1700000000
                        }},
                        "data": "btn_click"
                    }}
                }}"#
            ))
            .unwrap();
            let msg = parse_update(&update).unwrap().inbound;
            assert_eq!(
                msg.peer_kind.as_deref(),
                Some(expected),
                "chat type {chat_type:?}"
            );
        }
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

        let p = parse_update(&update).unwrap();
        let input_kind = p.input_kind;
        let (msg, audio) = (p.inbound, p.audio);
        assert_eq!(msg.text, "[Voice message]");
        let audio = audio.unwrap();
        assert_eq!(audio.file_id, "abc");
        assert_eq!(audio.mime_type, "audio/ogg");
        assert_eq!(audio.duration, 5);
        assert_eq!(input_kind, TelegramInputKind::Voice);
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

        let p = parse_update(&update).unwrap();
        let input_kind = p.input_kind;
        let (msg, audio) = (p.inbound, p.audio);
        assert_eq!(msg.text, "[Audio]");
        let audio = audio.unwrap();
        assert_eq!(audio.file_id, "audio123");
        assert_eq!(audio.mime_type, "audio/mpeg");
        assert_eq!(audio.duration, 180);
        assert_eq!(input_kind, TelegramInputKind::Audio);
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
    fn forum_topic_message_keeps_topic_thread_id() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 10,
                "message": {
                    "message_id": 50,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": -100123, "type": "supergroup", "is_forum": true },
                    "date": 1700000000,
                    "message_thread_id": 99,
                    "is_topic_message": true,
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
    fn plain_group_reply_anchor_is_not_a_thread() {
        // In a non-forum group, a reply carries message_thread_id equal to the
        // replied-to message id (a reply-UI anchor). Treating it as a thread
        // would fork a new session for every reply in the group.
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 11,
                "message": {
                    "message_id": 51,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": -100123, "type": "supergroup" },
                    "date": 1700000000,
                    "message_thread_id": 40,
                    "reply_to_message": {
                        "message_id": 40,
                        "chat": { "id": -100123, "type": "supergroup" },
                        "date": 1699999000,
                        "text": "original"
                    },
                    "text": "replying in plain group"
                }
            }"#,
        )
        .unwrap();

        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(
            msg.thread_id, None,
            "reply anchor must not become a thread id"
        );
        // Session key must stay the group-level session, not fork per reply.
        assert_eq!(
            msg.session_key("telegram", msg.thread_id.as_deref().unwrap_or("")),
            "telegram:42:"
        );
    }

    #[test]
    fn forum_general_topic_reply_anchor_dropped() {
        // Replies in a forum's General topic carry a reply anchor but no
        // is_topic_message flag — they must land in the same (thread-less)
        // session as non-reply General messages.
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 12,
                "message": {
                    "message_id": 52,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": -100123, "type": "supergroup", "is_forum": true },
                    "date": 1700000000,
                    "message_thread_id": 45,
                    "reply_to_message": {
                        "message_id": 45,
                        "chat": { "id": -100123, "type": "supergroup", "is_forum": true },
                        "date": 1699999000,
                        "text": "general topic original"
                    },
                    "text": "reply in General"
                }
            }"#,
        )
        .unwrap();

        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(msg.thread_id, None);
    }

    #[test]
    fn dm_reply_has_no_thread_id() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 13,
                "message": {
                    "message_id": 53,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "reply_to_message": {
                        "message_id": 30,
                        "chat": { "id": 42, "type": "private" },
                        "date": 1699999000,
                        "text": "earlier"
                    },
                    "text": "dm reply"
                }
            }"#,
        )
        .unwrap();

        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(msg.thread_id, None);
        assert_eq!(msg.session_key("telegram", ""), "telegram:42:");
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
    fn binary_caption_dropped_photo_falls_back_to_placeholder() {
        // Caption contains 0x01 (SOH) — raw byte leaked from a binary document.
        // Expect: caption discarded, photo placeholder shown instead.
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 30,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "photo": [{ "file_id": "abc", "file_unique_id": "u1", "width": 100, "height": 100 }],
                    "caption": "hello\u0001world"
                }
            }"#,
        )
        .unwrap();

        let p = parse_update(&update).unwrap();
        assert_eq!(p.inbound.text, "[Photo]");
        assert!(p.photo.is_some());
    }

    #[test]
    fn binary_document_filename_falls_back_to_placeholder() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 31,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "document": { "file_id": "abc", "file_unique_id": "u1", "file_name": "junk\u0000\u0001.bin" }
                }
            }"#,
        )
        .unwrap();

        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(msg.text, "[Document]");
    }

    #[test]
    fn multibyte_utf8_caption_is_not_flagged_as_binary() {
        // Emojis, accents, and newlines must pass through untouched.
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 32,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "photo": [{ "file_id": "abc", "file_unique_id": "u1", "width": 100, "height": 100 }],
                    "caption": "café 🎉\nline two\tindented"
                }
            }"#,
        )
        .unwrap();

        let msg = parse_update(&update).unwrap().inbound;
        assert_eq!(msg.text, "café 🎉\nline two\tindented");
    }

    #[test]
    fn binary_text_message_returns_none() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 33,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "text": "\u0002\u0003\u0004"
                }
            }"#,
        )
        .unwrap();

        assert!(parse_update(&update).is_none());
    }

    #[test]
    fn is_binary_content_boundaries() {
        assert!(!is_binary_content(""));
        assert!(!is_binary_content("plain text"));
        assert!(!is_binary_content("tab\there\nnewline\rreturn"));
        assert!(is_binary_content("nul\0"));
        assert!(is_binary_content("\u{0008}backspace"));
        assert!(is_binary_content("\u{000E}so"));
        assert!(is_binary_content("\u{001F}us"));
        // 0x0B, 0x0C are in the forbidden range 0x0E..=0x1F? No, they're 0x0B/0x0C.
        // 0x0B (vertical tab) and 0x0C (form feed) are in 0x00..=0x08? No — they're between.
        // Actually 0x0B and 0x0C fall in neither range → should be allowed.
        assert!(!is_binary_content("\u{000B}vtab"));
        assert!(!is_binary_content("\u{000C}formfeed"));
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
