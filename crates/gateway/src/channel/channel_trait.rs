//! Unified trait for native channel implementations.
//!
//! All native channels (Telegram, Slack, Discord, Twilio, Teams, Google Chat)
//! implement `NativeChannel` so the webhook dispatcher can do a single registry
//! lookup instead of a hardcoded if-chain.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode};
use tokio::sync::RwLock;

use cortex::config::Config;
use cortex::db::Database;

use crate::handler::{self, InboundMessage};
use crate::health::ChannelHealthRegistry;
use crate::session_queue::SessionQueue;

/// HTTP response type used by webhook handlers.
pub type WebhookResponse = (StatusCode, axum::Json<serde_json::Value>);

/// Result of parsing and verifying an inbound webhook.
#[allow(clippy::large_enum_variant)]
pub enum WebhookOutcome {
    /// A message was successfully parsed and is ready for agent processing.
    Message {
        inbound: InboundMessage,
        session_key: String,
        /// Opaque channel-specific context needed for sending the response.
        /// Serialized to JSON so it can be stored in the delivery queue.
        response_context: serde_json::Value,
    },
    /// The webhook was valid but requires a specific HTTP response (e.g.,
    /// Slack URL verification challenge, Discord PING/PONG).
    ProtocolResponse(WebhookResponse),
    /// The webhook was valid but should be ignored (duplicate, filtered, etc.).
    Skip,
}

/// Context passed to the channel during webhook handling.
pub struct WebhookContext<'a> {
    /// Gateway configuration.
    pub config: &'a Config,
    /// Shared health registry for recording metrics.
    pub health: &'a Arc<RwLock<ChannelHealthRegistry>>,
}

/// Unified interface for native channel implementations.
///
/// Each channel encapsulates its API client, credentials, dedup state, and
/// response formatting. The gateway server registers channels in a
/// `HashMap<String, Arc<dyn NativeChannel>>` and dispatches via lookup.
#[async_trait]
pub trait NativeChannel: Send + Sync {
    /// Channel name(s) that this implementation handles.
    /// Returns the primary name and any aliases (e.g., `["twilio", "whatsapp", "sms"]`).
    fn names(&self) -> Vec<&str>;

    /// Parse and verify an inbound webhook request.
    ///
    /// This runs synchronously before the HTTP 200 is returned. It must:
    /// 1. Verify the webhook signature/token
    /// 2. Deduplicate if needed
    /// 3. Parse into an `InboundMessage`
    /// 4. Compute the session key
    /// 5. Build response context for later use by `send_response`
    ///
    /// Pre-enqueue work (e.g., Telegram audio transcription, Slack file downloads)
    /// should also happen here.
    async fn handle_webhook(
        &self,
        headers: &HeaderMap,
        body: &str,
        ctx: &WebhookContext<'_>,
    ) -> Result<WebhookOutcome>;

    /// Send a response to the channel after the agent has produced output.
    ///
    /// `response_context` is the opaque JSON from `WebhookOutcome::Message`.
    /// The channel uses it to reconstruct the delivery target (chat_id, thread,
    /// interaction token, etc.).
    async fn send_response(
        &self,
        response_text: &str,
        response_context: &serde_json::Value,
        health: &Arc<RwLock<ChannelHealthRegistry>>,
    ) -> Result<()>;

    /// Mutate channel-specific response context after the shared DB work has
    /// completed but before delivery is queued or sent.
    async fn prepare_response_context(
        &self,
        response_context: &mut serde_json::Value,
        db: Option<&Database>,
        config: &Config,
    ) -> Result<()> {
        let _ = (response_context, db, config);
        Ok(())
    }

    /// Whether this channel can attach local files to replies via
    /// [`NativeChannel::send_media`]. Channels that leave this `false` get a
    /// text fallback notice from the media-egress path instead of a silent
    /// drop.
    fn supports_media_attachments(&self) -> bool {
        false
    }

    /// Send one local file as an attachment on this channel, using
    /// `response_context` to reconstruct the delivery target. `caption` is a
    /// short optional caption (channels without a caption concept may ignore
    /// it — the filename is still shown by the platform).
    ///
    /// Callers must validate the path first
    /// (`cortex::agent::media_markers::validate_media_path`); implementations
    /// may assume it points at an existing regular file.
    async fn send_media(
        &self,
        path: &std::path::Path,
        caption: Option<&str>,
        response_context: &serde_json::Value,
        health: &Arc<RwLock<ChannelHealthRegistry>>,
    ) -> Result<()> {
        let _ = (caption, response_context, health);
        anyhow::bail!(
            "channel {} has no attachment support for {}",
            self.names()[0],
            path.display()
        )
    }

