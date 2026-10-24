//! The channel tap: copy a channel message to the subscriptions that want it.
//!
//! Called on the reply path, so it must not slow it down. The check is one
//! read of an in-memory set; only when some active subscription listens on
//! the channel is a task spawned to match and store the event. The normal
//! reply to the message is unchanged either way.
//!
//! The tap sees what the channel adapter lets through: Slack's allowed
//! channels and DMs, Telegram's approved senders, and so on. Adapters already
//! drop Borg's own messages (Slack's echo guard), which is what keeps a
//! subscription that replies into a channel from waking itself.

use cortex::config::Config;
use cortex::subscriptions::ingest::{fan_out, FanoutEvent};
use cortex::subscriptions::{wake, EventOrigin, Source};
use serde_json::json;

use crate::handler::InboundMessage;

/// Copy `inbound` to matching channel subscriptions, in the background.
pub fn on_channel_message(
    channel: &str,
    bot_mention: Option<&str>,
    inbound: &InboundMessage,
    session_key: &str,
    config: &Config,
) {
    if !config.subscriptions.enabled || !wake::is_channel_tapped(channel) {
        return;
    }
    let payload = channel_payload(bot_mention, inbound);
    let origin = EventOrigin {
        channel: channel.to_string(),
        sender: inbound.sender_id.clone(),
        thread_id: inbound
            .thread_ts
            .clone()
            .or_else(|| inbound.thread_id.clone())
            .or_else(|| inbound.message_id.clone()),
        session_key: Some(session_key.to_string()),
    };
    let channel = channel.to_string();
    let cfg = config.subscriptions.clone();
    tokio::spawn(async move {
        let db = match cortex::db::Database::open_with_timeout(
            cortex::db::Database::GATEWAY_BUSY_TIMEOUT_MS,
        )
        .await
        {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!(%channel, "subscription tap: open db: {e:#}");
                return;
            }
        };
        let ev = FanoutEvent {
            source: Source::Channel,
            provider: &channel,
            payload: &payload,
            origin: Some(&origin),
            born_in_session: None,
        };
        if let Err(e) = fan_out(&db, &ev, &cfg).await {
            tracing::warn!(%channel, "subscription tap: store event: {e:#}");
        }
    });
}

/// The payload shape the channel presets read.
pub fn channel_payload(bot_mention: Option<&str>, inbound: &InboundMessage) -> serde_json::Value {
    let mentioned = bot_mention
        .is_some_and(|m| !m.is_empty() && inbound.text.to_lowercase().contains(&m.to_lowercase()));
    let kind = if inbound.reaction.is_some() {
        "reaction"
    } else if mentioned {
        "mention"
    } else {
        "message"
    };
    json!({
        "kind": kind,
        "text": inbound.text,
        "channel_id": inbound.channel_id,
        "sender": inbound.sender_id,
        "thread_id": inbound.thread_ts.as_ref().or(inbound.thread_id.as_ref()),
        "message_id": inbound.message_id,
        "reaction": inbound.reaction,
        "mentioned": mentioned,
        "peer_kind": inbound.peer_kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inbound(text: &str, reaction: Option<&str>) -> InboundMessage {
        serde_json::from_value(json!({
            "sender_id": "U1",
            "text": text,
            "channel_id": "C1",
            "message_id": "1.2",
            "thread_ts": "1.0",
            "reaction": reaction,
        }))
        .unwrap()
    }

    #[test]
    fn payload_marks_mentions_and_reactions() {
        let p = channel_payload(Some("<@UBORG>"), &inbound("hey <@uborg> look", None));
        assert_eq!(p["kind"], "mention");
        assert_eq!(p["mentioned"], true);
        assert_eq!(p["thread_id"], "1.0");

        let p = channel_payload(Some("<@UBORG>"), &inbound("plain", None));
        assert_eq!(p["kind"], "message");
        assert_eq!(p["mentioned"], false);

        let p = channel_payload(None, &inbound("", Some("eyes")));
        assert_eq!(p["kind"], "reaction");
        assert_eq!(p["reaction"], "eyes");
    }
}
