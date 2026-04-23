//! End-to-end integration tests for the Slack webhook pipeline.
//!
//! These tests drive `handle_slack_webhook` with realistic JSON payloads to
//! verify the full flow: signature verification → dedup → parse → InboundMessage.
//!
//! They act as regression tests for specific correctness bugs:
//!   * `app_mention` + `message` double-processing (channel, ts) dedup
//!   * `file_share` subtype drop
//!   * bot `<@BOTID>` mention stripping

#![allow(
    clippy::approx_constant,
    clippy::assertions_on_constants,
    clippy::const_is_empty,
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::identity_op,
    clippy::items_after_test_module,
    clippy::len_zero,
    clippy::manual_range_contains,
    clippy::needless_borrow,
    clippy::needless_collect,
    clippy::redundant_clone,
    clippy::redundant_closure_for_method_calls,
    clippy::uninlined_format_args,
    clippy::unnecessary_cast,
    clippy::unnecessary_map_or,
    clippy::unwrap_used,
    clippy::useless_format,
    clippy::useless_vec
)]

use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue};
use borg_gateway::slack::dedup::EventDeduplicator;
use borg_gateway::slack::echo::EchoCache;
use borg_gateway::slack::{handle_slack_webhook, SlackWebhookResult};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::sync::Mutex;

type HmacSha256 = Hmac<Sha256>;

// ── helpers ─────────────────────────────────────────────────────────────────

