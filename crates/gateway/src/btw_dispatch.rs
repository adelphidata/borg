//! Ephemeral `/btw` dispatcher shared across native gateway channels.
//!
//! When an inbound message starts with `/btw `, native channels divert it
//! through this module instead of `handler::invoke_agent`. The result is
//! delivered as a threaded follow-up so the side question never lands in the
//! main session's `messages` rows and never derails the primary thread.
//!
//! The runner is injected so unit tests can exercise the dispatch flow
//! without spinning up a real LLM.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::RwLock;

use cortex::agent::run_btw;
use cortex::config::Config;
use cortex::db::{Database, MessageRow};
use cortex::types::{Message, MessageContent};

use crate::channel::channel_trait::NativeChannel;
use crate::handler::InboundMessage;
use crate::health::ChannelHealthRegistry;

/// JSON key used by per-channel delivery code to recognise that a response
/// should land as a threaded follow-up to the inbound message.
pub const BTW_THREAD_ANCHOR_KEY: &str = "btw_thread_anchor";

/// Usage hint returned when a user types bare `/btw` with no question.
pub const BTW_USAGE: &str =
    "Usage: /btw <question> — ask a side question without disturbing the main thread.";

/// Mark a channel response context as a `/btw` threaded reply. Per-channel
/// `send_response` implementations inspect this flag and translate it to the
/// platform's reply-threading primitive (Slack `thread_ts`, Telegram
/// `reply_to_message_id`, etc.).
pub fn mark_threaded(response_context: &mut serde_json::Value) {
    if let Some(obj) = response_context.as_object_mut() {
        obj.insert(
            BTW_THREAD_ANCHOR_KEY.to_string(),
            serde_json::Value::Bool(true),
        );
    }
}

/// Returns true if the response context is flagged as a `/btw` threaded reply.
pub fn is_threaded(response_context: &serde_json::Value) -> bool {
    response_context
        .get(BTW_THREAD_ANCHOR_KEY)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Visual marker prepended to `/btw` answers on channels that have no native
/// reply-threading primitive (Discord, Twilio SMS/WhatsApp). Lets the user
/// recognise the message as a side answer rather than a continuation of the
/// main thread.
pub const BTW_REPLY_PREFIX: &str = "> /btw reply:\n";

/// Prefix `text` with `BTW_REPLY_PREFIX` if `response_context` carries the
/// /btw threading flag. Channels without native threading call this from
/// their `send_response` path.
pub fn maybe_prefix_btw(text: &str, response_context: &serde_json::Value) -> String {
    if is_threaded(response_context) {
        format!("{BTW_REPLY_PREFIX}{text}")
    } else {
        text.to_string()
    }
}

/// Load a transcript snapshot for the channel session keyed by `(channel, sender)`.
///
/// Returns an empty Vec if no channel session exists yet, the DB cannot be
/// opened, or the session has no messages — `run_btw` works fine with an empty
/// snapshot, and a missing snapshot must not turn into a hard failure on the
/// user-facing side question.
pub async fn load_channel_snapshot(channel_name: &str, sender_id: &str) -> Vec<Message> {
    let db = match Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS).await {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!("/btw: failed to open DB for snapshot: {e:#}");
            return Vec::new();
        }
    };

    // We only want the existing session — never create a new one for /btw.
    let session_id = match db.lookup_channel_session(channel_name, sender_id).await {
        Ok(Some(id)) => id,
        Ok(None) => return Vec::new(),
        Err(e) => {
            tracing::warn!("/btw: failed to look up channel session: {e:#}");
            return Vec::new();
        }
    };

    match db.load_session_lineage_rows(&session_id).await {
        Ok(rows) => rows.iter().filter_map(message_row_to_message).collect(),
        Err(e) => {
            tracing::warn!("/btw: failed to load session messages: {e:#}");
            Vec::new()
        }
    }
}

/// Convert a stored DB row into a transcript `Message`. Tool messages and
/// rows with no plain-text content are dropped — `/btw` runs without tools,
/// so replaying tool exchanges into its prompt would only confuse the model.
fn message_row_to_message(row: &MessageRow) -> Option<Message> {
    let content = row.content.as_ref()?.clone();
    let role = match row.role.as_str() {
        "user" => cortex::types::Role::User,
        "assistant" => cortex::types::Role::Assistant,
        "system" => cortex::types::Role::System,
        _ => return None, // tool / unknown
    };
    Some(Message {
        role,
        content: Some(MessageContent::Text(content)),
        tool_calls: None,
        tool_call_id: None,
        timestamp: row.timestamp.clone(),
        seq: Some(row.id),
    })
}

/// Outcome of `dispatch_btw_message` — communicated back to the dispatcher so
/// it can surface a useful HTTP status if delivery fails. Today every variant
/// resolves to HTTP 200 (gateway never wants the platform to retry a
/// /btw-driven webhook), so the enum is informational.
#[derive(Debug)]
pub enum BtwOutcome {
    /// Reply delivered (or attempted) successfully.
    Delivered,
    /// User typed bare `/btw` — usage hint was sent.
    Usage,
}

