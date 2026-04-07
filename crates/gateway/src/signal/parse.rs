use crate::constants::{PEER_KIND_DIRECT, PEER_KIND_GROUP};
use crate::handler::InboundMessage;

use super::types::{Mention, SignalEnvelope};

/// Replace Signal mention placeholders (U+FFFC) with `@name` text.
///
/// Signal uses the Object Replacement Character at `mention.start` for
/// `mention.length` chars. We replace each placeholder with `@<uuid>` or
/// `@<number>` from the corresponding [`Mention`] entry.
fn resolve_mentions(text: &str, mentions: &[Mention]) -> String {
    let mut result: Vec<char> = text.chars().collect();
    let mut sorted: Vec<&Mention> = mentions.iter().collect();
    // Process in reverse order so earlier indices stay valid after replacement.
    sorted.sort_by(|a, b| b.start.unwrap_or(0).cmp(&a.start.unwrap_or(0)));

    for mention in sorted {
        let start = mention.start.unwrap_or(0) as usize;
        let length = mention.length.unwrap_or(1) as usize;
        let name = mention
            .uuid
            .as_deref()
            .or(mention.number.as_deref())
            .unwrap_or("unknown");
        let replacement: Vec<char> = format!("@{name}").chars().collect();

        if start < result.len() {
            let end = (start + length).min(result.len());
            result.splice(start..end, replacement);
        }
    }

    result.into_iter().collect()
}