fn sign(secret: &str, timestamp: &str, body: &str) -> String {
    let basestring = format!("v0:{timestamp}:{body}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(basestring.as_bytes());
    format!("v0={}", hex::encode(mac.finalize().into_bytes()))
}

fn signed_headers(secret: &str, body: &str) -> HeaderMap {
    let ts = chrono::Utc::now().timestamp().to_string();
    let sig = sign(secret, &ts, body);
    let mut h = HeaderMap::new();
    h.insert(
        "x-slack-request-timestamp",
        HeaderValue::from_str(&ts).unwrap(),
    );
    h.insert("x-slack-signature", HeaderValue::from_str(&sig).unwrap());
    h
}

/// Build a message event_callback envelope with the given event_id.
fn message_envelope(event_id: &str, channel: &str, ts: &str, text: &str) -> String {
    format!(
        r#"{{"type":"event_callback","token":"tok","team_id":"T1","event_id":"{event_id}","event":{{"type":"message","user":"U_USER","text":"{text}","ts":"{ts}","channel":"{channel}","channel_type":"channel"}}}}"#
    )
}

/// Build an app_mention event_callback envelope with the given event_id, sharing
/// a channel and ts with a paired `message` event (the realistic Slack pattern).
fn app_mention_envelope(event_id: &str, channel: &str, ts: &str, text: &str) -> String {
    format!(
        r#"{{"type":"event_callback","token":"tok","team_id":"T1","event_id":"{event_id}","event":{{"type":"app_mention","user":"U_USER","text":"{text}","ts":"{ts}","channel":"{channel}","channel_type":"channel"}}}}"#
    )
}

// ── URL verification ────────────────────────────────────────────────────────

#[tokio::test]
async fn url_verification_returns_challenge() {
    let body = r#"{"type":"url_verification","token":"tok","challenge":"abc123xyz"}"#;
    let result = handle_slack_webhook(&HeaderMap::new(), body, None, None, None, None)
        .await
        .expect("should succeed");
    match result {
        SlackWebhookResult::Challenge(c) => assert_eq!(c, "abc123xyz"),
        _ => panic!(
            "expected Challenge, got {:?}",
            std::mem::discriminant(&result)
        ),
    }
}

#[tokio::test]
async fn signature_verification_rejects_bad_signature() {
    let body = message_envelope("Ev1", "C1", "111.222", "hello");
    let headers = signed_headers("wrong-secret", &body);
    let result = handle_slack_webhook(&headers, &body, Some("real-secret"), None, None, None).await;
    assert!(result.is_err(), "bad signature must fail");
}

#[tokio::test]
async fn signature_verification_accepts_valid_signature() {
    let secret = "s3cret";
    let body = message_envelope("Ev1", "C1", "111.222", "hello");
    let headers = signed_headers(secret, &body);
    let result = handle_slack_webhook(&headers, &body, Some(secret), None, None, None)
        .await
        .expect("valid signature should pass");
    assert!(matches!(result, SlackWebhookResult::Message(_)));
}

// ── event_id dedup (existing behavior, regression guard) ───────────────────

#[tokio::test]
async fn duplicate_event_id_is_skipped() {
    let dedup = Arc::new(Mutex::new(EventDeduplicator::new()));
    let body = message_envelope("Ev1", "C1", "111.222", "hello");

    let first = handle_slack_webhook(&HeaderMap::new(), &body, None, Some(&dedup), None, None)
        .await
        .unwrap();
    assert!(matches!(first, SlackWebhookResult::Message(_)));

    let second = handle_slack_webhook(&HeaderMap::new(), &body, None, Some(&dedup), None, None)
        .await
        .unwrap();
    assert!(matches!(second, SlackWebhookResult::Skip));
}

// ── (channel, ts) dedup — regression guard for app_mention/message race ───

/// REGRESSION: Slack delivers both `message` and `app_mention` events for the
/// same user @mention in a channel. They share channel+ts but have different
/// event_ids, so event_id dedup alone wouldn't catch them — the bot would reply
/// twice. The (channel, ts) dedup keeps this to a single response.
#[tokio::test]
async fn message_then_app_mention_same_channel_ts_is_deduped() {
    let dedup = Arc::new(Mutex::new(EventDeduplicator::new()));

    // First: the plain `message` event arrives.
    let msg = message_envelope("EvMsg", "C1", "111.222", "hi");
    let r1 = handle_slack_webhook(&HeaderMap::new(), &msg, None, Some(&dedup), None, None)
        .await
        .unwrap();
    assert!(matches!(r1, SlackWebhookResult::Message(_)));

    // Then: Slack also sends an `app_mention` with a DIFFERENT event_id but the
    // SAME channel+ts. This must be deduped so the agent doesn't respond twice.
    let mention = app_mention_envelope("EvMention", "C1", "111.222", "hi");
    let r2 = handle_slack_webhook(&HeaderMap::new(), &mention, None, Some(&dedup), None, None)
        .await
        .unwrap();
    assert!(
        matches!(r2, SlackWebhookResult::Skip),
        "app_mention with same (channel, ts) must be skipped"
    );
}

#[tokio::test]
async fn app_mention_then_message_same_channel_ts_is_deduped() {
    // The reverse ordering — Slack may deliver app_mention first.
    let dedup = Arc::new(Mutex::new(EventDeduplicator::new()));

    let mention = app_mention_envelope("EvMention", "C1", "111.222", "hi");
    let r1 = handle_slack_webhook(&HeaderMap::new(), &mention, None, Some(&dedup), None, None)
        .await
        .unwrap();
    assert!(matches!(r1, SlackWebhookResult::Message(_)));

    let msg = message_envelope("EvMsg", "C1", "111.222", "hi");
    let r2 = handle_slack_webhook(&HeaderMap::new(), &msg, None, Some(&dedup), None, None)
        .await
        .unwrap();
    assert!(matches!(r2, SlackWebhookResult::Skip));
}

#[tokio::test]
async fn different_channel_same_ts_is_not_deduped() {
    let dedup = Arc::new(Mutex::new(EventDeduplicator::new()));

    let a = message_envelope("EvA", "C1", "111.222", "hi");
    let b = message_envelope("EvB", "C2", "111.222", "hi");

    let r1 = handle_slack_webhook(&HeaderMap::new(), &a, None, Some(&dedup), None, None)
        .await
        .unwrap();
    let r2 = handle_slack_webhook(&HeaderMap::new(), &b, None, Some(&dedup), None, None)
        .await
        .unwrap();

    assert!(matches!(r1, SlackWebhookResult::Message(_)));
    assert!(
        matches!(r2, SlackWebhookResult::Message(_)),
        "same ts in different channels are distinct events"
    );
}

// ── file_share subtype — regression guard for dropped file messages ───────

/// REGRESSION: Prior to the fix, `parse.rs` filtered out the `file_share`
/// subtype entirely, which silently dropped every user file upload (since
/// `file_share` is exactly the subtype Slack uses when a user uploads a file,
/// with or without a caption).
#[tokio::test]
async fn file_share_with_caption_flows_through() {
    let body = r#"{
        "type": "event_callback",
        "token": "tok",
        "team_id": "T1",
        "event_id": "EvFile",
        "event": {
            "type": "message",
            "subtype": "file_share",
            "user": "U_USER",
            "text": "please review",
            "ts": "111.222",
            "channel": "C1",
            "channel_type": "channel",
            "files": [{
                "id": "F1",
                "name": "report.pdf",
                "mimetype": "application/pdf",
                "url_private": "https://files.slack.com/files-pri/T1-F1/report.pdf",
                "size": 1024,
                "filetype": "pdf"
            }]
        }
    }"#;

    let result = handle_slack_webhook(&HeaderMap::new(), body, None, None, None, None)
        .await
        .unwrap();

    match result {
        SlackWebhookResult::Message(msg) => {
            assert_eq!(msg.text, "please review");
            assert_eq!(msg.attachments.len(), 1);
            assert_eq!(msg.attachments[0].mime_type, "application/pdf");
            assert_eq!(msg.attachments[0].filename.as_deref(), Some("report.pdf"));
        }
        _ => panic!("file_share with caption must be delivered to the agent"),
    }
}

