use regex::Regex;

use super::types::Activity;
use crate::constants::{PEER_KIND_DIRECT, PEER_KIND_GROUP};
use crate::handler::InboundMessage;

/// Parse a Teams `Activity` into an `InboundMessage`.
///
/// Returns `None` for:
/// - Non-message/non-invoke activity types
/// - Activities without text
/// - Bot self-messages (from.id matches recipient.id)
pub fn parse_activity(activity: &Activity) -> Option<InboundMessage> {
    // messageReaction activities have no text — surface them through the
    // reaction field on InboundMessage so handlers can route them separately.
    if activity.activity_type == "messageReaction" {
        return parse_message_reaction(activity);
    }

    // Handle message and invoke activities
    if activity.activity_type != "message" && activity.activity_type != "invoke" {
        return None;
    }

    // Skip bot self-messages to prevent loops
    if let (Some(from), Some(recipient)) = (&activity.from, &activity.recipient) {
        if from.id == recipient.id {
            return None;
        }
    }

    // Extract text based on activity type
    let raw_text = if activity.activity_type == "invoke" {
        activity
            .value
            .as_ref()
            .and_then(|v| {
                v.get("text")
                    .and_then(|t| t.as_str())
                    .or_else(|| v.get("action").and_then(|a| a.as_str()))
            })
            .map(ToString::to_string)
    } else {
        activity.text.clone()
    };
    let raw_text = raw_text.as_deref()?;

    // Extract quoted content from HTML blockquotes in reply messages
    let quote_context = activity
        .reply_to_id
        .as_ref()
        .and_then(|_| extract_quote_text(raw_text));

    // Process mention tags: bot mentions stripped, user mentions preserved as @Name
    let bot_id = activity.recipient.as_ref().map(|r| r.id.as_str());
    let text = resolve_mention_tags(raw_text, activity.entities.as_deref(), bot_id);
    let text = strip_html_tags(&text);
    let mut text = text.trim().to_string();

    // Prepend quote context for reply messages
    if let Some(ref quote) = quote_context {
        text = format!("> {quote}\n{text}");
    }

    if text.is_empty() {
        return None;
    }

    let sender_id = activity.from.as_ref()?.id.clone();

    Some(InboundMessage {
        sender_id,
        text,
        channel_id: activity.conversation.as_ref().map(|c| c.id.clone()),
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

/// Parse a Bot Framework `messageReaction` activity into an `InboundMessage`.
///
/// Reactions don't carry text; we encode the reaction event as
/// `InboundMessage.reaction = Some("+like")` (added) or `Some("-like")`
/// (removed). Bot self-reactions are skipped. `text` is left empty so
/// downstream handlers can branch on `reaction.is_some()` without parsing.
fn parse_message_reaction(activity: &Activity) -> Option<InboundMessage> {
    if let (Some(from), Some(recipient)) = (&activity.from, &activity.recipient) {
        if from.id == recipient.id {
            return None;
        }
    }

    let added: Option<&str> = activity
        .reactions_added
        .as_ref()
        .and_then(|v| v.first())
        .map(|r| r.reaction_type.as_str());
    let removed: Option<&str> = activity
        .reactions_removed
        .as_ref()
        .and_then(|v| v.first())
        .map(|r| r.reaction_type.as_str());

    let reaction = match (added, removed) {
        (Some(a), _) => format!("+{a}"),
        (None, Some(r)) => format!("-{r}"),
        (None, None) => return None,
    };

    let sender_id = activity.from.as_ref()?.id.clone();

    Some(InboundMessage {
        sender_id,
        text: String::new(),
        channel_id: activity.conversation.as_ref().map(|c| c.id.clone()),
        thread_id: activity.reply_to_id.clone(),
        message_id: Some(activity.id.clone()),
        thread_ts: activity.reply_to_id.clone(),
        attachments: Vec::new(),
        reaction: Some(reaction),
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

/// Check if a conversationUpdate activity indicates the bot was added.
pub fn is_bot_added(activity: &Activity) -> bool {
    if activity.activity_type != "conversationUpdate" {
        return false;
    }
    let bot_id = match activity.recipient.as_ref() {
        Some(r) => r.id.as_str(),
        None => return false,
    };
    activity
        .members_added
        .as_ref()
        .map(|members| members.iter().any(|m| m.id == bot_id))
        .unwrap_or(false)
}

/// Process `<at>...</at>` mention tags in Teams message text.
///
/// Bot mentions (matched via entity data) are stripped entirely.
/// Non-bot mentions are replaced with `@Name` to preserve context.
/// When no entities are provided, all mentions are stripped (conservative fallback).
fn resolve_mention_tags(
    text: &str,
    entities: Option<&[super::types::Entity]>,
    bot_id: Option<&str>,
) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re =
        RE.get_or_init(|| Regex::new(r"<at>([^<]*)</at>\s*").unwrap_or_else(|_| unreachable!()));

    let entities = match entities {
        Some(e) => e,
        None => return re.replace_all(text, "").to_string(),
    };

    // Collect bot mention names from entities
    let bot_names: Vec<&str> = entities
        .iter()
        .filter(|e| e.entity_type == "mention")
        .filter(|e| {
            e.mentioned
                .as_ref()
                .map(|m| bot_id.is_some_and(|bid| m.id == bid))
                .unwrap_or(false)
        })
        .filter_map(|e| e.mentioned.as_ref()?.name.as_deref())
        .collect();

    re.replace_all(text, |caps: &regex::Captures| {
        let name = &caps[1];
        if bot_names.contains(&name) {
            String::new()
        } else {
            format!("@{name} ")
        }
    })
    .to_string()
}

/// Extract quoted text from Teams HTML message content.
///
/// Teams wraps quoted content in `<blockquote>...</blockquote>` tags.
fn extract_quote_text(text: &str) -> Option<String> {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?s)<blockquote[^>]*>(.*?)</blockquote>").unwrap_or_else(|_| unreachable!())
    });

    re.captures(text)
        .map(|caps| strip_html_tags(&caps[1]).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Minimal HTML tag stripping for extracting plain text from Teams content.
fn strip_html_tags(html: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"<[^>]+>").unwrap_or_else(|_| unreachable!()));
    re.replace_all(html, "").to_string()
}

#[cfg(test)]
mod tests {
    use super::super::types::{ChannelAccount, ConversationAccount, Entity};
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
            value: None,
            reactions_added: None,
            reactions_removed: None,
        }
    }

    // ── messageReaction parsing ──

    #[test]
    fn message_reaction_added_populates_reaction_field() {
        use super::super::types::MessageReaction;
        let mut activity = make_activity();
        activity.activity_type = "messageReaction".to_string();
        activity.text = None;
        activity.reply_to_id = Some("parent-msg-7".to_string());
        activity.reactions_added = Some(vec![MessageReaction {
            reaction_type: "like".to_string(),
        }]);
        let msg = parse_activity(&activity).expect("messageReaction parses");
        assert_eq!(msg.reaction.as_deref(), Some("+like"));
        assert_eq!(msg.thread_id.as_deref(), Some("parent-msg-7"));
        assert!(msg.text.is_empty());
    }

    #[test]
    fn message_reaction_removed_uses_minus_prefix() {
        use super::super::types::MessageReaction;
        let mut activity = make_activity();
        activity.activity_type = "messageReaction".to_string();
        activity.text = None;
        activity.reactions_removed = Some(vec![MessageReaction {
            reaction_type: "heart".to_string(),
        }]);
        let msg = parse_activity(&activity).expect("messageReaction parses");
        assert_eq!(msg.reaction.as_deref(), Some("-heart"));
    }

    #[test]
    fn message_reaction_with_no_added_or_removed_returns_none() {
        let mut activity = make_activity();
        activity.activity_type = "messageReaction".to_string();
        activity.text = None;
        // both added and removed are None
        assert!(parse_activity(&activity).is_none());
    }

    #[test]
    fn message_reaction_from_self_skipped() {
        use super::super::types::MessageReaction;
        let mut activity = make_activity();
        activity.activity_type = "messageReaction".to_string();
        activity.text = None;
        activity.from = Some(ChannelAccount {
            id: "bot-1".to_string(), // matches recipient.id
            name: Some("MyBot".to_string()),
        });
        activity.reactions_added = Some(vec![MessageReaction {
            reaction_type: "like".to_string(),
        }]);
        assert!(parse_activity(&activity).is_none());
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
        // No entities provided — all mentions stripped (conservative fallback)
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

    // ── Mention entity tests ──

    #[test]
    fn bot_mention_stripped_with_entities() {
        let mut activity = make_activity();
        activity.text = Some("<at>MyBot</at> help me".to_string());
        activity.entities = Some(vec![Entity {
            entity_type: "mention".to_string(),
            mentioned: Some(ChannelAccount {
                id: "bot-1".to_string(),
                name: Some("MyBot".to_string()),
            }),
        }]);
        let msg = parse_activity(&activity).unwrap();
        assert_eq!(msg.text, "help me");
    }

    #[test]
    fn user_mention_preserved_as_at_name() {
        let mut activity = make_activity();
        activity.text = Some("<at>Alice</at> what do you think?".to_string());
        activity.entities = Some(vec![Entity {
            entity_type: "mention".to_string(),
            mentioned: Some(ChannelAccount {
                id: "user-2".to_string(),
                name: Some("Alice".to_string()),
            }),
        }]);
        let msg = parse_activity(&activity).unwrap();
        assert!(msg.text.contains("@Alice"));
        assert!(msg.text.contains("what do you think?"));
    }

    #[test]
    fn mixed_bot_and_user_mentions() {
        let mut activity = make_activity();
        activity.text = Some("<at>MyBot</at> ask <at>Alice</at> about it".to_string());
        activity.entities = Some(vec![
            Entity {
                entity_type: "mention".to_string(),
                mentioned: Some(ChannelAccount {
                    id: "bot-1".to_string(),
                    name: Some("MyBot".to_string()),
                }),
            },
            Entity {
                entity_type: "mention".to_string(),
                mentioned: Some(ChannelAccount {
                    id: "user-2".to_string(),
                    name: Some("Alice".to_string()),
                }),
            },
        ]);
        let msg = parse_activity(&activity).unwrap();
        assert!(!msg.text.contains("MyBot"));
        assert!(msg.text.contains("@Alice"));
        assert!(msg.text.contains("about it"));
    }

    #[test]
    fn no_entities_strips_all_mentions() {
        let mut activity = make_activity();
        activity.text = Some("<at>Someone</at> hello".to_string());
        activity.entities = None;
        let msg = parse_activity(&activity).unwrap();
        assert_eq!(msg.text, "hello");
    }

    // ── Quote extraction tests ──

    #[test]
    fn quote_extracted_from_blockquote() {
        assert_eq!(
            extract_quote_text("<blockquote>original text</blockquote>rest"),
            Some("original text".to_string())
        );
    }

    #[test]
    fn quote_with_inner_html_stripped() {
        assert_eq!(
            extract_quote_text("<blockquote><p>quoted</p></blockquote>"),
            Some("quoted".to_string())
        );
    }

    #[test]
    fn no_blockquote_returns_none() {
        assert!(extract_quote_text("plain text message").is_none());
    }

    #[test]
    fn quote_prepended_to_reply() {
        let mut activity = make_activity();
        activity.reply_to_id = Some("parent-1".to_string());
        activity.text = Some("<blockquote>quoted content</blockquote>reply text".to_string());
        let msg = parse_activity(&activity).unwrap();
        assert!(msg.text.starts_with("> quoted content\n"));
        assert!(msg.text.contains("reply text"));
    }

    // ── is_bot_added tests ──

    #[test]
    fn is_bot_added_true() {
        let mut activity = make_activity();
        activity.activity_type = "conversationUpdate".to_string();
        activity.members_added = Some(vec![ChannelAccount {
            id: "bot-1".to_string(),
            name: Some("MyBot".to_string()),
        }]);
        assert!(is_bot_added(&activity));
    }

    #[test]
    fn is_bot_added_false_user_added() {
        let mut activity = make_activity();
        activity.activity_type = "conversationUpdate".to_string();
        activity.members_added = Some(vec![ChannelAccount {
            id: "user-new".to_string(),
            name: Some("NewUser".to_string()),
        }]);
        assert!(!is_bot_added(&activity));
    }

    #[test]
    fn is_bot_added_wrong_type() {
        let activity = make_activity();
        assert!(!is_bot_added(&activity));
    }

    #[test]
    fn is_bot_added_no_recipient() {
        let mut activity = make_activity();
        activity.activity_type = "conversationUpdate".to_string();
        activity.recipient = None;
        activity.members_added = Some(vec![ChannelAccount {
            id: "bot-1".to_string(),
            name: Some("MyBot".to_string()),
        }]);
        assert!(!is_bot_added(&activity));
    }

    // ── Invoke activity tests ──

    #[test]
    fn invoke_activity_parsed() {
        let mut activity = make_activity();
        activity.activity_type = "invoke".to_string();
        activity.text = None;
        activity.value = Some(serde_json::json!({"text": "button clicked"}));
        let msg = parse_activity(&activity).unwrap();
        assert_eq!(msg.text, "button clicked");
    }

    #[test]
    fn invoke_activity_action_fallback() {
        let mut activity = make_activity();
        activity.activity_type = "invoke".to_string();
        activity.text = None;
        activity.value = Some(serde_json::json!({"action": "submit"}));
        let msg = parse_activity(&activity).unwrap();
        assert_eq!(msg.text, "submit");
    }

    #[test]
    fn invoke_activity_no_value_returns_none() {
        let mut activity = make_activity();
        activity.activity_type = "invoke".to_string();
        activity.text = None;
        activity.value = None;
        assert!(parse_activity(&activity).is_none());
    }
}