    /// Return the bot mention string for group activation filtering.
    /// E.g., `Some("@mybot")` for Telegram, `Some("<@U123>")` for Slack.
    fn bot_mention(&self) -> Option<String> {
        None
    }

    /// Start a typing indicator. Returns a handle that stops typing when dropped
    /// or when `stop()` is called. Returns `None` if the channel doesn't support typing.
    fn start_typing(&self, response_context: &serde_json::Value) -> Option<TypingHandle> {
        let _ = response_context;
        None
    }
}

/// Handle for stopping a typing indicator.
pub struct TypingHandle {
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl TypingHandle {
    pub fn new(
        stop_tx: tokio::sync::oneshot::Sender<()>,
        join: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            stop_tx: Some(stop_tx),
            join: Some(join),
        }
    }

    /// Spawn a [`crate::typing_keepalive::run_keepalive`] loop and wrap it in
    /// a handle. `tick` re-sends the platform's typing action every
    /// `config.keepalive_interval` until the handle is stopped or dropped.
    pub fn spawn_keepalive<F, Fut>(
        config: crate::typing_keepalive::TypingKeepaliveConfig,
        tick: F,
    ) -> Self
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<()>> + Send,
    {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let join = tokio::spawn(async move {
            crate::typing_keepalive::run_keepalive(config, stop_rx, tick).await;
        });
        Self::new(stop_tx, join)
    }

    /// Send the stop signal and wait for the typing task to finish.
    pub async fn stop(mut self) {
        // Both discards mean "the typing task already ended" — a send error or
        // a JoinError here is the success case, not a failure to report.
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

impl Drop for TypingHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        // Can't await in Drop — the task will see the stop signal and exit
    }
}

/// Registry of native channel implementations.
pub struct NativeChannelRegistry {
    channels: std::collections::HashMap<String, Arc<dyn NativeChannel>>,
}

impl NativeChannelRegistry {
    pub fn new() -> Self {
        Self {
            channels: std::collections::HashMap::new(),
        }
    }

    /// Register a native channel. All names returned by `channel.names()` are mapped.
    pub fn register(&mut self, channel: Arc<dyn NativeChannel>) {
        for name in channel.names() {
            self.channels.insert(name.to_string(), channel.clone());
        }
    }

    /// Look up a channel by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn NativeChannel>> {
        self.channels.get(name)
    }

    /// Check if a channel name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.channels.contains_key(name)
    }

    /// List all unique registered channels (deduplicated by primary name).
    pub fn list(&self) -> Vec<&Arc<dyn NativeChannel>> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for channel in self.channels.values() {
            let primary = channel.names()[0];
            if seen.insert(primary) {
                result.push(channel);
            }
        }
        result
    }
}

