//! Subscriptions: outside events that wake the agent.
//!
//! A subscription is *source* + *filter* + *instruction* + *delivery*. A
//! producer (the `/v1/events/{id}` endpoint, the channel tap, Borg's own goal
//! and task buses) inserts a `pending` row into `subscription_events` and
//! [`wake::poke`]s the dispatcher in `borgd`. The table is the queue, so a
//! restart or a slow consumer never loses an event; the dispatcher claims
//! rows, runs one tool-using turn in the subscription's own session
//! (`subscription:{id}`), and records the outcome.
//!
//! Storm control lives in the insert path
//! ([`crate::db::Database::insert_subscription_event`]) and the claim path,
//! not in memory: a 9 000-event burst becomes a few hundred rows, a few digest
//! runs, and one self-pause.

pub mod filter;
pub mod ingest;
pub mod manage;
pub mod presets;
pub mod prompt;
pub mod wake;

use serde::{Deserialize, Serialize};

/// Session-key prefix of every subscription's own session.
pub const SESSION_PREFIX: &str = "subscription:";

/// The session a subscription's runs share.
pub fn session_key(subscription_id: &str) -> String {
    format!("{SESSION_PREFIX}{subscription_id}")
}

/// Whether `session_key` is a subscription session. Events born inside one
/// are never fed back into a subscription, which is what keeps a run from
/// waking itself.
pub fn is_subscription_session(session_key: &str) -> bool {
    session_key.starts_with(SESSION_PREFIX)
}

/// Refuse a call that would let a subscription run set up more work that
/// wakes Borg again: scheduling a task, starting a goal, or changing a
/// subscription. Reading stays allowed.
///
/// `root_session` is the session at the root of the agent tree, so a
/// sub-agent a run spawned is held to the same rule. Without this, a run
/// that schedules a follow-up whose result it listens to is a loop that
/// only the run budget stops.
pub fn run_refusal(root_session: &str, tool: &str, args: &serde_json::Value) -> Option<String> {
    use crate::tool::names as tn;
    if !is_subscription_session(root_session) {
        return None;
    }
    let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let reads = matches!(action, "list" | "get" | "history" | "events" | "status");
    let refused = match tool {
        tn::SUBSCRIBE => !crate::tool_handlers::subscribe::is_read_action(args),
        tn::SCHEDULE | tn::MANAGE_TASKS | tn::MANAGE_CRON => !reads,
        tn::CREATE_GOAL => true,
        _ => false,
    };
    refused.then(|| {
        format!(
            "Error: `{tool}` cannot change anything inside a subscription run, because new work started here could wake this subscription again. Say in your reply what should be set up, and the user can do it."
        )
    })
}

/// Agent-loop steps a turn in `session_id` may take. A subscription run gets
/// the lower `subscriptions.max_steps`: nobody watches it, and a burst may
/// start many of them, so a confused run stops early instead of spending
/// the full interactive budget.
pub fn step_limit(config: &crate::config::Config, session_id: &str) -> usize {
    let interactive = config.conversation.max_iterations as usize;
    if is_subscription_session(session_id) {
        interactive.min(config.subscriptions.max_steps.max(1) as usize)
    } else {
        interactive
    }
}

/// Where a subscription's events come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// An outside service POSTs to the subscription's endpoint.
    Webhook,
    /// A message on a channel Borg is already connected to (Slack, …).
    Channel,
    /// An inbound-email service POSTs a parsed message to the endpoint.
    Email,
    /// Borg's own goals, scheduled tasks, heartbeat and sub-agents.
    Internal,
}

impl Source {
    /// Stored form.
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Webhook => "webhook",
            Source::Channel => "channel",
            Source::Email => "email",
            Source::Internal => "internal",
        }
    }

    /// Parse the stored form.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "webhook" => Some(Source::Webhook),
            "channel" => Some(Source::Channel),
            "email" => Some(Source::Email),
            "internal" => Some(Source::Internal),
            _ => None,
        }
    }

    /// Whether events arrive on the HTTP endpoint (and so need a key).
    pub fn uses_endpoint(self) -> bool {
        matches!(self, Source::Webhook | Source::Email)
    }
}

