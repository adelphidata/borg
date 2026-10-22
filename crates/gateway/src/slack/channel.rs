//! `NativeChannel` implementation for Slack.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode};
use tokio::sync::{Mutex, RwLock};

use crate::channel::channel_trait::{NativeChannel, TypingHandle, WebhookContext, WebhookOutcome};
use crate::health::ChannelHealthRegistry;
use crate::message_processing::voice;

use super::api::SlackClient;
use super::dedup::EventDeduplicator;
use super::SlackWebhookResult;

/// Slack-specific response context stored in the delivery queue.
#[derive(serde::Serialize, serde::Deserialize)]
struct SlackResponseContext {
    channel_id: String,
    thread_ts: Option<String>,
    message_ts: Option<String>,
}

/// Slack native channel implementation.
pub struct SlackChannel {
    pub client: Arc<SlackClient>,
    pub signing_secret: Option<String>,
    pub dedup: Arc<Mutex<EventDeduplicator>>,
    pub bot_user_id: Option<String>,
    /// Configuration handle — needed to build the STT pipeline for
    /// inbound voice/audio file transcription (parity with Telegram).
    pub config: cortex::config::Config,
}

#[async_trait]
impl NativeChannel for SlackChannel {
    fn names(&self) -> Vec<&str> {
        vec!["slack"]
    }

    async fn handle_webhook(
        &self,
        headers: &HeaderMap,
        body: &str,
        _ctx: &WebhookContext<'_>,
    ) -> Result<WebhookOutcome> {
        // Fail closed: refuse inbound traffic when no signing secret is
        // configured, rather than silently accepting unauthenticated requests.
        crate::verify_common::require_verification_secret(
            "slack",
            self.signing_secret.as_deref(),
            "SLACK_SIGNING_SECRET",
        )?;
        let webhook_result = super::handle_slack_webhook(
            headers,
            body,
            self.signing_secret.as_deref(),
            Some(&self.dedup),
            self.client.bot_user_id(),
            Some(self.client.echo_cache()),
        )
        .await?;

        let mut inbound = match webhook_result {
            SlackWebhookResult::Challenge(challenge) => {
                return Ok(WebhookOutcome::ProtocolResponse((
                    StatusCode::OK,
                    axum::Json(serde_json::json!({ "challenge": challenge })),
                )));
            }
            SlackWebhookResult::Skip => return Ok(WebhookOutcome::Skip),
            SlackWebhookResult::Message(inbound) => *inbound,
        };

        if !super::channel_allowed(
            self.config.gateway.slack_channel_allowlist.as_deref(),
            inbound.channel_id.as_deref(),
        ) {
            tracing::info!(
                "Slack message from channel {:?} dropped: not in gateway.slack_channel_allowlist",
                inbound.channel_id
            );
            return Ok(WebhookOutcome::Skip);
        }

        // A sender the queued turn will challenge or deny gets none of the
        // pre-turn work: no reaction, no file download, no paid STT, no
        // history read. The plain text still goes through so the challenge
        // itself is delivered. Placeholder URLs are dropped with the
        // enrichment — nothing downstream can use them.
        let may_enrich = crate::handler::sender_may_enrich(&self.config, "slack", &inbound).await;
        if !may_enrich {
            inbound.attachments.clear();
        }
        if may_enrich {
            // Ack reaction — immediate visual feedback
            if let (Some(ref ch), Some(ref ts)) = (&inbound.channel_id, &inbound.message_id) {
                self.client.add_reaction(ch, ts, "eyes").await;
            }

            // Download file attachments (placeholder URLs → base64 data).
            // Voice/audio files take a side-path through the STT pipeline so
            // the agent sees `[Voice transcript]: …` in the message text
            // (parity with Telegram + Twilio inbound voice).
            let transcriber =
                cortex::media::understanding::AudioTranscriber::from_config(&self.config);
            let mut resolved_attachments = Vec::new();
            let mut voice_transcripts: Vec<String> = Vec::new();
            let mut failed_kinds: Vec<&'static str> = Vec::new();
            for att in &inbound.attachments {
                if att.data.starts_with("https://") {
                    match self.client.download_file(&att.data).await {
                        Ok((bytes, content_type)) => {
                            if content_type.starts_with("audio/") {
                                if let Some(ref t) = transcriber {
                                    let filename = att
                                        .filename
                                        .clone()
                                        .unwrap_or_else(|| "slack-voice.audio".to_string());
                                    voice_transcripts.extend(
                                        voice::transcribe_or_warn(
                                            t,
                                            &bytes,
                                            &content_type,
                                            &filename,
                                            "slack",
                                        )
                                        .await,
                                    );
                                }
                            }
                            use base64::Engine;
                            resolved_attachments.push(crate::handler::InboundAttachment {
                                mime_type: content_type,
                                data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                                filename: att.filename.clone(),
                            });
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to download Slack file {}: {e}",
                                att.filename.as_deref().unwrap_or("unknown")
                            );
                            failed_kinds.push(
                                crate::message_processing::media_failure::kind_for_mime(
                                    &att.mime_type,
                                ),
                            );
                        }
                    }
                } else {
                    resolved_attachments.push(att.clone());
                }
            }
            inbound.attachments = resolved_attachments;
            // Surface failed downloads to the agent instead of silently dropping
            // the attachment from the message.
            for kind in failed_kinds {
                crate::message_processing::media_failure::append_download_failure(
                    &mut inbound.text,
                    kind,
                );
            }
            voice::append_voice_transcripts(&mut inbound.text, &voice_transcripts);

            // Only inject channel history for top-level @mentions: a threaded reply
            // already carries its own conversation context, so re-injecting full
            // channel history would duplicate it and bloat the prompt.
            if inbound.text.contains("<@") && inbound.thread_ts.is_none() {
                if let Some(ref channel) = inbound.channel_id {
                    match self.client.conversations_history(channel, 10).await {
                        Ok(history) if !history.is_empty() => {
                            // Cap injected history so it never dominates the agent's token budget.
                            const MAX_CONTEXT_CHARS: usize = 8000;
                            let mut context = String::new();
                            for m in history.iter().rev() {
                                let line = format!("<@{}>: {}\n", m.user, m.text);
                                if context.len() + line.len() > MAX_CONTEXT_CHARS {
                                    break;
                                }
                                context.push_str(&line);
                            }
                            if !context.is_empty() {
                                inbound.text = format!(
                                    "[Channel context]\n{context}[Current message]\n{}",
                                    inbound.text
                                );
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!("Failed to fetch channel history for context: {e}");
                        }
                    }
                }
            }
        }

        let session_key = inbound.session_key("slack", inbound.thread_id.as_deref().unwrap_or(""));

        let response_context = serde_json::to_value(SlackResponseContext {
            channel_id: inbound.channel_id.clone().unwrap_or_default(),
            thread_ts: inbound.thread_ts.clone(),
            message_ts: inbound.message_id.clone(),
        })?;

        Ok(WebhookOutcome::Message {
            inbound,
            session_key,
            response_context,
        })
    }

