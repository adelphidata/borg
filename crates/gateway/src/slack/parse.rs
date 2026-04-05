use tokio::sync::Mutex;

use super::echo::EchoCache;
use super::types::EventCallback;
use crate::constants::{PEER_KIND_DIRECT, PEER_KIND_GROUP};
use crate::handler::{InboundAttachment, InboundMessage};

/// Remove bot @mention tokens from Slack message text.
///
/// Slack encodes user mentions as `<@U0BOT>` or `<@U0BOT|display-name>`. When the
/// bot is mentioned in a channel, the raw token arrives in the inbound text and
/// pollutes the agent's prompt. This strips every occurrence of the bot's own
/// mention (including the optional `|name` display form) and trims surrounding
/// whitespace. Mentions of other users are left intact.
pub(super) fn strip_bot_mention(text: &str, bot_user_id: &str) -> String {
    if bot_user_id.is_empty() || text.is_empty() {
        return text.trim().to_string();
    }

    let prefix = format!("<@{bot_user_id}");
    let mut out = String::with_capacity(text.len());
    let mut remaining = text;

    while let Some(idx) = remaining.find(&prefix) {
        out.push_str(&remaining[..idx]);
        let after_prefix = &remaining[idx + prefix.len()..];
        // The character after `<@BOTID` must be either `>` or `|` for this to be
        // the bot mention (and not a longer user id that happens to share a prefix).
        match after_prefix.chars().next() {
            Some('>') => {
                remaining = &after_prefix[1..];
            }
            Some('|') => {
                // Skip through the closing `>`
                if let Some(end) = after_prefix.find('>') {
                    remaining = &after_prefix[end + 1..];
                } else {
                    // Malformed — no closing `>`. Keep the rest verbatim and stop.
                    out.push_str(&remaining[idx..]);
                    return out.trim().to_string();
                }
            }
            _ => {
                // Not our mention (e.g. `<@U0BOTX>` where X makes it a different user).
                // Emit one char of the match and continue scanning after it.
                out.push_str(&remaining[idx..idx + 1]);
                remaining = &remaining[idx + 1..];
            }
        }
    }
    out.push_str(remaining);
    // Collapse any runs of whitespace created by the removal, then trim.
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
}