/// Convert a signal-cli SSE envelope into an `InboundMessage`.
///
/// Returns `None` for:
/// - Own messages (source matches `own_account`)
/// - Sync messages (messages sent from another device on the same account)
/// - Receipt-only or typing-only envelopes
/// - Envelopes with no text content and no reaction
pub fn parse_envelope(envelope: &SignalEnvelope, own_account: &str) -> Option<InboundMessage> {
    let env = &envelope.envelope;

    // Determine sender — prefer source, fall back to source_number, then source_uuid
    let sender = env
        .source
        .as_deref()
        .or(env.source_number.as_deref())
        .or(env.source_uuid.as_deref())?;

    // Filter own messages (loop protection)
    if sender == own_account {
        return None;
    }
    // Also check if the envelope's account field matches the sender
    if let Some(ref account) = envelope.account {
        if sender == account {
            return None;
        }
    }

    // Filter sync messages (sent from another device on our account)
    if env.sync_message.is_some() {
        return None;
    }

    // Extract data message: prefer direct data_message, fall back to editMessage wrapper
    let is_edit;
    let data_msg = if let Some(ref dm) = env.data_message {
        is_edit = false;
        dm
    } else if let Some(ref edit) = env.edit_message {
        is_edit = true;
        edit.data_message.as_ref()?
    } else {
        return None;
    };

    // Build text from message content
    let mut text = data_msg.message.clone().unwrap_or_default();

    // Resolve mention placeholders (U+FFFC -> @name)
    if let Some(ref mentions) = data_msg.mentions {
        if !mentions.is_empty() {
            text = resolve_mentions(&text, mentions);
        }
    }

    // Prepend quoted message context
    if let Some(ref quote) = data_msg.quote {
        if let Some(ref quote_text) = quote.text {
            if !quote_text.is_empty() {
                let author = quote.author.as_deref().unwrap_or("someone");
                text = format!("> {author}: {quote_text}\n{text}");
            }
        }
    }

    // Prefix edit messages
    if is_edit {
        text = format!("[Edit] {text}");
    }

    // Append attachment placeholders
    if let Some(ref attachments) = data_msg.attachments {
        for att in attachments {
            let desc = att
                .filename
                .as_deref()
                .or(att.content_type.as_deref())
                .unwrap_or("file");
            if text.is_empty() {
                text = format!("[Attachment: {desc}]");
            } else {
                text = format!("{text}\n[Attachment: {desc}]");
            }
        }
    }

    // Handle reaction-only messages
    let reaction = data_msg.reaction.as_ref().map(|r| {
        if r.is_remove {
            format!("-{}", r.emoji)
        } else {
            r.emoji.clone()
        }
    });

    // If no text and no reaction, skip
    if text.is_empty() && reaction.is_none() {
        return None;
    }

    // For reaction-only messages, set text with target context
    if text.is_empty() {
        if let Some(ref r) = data_msg.reaction {
            let target = r.target_author.as_deref().unwrap_or("unknown");
            if r.is_remove {
                text = format!("[Removed reaction {} from {}'s message]", r.emoji, target);
            } else {
                text = format!("[Reacted {} to {}'s message]", r.emoji, target);
            }
        }
    }

    // Map group_info -> channel_id
    let channel_id = data_msg.group_info.as_ref().map(|g| g.group_id.clone());

    // Use the data message timestamp as message_id
    let message_id = if data_msg.timestamp != 0 {
        Some(data_msg.timestamp.to_string())
    } else {
        None
    };

    Some(InboundMessage {
        sender_id: sender.to_string(),
        text,
        channel_id,
        thread_id: None,
        message_id,
        thread_ts: None,
        attachments: vec![],
        reaction,
        metadata: serde_json::json!({
            "source_uuid": env.source_uuid,
            "timestamp": env.timestamp,
        }),
        peer_kind: if data_msg.group_info.is_some() {
            Some(PEER_KIND_GROUP.to_string())
        } else {
            Some(PEER_KIND_DIRECT.to_string())
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::types::*;

    fn make_envelope(json: &str) -> SignalEnvelope {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn dm_text_message() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "sourceUuid": "uuid-123",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "message": "Hello"
                    }
                },
                "account": "+15559876543"
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert_eq!(msg.sender_id, "+15551234567");
        assert_eq!(msg.text, "Hello");
        assert!(msg.channel_id.is_none());
        assert_eq!(msg.message_id.as_deref(), Some("1700000000000"));
    }

    #[test]
    fn group_text_message() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "message": "Hello group",
                        "groupInfo": {
                            "groupId": "group123"
                        }
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert_eq!(msg.text, "Hello group");
        assert_eq!(msg.channel_id.as_deref(), Some("group123"));
    }

    #[test]
    fn own_message_filtered_by_account() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15559876543",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "message": "My own message"
                    }
                },
                "account": "+15559876543"
            }"#,
        );
        assert!(parse_envelope(&env, "+15559876543").is_none());
    }

    #[test]
    fn own_message_filtered_by_own_account_param() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15559876543",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "message": "My own message"
                    }
                }
            }"#,
        );
        assert!(parse_envelope(&env, "+15559876543").is_none());
    }

    #[test]
    fn sync_message_filtered() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15559876543",
                    "timestamp": 1700000000000,
                    "syncMessage": {
                        "sentMessage": {
                            "destination": "+15551234567",
                            "message": "Sent from another device"
                        }
                    }
                }
            }"#,
        );
        assert!(parse_envelope(&env, "+15550000000").is_none());
    }

    #[test]
    fn receipt_only_filtered() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "receiptMessage": {
                        "when": 1700000000000,
                        "isDelivery": true
                    }
                }
            }"#,
        );
        assert!(parse_envelope(&env, "+15559876543").is_none());
    }

    #[test]
    fn typing_only_filtered() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "typingMessage": {
                        "action": "STARTED"
                    }
                }
            }"#,
        );
        assert!(parse_envelope(&env, "+15559876543").is_none());
    }

    #[test]
    fn attachment_adds_placeholder() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "message": "Look at this",
                        "attachments": [
                            {
                                "contentType": "image/jpeg",
                                "filename": "photo.jpg",
                                "size": 50000
                            }
                        ]
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert!(msg.text.contains("Look at this"));
        assert!(msg.text.contains("[Attachment: photo.jpg]"));
    }

    #[test]
    fn attachment_only_message() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "attachments": [
                            {
                                "contentType": "application/pdf"
                            }
                        ]
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert_eq!(msg.text, "[Attachment: application/pdf]");
    }

    #[test]
    fn reaction_message() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "reaction": {
                            "emoji": "👍",
                            "targetAuthor": "+15559876543",
                            "targetSentTimestamp": 1699999000000,
                            "isRemove": false
                        }
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert_eq!(msg.reaction.as_deref(), Some("👍"));
        assert!(msg.text.contains("[Reacted 👍 to +15559876543's message]"));
    }

    #[test]
    fn reaction_remove() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "reaction": {
                            "emoji": "👍",
                            "targetAuthor": "+15559876543",
                            "targetSentTimestamp": 1699999000000,
                            "isRemove": true
                        }
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert_eq!(msg.reaction.as_deref(), Some("-👍"));
        assert!(msg
            .text
            .contains("[Removed reaction 👍 from +15559876543's message]"));
    }

    #[test]
    fn empty_data_message_filtered() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000
                    }
                }
            }"#,
        );
        assert!(parse_envelope(&env, "+15559876543").is_none());
    }

    #[test]
    fn uuid_sender_fallback() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "sourceUuid": "uuid-abc-123",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "message": "From UUID sender"
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert_eq!(msg.sender_id, "uuid-abc-123");
    }

    #[test]
    fn no_source_returns_none() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "message": "No sender"
                    }
                }
            }"#,
        );
        assert!(parse_envelope(&env, "+15559876543").is_none());
    }

    // ── Mention tests ──

    #[test]
    fn mention_replaced_in_text() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "message": "\uFFFC hello",
                        "mentions": [
                            {"start": 0, "length": 1, "uuid": "abc-123"}
                        ]
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert_eq!(msg.text, "@abc-123 hello");
    }

    #[test]
    fn multiple_mentions_resolved() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "message": "\uFFFC and \uFFFC",
                        "mentions": [
                            {"start": 0, "length": 1, "uuid": "user-a"},
                            {"start": 6, "length": 1, "uuid": "user-b"}
                        ]
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert!(msg.text.contains("@user-a"));
        assert!(msg.text.contains("@user-b"));
    }

    #[test]
    fn mention_with_number_fallback() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "message": "\uFFFC hi",
                        "mentions": [
                            {"start": 0, "length": 1, "number": "+15559999999"}
                        ]
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert!(msg.text.contains("@+15559999999"));
    }

    #[test]
    fn mention_no_id_uses_unknown() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "message": "\uFFFC hi",
                        "mentions": [
                            {"start": 0, "length": 1}
                        ]
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert!(msg.text.contains("@unknown"));
    }

    #[test]
    fn no_mentions_text_unchanged() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "message": "plain text"
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert_eq!(msg.text, "plain text");
    }

    // ── Quote tests ──

    #[test]
    fn quote_prepended_to_text() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "message": "my reply",
                        "quote": {
                            "id": 1699999000000,
                            "author": "+15559876543",
                            "text": "original message"
                        }
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert_eq!(msg.text, "> +15559876543: original message\nmy reply");
    }

    #[test]
    fn quote_without_text_ignored() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "message": "my reply",
                        "quote": {
                            "id": 1699999000000,
                            "author": "+15559876543"
                        }
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert_eq!(msg.text, "my reply");
    }

    #[test]
    fn quote_without_author_defaults() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "message": "my reply",
                        "quote": {
                            "text": "quoted text"
                        }
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert!(msg.text.starts_with("> someone: quoted text"));
    }

    // ── Edit message tests ──

    #[test]
    fn edit_message_parsed() {
        let env = make_envelope(
            r#"{
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
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert_eq!(msg.text, "[Edit] corrected text");
        assert_eq!(msg.sender_id, "+15551234567");
    }

    #[test]
    fn edit_message_with_mentions() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000001000,
                    "editMessage": {
                        "targetSentTimestamp": 1700000000000,
                        "dataMessage": {
                            "timestamp": 1700000001000,
                            "message": "\uFFFC corrected",
                            "mentions": [
                                {"start": 0, "length": 1, "uuid": "user-x"}
                            ]
                        }
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert_eq!(msg.text, "[Edit] @user-x corrected");
    }

    #[test]
    fn edit_message_no_data_message_returns_none() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000001000,
                    "editMessage": {
                        "targetSentTimestamp": 1700000000000
                    }
                }
            }"#,
        );
        assert!(parse_envelope(&env, "+15559876543").is_none());
    }

    // ── Reaction context tests ──

    #[test]
    fn reaction_includes_target_author() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "reaction": {
                            "emoji": "❤",
                            "targetAuthor": "+15550001111",
                            "targetSentTimestamp": 1699999000000,
                            "isRemove": false
                        }
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert_eq!(msg.text, "[Reacted ❤ to +15550001111's message]");
    }

    #[test]
    fn reaction_unknown_target_author() {
        let env = make_envelope(
            r#"{
                "envelope": {
                    "source": "+15551234567",
                    "timestamp": 1700000000000,
                    "dataMessage": {
                        "timestamp": 1700000000000,
                        "reaction": {
                            "emoji": "👍",
                            "isRemove": false
                        }
                    }
                }
            }"#,
        );
        let msg = parse_envelope(&env, "+15559876543").unwrap();
        assert!(msg.text.contains("unknown's message"));
    }
}
