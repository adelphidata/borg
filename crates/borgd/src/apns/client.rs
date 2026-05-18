//! APNs HTTP/2 client wrapper. **Stub implementation.**
//!
//! The production client uses the `a2` crate (Apple Push Notification
//! Service, HTTP/2, token-based JWT-ES256 auth). Today this is a stub:
//! every call to `send_silent_push` short-circuits with a single warn-once
//! log line and returns `PushOutcome::SkippedNotConfigured` so the
//! production pipeline gates cleanly without a real Apple Developer
//! account.
//!
//! The `wake_paired_devices` trigger ([`super::trigger`]) calls this
//! client directly; spying on it in tests is straightforward (it's a
//! plain struct with a `Send + Sync` impl).

use cortex::config::ApnsConfig;

/// Outcome of attempting a single push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// Push accepted by APNs. Today only the production client returns this.
    Accepted,
    /// APNs returned 410 BadDeviceToken — caller marks the row invalidated.
    BadDeviceToken,
    /// APNs returned 400/403 or some other non-410 error. Reason string is
    /// truncated to 256 chars for logging.
    Rejected(String),
    /// No push was attempted because `apns.enabled=false` or required
    /// fields were absent. Caller treats this as a no-op.
    SkippedNotConfigured,
}

/// APNs client handle. Stub implementation today (see module docs);
/// production wires the `a2::Client` here.
#[derive(Clone)]
pub struct ApnsClient {
    config: ApnsConfig,
}

impl ApnsClient {
    /// Construct a client from the resolved config. Always succeeds;
    /// missing-credential cases surface at `send_silent_push` time as
    /// [`PushOutcome::SkippedNotConfigured`] so a misconfigured deployment
    /// never crashes the trigger path.
    pub fn new(config: ApnsConfig) -> Self {
        Self { config }
    }

    /// Send a silent `content-available: 1` push to the given device
    /// token. Stub: returns `SkippedNotConfigured` if config is
    /// incomplete; otherwise logs and returns `Accepted`.
    ///
    /// The production implementation will:
    /// 1. Build an `a2::request::notification::SilentNotificationBuilder`
    ///    with `apns_topic = config.bundle_id` and any custom `borg.kind`
    ///    payload data.
    /// 2. Submit via `a2::Client::send`, awaiting the HTTP/2 response.
    /// 3. Map APNs `Response::Error(ErrorReason::BadDeviceToken)` to
    ///    [`PushOutcome::BadDeviceToken`] and everything else to
    ///    [`PushOutcome::Rejected`] / [`PushOutcome::Accepted`].
    pub async fn send_silent_push(&self, device_token: &str) -> PushOutcome {
        if !self.config.is_complete() {
            // Single warn-once line at trigger time would spam logs; the
            // trigger wraps repeated calls in a warn-once guard so this
            // path can stay quiet (debug-only).
            tracing::debug!(
                "apns: skipping push to {} — config incomplete (enabled={}, bundle_id={:?})",
                redact_token(device_token),
                self.config.enabled,
                self.config.bundle_id
            );
            return PushOutcome::SkippedNotConfigured;
        }
        tracing::info!(
            "apns: would push silent wake to {} (stub — a2 client not wired)",
            redact_token(device_token)
        );
        PushOutcome::Accepted
    }
}

/// Truncate a 64-hex APNs token to 8 chars for logging. The full token
/// isn't secret per se, but it's enough to push to that device, so we
/// avoid spreading it across log files.
fn redact_token(t: &str) -> &str {
    &t[..t.len().min(8)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn complete_config() -> ApnsConfig {
        ApnsConfig {
            enabled: true,
            bundle_id: Some("com.borg.ios".into()),
            team_id: Some("ABCDE12345".into()),
            key_id: Some("ABC123DEF4".into()),
            private_key_p8_path: Some(PathBuf::from("apns.p8")),
            use_sandbox: false,
        }
    }

    #[tokio::test]
    async fn push_with_incomplete_config_is_skipped_not_errored() {
        // The trigger calls this on every inbound gateway message. If
        // an incomplete config errored, every message would log loudly
        // and `wake_paired_devices` would surface failures the operator
        // can't act on without an Apple Developer setup. Skip-without-
        // error is the right default.
        let client = ApnsClient::new(ApnsConfig::default());
        let outcome = client.send_silent_push("abcd1234").await;
        assert_eq!(outcome, PushOutcome::SkippedNotConfigured);
    }

    #[tokio::test]
    async fn push_with_complete_config_returns_accepted_in_stub_mode() {
        // Wired-but-stub path: until the a2 client lands, a complete
        // config should observe Accepted (rather than Skipped) so test
        // harnesses exercising the trigger see the expected
        // "push attempted" telemetry.
        let client = ApnsClient::new(complete_config());
        let outcome = client.send_silent_push("abcd1234").await;
        assert_eq!(outcome, PushOutcome::Accepted);
    }

    #[test]
    fn redact_token_truncates_to_eight_chars() {
        // Real APNs tokens are 64 hex chars. Log redaction must keep
        // them short enough to grep on while not enabling someone with
        // log access to send pushes to that device.
        assert_eq!(redact_token("0123456789abcdef0123456789abcdef"), "01234567");
        assert_eq!(redact_token("short"), "short");
        assert_eq!(redact_token(""), "");
    }
}