/// Dispatch a `/btw` side question through the supplied channel. The reply is
/// sent directly via `channel.send_response` with the threading flag set; the
/// at-least-once delivery queue is intentionally bypassed so the side question
/// leaves no persistent trace.
///
/// `run` produces the answer text given `(question, transcript_snapshot)`. In
/// production this wraps `cortex::agent::run_btw`; tests inject a stub.
pub async fn dispatch_btw_message<F, Fut>(
    channel: &Arc<dyn NativeChannel>,
    inbound: &InboundMessage,
    question: &str,
    mut response_context: serde_json::Value,
    health: &Arc<RwLock<ChannelHealthRegistry>>,
    run: F,
) -> Result<BtwOutcome>
where
    F: FnOnce(String, Vec<Message>) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    mark_threaded(&mut response_context);

    if question.trim().is_empty() {
        channel
            .send_response(BTW_USAGE, &response_context, health)
            .await?;
        return Ok(BtwOutcome::Usage);
    }

    let channel_name = channel.names()[0].to_string();
    let snapshot = load_channel_snapshot(&channel_name, &inbound.sender_id).await;

    let response_text = match run(question.to_string(), snapshot).await {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => {
            tracing::warn!("/btw: empty response for sender {}", inbound.sender_id);
            "(/btw: no answer produced)".to_string()
        }
        Err(e) => {
            tracing::warn!("/btw: run failed: {e:#}");
            crate::handler::format_gateway_error(&e)
        }
    };

    channel
        .send_response(&response_text, &response_context, health)
        .await?;
    Ok(BtwOutcome::Delivered)
}