/// Subscription status values.
pub mod status {
    /// Events are accepted and run.
    pub const ACTIVE: &str = "active";
    /// The user paused it; the endpoint answers 409.
    pub const PAUSED: &str = "paused";
    /// It paused itself after a burst or a run of failures; only a
    /// deliberate resume clears it.
    pub const STORM: &str = "storm";
}

/// Event status values.
pub mod event_status {
    /// Waiting for the dispatcher.
    pub const PENDING: &str = "pending";
    /// Claimed into a run.
    pub const RUNNING: &str = "running";
    /// The run finished and reported.
    pub const SUCCEEDED: &str = "succeeded";
    /// The run failed or timed out.
    pub const FAILED: &str = "failed";
    /// Never ran: did not match the filter, or the queue was full.
    pub const SKIPPED: &str = "skipped";
    /// Ran and decided nothing needed attention (`[SILENT]`).
    pub const SILENT: &str = "silent";
}

/// Where a channel event came from, so "reply where it came from" can answer
/// in the same thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventOrigin {
    /// Channel name (`slack`, `telegram`, …).
    pub channel: String,
    /// Sender id on that channel.
    pub sender: String,
    /// The chat the message was in (a Slack or Discord channel id, a
    /// Telegram chat id). A reply goes here, so "reply where it came from"
    /// answers in the channel, not in the sender's direct messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<String>,
    /// Thread to reply in, when the channel has threads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// The gateway session key of the conversation the message belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
}

impl EventOrigin {
    /// The address a reply is sent to: the chat, else the sender.
    pub fn reply_address(&self) -> &str {
        self.chat
            .as_deref()
            .filter(|c| !c.is_empty())
            .unwrap_or(&self.sender)
    }
}

/// Prefix of a subscription id.
pub const ID_PREFIX: &str = "sub_";
/// Prefix of an event id.
pub const EVENT_ID_PREFIX: &str = "evt_";
/// Prefix of a run id.
pub const RUN_ID_PREFIX: &str = "run_";
/// Prefix of an endpoint key, so a leaked one is recognisable in a scan.
pub const KEY_PREFIX: &str = "bsk_";
/// Kind of an event queued by Test. It runs even while paused.
pub const TEST_KIND: &str = "test";

/// A short random id: `prefix` + 12 lowercase base32 characters.
pub fn new_id(prefix: &str) -> String {
    use rand::RngExt;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut rng = rand::rng();
    let tail: String = (0..12)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect();
    format!("{prefix}{tail}")
}

/// A fresh endpoint key: [`KEY_PREFIX`] + 64 hex characters (256 bits).
pub fn generate_key() -> String {
    use rand::RngExt;
    let mut buf = [0u8; 32];
    rand::rng().fill(&mut buf);
    format!("{KEY_PREFIX}{}", crate::util::hex::hex_encode(&buf))
}

