use super::types::{ChatEvent, EventType};
use crate::constants::{PEER_KIND_DIRECT, PEER_KIND_GROUP};
use crate::handler::InboundMessage;

/// Parse a Google Chat event into an `InboundMessage`.
///
/// Returns `None` for:
/// - Non-message events (ADDED_TO_SPACE, REMOVED_FROM_SPACE, etc.)
/// - Bot messages (sender user_type is "BOT")
/// - Events without message text
pub fn parse_event(event: &ChatEvent) -> Option<InboundMessage> {
    // Only process MESSAGE events
    if event.event_type != EventType::Message {
        return None;
    }

    let message = event.message.as_ref()?;

    // Determine sender — prefer message.sender, fall back to event.user
    let sender = message.sender.as_ref().or(event.user.as_ref());

    // Skip BOT user types to prevent self-reply loops
    if let Some(s) = &sender {
        if s.user_type.as_deref() == Some("BOT") {
            return None;
        }
    }

    // Extract sender_id from user resource name (e.g. "users/12345")
    let sender_id = sender.and_then(|s| s.name.as_deref()).unwrap_or("unknown");

    // Prefer argument_text over text (argument_text strips @mentions)
    let text = message
        .argument_text
        .as_deref()
        .or(message.text.as_deref())
        .unwrap_or("");

    // Audio attachments — captured as `InboundAttachment { data = resourceName }`
    // placeholders. The channel handler downloads + transcribes them.
    let mut attachments: Vec<crate::handler::InboundAttachment> = Vec::new();
    if let Some(ref atts) = message.attachment {
        for att in atts {
            let Some(ref resource) = att.attachment_data_ref else {
                continue;
            };
            let Some(ref name) = resource.resource_name else {
                continue;
            };
            attachments.push(crate::handler::InboundAttachment {
                mime_type: att
                    .content_type
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".to_string()),
                data: name.clone(),
                filename: att.content_name.clone(),
            });
        }
    }

    // Reject empty payload — no text AND no attachments to download.
    if text.is_empty() && attachments.is_empty() {
        return None;
    }

    let channel_id = event.space.as_ref().and_then(|s| s.name.clone());
    let thread_id = message.thread.as_ref().and_then(|t| t.name.clone());
    let message_id = message.name.clone();

    Some(InboundMessage {
        sender_id: sender_id.to_string(),
        text: text.to_string(),
        channel_id,
        thread_id,
        message_id,
        thread_ts: None,
        attachments,
        reaction: None,
        metadata: serde_json::Value::Null,
        peer_kind: event
            .space
            .as_ref()
            .and_then(|s| s.space_type.as_deref())
            .map(|t| {
                if t == "DM" {
                    PEER_KIND_DIRECT.to_string()
                } else {
                    PEER_KIND_GROUP.to_string()
                }
            }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google_chat::types::*;

    fn make_message_event() -> ChatEvent {
        ChatEvent {
            event_type: EventType::Message,
            event_time: None,
            token: None,
            message: Some(ChatMessage {
                name: Some("spaces/SPACE1/messages/MSG1".into()),
                sender: Some(ChatUser {
                    name: Some("users/USER1".into()),
                    display_name: Some("Alice".into()),
                    user_type: Some("HUMAN".into()),
                }),
                text: Some("@Bot hello there".into()),
                argument_text: Some("hello there".into()),
                thread: Some(Thread {
                    name: Some("spaces/SPACE1/threads/THREAD1".into()),
                }),
                create_time: None,
                attachment: None,
            }),
            user: Some(ChatUser {
                name: Some("users/USER1".into()),
                display_name: Some("Alice".into()),
                user_type: Some("HUMAN".into()),
            }),
            space: Some(Space {
                name: Some("spaces/SPACE1".into()),
                display_name: Some("General".into()),
                space_type: Some("ROOM".into()),
            }),
        }
    }

    #[test]
    fn parse_message_event() {
        let event = make_message_event();
        let msg = parse_event(&event).unwrap();

        assert_eq!(msg.sender_id, "users/USER1");
        assert_eq!(msg.text, "hello there"); // argument_text preferred
        assert_eq!(msg.channel_id.as_deref(), Some("spaces/SPACE1"));
        assert_eq!(
            msg.thread_id.as_deref(),
            Some("spaces/SPACE1/threads/THREAD1")
        );
        assert_eq!(
            msg.message_id.as_deref(),
            Some("spaces/SPACE1/messages/MSG1")
        );
        assert!(msg.thread_ts.is_none());
        assert!(msg.attachments.is_empty());
    }

    #[test]
    fn argument_text_preferred_over_text() {
        let mut event = make_message_event();
        if let Some(ref mut msg) = event.message {
            msg.text = Some("@Bot do something".into());
            msg.argument_text = Some("do something".into());
        }

        let msg = parse_event(&event).unwrap();
        assert_eq!(msg.text, "do something");
    }

    #[test]
    fn falls_back_to_text_when_no_argument_text() {
        let mut event = make_message_event();
        if let Some(ref mut msg) = event.message {
            msg.argument_text = None;
            msg.text = Some("plain text".into());
        }

        let msg = parse_event(&event).unwrap();
        assert_eq!(msg.text, "plain text");
    }

    #[test]
    fn bot_message_returns_none() {
        let mut event = make_message_event();
        if let Some(ref mut msg) = event.message {
            if let Some(ref mut sender) = msg.sender {
                sender.user_type = Some("BOT".into());
            }
        }

        assert!(parse_event(&event).is_none());
    }

    #[test]
    fn bot_user_on_event_level_returns_none() {
        let mut event = make_message_event();
        // Remove message-level sender, set event-level user as BOT
        if let Some(ref mut msg) = event.message {
            msg.sender = None;
        }
        event.user = Some(ChatUser {
            name: Some("users/BOT1".into()),
            display_name: Some("MyBot".into()),
            user_type: Some("BOT".into()),
        });

        assert!(parse_event(&event).is_none());
    }

    #[test]
    fn non_message_event_returns_none() {
        let event = ChatEvent {
            event_type: EventType::AddedToSpace,
            event_time: None,
            token: None,
            message: None,
            user: Some(ChatUser {
                name: Some("users/USER1".into()),
                display_name: Some("Alice".into()),
                user_type: Some("HUMAN".into()),
            }),
            space: Some(Space {
                name: Some("spaces/SPACE1".into()),
                display_name: None,
                space_type: Some("DM".into()),
            }),
        };

        assert!(parse_event(&event).is_none());
    }

    #[test]
    fn removed_from_space_returns_none() {
        let event = ChatEvent {
            event_type: EventType::RemovedFromSpace,
            event_time: None,
            token: None,
            message: None,
            user: None,
            space: None,
        };

        assert!(parse_event(&event).is_none());
    }

    #[test]
    fn no_message_returns_none() {
        let event = ChatEvent {
            event_type: EventType::Message,
            event_time: None,
            token: None,
            message: None,
            user: None,
            space: None,
        };

        assert!(parse_event(&event).is_none());
    }

    #[test]
    fn empty_text_returns_none() {
        let mut event = make_message_event();
        if let Some(ref mut msg) = event.message {
            msg.text = Some(String::new());
            msg.argument_text = Some(String::new());
        }

        assert!(parse_event(&event).is_none());
    }

    #[test]
    fn no_text_returns_none() {
        let mut event = make_message_event();
        if let Some(ref mut msg) = event.message {
            msg.text = None;
            msg.argument_text = None;
        }

        assert!(parse_event(&event).is_none());
    }

    #[test]
    fn falls_back_to_event_user_for_sender() {
        let mut event = make_message_event();
        if let Some(ref mut msg) = event.message {
            msg.sender = None;
        }
        event.user = Some(ChatUser {
            name: Some("users/FALLBACK_USER".into()),
            display_name: Some("Fallback".into()),
            user_type: Some("HUMAN".into()),
        });

        let msg = parse_event(&event).unwrap();
        assert_eq!(msg.sender_id, "users/FALLBACK_USER");
    }

    #[test]
    fn missing_sender_uses_unknown() {
        let mut event = make_message_event();
        if let Some(ref mut msg) = event.message {
            msg.sender = None;
        }
        event.user = None;

        let msg = parse_event(&event).unwrap();
        assert_eq!(msg.sender_id, "unknown");
    }

    #[test]
    fn missing_space_gives_no_channel_id() {
        let mut event = make_message_event();
        event.space = None;

        let msg = parse_event(&event).unwrap();
        assert!(msg.channel_id.is_none());
    }

    #[test]
    fn missing_thread_gives_no_thread_id() {
        let mut event = make_message_event();
        if let Some(ref mut msg) = event.message {
            msg.thread = None;
        }

        let msg = parse_event(&event).unwrap();
        assert!(msg.thread_id.is_none());
    }
}