impl Default for NativeChannelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Unified webhook dispatch: parse → enqueue → agent → send response.
///
/// This replaces the per-channel `handle_*_webhook` functions in server.rs
/// with a single generic flow.
pub async fn dispatch_webhook(
    channel: &Arc<dyn NativeChannel>,
    headers: &HeaderMap,
    body: &str,
    ctx: &WebhookContext<'_>,
    session_queue: &SessionQueue,
    request_timeout: std::time::Duration,
) -> WebhookResponse {
    let outcome = match channel.handle_webhook(headers, body, ctx).await {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::warn!(
                "Webhook verification/parse error for {}: {e:#}",
                channel.names()[0]
            );
            return ok_response();
        }
    };

    match outcome {
        WebhookOutcome::ProtocolResponse(resp) => resp,
        WebhookOutcome::Skip => ok_response(),
        WebhookOutcome::Message {
            inbound,
            session_key,
            response_context,
        } => {
            let channel = channel.clone();
            let config = ctx.config.clone();
            let health = ctx.health.clone();

            // /btw side questions short-circuit the main agent path entirely:
            // they run a tool-less ephemeral turn against the existing
            // transcript and deliver the answer as a threaded reply, with no
            // DB writes, no delivery queue row, no typing indicator. This
            // preserves the "doesn't derail the main thread" invariant the
            // TUI popup path already enforces.
            if let Some(question) = cortex::agent::parse_btw_command(&inbound.text) {
                let question = question.to_string();
                let channel_for_btw = channel.clone();
                let health_for_btw = health.clone();
                let config_for_btw = config.clone();
                // Use a `:btw` suffix so a side question doesn't queue behind
                // an in-flight main agent turn for the same user — that would
                // defeat "non-blocking side question." Multiple /btw from the
                // same sender still serialize, which is fine.
                let btw_session_key = format!("{session_key}:btw");
                let enqueued = session_queue
                    .enqueue(
                        btw_session_key,
                        Box::pin(async move {
                            let runner = |q: String, snap: Vec<cortex::types::Message>| {
                                crate::btw_dispatch::run_btw_default(config_for_btw, q, snap)
                            };
                            if let Err(e) = crate::btw_dispatch::dispatch_btw_message(
                                &channel_for_btw,
                                &inbound,
                                &question,
                                response_context,
                                &health_for_btw,
                                runner,
                            )
                            .await
                            {
                                tracing::warn!(
                                    "{} /btw dispatch failed: {e:#}",
                                    channel_for_btw.names()[0]
                                );
                            }
                        }),
                    )
                    .await;
                if !enqueued {
                    return service_unavailable_response();
                }
                return ok_response();
            }

            let enqueued = session_queue
                .enqueue(
                    session_key,
                    Box::pin(async move {
                        let mut response_context = response_context;
                        let typing = channel.start_typing(&response_context);
                        let bot_mention = channel.bot_mention();
                        let channel_name = channel.names()[0].to_string();

                        // Timeout only wraps the LLM agent call — delivery
                        // operations must always complete to avoid lost responses.
                        let agent_result = tokio::time::timeout(
                            request_timeout,
                            handler::invoke_agent(
                                &channel_name,
                                &inbound,
                                &config,
                                Some(&health),
                                bot_mention.as_deref(),
                            ),
                        )
                        .await;

                        if let Some(t) = typing {
                            t.stop().await;
                        }

                        let response_text = match agent_result {
                            Ok(Ok((text, _session_id))) => text,
                            Ok(Err(e)) => {
                                // Per-message transient agent failure. The
                                // formatted error is delivered back to the user
                                // via the channel; logging at WARN every time
                                // floods daemon.log on a flaky LLM. DEBUG with
                                // structured channel field keeps it triagable.
                                tracing::debug!(channel = %channel_name, error = %format!("{e:#}"), "agent error");
                                crate::handler::format_gateway_error(&e)
                            }
                            Err(_) => {
                                tracing::debug!(channel = %channel_name, "agent timed out");
                                cortex::error_format::format_error_with_context(
                                    "request timed out",
                                    cortex::error_format::ErrorContext::Gateway,
                                )
                            }
                        };

                        if response_text.trim().is_empty() {
                            tracing::debug!(
                                "{channel_name} response empty after trim, skipping delivery"
                            );
                            return;
                        }

                        // Output-redaction chokepoint. Strip any detected
                        // secrets from agent output before it enters the
                        // delivery queue (so a queued retry can't replay a
                        // secret) and before live `send_response`. Shares the
                        // single `redact_channel_output` helper with the
                        // synchronous return path so both stay in lockstep.
                        // Toggle gated by `security.output_redaction`.
                        let response_text = crate::handler::redact_channel_output(
                            &channel_name,
                            &inbound.sender_id,
                            inbound.channel_id.as_deref(),
                            &response_text,
                        );

                        // Media egress: pull `[MEDIA:<path>]` markers out of the
                        // reply. Attachments are delivered after the response
                        // context is prepared (below); failures become text
                        // notices so they ride the normal delivery path.
                        let (clean_text, media_paths) =
                            cortex::agent::media_markers::extract_media_markers(&response_text);

                        // Persist to delivery queue for at-least-once guarantee.
                        // A single DB connection is reused for enqueue + status update.
                        let delivery_id = uuid::Uuid::new_v4().to_string();
                        let db = match Database::open_with_timeout(
                            Database::GATEWAY_BUSY_TIMEOUT_MS,
                        )
                        .await
                        {
                            Ok(db) => Some(db),
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to open DB for {channel_name} delivery queue \
                                     (at-least-once guarantee disabled for this message): {e}"
                                );
                                None
                            }
                        };

                        if let Err(e) = channel
                            .prepare_response_context(&mut response_context, db.as_ref(), &config)
                            .await
                        {
                            tracing::warn!(
                                "Failed to prepare {channel_name} response context: {e}"
                            );
                        }

                        // Deliver attachments first so any failure notice can
                        // be appended to the text reply below. Attachment
                        // sends are live-only (not replayed by the delivery
                        // queue's retry loop); the queued text carries the
                        // notices for anything that did not go out.
                        let (any_media_sent, media_notices) = if media_paths.is_empty() {
                            (false, Vec::new())
                        } else {
                            crate::message_processing::media_egress::deliver_reply_media(
                                channel.as_ref(),
                                &media_paths,
                                &config.security,
                                clean_text.trim().is_empty(),
                                &response_context,
                                &health,
                            )
                            .await
                        };

                        let response_text =
                            match crate::message_processing::media_egress::compose_reply_text(
                                &clean_text,
                                &media_notices,
                            ) {
                                Some(text) => text,
                                None => {
                                    // Marker-only reply whose attachments all
                                    // went out — the attachments carry the
                                    // content, nothing left to send as text.
                                    debug_assert!(any_media_sent || media_paths.is_empty());
                                    tracing::debug!(
                                        "{channel_name} reply was attachment-only; text send skipped"
                                    );
                                    return;
                                }
                            };

                        let payload = serde_json::json!({
                            "text": response_text,
                            "response_context": response_context,
                        });
                        let payload_str = payload.to_string();

                        if let Some(ref db) = db {
                            let new_delivery = cortex::db::NewDelivery {
                                id: &delivery_id,
                                channel_name: &channel_name,
                                sender_id: &inbound.sender_id,
                                channel_id: inbound.channel_id.as_deref(),
                                session_id: None,
                                payload_json: &payload_str,
                                max_retries: 3,
                            };
                            if let Err(e) = db.enqueue_delivery(&new_delivery).await {
                                tracing::warn!("Failed to enqueue {channel_name} delivery: {e}");
                            }
                        }

                        // The delivery row is enqueued first so a crash between
                        // here and `mark_delivered` leaves work the drain loop
                        // can retry rather than a silently dropped reply.
                        match channel
                            .send_response(&response_text, &response_context, &health)
                            .await
                        {
                            Ok(()) => {
                                if let Some(ref db) = db {
                                    if let Err(e) = db.mark_delivered(&delivery_id).await {
                                        tracing::warn!("Failed to mark delivery {delivery_id} as delivered: {e}");
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to send {channel_name} response: {e}");
                                // Mark as failed — drain loop will retry
                                if let Some(ref db) = db {
                                    // 30s backs off past a transient channel-API
                                    // blip without stalling a user-visible reply.
                                    let retry_at = chrono::Utc::now().timestamp() + 30;
                                    if let Err(e2) = db
                                        .mark_failed(&delivery_id, &e.to_string(), Some(retry_at))
                                        .await
                                    {
                                        tracing::warn!("Failed to mark delivery {delivery_id} as failed: {e2}");
                                    }
                                }
                            }
                        }
                    }),
                )
                .await;

            if !enqueued {
                return service_unavailable_response();
            }
            ok_response()
        }
    }
}