/// Serialise `payload`, keeping at most `max_bytes`. Returns the stored text
/// and whether it was cut. A cut payload is stored as a small JSON object with
/// the head of the original, so the row stays valid JSON.
pub fn cap_payload(payload: &serde_json::Value, max_bytes: usize) -> (String, bool) {
    let text = serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string());
    if text.len() <= max_bytes {
        return (text, false);
    }
    // The head is re-escaped inside the wrapper, which can grow it up to six
    // times (a control character becomes `\u0000`), so shrink until it fits.
    let mut end = (max_bytes / 2).min(text.len());
    loop {
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let capped = serde_json::json!({
            "_truncated": true,
            "_original_bytes": text.len(),
            "_head": &text[..end],
        })
        .to_string();
        if capped.len() <= max_bytes || end == 0 {
            return (capped, true);
        }
        end /= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_run_may_read_its_schedules_but_not_start_more_work() {
        let run = "subscription:sub_abc";
        let cases = [
            (
                run,
                "schedule",
                json!({"action": "create", "type": "prompt"}),
                true,
            ),
            (run, "schedule", json!({"action": "list"}), false),
            (run, "manage_tasks", json!({"action": "resume"}), true),
            (run, "manage_tasks", json!({"action": "get"}), false),
            (run, "manage_cron", json!({"action": "update"}), true),
            (run, "create_goal", json!({"objective": "x"}), true),
            (run, "subscribe", json!({"action": "create"}), true),
            (run, "subscribe", json!({"action": "events"}), false),
            (run, "schedule", json!({}), true),
            (run, "read_file", json!({"path": "/x"}), false),
            ("chat-123", "schedule", json!({"action": "create"}), false),
            ("chat-123", "create_goal", json!({}), false),
        ];
        for (session, tool, args, refused) in cases {
            assert_eq!(
                run_refusal(session, tool, &args).is_some(),
                refused,
                "{session} {tool} {args}"
            );
        }
    }

    #[test]
    fn ids_and_keys_have_their_prefix_and_differ() {
        let a = new_id(ID_PREFIX);
        let b = new_id(ID_PREFIX);
        assert!(a.starts_with("sub_") && a.len() == 16, "{a}");
        assert_ne!(a, b);
        let k = generate_key();
        assert!(k.starts_with(KEY_PREFIX) && k.len() == KEY_PREFIX.len() + 64);
        assert_ne!(k, generate_key());
    }

    #[test]
    fn subscription_runs_get_the_lower_step_limit() {
        let mut config = crate::config::Config::default();
        config.conversation.max_iterations = 200;
        config.subscriptions.max_steps = 40;
        assert_eq!(step_limit(&config, &session_key("sub_x")), 40);
        assert_eq!(step_limit(&config, "U1:thread"), 200);
        config.conversation.max_iterations = 10;
        assert_eq!(
            step_limit(&config, &session_key("sub_x")),
            10,
            "never above the interactive limit"
        );
        config.subscriptions.max_steps = 0;
        assert_eq!(
            step_limit(&config, &session_key("sub_x")),
            1,
            "zero still allows one step"
        );
    }

    #[test]
    fn session_keys_round_trip_through_the_prefix_check() {
        assert!(is_subscription_session(&session_key("sub_x")));
        assert!(!is_subscription_session("heartbeat:scheduled"));
        assert!(!is_subscription_session("U123:thread"));
    }

    #[test]
    fn small_payloads_are_stored_whole_and_big_ones_are_cut_to_the_cap() {
        let small = serde_json::json!({"a": 1});
        assert_eq!(cap_payload(&small, 1024), ("{\"a\":1}".to_string(), false));

        let big = serde_json::json!({"blob": "x".repeat(10_000)});
        let (text, cut) = cap_payload(&big, 2048);
        assert!(cut);
        assert!(text.len() <= 2048, "stored {} bytes", text.len());
        let back: serde_json::Value = serde_json::from_str(&text).expect("still JSON");
        assert_eq!(back["_truncated"], true);
        assert!(back["_original_bytes"].as_u64().unwrap() > 10_000);
    }

    #[test]
    fn payloads_that_grow_when_escaped_still_fit_the_cap() {
        for blob in ["\"".repeat(10_000), "\u{1}".repeat(10_000)] {
            let (text, cut) = cap_payload(&serde_json::json!({ "blob": blob }), 2048);
            assert!(cut);
            assert!(text.len() <= 2048, "stored {} bytes", text.len());
            serde_json::from_str::<serde_json::Value>(&text).expect("still JSON");
        }
    }

    #[test]
    fn source_round_trips_through_its_stored_form() {
        for s in [
            Source::Webhook,
            Source::Channel,
            Source::Email,
            Source::Internal,
        ] {
            assert_eq!(Source::parse(s.as_str()), Some(s));
        }
        assert_eq!(Source::parse("cron"), None);
    }
}