/// Parse a Slack `EventCallback` into an `InboundMessage`.
///
/// Returns `None` for:
/// - Bot messages (to avoid self-reply loops)
/// - Messages from the bot's own user ID (echo detection)
/// - Messages matching recently sent text (echo cache)
/// - Non-text message subtypes (edits, deletes, joins, etc.)
/// - Events without text
/// - Unrecognized event types (not `message` or `app_mention`)
pub async fn parse_event(
    callback: &EventCallback,
    bot_user_id: Option<&str>,
    echo_cache: Option<&Mutex<EchoCache>>,
) -> Option<InboundMessage> {
    let event = &callback.event;

    // Skip bot messages to prevent loops
    if event.bot_id.is_some() {
        return None;
    }

    // Skip messages from the bot's own user ID (defense-in-depth)
    if let (Some(sender), Some(bot_id)) = (event.user.as_deref(), bot_user_id) {
        if sender == bot_id {
            return None;
        }
    }

    // Only handle message and app_mention events
    if event.event_type != "message" && event.event_type != "app_mention" {
        return None;
    }

    // Skip non-standard message subtypes.
    // NOTE: `file_share` is intentionally NOT filtered — it's the subtype Slack uses
    // when a user uploads a file (with or without a caption), and dropping it would
    // silently lose all user file uploads.
    if let Some(ref subtype) = event.subtype {
        match subtype.as_str() {
            "bot_message" | "message_changed" | "message_deleted" | "channel_join"
            | "channel_leave" => return None,
            _ => {}
        }
    }

    let sender_id = event.user.as_deref()?;

    // Extract file metadata as placeholder attachments (URLs only, downloaded later)
    let attachments = event
        .files
        .as_ref()
        .map(|files| {
            files
                .iter()
                .map(|f| InboundAttachment {
                    mime_type: f
                        .mimetype
                        .clone()
                        .unwrap_or_else(|| "application/octet-stream".to_string()),
                    data: f.url_private.clone().unwrap_or_default(),
                    filename: f.name.clone(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // Normalize text: strip bot @mention tokens (e.g. `<@U0BOT>` or `<@U0BOT|name>`)
    // so they don't pollute the agent's prompt. An @mention-only message with no
    // remaining content is still valid if attachments are present.
    let raw_text = event.text.as_deref().unwrap_or("");
    let text = match bot_user_id {
        Some(bot_id) => strip_bot_mention(raw_text, bot_id),
        None => raw_text.to_string(),
    };

    // A message is valid if it has either non-empty text OR at least one attachment.
    // This allows pure file uploads (file_share with empty text) to flow through
    // and allows bot-mention-only messages when files are attached.
    if text.is_empty() && attachments.is_empty() {
        return None;
    }

    // Check echo cache — skip if this text matches a recently sent outbound message.
    // Only meaningful for non-empty text; attachment-only messages bypass the cache.
    if !text.is_empty() {
        if let Some(cache) = echo_cache {
            if cache.lock().await.is_echo(&text) {
                return None;
            }
        }
    }

    let peer_kind = match event.channel_type.as_deref() {
        Some("im") => Some(PEER_KIND_DIRECT.to_string()),
        Some("channel") | Some("group") | Some("mpim") => Some(PEER_KIND_GROUP.to_string()),
        _ => None,
    };

    Some(InboundMessage {
        sender_id: sender_id.to_string(),
        text,
        channel_id: event.channel.clone(),
        // Populate thread_id from thread_ts so the gateway handler isolates
        // per-thread conversation history. `thread_ts` is Slack's parent-message
        // timestamp and is None for top-level (non-threaded) messages.
        thread_id: event.thread_ts.clone(),
        message_id: event.ts.clone(),
        thread_ts: event.thread_ts.clone(),
        attachments,
        reaction: None,
        metadata: serde_json::json!({
            "event_type": event.event_type,
        }),
        peer_kind,
    })
}

#[cfg(test)]
mod tests {
    use super::super::types::SlackEvent;
    use super::*;
    use std::sync::Arc;

    fn make_event(event: SlackEvent) -> EventCallback {
        EventCallback {
            token: Some("tok".into()),
            team_id: Some("T123".into()),
            event_id: Some("Ev123".into()),
            event,
        }
    }

    fn make_slack_event(event_type: &str) -> SlackEvent {
        SlackEvent {
            event_type: event_type.to_string(),
            subtype: None,
            user: Some("U456".to_string()),
            text: Some("hello".to_string()),
            ts: Some("1234567890.123456".to_string()),
            thread_ts: None,
            channel: Some("C789".to_string()),
            channel_type: Some("channel".to_string()),
            bot_id: None,
            files: None,
        }
    }

    #[tokio::test]
    async fn parse_message_event() {
        let cb = make_event(make_slack_event("message"));
        let msg = parse_event(&cb, None, None).await.unwrap();
        assert_eq!(msg.sender_id, "U456");
        assert_eq!(msg.text, "hello");
        assert_eq!(msg.channel_id.as_deref(), Some("C789"));
        assert_eq!(msg.message_id.as_deref(), Some("1234567890.123456"));
        assert!(msg.thread_ts.is_none());
    }

    #[tokio::test]
    async fn parse_app_mention_event() {
        let cb = make_event(make_slack_event("app_mention"));
        let msg = parse_event(&cb, None, None).await.unwrap();
        assert_eq!(msg.sender_id, "U456");
        assert_eq!(msg.text, "hello");
    }

    #[tokio::test]
    async fn parse_threaded_message() {
        let mut event = make_slack_event("message");
        event.thread_ts = Some("1234567890.000001".to_string());
        let cb = make_event(event);
        let msg = parse_event(&cb, None, None).await.unwrap();
        assert_eq!(msg.thread_ts.as_deref(), Some("1234567890.000001"));
        // thread_id mirrors thread_ts so the gateway handler isolates per-thread sessions.
        assert_eq!(msg.thread_id.as_deref(), Some("1234567890.000001"));
    }

    #[tokio::test]
    async fn parse_non_threaded_message_has_no_thread_id() {
        let cb = make_event(make_slack_event("message"));
        let msg = parse_event(&cb, None, None).await.unwrap();
        assert!(msg.thread_id.is_none());
    }

    #[tokio::test]
    async fn bot_message_returns_none() {
        let mut event = make_slack_event("message");
        event.bot_id = Some("B123".to_string());
        let cb = make_event(event);
        assert!(parse_event(&cb, None, None).await.is_none());
    }

    #[tokio::test]
    async fn bot_user_id_returns_none() {
        let cb = make_event(make_slack_event("message"));
        // sender U456 matches bot_user_id
        assert!(parse_event(&cb, Some("U456"), None).await.is_none());
    }

    #[tokio::test]
    async fn different_bot_user_id_parses() {
        let cb = make_event(make_slack_event("message"));
        let msg = parse_event(&cb, Some("U999"), None).await.unwrap();
        assert_eq!(msg.sender_id, "U456");
    }

    #[tokio::test]
    async fn echo_cache_filters_echo() {
        let cache = Arc::new(Mutex::new(EchoCache::new()));
        cache.lock().await.remember("hello");

        let cb = make_event(make_slack_event("message"));
        assert!(parse_event(&cb, None, Some(&cache)).await.is_none());
    }

    #[tokio::test]
    async fn echo_cache_allows_non_echo() {
        let cache = Arc::new(Mutex::new(EchoCache::new()));
        cache.lock().await.remember("something else");

        let cb = make_event(make_slack_event("message"));
        let msg = parse_event(&cb, None, Some(&cache)).await.unwrap();
        assert_eq!(msg.text, "hello");
    }

    #[tokio::test]
    async fn no_text_returns_none() {
        let mut event = make_slack_event("message");
        event.text = None;
        let cb = make_event(event);
        assert!(parse_event(&cb, None, None).await.is_none());
    }

    #[tokio::test]
    async fn empty_text_returns_none() {
        let mut event = make_slack_event("message");
        event.text = Some(String::new());
        let cb = make_event(event);
        assert!(parse_event(&cb, None, None).await.is_none());
    }

    #[tokio::test]
    async fn no_user_returns_none() {
        let mut event = make_slack_event("message");
        event.user = None;
        let cb = make_event(event);
        assert!(parse_event(&cb, None, None).await.is_none());
    }

    #[tokio::test]
    async fn unknown_event_type_returns_none() {
        let cb = make_event(make_slack_event("reaction_added"));
        assert!(parse_event(&cb, None, None).await.is_none());
    }

    #[tokio::test]
    async fn message_changed_subtype_returns_none() {
        let mut event = make_slack_event("message");
        event.subtype = Some("message_changed".to_string());
        let cb = make_event(event);
        assert!(parse_event(&cb, None, None).await.is_none());
    }

    #[tokio::test]
    async fn bot_message_subtype_returns_none() {
        let mut event = make_slack_event("message");
        event.subtype = Some("bot_message".to_string());
        let cb = make_event(event);
        assert!(parse_event(&cb, None, None).await.is_none());
    }

    #[tokio::test]
    async fn channel_join_subtype_returns_none() {
        let mut event = make_slack_event("message");
        event.subtype = Some("channel_join".to_string());
        let cb = make_event(event);
        assert!(parse_event(&cb, None, None).await.is_none());
    }

    #[tokio::test]
    async fn channel_type_channel_is_group_peer_kind() {
        let cb = make_event(make_slack_event("message"));
        let msg = parse_event(&cb, None, None).await.unwrap();
        assert_eq!(msg.peer_kind.as_deref(), Some("group"));
    }

    #[tokio::test]
    async fn channel_type_im_is_direct_peer_kind() {
        let mut event = make_slack_event("message");
        event.channel_type = Some("im".to_string());
        let cb = make_event(event);
        let msg = parse_event(&cb, None, None).await.unwrap();
        assert_eq!(msg.peer_kind.as_deref(), Some("direct"));
    }

    #[tokio::test]
    async fn app_mention_stores_event_type_metadata() {
        let cb = make_event(make_slack_event("app_mention"));
        let msg = parse_event(&cb, None, None).await.unwrap();
        assert_eq!(msg.metadata["event_type"], "app_mention");
    }

    #[tokio::test]
    async fn missing_channel_still_parses() {
        let mut event = make_slack_event("message");
        event.channel = None;
        let cb = make_event(event);
        let msg = parse_event(&cb, None, None).await.unwrap();
        assert!(msg.channel_id.is_none());
    }

    #[tokio::test]
    async fn parse_event_with_files() {
        use super::super::types::SlackFile;

        let mut event = make_slack_event("message");
        event.files = Some(vec![SlackFile {
            id: "F123".to_string(),
            name: Some("test.pdf".to_string()),
            mimetype: Some("application/pdf".to_string()),
            url_private: Some("https://files.slack.com/files-pri/T123-F123/test.pdf".to_string()),
            size: Some(1024),
            filetype: Some("pdf".to_string()),
        }]);
        let cb = make_event(event);
        let msg = parse_event(&cb, None, None).await.unwrap();
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].mime_type, "application/pdf");
        assert_eq!(msg.attachments[0].filename.as_deref(), Some("test.pdf"));
    }

    // --- file_share subtype regression tests ---

    fn slack_file(name: &str, mime: &str) -> super::super::types::SlackFile {
        super::super::types::SlackFile {
            id: "F123".to_string(),
            name: Some(name.to_string()),
            mimetype: Some(mime.to_string()),
            url_private: Some(format!(
                "https://files.slack.com/files-pri/T123-F123/{name}"
            )),
            size: Some(1024),
            filetype: Some("bin".to_string()),
        }
    }

    #[tokio::test]
    async fn file_share_subtype_with_caption_parses() {
        let mut event = make_slack_event("message");
        event.subtype = Some("file_share".to_string());
        event.text = Some("check this out".to_string());
        event.files = Some(vec![slack_file("report.pdf", "application/pdf")]);
        let cb = make_event(event);
        let msg = parse_event(&cb, None, None).await.unwrap();
        assert_eq!(msg.text, "check this out");
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].filename.as_deref(), Some("report.pdf"));
    }

    #[tokio::test]
    async fn file_share_subtype_with_empty_text_still_parses() {
        let mut event = make_slack_event("message");
        event.subtype = Some("file_share".to_string());
        event.text = Some(String::new());
        event.files = Some(vec![slack_file("pic.png", "image/png")]);
        let cb = make_event(event);
        let msg = parse_event(&cb, None, None).await.unwrap();
        assert!(msg.text.is_empty());
        assert_eq!(msg.attachments.len(), 1);
    }

    #[tokio::test]
    async fn file_share_subtype_with_no_files_and_no_text_returns_none() {
        let mut event = make_slack_event("message");
        event.subtype = Some("file_share".to_string());
        event.text = Some(String::new());
        event.files = None;
        let cb = make_event(event);
        assert!(parse_event(&cb, None, None).await.is_none());
    }

    // --- bot mention stripping tests ---

    #[test]
    fn strip_bot_mention_no_mention() {
        assert_eq!(strip_bot_mention("hello world", "U0BOT"), "hello world");
    }

    #[test]
    fn strip_bot_mention_simple() {
        assert_eq!(strip_bot_mention("<@U0BOT> hello", "U0BOT"), "hello");
    }

    #[test]
    fn strip_bot_mention_display_form() {
        assert_eq!(strip_bot_mention("<@U0BOT|borg> hello", "U0BOT"), "hello");
    }

    #[test]
    fn strip_bot_mention_at_end() {
        assert_eq!(strip_bot_mention("hey <@U0BOT>", "U0BOT"), "hey");
    }

    #[test]
    fn strip_bot_mention_multiple() {
        assert_eq!(
            strip_bot_mention("<@U0BOT> and <@U0BOT>!", "U0BOT"),
            "and !"
        );
    }

    #[test]
    fn strip_bot_mention_mention_only() {
        assert_eq!(strip_bot_mention("<@U0BOT>", "U0BOT"), "");
    }

    #[test]
    fn strip_bot_mention_leaves_other_users_intact() {
        assert_eq!(
            strip_bot_mention("<@U0BOT> ping <@U0OTHER>", "U0BOT"),
            "ping <@U0OTHER>"
        );
    }

    #[test]
    fn strip_bot_mention_no_false_match_on_prefix_collision() {
        // `<@U0BOTX>` must NOT be stripped when the bot id is `U0BOT`.
        assert_eq!(
            strip_bot_mention("hello <@U0BOTX>", "U0BOT"),
            "hello <@U0BOTX>"
        );
    }

    #[test]
    fn strip_bot_mention_empty_bot_id_noop() {
        assert_eq!(strip_bot_mention("  hello  ", ""), "hello");
    }

    #[tokio::test]
    async fn parse_strips_bot_mention_when_bot_user_id_set() {
        let mut event = make_slack_event("app_mention");
        event.text = Some("<@U0BOT> summarize this".to_string());
        let cb = make_event(event);
        let msg = parse_event(&cb, Some("U0BOT"), None).await.unwrap();
        assert_eq!(msg.text, "summarize this");
    }

    #[tokio::test]
    async fn parse_mention_only_with_attachment_still_parses() {
        let mut event = make_slack_event("message");
        event.subtype = Some("file_share".to_string());
        event.text = Some("<@U0BOT>".to_string());
        event.files = Some(vec![slack_file("screenshot.png", "image/png")]);
        let cb = make_event(event);
        let msg = parse_event(&cb, Some("U0BOT"), None).await.unwrap();
        assert!(msg.text.is_empty());
        assert_eq!(msg.attachments.len(), 1);
    }

    #[tokio::test]
    async fn parse_mention_only_no_attachment_returns_none() {
        let mut event = make_slack_event("message");
        event.text = Some("<@U0BOT>".to_string());
        let cb = make_event(event);
        assert!(parse_event(&cb, Some("U0BOT"), None).await.is_none());
    }
}
