//! Outbound media egress: deliver `[MEDIA:<path>]` reply attachments.
//!
//! The agent requests attachments by putting `[MEDIA:/path]` on its own line
//! (parsed by `cortex::agent::media_markers`). This module owns the delivery
//! half of that promise for gateway channels: validate each path against the
//! security blocklist, hand it to the channel's `send_media`, and — because a
//! reported attachment must never silently vanish — turn every refusal,
//! unsupported channel, or send failure into a short notice appended to the
//! text reply.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::channel::channel_trait::NativeChannel;
use crate::health::ChannelHealthRegistry;

/// Human-readable notice for an attachment that could not be sent. Appended
/// to the text reply so the user learns the file exists and why it is
/// missing (mirror of the inbound `media_failure` placeholders).
pub(crate) fn attachment_notice(path: &Path, reason: &str) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    format!("[attachment \"{name}\" could not be sent: {reason}]")
}

/// Compose the final outbound text from the marker-stripped reply and any
/// attachment-failure notices.
///
/// Returns `None` when there is nothing left to send as text — a marker-only
/// reply whose attachments all went out (the attachments themselves carry the
/// content). Notices always force a text send: a failed attachment must
/// never be silent.
pub(crate) fn compose_reply_text(clean_text: &str, notices: &[String]) -> Option<String> {
    let mut text = clean_text.trim_end().to_string();
    if !notices.is_empty() {
        if !text.trim().is_empty() {
            text.push_str("\n\n");
        } else {
            text.clear();
        }
        text.push_str(&notices.join("\n"));
    }
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Validate and deliver each extracted media path on `channel`.
///
/// Returns `(any_sent, notices)`: whether at least one attachment was
/// delivered, plus one notice per path that was refused (security/validation),
/// unsupported (channel has no attachment transport), or failed to send. Every
/// non-sent path is also logged via `tracing::warn!` — nothing is dropped
/// silently.
///
/// When `text_is_empty` is set (the marker-stripped reply has no text), each
/// attachment is sent with its filename as a minimal caption so the message
/// is not a bare uncaptioned upload.
pub(crate) async fn deliver_reply_media(
    channel: &dyn NativeChannel,
    paths: &[PathBuf],
    security: &cortex::config::SecurityConfig,
    text_is_empty: bool,
    response_context: &serde_json::Value,
    health: &Arc<RwLock<ChannelHealthRegistry>>,
) -> (bool, Vec<String>) {
    let channel_name = channel.names()[0].to_string();
    let mut any_sent = false;
    let mut notices = Vec::new();

    for path in paths {
        if let Err(reason) = cortex::agent::media_markers::validate_media_path(path, security) {
            tracing::warn!(
                channel = %channel_name,
                path = %path.display(),
                %reason,
                "refusing [MEDIA:] attachment"
            );
            notices.push(attachment_notice(path, &reason));
            continue;
        }
        if !channel.supports_media_attachments() {
            tracing::warn!(
                channel = %channel_name,
                path = %path.display(),
                "channel has no attachment support; sending notice instead"
            );
            notices.push(attachment_notice(
                path,
                "this channel does not support attachments",
            ));
            continue;
        }
        let caption_owned = text_is_empty
            .then(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .flatten();
        match channel
            .send_media(path, caption_owned.as_deref(), response_context, health)
            .await
        {
            Ok(()) => any_sent = true,
            Err(e) => {
                tracing::warn!(
                    channel = %channel_name,
                    path = %path.display(),
                    error = %format!("{e:#}"),
                    "failed to send [MEDIA:] attachment"
                );
                notices.push(attachment_notice(path, "delivery failed"));
            }
        }
    }

    (any_sent, notices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use axum::http::HeaderMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::channel::channel_trait::{WebhookContext, WebhookOutcome};

    /// Mock channel with configurable attachment behavior.
    struct MediaMockChannel {
        supports: bool,
        fail_send: bool,
        send_count: AtomicU32,
    }

    #[async_trait]
    impl NativeChannel for MediaMockChannel {
        fn names(&self) -> Vec<&str> {
            vec!["mock"]
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
        fn supports_media_attachments(&self) -> bool {
            self.supports
        }
        async fn send_media(
            &self,
            _path: &Path,
            _caption: Option<&str>,
            _ctx: &serde_json::Value,
            _health: &Arc<RwLock<ChannelHealthRegistry>>,
        ) -> Result<()> {
            self.send_count.fetch_add(1, Ordering::SeqCst);
            if self.fail_send {
                anyhow::bail!("api unavailable")
            }
            Ok(())
        }
    }

    fn health() -> Arc<RwLock<ChannelHealthRegistry>> {
        Arc::new(RwLock::new(ChannelHealthRegistry::new()))
    }

    fn temp_media_file(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, b"bytes").expect("write media file");
        p
    }

    #[test]
    fn compose_appends_notices_and_skips_empty() {
        let notice = attachment_notice(Path::new("/tmp/chart.png"), "delivery failed");

        // Text + notice: notice lands on its own paragraph.
        let composed = compose_reply_text("Here is the chart.", std::slice::from_ref(&notice))
            .expect("text with notices must send");
        assert!(composed.starts_with("Here is the chart."));
        assert!(
            composed.ends_with("[attachment \"chart.png\" could not be sent: delivery failed]"),
            "notice must name the file and reason: {composed}"
        );

        // Empty text + notice: the notice alone must still go out — a failed
        // attachment is never silent.
        assert_eq!(
            compose_reply_text("", std::slice::from_ref(&notice)),
            Some(notice)
        );

        // Empty text, no notices (marker-only reply, all attachments sent):
        // nothing left to send as text.
        assert_eq!(compose_reply_text("   \n", &[]), None);

        // Plain text with no attachments passes through.
        assert_eq!(compose_reply_text("hi", &[]), Some("hi".to_string()));
    }

    /// Send failure produces a fallback notice; a successful sibling still
    /// counts as sent. This is the "NEVER silently drop" decision path.
    #[tokio::test]
    async fn send_failure_yields_notice_not_silence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = temp_media_file(&dir, "report.pdf");
        let channel = MediaMockChannel {
            supports: true,
            fail_send: true,
            send_count: AtomicU32::new(0),
        };
        let (any_sent, notices) = deliver_reply_media(
            &channel,
            std::slice::from_ref(&file),
            &cortex::config::SecurityConfig::default(),
            false,
            &serde_json::json!({}),
            &health(),
        )
        .await;
        assert!(!any_sent);
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].contains("report.pdf") && notices[0].contains("delivery failed"),
            "notice must name the file and the failure: {}",
            notices[0]
        );
    }

    #[tokio::test]
    async fn unsupported_channel_notices_without_calling_send() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = temp_media_file(&dir, "chart.png");
        let channel = MediaMockChannel {
            supports: false,
            fail_send: false,
            send_count: AtomicU32::new(0),
        };
        let (any_sent, notices) = deliver_reply_media(
            &channel,
            std::slice::from_ref(&file),
            &cortex::config::SecurityConfig::default(),
            false,
            &serde_json::json!({}),
            &health(),
        )
        .await;
        assert!(!any_sent);
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("does not support attachments"));
        assert_eq!(
            channel.send_count.load(Ordering::SeqCst),
            0,
            "send_media must not be called on an unsupported channel"
        );
    }

    /// A blocked path must be refused before `send_media` is ever consulted —
    /// the injected `[MEDIA:~/.ssh/id_rsa]` case at the gateway seam.
    #[tokio::test]
    async fn blocked_path_is_refused_and_never_sent() {
        let home = dirs::home_dir().expect("home dir");
        let key = home.join(".ssh").join("id_rsa");
        let channel = MediaMockChannel {
            supports: true,
            fail_send: false,
            send_count: AtomicU32::new(0),
        };
        let (any_sent, notices) = deliver_reply_media(
            &channel,
            std::slice::from_ref(&key),
            &cortex::config::SecurityConfig::default(),
            false,
            &serde_json::json!({}),
            &health(),
        )
        .await;
        assert!(!any_sent);
        assert_eq!(
            channel.send_count.load(Ordering::SeqCst),
            0,
            "a blocked path must never reach send_media"
        );
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].contains("blocked by security policy"),
            "refusal must cite the policy: {}",
            notices[0]
        );
    }

    #[tokio::test]
    async fn successful_send_produces_no_notice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = temp_media_file(&dir, "a.png");
        let b = temp_media_file(&dir, "b.pdf");
        let channel = MediaMockChannel {
            supports: true,
            fail_send: false,
            send_count: AtomicU32::new(0),
        };
        let (any_sent, notices) = deliver_reply_media(
            &channel,
            &[a, b],
            &cortex::config::SecurityConfig::default(),
            false,
            &serde_json::json!({}),
            &health(),
        )
        .await;
        assert!(any_sent);
        assert!(notices.is_empty(), "clean sends must not add notices");
        assert_eq!(channel.send_count.load(Ordering::SeqCst), 2);
    }
}