fn ok_response() -> WebhookResponse {
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "ok": true })),
    )
}

fn service_unavailable_response() -> WebhookResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({ "error": "Server at capacity, try again later" })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Mock native channel for testing the dispatch flow.
    struct MockChannel {
        name: String,
        call_count: Arc<AtomicU32>,
        send_count: Arc<AtomicU32>,
        should_skip: bool,
    }

    #[async_trait]
    impl NativeChannel for MockChannel {
        fn names(&self) -> Vec<&str> {
            vec![&self.name]
        }

        async fn handle_webhook(
            &self,
            _headers: &HeaderMap,
            body: &str,
            _ctx: &WebhookContext<'_>,
        ) -> Result<WebhookOutcome> {
            self.call_count.fetch_add(1, Ordering::SeqCst);

            if self.should_skip {
                return Ok(WebhookOutcome::Skip);
            }

            Ok(WebhookOutcome::Message {
                inbound: InboundMessage {
                    sender_id: "user1".to_string(),
                    text: body.to_string(),
                    channel_id: Some("ch1".to_string()),
                    thread_id: None,
                    message_id: None,
                    thread_ts: None,
                    attachments: vec![],
                    reaction: None,
                    metadata: serde_json::Value::Null,
                    peer_kind: Some("direct".to_string()),
                },
                session_key: format!("mock:user1:"),
                response_context: serde_json::json!({"chat_id": "ch1"}),
            })
        }

        async fn send_response(
            &self,
            _response_text: &str,
            _response_context: &serde_json::Value,
            _health: &Arc<RwLock<ChannelHealthRegistry>>,
        ) -> Result<()> {
            self.send_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn registry_lookup_by_name() {
        let mut registry = NativeChannelRegistry::new();

        let channel = Arc::new(MockChannel {
            name: "telegram".to_string(),
            call_count: Arc::new(AtomicU32::new(0)),
            send_count: Arc::new(AtomicU32::new(0)),
            should_skip: false,
        });

        registry.register(channel);

        assert!(registry.contains("telegram"));
        assert!(!registry.contains("slack"));
        assert!(registry.get("telegram").is_some());
        assert!(registry.get("slack").is_none());
    }

    #[test]
    fn registry_multi_name_channel() {
        let mut registry = NativeChannelRegistry::new();

        struct MultiNameChannel;

        #[async_trait]
        impl NativeChannel for MultiNameChannel {
            fn names(&self) -> Vec<&str> {
                vec!["twilio", "whatsapp", "sms"]
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
                _t: &str,
                _c: &serde_json::Value,
                _h: &Arc<RwLock<ChannelHealthRegistry>>,
            ) -> Result<()> {
                Ok(())
            }
        }

        registry.register(Arc::new(MultiNameChannel));

        assert!(registry.contains("twilio"));
        assert!(registry.contains("whatsapp"));
        assert!(registry.contains("sms"));
        // All aliases point to the same channel
        assert!(std::ptr::eq(
            Arc::as_ptr(registry.get("twilio").unwrap()),
            Arc::as_ptr(registry.get("whatsapp").unwrap()),
        ));
    }

    #[test]
    fn registry_list_deduplicates() {
        let mut registry = NativeChannelRegistry::new();

        struct MultiNameChannel;

        #[async_trait]
        impl NativeChannel for MultiNameChannel {
            fn names(&self) -> Vec<&str> {
                vec!["twilio", "whatsapp", "sms"]
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
                _t: &str,
                _c: &serde_json::Value,
                _h: &Arc<RwLock<ChannelHealthRegistry>>,
            ) -> Result<()> {
                Ok(())
            }
        }

        registry.register(Arc::new(MultiNameChannel));

        // Even though 3 names are registered, list() should return 1 unique channel
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn typing_handle_drop_sends_stop() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let join = tokio::spawn(async move {
                let _ = rx.await;
            });
            let handle = TypingHandle::new(tx, join);
            // Dropping the handle should send the stop signal
            drop(handle);
            // Give the task a moment to complete
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
    }

    #[tokio::test]
    async fn typing_handle_explicit_stop() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let stopped = Arc::new(AtomicU32::new(0));
        let stopped_clone = stopped.clone();
        let join = tokio::spawn(async move {
            let _ = rx.await;
            stopped_clone.fetch_add(1, Ordering::SeqCst);
        });
        let handle = TypingHandle::new(tx, join);
        handle.stop().await;
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn registry_default_is_empty() {
        let registry = NativeChannelRegistry::default();
        assert!(!registry.contains("anything"));
        assert!(registry.list().is_empty());
    }

    #[test]
    fn registry_overwrite_same_name() {
        let mut registry = NativeChannelRegistry::new();

        let channel1 = Arc::new(MockChannel {
            name: "test".to_string(),
            call_count: Arc::new(AtomicU32::new(0)),
            send_count: Arc::new(AtomicU32::new(0)),
            should_skip: false,
        });
        let channel2 = Arc::new(MockChannel {
            name: "test".to_string(),
            call_count: Arc::new(AtomicU32::new(0)),
            send_count: Arc::new(AtomicU32::new(0)),
            should_skip: true,
        });

        registry.register(channel1);
        registry.register(channel2.clone());

        // Second registration should overwrite the first
        let got = registry.get("test").unwrap();
        let channel2_dyn: Arc<dyn NativeChannel> = channel2;
        assert!(std::ptr::eq(Arc::as_ptr(got), Arc::as_ptr(&channel2_dyn)));
    }

    #[test]
    fn native_registry_count_multiple_channels() {
        let mut registry = NativeChannelRegistry::new();

        let telegram = Arc::new(MockChannel {
            name: "telegram".to_string(),
            call_count: Arc::new(AtomicU32::new(0)),
            send_count: Arc::new(AtomicU32::new(0)),
            should_skip: false,
        });
        let slack = Arc::new(MockChannel {
            name: "slack".to_string(),
            call_count: Arc::new(AtomicU32::new(0)),
            send_count: Arc::new(AtomicU32::new(0)),
            should_skip: false,
        });

        registry.register(telegram);
        registry.register(slack);

        assert_eq!(registry.list().len(), 2);
        assert!(registry.contains("telegram"));
        assert!(registry.contains("slack"));
    }

    /// Mock channel that returns a protocol response.
    struct ProtocolChannel;

    #[async_trait]
    impl NativeChannel for ProtocolChannel {
        fn names(&self) -> Vec<&str> {
            vec!["protocol"]
        }
        async fn handle_webhook(
            &self,
            _h: &HeaderMap,
            _b: &str,
            _c: &WebhookContext<'_>,
        ) -> Result<WebhookOutcome> {
            Ok(WebhookOutcome::ProtocolResponse((
                StatusCode::OK,
                axum::Json(serde_json::json!({"challenge": "test123"})),
            )))
        }
        async fn send_response(
            &self,
            _t: &str,
            _c: &serde_json::Value,
            _h: &Arc<RwLock<ChannelHealthRegistry>>,
        ) -> Result<()> {
            Ok(())
        }
    }

    /// Mock channel that returns an error from handle_webhook.
    struct ErrorChannel;

    #[async_trait]
    impl NativeChannel for ErrorChannel {
        fn names(&self) -> Vec<&str> {
            vec!["error"]
        }
        async fn handle_webhook(
            &self,
            _h: &HeaderMap,
            _b: &str,
            _c: &WebhookContext<'_>,
        ) -> Result<WebhookOutcome> {
            Err(anyhow::anyhow!("webhook verification failed"))
        }
        async fn send_response(
            &self,
            _t: &str,
            _c: &serde_json::Value,
            _h: &Arc<RwLock<ChannelHealthRegistry>>,
        ) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn dispatch_protocol_response_returns_directly() {
        let channel: Arc<dyn NativeChannel> = Arc::new(ProtocolChannel);
        let config = Config::default();
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
        let ctx = WebhookContext {
            config: &config,
            health: &health,
        };
        let queue = SessionQueue::new(10);

        let (status, body) = dispatch_webhook(
            &channel,
            &HeaderMap::new(),
            "",
            &ctx,
            &queue,
            crate::constants::GATEWAY_HTTP_TIMEOUT,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["challenge"], "test123");
    }

    #[tokio::test]
    async fn dispatch_error_returns_ok() {
        // Webhook errors should still return 200 to prevent platform retries
        let channel: Arc<dyn NativeChannel> = Arc::new(ErrorChannel);
        let config = Config::default();
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
        let ctx = WebhookContext {
            config: &config,
            health: &health,
        };
        let queue = SessionQueue::new(10);

        let (status, body) = dispatch_webhook(
            &channel,
            &HeaderMap::new(),
            "",
            &ctx,
            &queue,
            crate::constants::GATEWAY_HTTP_TIMEOUT,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["ok"], true);
    }

    #[tokio::test]
    async fn dispatch_skip_returns_ok() {
        let channel: Arc<dyn NativeChannel> = Arc::new(MockChannel {
            name: "skip".to_string(),
            call_count: Arc::new(AtomicU32::new(0)),
            send_count: Arc::new(AtomicU32::new(0)),
            should_skip: true,
        });
        let config = Config::default();
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
        let ctx = WebhookContext {
            config: &config,
            health: &health,
        };
        let queue = SessionQueue::new(10);

        let (status, body) = dispatch_webhook(
            &channel,
            &HeaderMap::new(),
            "test body",
            &ctx,
            &queue,
            crate::constants::GATEWAY_HTTP_TIMEOUT,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["ok"], true);
    }

    #[test]
    fn native_channel_default_bot_mention_is_none() {
        struct MinimalChannel;

        #[async_trait]
        impl NativeChannel for MinimalChannel {
            fn names(&self) -> Vec<&str> {
                vec!["minimal"]
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
                _t: &str,
                _c: &serde_json::Value,
                _h: &Arc<RwLock<ChannelHealthRegistry>>,
            ) -> Result<()> {
                Ok(())
            }
        }

        let ch = MinimalChannel;
        assert!(ch.bot_mention().is_none());
        assert!(ch.start_typing(&serde_json::json!({})).is_none());
    }
}