    async fn send_response(
        &self,
        response_text: &str,
        response_context: &serde_json::Value,
        health: &Arc<RwLock<ChannelHealthRegistry>>,
    ) -> Result<()> {
        let ctx: SlackResponseContext = serde_json::from_value(response_context.clone())?;

        let formatted = super::format::markdown_to_mrkdwn(response_text);

        // /btw replies must land in a thread off the inbound message even when
        // the user wrote `/btw …` at the top level of a channel — otherwise the
        // side question pollutes the channel's main timeline.
        let thread_ts = if crate::btw_dispatch::is_threaded(response_context) {
            let resolved = ctx.thread_ts.as_deref().or(ctx.message_ts.as_deref());
            if resolved.is_none() {
                tracing::warn!(
                    "/btw on Slack with no thread_ts AND no message_ts — answer will post top-level"
                );
            }
            resolved
        } else {
            ctx.thread_ts.as_deref()
        };

        if let Err(e) = self
            .client
            .post_message(&ctx.channel_id, &formatted, thread_ts)
            .await
        {
            health.write().await.record_error("slack", &e.to_string());
            return Err(e);
        }

        health.write().await.record_outbound("slack");

        // Replace ack reaction with done reaction
        if let Some(ref ts) = ctx.message_ts {
            self.client
                .remove_reaction(&ctx.channel_id, ts, "eyes")
                .await;
            self.client
                .add_reaction(&ctx.channel_id, ts, "white_check_mark")
                .await;
        }

        Ok(())
    }

    fn supports_media_attachments(&self) -> bool {
        true
    }

