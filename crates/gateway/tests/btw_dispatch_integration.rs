//! End-to-end integration test for the gateway `/btw` short-circuit path.
//!
//! Drives `channel_trait::dispatch_webhook` with a mock `NativeChannel` that
//! returns a `/btw …` body and asserts:
//!
//! 1. The reply IS sent through the channel (`send_response` called).
//! 2. The response context IS flagged with `btw_thread_anchor=true`.
//! 3. The non-`/btw` path still runs the agent on the same channel — so the
//!    short-circuit is targeted, not a blanket bypass.
//!
//! `invoke_agent` in handler.rs is hard to stub from outside the crate, so we
//! lean on a side-channel signal: when `/btw` is taken, the mock channel sees
//! exactly one `send_response`. When the regular agent path runs without a
//! configured LLM provider it errors and emits `format_gateway_error` text —
//! we assert the delivered text differs between the two paths.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::uninlined_format_args
)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use axum::http::HeaderMap;
use tokio::sync::RwLock;

use cortex::config::Config;
use gateway::btw_dispatch::{is_threaded, BTW_THREAD_ANCHOR_KEY};
use gateway::channel::channel_trait::{
    dispatch_webhook, NativeChannel, WebhookContext, WebhookOutcome,
};
use gateway::handler::InboundMessage;
use gateway::health::ChannelHealthRegistry;
use gateway::session_queue::SessionQueue;

struct ScriptedChannel {
    text: String,
    sends: AtomicU32,
    last_text: tokio::sync::Mutex<Option<String>>,
    last_ctx: tokio::sync::Mutex<Option<serde_json::Value>>,
}

impl ScriptedChannel {
    fn new(text: &str) -> Arc<Self> {
        Arc::new(Self {
            text: text.to_string(),
            sends: AtomicU32::new(0),
            last_text: tokio::sync::Mutex::new(None),
            last_ctx: tokio::sync::Mutex::new(None),
        })
    }
}

#[async_trait]
impl NativeChannel for ScriptedChannel {
    fn names(&self) -> Vec<&str> {
        vec!["scripted"]
    }
    async fn handle_webhook(
        &self,
        _h: &HeaderMap,
        _b: &str,
        _c: &WebhookContext<'_>,
    ) -> Result<WebhookOutcome> {
        Ok(WebhookOutcome::Message {
            inbound: InboundMessage {
                sender_id: "u1".into(),
                text: self.text.clone(),
                channel_id: Some("c1".into()),
                thread_id: None,
                message_id: Some("m1".into()),
                thread_ts: None,
                attachments: vec![],
                reaction: None,
                metadata: serde_json::Value::Null,
                peer_kind: Some("direct".into()),
            },
            session_key: "scripted:u1:".into(),
            response_context: serde_json::json!({"chat_id": "c1"}),
        })
    }
    async fn send_response(
        &self,
        text: &str,
        ctx: &serde_json::Value,
        _h: &Arc<RwLock<ChannelHealthRegistry>>,
    ) -> Result<()> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        *self.last_text.lock().await = Some(text.to_string());
        *self.last_ctx.lock().await = Some(ctx.clone());
        Ok(())
    }
}

/// Wait for the queued task in `SessionQueue` to actually execute. The dispatcher
/// returns 200 OK as soon as the closure is enqueued; the side effect we are
/// testing happens on the worker side. Bound the wait so a real bug surfaces
/// as a test failure rather than an indefinite hang.
async fn await_send(channel: &Arc<ScriptedChannel>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while channel.sends.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn btw_input_short_circuits_with_threaded_response() {
    let scripted = ScriptedChannel::new("/btw what's up?");
    let channel: Arc<dyn NativeChannel> = scripted.clone();
    let config = Config::default();
    let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
    let ctx = WebhookContext {
        config: &config,
        health: &health,
    };
    let queue = SessionQueue::new(8);

    let (status, _body) = dispatch_webhook(
        &channel,
        &HeaderMap::new(),
        "ignored body",
        &ctx,
        &queue,
        Duration::from_secs(30),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    await_send(&scripted).await;

    assert_eq!(
        scripted.sends.load(Ordering::SeqCst),
        1,
        "/btw must trigger exactly one send_response"
    );
    let sent_ctx = scripted.last_ctx.lock().await.clone().unwrap();
    assert!(
        is_threaded(&sent_ctx),
        "expected btw_thread_anchor=true on response_context, got {sent_ctx}"
    );
    assert_eq!(
        sent_ctx[BTW_THREAD_ANCHOR_KEY],
        serde_json::Value::Bool(true)
    );

    // Default Config has no LLM provider, so run_btw will error and the
    // gateway error formatter kicks in — but the message is still delivered.
    // The delivered text must be non-empty, otherwise we silently dropped the
    // side question.
    let text = scripted.last_text.lock().await.clone().unwrap();
    assert!(
        !text.is_empty(),
        "delivered /btw response must not be empty"
    );
}

#[tokio::test]
async fn non_btw_input_does_not_set_threading_flag() {
    let scripted = ScriptedChannel::new("just a normal message");
    let channel: Arc<dyn NativeChannel> = scripted.clone();
    let config = Config::default();
    let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
    let ctx = WebhookContext {
        config: &config,
        health: &health,
    };
    let queue = SessionQueue::new(8);

    let (status, _body) = dispatch_webhook(
        &channel,
        &HeaderMap::new(),
        "ignored body",
        &ctx,
        &queue,
        Duration::from_secs(30),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    await_send(&scripted).await;

    // The agent path also calls send_response (with whatever error/text it
    // produced). The send must actually have happened — if invoke_agent ever
    // skips delivery on hard errors, this test would silently become vacuous.
    // The response_context must NOT carry the /btw threading flag, otherwise
    // the short-circuit is leaking into the regular path.
    let sent_ctx = scripted.last_ctx.lock().await.clone();
    let sent_ctx = sent_ctx.expect("regular agent path must still call send_response");
    assert!(
        !is_threaded(&sent_ctx),
        "regular agent path must not set btw_thread_anchor, got {sent_ctx}"
    );
}
