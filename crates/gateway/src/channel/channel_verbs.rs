//! Verb-shaped per-channel primitive trait.
//!
//! `NativeChannel` (in `channel_trait.rs`) captures the full webhook→agent→send
//! dispatch flow. This trait sits one layer below it: the small set of primitive
//! verbs every native channel implements, regardless of how the dispatch flow
//! invokes them.
//!
//! Today the eight native channels (telegram, slack, discord, teams, google_chat,
//! signal, twilio, imessage) each carry a hand-rolled `verify`, `send`, and a
//! per-channel typing-keepalive interval. Capturing those as a trait lets shared
//! infrastructure (rate-limit logging, metrics, integration test scaffolding)
//! depend on the abstraction instead of branching on channel name.
//!
//! Migration is incremental: implementing this trait is additive and does not
//! disturb any existing call site. Telegram is migrated as proof; the other
//! seven implementations follow in later passes.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use axum::http::HeaderMap;

/// Default keepalive interval for channels that do not override it.
///
/// Per-channel constants in `crate::constants` (e.g.
/// `TELEGRAM_TYPING_KEEPALIVE_INTERVAL`) take precedence; the default exists
/// only so a stub or test impl does not have to invent a number.
pub const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// Verb-shaped per-channel primitives.
///
/// Implementors expose:
/// - a stable `name()` (used for logging, metrics, registry lookup),
/// - the `keepalive_interval()` for typing indicators,
/// - signature `verify()` for inbound webhooks,
/// - text `send()` to a channel-specific `Target`.
///
/// `Target` is associated rather than a `serde_json::Value` because the existing
/// per-channel send paths are already strongly typed (Telegram chat_id is `i64`,
/// Discord channel_id is `String`, Twilio uses a `(from, to)` pair). Forcing
/// every send through JSON would erase that and add encode/decode overhead on
/// the hot path.
#[async_trait]
pub trait Channel: Send + Sync {
    /// Channel-specific destination for `send`.
    type Target: Send + Sync;

    /// Stable channel identifier (e.g. `"telegram"`). Must match the name used
    /// by `NativeChannel::names()` and the gateway routing layer so logs and
    /// metrics correlate.
    fn name(&self) -> &'static str;

    /// How often to refresh a typing indicator while the agent is working.
    /// Defaults to `DEFAULT_KEEPALIVE_INTERVAL`; channels with a documented
    /// platform-specific cadence override this to match the per-channel
    /// constant in `crate::constants`.
    fn keepalive_interval(&self) -> Duration {
        DEFAULT_KEEPALIVE_INTERVAL
    }

    /// Verify an inbound webhook signature. Implementations decide which
    /// headers are required and what HMAC / Ed25519 / token scheme to apply.
    /// Returns `Ok(())` on a valid request, `Err` on any verification failure.
    fn verify(&self, headers: &HeaderMap, body: &[u8]) -> Result<()>;

    /// Deliver a plain-text message to `target`. Channels that need richer
    /// delivery (replies, threads, attachments) continue to expose those on
    /// their concrete API client; this is the lowest common denominator that
    /// every native channel supports.
    async fn send(&self, target: &Self::Target, text: &str) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Stub impl that records every verify/send call so the test can assert
    /// the trait actually drives the underlying primitive — not just that it
    /// type-checks.
    struct StubChannel {
        verify_calls: Arc<AtomicUsize>,
        sent: Arc<std::sync::Mutex<Vec<(u64, String)>>>,
        accept_signature: bool,
    }

    #[async_trait]
    impl Channel for StubChannel {
        type Target = u64;

        fn name(&self) -> &'static str {
            "stub"
        }

        fn keepalive_interval(&self) -> Duration {
            Duration::from_secs(7)
        }

        fn verify(&self, _headers: &HeaderMap, _body: &[u8]) -> Result<()> {
            self.verify_calls.fetch_add(1, Ordering::SeqCst);
            if self.accept_signature {
                Ok(())
            } else {
                Err(anyhow::anyhow!("stub: bad signature"))
            }
        }

        async fn send(&self, target: &Self::Target, text: &str) -> Result<()> {
            self.sent.lock().unwrap().push((*target, text.to_string()));
            Ok(())
        }
    }

    fn make_stub(accept: bool) -> StubChannel {
        StubChannel {
            verify_calls: Arc::new(AtomicUsize::new(0)),
            sent: Arc::new(std::sync::Mutex::new(Vec::new())),
            accept_signature: accept,
        }
    }

    #[test]
    fn default_keepalive_is_used_when_not_overridden() {
        struct Bare;

        #[async_trait]
        impl Channel for Bare {
            type Target = ();
            fn name(&self) -> &'static str {
                "bare"
            }
            fn verify(&self, _h: &HeaderMap, _b: &[u8]) -> Result<()> {
                Ok(())
            }
            async fn send(&self, _t: &Self::Target, _x: &str) -> Result<()> {
                Ok(())
            }
        }

        assert_eq!(Bare.keepalive_interval(), DEFAULT_KEEPALIVE_INTERVAL);
        assert_ne!(Bare.keepalive_interval(), Duration::from_secs(7));
    }

    #[test]
    fn override_keepalive_wins_over_default() {
        let stub = make_stub(true);
        assert_eq!(stub.keepalive_interval(), Duration::from_secs(7));
    }

    #[test]
    fn verify_invokes_implementation_and_propagates_failure() {
        let stub = make_stub(false);
        let calls = stub.verify_calls.clone();
        let result = stub.verify(&HeaderMap::new(), b"some body bytes");
        assert!(result.is_err(), "verify must surface the impl's error");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "trait dispatch must call the impl exactly once"
        );
    }

    #[tokio::test]
    async fn send_routes_payload_to_typed_target() {
        let stub = make_stub(true);
        let sent = stub.sent.clone();
        stub.send(&42, "hello").await.unwrap();
        stub.send(&7, "world").await.unwrap();
        let captured = sent.lock().unwrap().clone();
        assert_eq!(
            captured,
            vec![(42_u64, "hello".to_string()), (7_u64, "world".to_string())]
        );
    }

    #[tokio::test]
    async fn name_is_stable_across_calls() {
        let stub = make_stub(true);
        assert_eq!(stub.name(), "stub");
        assert_eq!(stub.name(), "stub");
    }
}
