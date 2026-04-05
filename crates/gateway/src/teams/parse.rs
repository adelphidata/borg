use regex::Regex;

use super::types::Activity;
use crate::constants::{PEER_KIND_DIRECT, PEER_KIND_GROUP};
use crate::handler::InboundMessage;

/// Parse a Teams `Activity` into an `InboundMessage`.
///
/// Returns `None` for:
/// - Non-message activity types (conversationUpdate, typing, etc.)
/// - Activities without text
/// - Bot self-messages (from.id matches recipient.id)
pub fn parse_activity(activity: &Activity) -> Option<InboundMessage> {
    // Only handle message activities
    if activity.activity_type != "message" {
        return None;
    }

    // Skip bot self-messages to prevent loops
    if let (Some(from), Some(recipient)) = (&activity.from, &activity.recipient) {
        if from.id == recipient.id {
            return None;
        }
    }

    let raw_text = activity.text.as_deref()?;

    // Strip <at>...</at> mention tags from the text
    let text = strip_mention_tags(raw_text);
    let text = text.trim();

    if text.is_empty() {
        return None;
    }

    let sender_id = activity.from.as_ref()?.id.clone();

    Some(InboundMessage {
        sender_id,
        text: text.to_string(),
        channel_id: activity.conversation.as_ref().map(|c| c.id.clone()),
        // Populate thread_id from reply_to_id so the gateway handler isolates
        // per-reply-chain history. Top-level messages in a conversation have
        // no reply_to_id and share the conversation's base session.
        thread_id: activity.reply_to_id.clone(),
        message_id: Some(activity.id.clone()),
        thread_ts: activity.reply_to_id.clone(),
        attachments: Vec::new(),
        reaction: None,
        metadata: serde_json::Value::Null,
        peer_kind: activity
            .conversation
            .as_ref()
            .and_then(|c| c.is_group)
            .map(|g| {
                if g {
                    PEER_KIND_GROUP.to_string()
                } else {
                    PEER_KIND_DIRECT.to_string()
                }
            }),
    })
}

/// Remove `<at>...</at>` mention tags from Teams message text.
fn strip_mention_tags(text: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"<at>[^<]*</at>\s*").unwrap_or_else(|_| unreachable!()));
    re.replace_all(text, "").to_string()
}

#[cfg(test)]
mod tests {
    use super::super::types::{ChannelAccount, ConversationAccount};
    use super::*;

    fn make_activity() -> Activity {
        Activity {
            id: "act-1".to_string(),
            activity_type: "message".to_string(),
            text: Some("hello bot".to_string()),
            from: Some(ChannelAccount {
                id: "user-1".to_string(),
                name: Some("Alice".to_string()),
            }),
            conversation: Some(ConversationAccount {
                id: "conv-1".to_string(),
                name: Some("General".to_string()),
                is_group: Some(true),
            }),
            recipient: Some(ChannelAccount {
                id: "bot-1".to_string(),
                name: Some("MyBot".to_string()),
            }),
            service_url: Some("https://smba.trafficmanager.net/teams/".to_string()),
            reply_to_id: None,
            entities: None,
            timestamp: Some("2026-03-17T12:00:00Z".to_string()),
            members_added: None,
            members_removed: None,
        }
    }

    #[test]
    fn parse_message_activity() {
        let activity = make_activity();
        let msg = parse_activity(&activity).unwrap();
        assert_eq!(msg.sender_id, "user-1");
        assert_eq!(msg.text, "hello bot");
        assert_eq!(msg.channel_id.as_deref(), Some("conv-1"));
        assert_eq!(msg.message_id.as_deref(), Some("act-1"));
        assert!(msg.thread_ts.is_none());
    }

    #[test]
    fn parse_message_with_reply_to_id() {
        let mut activity = make_activity();
        activity.reply_to_id = Some("parent-1".to_string());
        let msg = parse_activity(&activity).unwrap();
        assert_eq!(msg.thread_ts.as_deref(), Some("parent-1"));
        // thread_id mirrors reply_to_id so the gateway handler isolates per-thread sessions.
        assert_eq!(msg.thread_id.as_deref(), Some("parent-1"));
    }

    #[test]
    fn parse_top_level_message_has_no_thread_id() {
        let activity = make_activity();
        let msg = parse_activity(&activity).unwrap();
        assert!(msg.thread_id.is_none());
    }

    #[test]
    fn strip_mention_tags_from_text() {
        let mut activity = make_activity();
        activity.text = Some("<at>MyBot</at> help me please".to_string());
        let msg = parse_activity(&activity).unwrap();
        assert_eq!(msg.text, "help me please");
    }

    #[test]
    fn strip_multiple_mention_tags() {
        let mut activity = make_activity();
        activity.text = Some("<at>Bot</at> <at>User</at> hello".to_string());
        let msg = parse_activity(&activity).unwrap();
        assert_eq!(msg.text, "hello");
    }

    #[test]
    fn non_message_activity_returns_none() {
        let mut activity = make_activity();
        activity.activity_type = "conversationUpdate".to_string();
        assert!(parse_activity(&activity).is_none());
    }

    #[test]
    fn typing_activity_returns_none() {
        let mut activity = make_activity();
        activity.activity_type = "typing".to_string();
        assert!(parse_activity(&activity).is_none());
    }

    #[test]
    fn no_text_returns_none() {
        let mut activity = make_activity();
        activity.text = None;
        assert!(parse_activity(&activity).is_none());
    }

    #[test]
    fn empty_text_returns_none() {
        let mut activity = make_activity();
        activity.text = Some(String::new());
        assert!(parse_activity(&activity).is_none());
    }

    #[test]
    fn only_mention_tag_returns_none() {
        let mut activity = make_activity();
        activity.text = Some("<at>MyBot</at>".to_string());
        assert!(parse_activity(&activity).is_none());
    }

    #[test]
    fn bot_self_message_returns_none() {
        let mut activity = make_activity();
        activity.from = Some(ChannelAccount {
            id: "bot-1".to_string(),
            name: Some("MyBot".to_string()),
        });
        // recipient is also bot-1
        assert!(parse_activity(&activity).is_none());
    }

    #[test]
    fn no_from_returns_none() {
        let mut activity = make_activity();
        activity.from = None;
        assert!(parse_activity(&activity).is_none());
    }

    #[test]
    fn missing_conversation_still_parses() {
        let mut activity = make_activity();
        activity.conversation = None;
        let msg = parse_activity(&activity).unwrap();
        assert!(msg.channel_id.is_none());
    }
}