/// Production runner — calls `cortex::agent::run_btw` with a fresh
/// cancellation token. Pulled out so callers can reference it directly when
/// they don't need a custom runner (i.e. everywhere outside tests).
pub async fn run_btw_default(
    config: Config,
    question: String,
    snapshot: Vec<Message>,
) -> Result<String> {
    run_btw(
        config,
        question,
        snapshot,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axum::http::HeaderMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::channel::channel_trait::{WebhookContext, WebhookOutcome};

    struct RecordingChannel {
        send_count: AtomicU32,
        last_text: tokio::sync::Mutex<Option<String>>,
        last_ctx: tokio::sync::Mutex<Option<serde_json::Value>>,
    }

    impl RecordingChannel {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                send_count: AtomicU32::new(0),
                last_text: tokio::sync::Mutex::new(None),
                last_ctx: tokio::sync::Mutex::new(None),
            })
        }
    }

    fn as_dyn(c: &Arc<RecordingChannel>) -> Arc<dyn NativeChannel> {
        c.clone()
    }

    #[async_trait]
    impl NativeChannel for RecordingChannel {
        fn names(&self) -> Vec<&str> {
            vec!["recording"]
        }
        async fn handle_webhook(
            &self,
            _h: &HeaderMap,
            _b: &str,
            _c: &WebhookContext<'_>,
        ) -> Result<WebhookOutcome> {
            Ok(WebhookOutcome::Skip)
        }
        async fn send_response(
            &self,
            text: &str,
            ctx: &serde_json::Value,
            _h: &Arc<RwLock<ChannelHealthRegistry>>,
        ) -> Result<()> {
            self.send_count.fetch_add(1, Ordering::SeqCst);
            *self.last_text.lock().await = Some(text.to_string());
            *self.last_ctx.lock().await = Some(ctx.clone());
            Ok(())
        }
    }

    fn inbound(text: &str) -> InboundMessage {
        InboundMessage {
            sender_id: "u1".into(),
            text: text.into(),
            channel_id: Some("c1".into()),
            thread_id: None,
            message_id: Some("m1".into()),
            thread_ts: None,
            attachments: vec![],
            reaction: None,
            metadata: serde_json::Value::Null,
            peer_kind: Some("direct".into()),
        }
    }

    #[test]
    fn mark_threaded_sets_flag() {
        let mut ctx = serde_json::json!({"chat_id": "x"});
        mark_threaded(&mut ctx);
        assert!(is_threaded(&ctx));
        assert_eq!(ctx["chat_id"], "x");
    }

    #[test]
    fn is_threaded_default_false() {
        let ctx = serde_json::json!({"chat_id": "x"});
        assert!(!is_threaded(&ctx));
    }

    #[test]
    fn maybe_prefix_btw_only_when_flagged() {
        let mut ctx = serde_json::json!({});
        assert_eq!(maybe_prefix_btw("answer", &ctx), "answer");
        mark_threaded(&mut ctx);
        let prefixed = maybe_prefix_btw("answer", &ctx);
        assert!(prefixed.starts_with("> /btw reply:\n"));
        assert!(prefixed.ends_with("answer"));
    }

    #[test]
    fn mark_threaded_on_non_object_is_noop() {
        let mut ctx = serde_json::json!("not-an-object");
        mark_threaded(&mut ctx);
        assert!(!is_threaded(&ctx));
    }

    #[tokio::test]
    async fn dispatch_runs_injected_runner_and_threads_reply() {
        let recording = RecordingChannel::new();
        let channel = as_dyn(&recording);
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
        let inbound = inbound("/btw what's up?");
        let ctx = serde_json::json!({"chat_id": "c1"});

        let runner = |q: String, snap: Vec<Message>| async move {
            assert_eq!(q, "what's up?");
            assert!(
                snap.is_empty(),
                "no DB session in unit test → empty snapshot"
            );
            Ok::<_, anyhow::Error>("answer".to_string())
        };

        let out = dispatch_btw_message(&channel, &inbound, "what's up?", ctx, &health, runner)
            .await
            .expect("dispatch ok");
        assert!(matches!(out, BtwOutcome::Delivered));

        assert_eq!(recording.send_count.load(Ordering::SeqCst), 1);
        let text = recording.last_text.lock().await.clone().unwrap();
        assert_eq!(text, "answer");
        let sent_ctx = recording.last_ctx.lock().await.clone().unwrap();
        assert!(
            is_threaded(&sent_ctx),
            "delivery context must carry threading flag"
        );
        assert_eq!(sent_ctx["chat_id"], "c1");
    }

    #[tokio::test]
    async fn dispatch_empty_question_sends_usage_without_running() {
        let recording = RecordingChannel::new();
        let channel = as_dyn(&recording);
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
        let inbound = inbound("/btw");
        let ctx = serde_json::json!({"chat_id": "c1"});

        let ran = Arc::new(AtomicU32::new(0));
        let ran_inner = ran.clone();
        let runner = move |_q: String, _snap: Vec<Message>| {
            let ran_inner = ran_inner.clone();
            async move {
                ran_inner.fetch_add(1, Ordering::SeqCst);
                Ok::<_, anyhow::Error>("should-not-run".to_string())
            }
        };

        let out = dispatch_btw_message(&channel, &inbound, "", ctx, &health, runner)
            .await
            .expect("dispatch ok");
        assert!(matches!(out, BtwOutcome::Usage));
        assert_eq!(ran.load(Ordering::SeqCst), 0, "runner must not be invoked");
        assert_eq!(recording.send_count.load(Ordering::SeqCst), 1);
        let text = recording.last_text.lock().await.clone().unwrap();
        assert!(
            text.contains("Usage:"),
            "must surface usage hint, got: {text}"
        );
        let sent_ctx = recording.last_ctx.lock().await.clone().unwrap();
        assert!(is_threaded(&sent_ctx), "usage reply must also be threaded");
    }

    #[tokio::test]
    async fn dispatch_runner_error_falls_back_to_format_gateway_error() {
        let recording = RecordingChannel::new();
        let channel = as_dyn(&recording);
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
        let inbound = inbound("/btw boom");
        let ctx = serde_json::json!({"chat_id": "c1"});

        let runner = |_q: String, _snap: Vec<Message>| async move {
            Err::<String, _>(anyhow::anyhow!("provider down"))
        };

        let out = dispatch_btw_message(&channel, &inbound, "boom", ctx, &health, runner)
            .await
            .expect("dispatch ok");
        assert!(matches!(out, BtwOutcome::Delivered));

        assert_eq!(recording.send_count.load(Ordering::SeqCst), 1);
        let text = recording.last_text.lock().await.clone().unwrap();
        assert!(!text.is_empty(), "error must produce a user-facing message");
    }

    #[test]
    fn message_row_to_message_keeps_user_and_assistant_roles() {
        let row = MessageRow {
            id: 1,
            session_id: "s".into(),
            role: "assistant".into(),
            content: Some("hi".into()),
            content_parts_json: None,
            tool_calls_json: None,
            tool_call_id: None,
            timestamp: Some("2026-01-01T00:00:00Z".into()),
            created_at: 0,
            ..Default::default()
        };
        let m = message_row_to_message(&row).expect("assistant row converts");
        assert!(matches!(m.role, cortex::types::Role::Assistant));
        match m.content {
            Some(MessageContent::Text(t)) => assert_eq!(t, "hi"),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn message_row_to_message_drops_tool_role() {
        let row = MessageRow {
            id: 1,
            session_id: "s".into(),
            role: "tool".into(),
            content: Some("tool output".into()),
            content_parts_json: None,
            tool_calls_json: None,
            tool_call_id: Some("call_1".into()),
            timestamp: None,
            created_at: 0,
            ..Default::default()
        };
        assert!(message_row_to_message(&row).is_none());
    }

    #[test]
    fn message_row_to_message_drops_empty_content() {
        let row = MessageRow {
            id: 1,
            session_id: "s".into(),
            role: "user".into(),
            content: None,
            content_parts_json: None,
            tool_calls_json: None,
            tool_call_id: None,
            timestamp: None,
            created_at: 0,
            ..Default::default()
        };
        assert!(message_row_to_message(&row).is_none());
    }
}