    async fn send_media(
        &self,
        path: &std::path::Path,
        caption: Option<&str>,
        response_context: &serde_json::Value,
        health: &Arc<RwLock<ChannelHealthRegistry>>,
    ) -> Result<()> {
        use anyhow::Context as _;

        let ctx: SlackResponseContext = serde_json::from_value(response_context.clone())?;
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("failed to read media file {}", path.display()))?;
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment");
        // Slack's file upload has no separate caption field at this seam; the
        // filename is displayed with the upload, which covers the minimal
        // caption the egress path asks for.
        let _ = caption;

        match self
            .client
            .upload_file(&ctx.channel_id, &bytes, filename, None)
            .await
        {
            Ok(()) => {
                health.write().await.record_outbound("slack");
                Ok(())
            }
            Err(e) => {
                health.write().await.record_error("slack", &e.to_string());
                Err(e)
            }
        }
    }

    fn bot_mention(&self) -> Option<String> {
        self.bot_user_id.as_ref().map(|id| format!("<@{id}>"))
    }

    fn start_typing(&self, response_context: &serde_json::Value) -> Option<TypingHandle> {
        let ctx: SlackResponseContext = serde_json::from_value(response_context.clone()).ok()?;
        let client = self.client.clone();

        Some(TypingHandle::spawn_keepalive(
            crate::typing_keepalive::TypingKeepaliveConfig {
                // Re-send before Slack's typing indicator decays (~5s) so it stays visible.
                keepalive_interval: std::time::Duration::from_secs(3),
                label: "slack",
            },
            move || {
                let client = client.clone();
                let ch = ctx.channel_id.clone();
                let ts = ctx.thread_ts.clone();
                async move {
                    client
                        .set_thread_status(&ch, ts.as_deref(), "is typing...")
                        .await
                }
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-signing-secret";

    fn make_channel(base: &str, bot_user_id: Option<&str>) -> SlackChannel {
        SlackChannel {
            client: Arc::new(SlackClient::with_base("xoxb-test", base).unwrap()),
            signing_secret: Some(SECRET.to_string()),
            dedup: Arc::new(Mutex::new(EventDeduplicator::new())),
            bot_user_id: bot_user_id.map(str::to_string),
            config: open_policy_config(),
        }
    }

    /// Enrichment (reactions, downloads, history) only runs for an admitted
    /// sender; these tests exercise the enrichment itself, so they open the
    /// channel to every sender.
    fn open_policy_config() -> cortex::config::Config {
        let mut config = cortex::config::Config::default();
        config.gateway.dm_policy = cortex::pairing::DmPolicy::Open;
        config
    }

    fn response_ctx(
        channel: &str,
        thread_ts: Option<&str>,
        message_ts: Option<&str>,
    ) -> serde_json::Value {
        serde_json::to_value(SlackResponseContext {
            channel_id: channel.to_string(),
            thread_ts: thread_ts.map(str::to_string),
            message_ts: message_ts.map(str::to_string),
        })
        .unwrap()
    }

    #[test]
    fn names_is_exactly_slack() {
        let channel = make_channel("http://127.0.0.1:1", None);
        assert_eq!(channel.names(), vec!["slack"]);
    }

    /// The mention token must wrap the configured bot user id in Slack's
    /// `<@ID>` syntax — mention-mode activation compares inbound text
    /// against exactly this string.
    #[test]
    fn bot_mention_wraps_configured_id() {
        let channel = make_channel("http://127.0.0.1:1", Some("UBOT99"));
        assert_eq!(channel.bot_mention().as_deref(), Some("<@UBOT99>"));
        let unconfigured = make_channel("http://127.0.0.1:1", None);
        assert_eq!(unconfigured.bot_mention(), None);
    }

    /// Slack implements `send_media` via files.upload, so it must advertise
    /// media support — otherwise the egress path degrades every attachment
    /// to a text description.
    #[test]
    fn supports_media_attachments_because_upload_is_implemented() {
        let channel = make_channel("http://127.0.0.1:1", None);
        assert!(channel.supports_media_attachments());
    }

    #[test]
    fn response_context_roundtrip() {
        let ctx = SlackResponseContext {
            channel_id: "C123".to_string(),
            thread_ts: Some("1234567890.123456".to_string()),
            message_ts: Some("1234567890.654321".to_string()),
        };
        let json = serde_json::to_value(&ctx).unwrap();
        let back: SlackResponseContext = serde_json::from_value(json).unwrap();
        assert_eq!(back.channel_id, "C123");
        assert_eq!(back.thread_ts.as_deref(), Some("1234567890.123456"));
        assert_eq!(back.message_ts.as_deref(), Some("1234567890.654321"));
    }

    /// `/btw` arriving at the top level of a channel must fall back to the
    /// inbound `message_ts` so the answer lands in a thread under the user's
    /// question instead of polluting the main channel timeline. Mirrors the
    /// fallback applied inside `send_response`.
    #[test]
    fn btw_thread_anchor_falls_back_to_message_ts() {
        let mut ctx = serde_json::to_value(SlackResponseContext {
            channel_id: "C123".to_string(),
            thread_ts: None,
            message_ts: Some("1700000000.111111".to_string()),
        })
        .unwrap();
        crate::btw_dispatch::mark_threaded(&mut ctx);

        let parsed: SlackResponseContext = serde_json::from_value(ctx.clone()).unwrap();
        let resolved = if crate::btw_dispatch::is_threaded(&ctx) {
            parsed.thread_ts.as_deref().or(parsed.message_ts.as_deref())
        } else {
            parsed.thread_ts.as_deref()
        };
        assert_eq!(resolved, Some("1700000000.111111"));
    }

    /// Without the threading flag, top-level posts stay top-level. Guards
    /// against an over-aggressive fallback that would re-thread every reply.
    #[test]
    fn no_thread_anchor_preserves_top_level_posting() {
        let ctx = serde_json::to_value(SlackResponseContext {
            channel_id: "C123".to_string(),
            thread_ts: None,
            message_ts: Some("1700000000.111111".to_string()),
        })
        .unwrap();

        let parsed: SlackResponseContext = serde_json::from_value(ctx.clone()).unwrap();
        let resolved = if crate::btw_dispatch::is_threaded(&ctx) {
            parsed.thread_ts.as_deref().or(parsed.message_ts.as_deref())
        } else {
            parsed.thread_ts.as_deref()
        };
        assert_eq!(resolved, None);
    }

    mod outbound {
        use super::*;
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn ok_json() -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true }))
        }

        /// `send_response` must actually post the mrkdwn-formatted text to
        /// chat.postMessage and then swap the ack reaction (`eyes`) for the
        /// done reaction (`white_check_mark`) on the inbound message.
        #[tokio::test]
        async fn send_response_posts_formatted_text_and_swaps_reactions() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/chat.postMessage"))
                .and(body_json(serde_json::json!({
                    "channel": "C1",
                    "text": "*bold* done",
                })))
                .respond_with(ok_json())
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/reactions.remove"))
                .and(body_json(serde_json::json!({
                    "channel": "C1", "timestamp": "5.5", "name": "eyes",
                })))
                .respond_with(ok_json())
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/reactions.add"))
                .and(body_json(serde_json::json!({
                    "channel": "C1", "timestamp": "5.5", "name": "white_check_mark",
                })))
                .respond_with(ok_json())
                .expect(1)
                .mount(&server)
                .await;

            let channel = make_channel(&server.uri(), None);
            let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
            channel
                .send_response(
                    "**bold** done",
                    &response_ctx("C1", None, Some("5.5")),
                    &health,
                )
                .await
                .expect("post must succeed");
        }

        /// A missing local file must surface as an error — reporting success
        /// for media that never left the machine is the exact failure the
        /// no-silent-swallowing invariant forbids.
        #[tokio::test]
        async fn send_media_errors_on_unreadable_file() {
            let server = MockServer::start().await;
            let channel = make_channel(&server.uri(), None);
            let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
            let err = channel
                .send_media(
                    std::path::Path::new("/nonexistent/borg-test-media.png"),
                    None,
                    &response_ctx("C1", None, None),
                    &health,
                )
                .await
                .expect_err("unreadable file must fail, never fake success");
            assert!(
                err.to_string().contains("failed to read media file"),
                "got: {err}"
            );
        }

        /// A readable file must reach files.upload exactly once.
        #[tokio::test]
        async fn send_media_uploads_file_bytes() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/files.upload"))
                .respond_with(ok_json())
                .expect(1)
                .mount(&server)
                .await;

            let dir = tempfile::tempdir().unwrap();
            let file_path = dir.path().join("report.txt");
            std::fs::write(&file_path, b"attachment-bytes").unwrap();

            let channel = make_channel(&server.uri(), None);
            let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
            channel
                .send_media(
                    &file_path,
                    Some("caption"),
                    &response_ctx("C1", None, None),
                    &health,
                )
                .await
                .expect("upload must succeed");
        }

        /// `start_typing` must hand back a live keepalive handle whose
        /// initial trigger posts the thread status; a malformed context
        /// yields no handle.
        #[tokio::test]
        async fn start_typing_fires_thread_status_for_valid_context() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/assistant.threads.setStatus"))
                .and(body_json(serde_json::json!({
                    "channel_id": "C1",
                    "thread_ts": "7.7",
                    "status": "is typing...",
                })))
                .respond_with(ok_json())
                .expect(1..)
                .mount(&server)
                .await;

            let channel = make_channel(&server.uri(), None);
            assert!(
                channel
                    .start_typing(&serde_json::json!("not-an-object"))
                    .is_none(),
                "malformed context must not spawn a typing loop"
            );

            let handle = channel
                .start_typing(&response_ctx("C1", Some("7.7"), None))
                .expect("valid context must return a typing handle");
            let mut fired = false;
            for _ in 0..250 {
                if !server
                    .received_requests()
                    .await
                    .unwrap_or_default()
                    .is_empty()
                {
                    fired = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            drop(handle);
            assert!(fired, "initial typing trigger must hit the status endpoint");
        }
    }

    mod webhook {
        use super::*;
        use hmac::{Hmac, KeyInit, Mac};
        use sha2::Sha256;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        type HmacSha256 = Hmac<Sha256>;

        fn signed_headers(body: &str) -> HeaderMap {
            let ts = chrono::Utc::now().timestamp().to_string();
            let basestring = format!("v0:{ts}:{body}");
            let mut mac = HmacSha256::new_from_slice(SECRET.as_bytes()).unwrap();
            mac.update(basestring.as_bytes());
            let sig = format!("v0={}", hex::encode(mac.finalize().into_bytes()));
            let mut h = HeaderMap::new();
            h.insert("x-slack-request-timestamp", ts.parse().unwrap());
            h.insert("x-slack-signature", sig.parse().unwrap());
            h
        }

        fn envelope(text: &str, thread_ts: Option<&str>) -> String {
            let mut event = serde_json::json!({
                "type": "message",
                "user": "U_USER",
                "text": text,
                "ts": "1700000000.000100",
                "channel": "C1",
                "channel_type": "channel",
            });
            if let Some(t) = thread_ts {
                event["thread_ts"] = serde_json::Value::String(t.to_string());
            }
            serde_json::json!({
                "type": "event_callback",
                "token": "tok",
                "team_id": "T1",
                "event_id": "Ev1",
                "event": event,
            })
            .to_string()
        }

        /// Mount conversations.history returning the given (user, text) rows
        /// (newest first, matching Slack's API order) plus a permissive
        /// reactions.add mock for the ack reaction.
        async fn mount_history(server: &MockServer, rows: &[(&str, &str)]) {
            let messages: Vec<serde_json::Value> = rows
                .iter()
                .enumerate()
                .map(|(i, (user, text))| {
                    serde_json::json!({ "user": user, "text": text, "ts": format!("{i}.0") })
                })
                .collect();
            Mock::given(method("GET"))
                .and(path("/conversations.history"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({ "ok": true, "messages": messages })),
                )
                .mount(server)
                .await;
            Mock::given(method("POST"))
                .and(path("/reactions.add"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })),
                )
                .mount(server)
                .await;
        }

        async fn run_webhook(channel: &SlackChannel, body: &str) -> String {
            let config = cortex::config::Config::default();
            let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
            let ctx = WebhookContext {
                config: &config,
                health: &health,
            };
            let outcome = channel
                .handle_webhook(&signed_headers(body), body, &ctx)
                .await
                .expect("webhook must be accepted");
            match outcome {
                WebhookOutcome::Message { inbound, .. } => inbound.text,
                _ => panic!("expected a Message outcome"),
            }
        }

        /// A top-level @mention must inject the channel history above the
        /// current message, oldest first, each line as `<@user>: text`.
        /// History rows are long enough (>90 chars) that any arithmetic
        /// slip in the budget accumulator collapses the context.
        #[tokio::test]
        async fn top_level_mention_injects_history_oldest_first() {
            let server = MockServer::start().await;
            let t1 = "a".repeat(100);
            let t2 = "b".repeat(100);
            let t3 = "c".repeat(100);
            // Slack returns newest first: t1 is newest, t3 oldest.
            mount_history(&server, &[("U1", &t1), ("U2", &t2), ("U3", &t3)]).await;

            let channel = make_channel(&server.uri(), None);
            let text = run_webhook(&channel, &envelope("<@UOTHER> ping", None)).await;
            assert_eq!(
                text,
                format!(
                    "[Channel context]\n<@U3>: {t3}\n<@U2>: {t2}\n<@U1>: {t1}\n[Current message]\n<@UOTHER> ping"
                )
            );
        }

        /// The 8000-char budget must drop the lines that overflow it —
        /// and only those. Three 3000-char lines: the first two fit
        /// (6000), the third would reach 9000 and is cut.
        #[tokio::test]
        async fn history_injection_stops_at_char_budget() {
            let server = MockServer::start().await;
            // Each line is `<@Ux>: ` (7) + text (2992) + `\n` (1) = 3000 chars.
            let t1 = "a".repeat(2992);
            let t2 = "b".repeat(2992);
            let t3 = "c".repeat(2992);
            mount_history(&server, &[("U1", &t1), ("U2", &t2), ("U3", &t3)]).await;

            let channel = make_channel(&server.uri(), None);
            let text = run_webhook(&channel, &envelope("<@UOTHER> ping", None)).await;
            assert_eq!(
                text,
                format!(
                    "[Channel context]\n<@U3>: {t3}\n<@U2>: {t2}\n[Current message]\n<@UOTHER> ping"
                ),
                "the newest line (9000 chars cumulative) must be cut; the first two kept"
            );
        }

        /// A line landing exactly on the 8000-char budget is still within
        /// budget and must be kept — the cap is exclusive, not inclusive.
        #[tokio::test]
        async fn history_line_on_exact_budget_boundary_is_kept() {
            let server = MockServer::start().await;
            // Two lines of exactly 4000 chars: cumulative hits exactly 8000.
            let t1 = "a".repeat(3992);
            let t2 = "b".repeat(3992);
            mount_history(&server, &[("U1", &t1), ("U2", &t2)]).await;

            let channel = make_channel(&server.uri(), None);
            let text = run_webhook(&channel, &envelope("<@UOTHER> ping", None)).await;
            assert_eq!(
                text,
                format!(
                    "[Channel context]\n<@U2>: {t2}\n<@U1>: {t1}\n[Current message]\n<@UOTHER> ping"
                ),
                "a line that lands exactly on the budget must not be cut"
            );
        }

        /// `gateway.slack_channel_allowlist` gates the events path: a message
        /// from an unlisted channel is dropped before it reaches the agent; a
        /// listed channel and a DM (`D…`) both pass.
        #[tokio::test]
        async fn channel_allowlist_drops_unlisted_channels() {
            let server = MockServer::start().await;
            mount_history(&server, &[]).await;

            for (channel_id, expect_message) in [("C2", false), ("C1", true), ("D9", true)] {
                let mut channel = make_channel(&server.uri(), None);
                channel.config.gateway.slack_channel_allowlist = Some(vec!["C1".to_string()]);
                let mut body: serde_json::Value =
                    serde_json::from_str(&envelope("hello", None)).unwrap();
                body["event"]["channel"] = serde_json::json!(channel_id);
                let body = body.to_string();

                let config = cortex::config::Config::default();
                let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
                let ctx = WebhookContext {
                    config: &config,
                    health: &health,
                };
                let outcome = channel
                    .handle_webhook(&signed_headers(&body), &body, &ctx)
                    .await
                    .expect("webhook must be accepted");
                let got_message = matches!(outcome, WebhookOutcome::Message { .. });
                assert_eq!(got_message, expect_message, "channel {channel_id}");
            }
        }

        /// A threaded @mention already carries its own conversation context;
        /// injecting channel history there would duplicate it. Even with
        /// history available, the text must pass through untouched.
        #[tokio::test]
        async fn threaded_mention_skips_history_injection() {
            let server = MockServer::start().await;
            mount_history(&server, &[("U1", "available history")]).await;

            let channel = make_channel(&server.uri(), None);
            let text =
                run_webhook(&channel, &envelope("<@UOTHER> in thread", Some("1699.0"))).await;
            assert_eq!(text, "<@UOTHER> in thread");
        }
    }
}