#[tokio::test]
async fn file_share_with_empty_caption_still_flows_through() {
    let body = r#"{
        "type": "event_callback",
        "token": "tok",
        "team_id": "T1",
        "event_id": "EvFile2",
        "event": {
            "type": "message",
            "subtype": "file_share",
            "user": "U_USER",
            "text": "",
            "ts": "111.222",
            "channel": "C1",
            "channel_type": "im",
            "files": [{
                "id": "F2",
                "name": "pic.png",
                "mimetype": "image/png",
                "url_private": "https://files.slack.com/files-pri/T1-F2/pic.png",
                "size": 2048,
                "filetype": "png"
            }]
        }
    }"#;

    let result = handle_slack_webhook(&HeaderMap::new(), body, None, None, None, None)
        .await
        .unwrap();

    match result {
        SlackWebhookResult::Message(msg) => {
            assert!(msg.text.is_empty());
            assert_eq!(msg.attachments.len(), 1);
            assert_eq!(msg.peer_kind.as_deref(), Some("direct"));
        }
        _ => panic!("empty-caption file upload must still be delivered"),
    }
}

// ── bot mention stripping — regression guard for prompt pollution ─────────

/// REGRESSION: Prior to the fix, `<@U0BOT> text` was forwarded to the agent
/// verbatim, polluting the prompt. The strip happens in `parse_event` when
/// `bot_user_id` is set.
#[tokio::test]
async fn channel_mention_strips_bot_id_token() {
    let body = r#"{
        "type": "event_callback",
        "token": "tok",
        "team_id": "T1",
        "event_id": "EvMen",
        "event": {
            "type": "app_mention",
            "user": "U_USER",
            "text": "<@U0BOT> what's the weather?",
            "ts": "111.222",
            "channel": "C1",
            "channel_type": "channel"
        }
    }"#;

    let result = handle_slack_webhook(&HeaderMap::new(), body, None, None, Some("U0BOT"), None)
        .await
        .unwrap();

    match result {
        SlackWebhookResult::Message(msg) => {
            assert_eq!(msg.text, "what's the weather?");
        }
        _ => panic!("expected message with stripped mention"),
    }
}

#[tokio::test]
async fn mention_display_form_is_also_stripped() {
    let body = r#"{
        "type": "event_callback",
        "token": "tok",
        "team_id": "T1",
        "event_id": "EvMen2",
        "event": {
            "type": "app_mention",
            "user": "U_USER",
            "text": "<@U0BOT|borg> ping",
            "ts": "111.222",
            "channel": "C1",
            "channel_type": "channel"
        }
    }"#;

    let result = handle_slack_webhook(&HeaderMap::new(), body, None, None, Some("U0BOT"), None)
        .await
        .unwrap();

    match result {
        SlackWebhookResult::Message(msg) => assert_eq!(msg.text, "ping"),
        _ => panic!("expected message"),
    }
}

// ── existing filter behavior (regression guards) ──────────────────────────

#[tokio::test]
async fn bot_message_is_skipped() {
    let body = r#"{
        "type": "event_callback",
        "token": "tok",
        "team_id": "T1",
        "event_id": "EvBot",
        "event": {
            "type": "message",
            "user": "U_BOT",
            "text": "hi from bot",
            "ts": "111.222",
            "channel": "C1",
            "bot_id": "B1"
        }
    }"#;
    let result = handle_slack_webhook(&HeaderMap::new(), body, None, None, None, None)
        .await
        .unwrap();
    assert!(matches!(result, SlackWebhookResult::Skip));
}

#[tokio::test]
async fn own_user_id_is_skipped_via_echo_detection() {
    // Defense in depth: a message from our own bot user id must be skipped
    // even if `bot_id` isn't set.
    let body = r#"{
        "type": "event_callback",
        "token": "tok",
        "team_id": "T1",
        "event_id": "EvSelf",
        "event": {
            "type": "message",
            "user": "U0BOT",
            "text": "self message",
            "ts": "111.222",
            "channel": "C1"
        }
    }"#;
    let result = handle_slack_webhook(&HeaderMap::new(), body, None, None, Some("U0BOT"), None)
        .await
        .unwrap();
    assert!(matches!(result, SlackWebhookResult::Skip));
}

#[tokio::test]
async fn echo_cache_filters_recently_sent_text() {
    let echo = Arc::new(Mutex::new(EchoCache::new()));
    echo.lock().await.remember("hello");

    let body = message_envelope("EvEcho", "C1", "111.222", "hello");
    let result = handle_slack_webhook(&HeaderMap::new(), &body, None, None, None, Some(&echo))
        .await
        .unwrap();
    assert!(matches!(result, SlackWebhookResult::Skip));
}

#[tokio::test]
async fn message_changed_subtype_is_skipped() {
    let body = r#"{
        "type": "event_callback",
        "token": "tok",
        "team_id": "T1",
        "event_id": "EvEdit",
        "event": {
            "type": "message",
            "subtype": "message_changed",
            "user": "U_USER",
            "text": "edited",
            "ts": "111.222",
            "channel": "C1"
        }
    }"#;
    let result = handle_slack_webhook(&HeaderMap::new(), body, None, None, None, None)
        .await
        .unwrap();
    assert!(matches!(result, SlackWebhookResult::Skip));
}
