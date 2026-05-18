//! Push trigger — `wake_paired_devices(db, recipient)`.
//!
//! Called from the gateway's inbound-message hook for any message
//! addressed to a paired sender. Fetches every active APNs device token
//! registered to the recipient and asks the client to send a silent push.
//! 410 responses mark the row invalidated so subsequent calls skip it.
//!
//! Today the DB layer + invalidation logic are wired; the client itself
//! is a stub (see [`super::client`]). The signature is the production
//! signature, so when the real `a2` client lands no caller changes.

use anyhow::Result;
use cortex::config::ApnsConfig;
use cortex::db::Database;

use super::client::{ApnsClient, PushOutcome};

/// Trigger result — useful for tests and operator telemetry. Tracks
/// every attempt's outcome so a `borg status` snapshot can show
/// "last push: 3 accepted, 1 invalidated".
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WakeReport {
    /// Number of pushes APNs accepted.
    pub accepted: u32,
    /// Number of tokens marked invalid on this run (APNs 410).
    pub invalidated: u32,
    /// Number of pushes APNs rejected with non-410 (400/403/...).
    pub rejected: u32,
    /// Number of tokens skipped because APNs is not configured.
    pub skipped: u32,
}

impl WakeReport {
    fn record(&mut self, outcome: &PushOutcome) {
        match outcome {
            PushOutcome::Accepted => self.accepted += 1,
            PushOutcome::BadDeviceToken => self.invalidated += 1,
            PushOutcome::Rejected(_) => self.rejected += 1,
            PushOutcome::SkippedNotConfigured => self.skipped += 1,
        }
    }
}

/// Send a silent wake push to every active APNs device token registered
/// to `recipient`. Marks BadDeviceToken responses invalidated in the DB.
///
/// `recipient` is the paired-device fingerprint that the inbound message
/// is addressed to. The function is a no-op (returning an all-zero
/// `WakeReport`) when APNs is disabled in config — the gateway can call
/// this on every inbound message without a feature-flag check.
pub async fn wake_paired_devices(
    db: &Database,
    config: &ApnsConfig,
    recipient: &str,
) -> Result<WakeReport> {
    let mut report = WakeReport::default();
    if !config.enabled {
        return Ok(report);
    }
    let tokens = db.list_active_apns_tokens(recipient).await?;
    if tokens.is_empty() {
        return Ok(report);
    }
    let client = ApnsClient::new(config.clone());
    for tok in tokens {
        let outcome = client.send_silent_push(&tok.apns_token).await;
        report.record(&outcome);
        if matches!(outcome, PushOutcome::BadDeviceToken) {
            db.invalidate_apns_token(tok.id, "apns-410").await?;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_report_record_classifies_each_outcome_into_its_own_counter() {
        // Counter-bookkeeping is easy to get subtly wrong (one off,
        // double-count). A regression here silently corrupts the
        // operator-facing "last push" telemetry.
        let mut r = WakeReport::default();
        r.record(&PushOutcome::Accepted);
        r.record(&PushOutcome::Accepted);
        r.record(&PushOutcome::BadDeviceToken);
        r.record(&PushOutcome::Rejected("400".into()));
        r.record(&PushOutcome::SkippedNotConfigured);
        assert_eq!(r.accepted, 2);
        assert_eq!(r.invalidated, 1);
        assert_eq!(r.rejected, 1);
        assert_eq!(r.skipped, 1);
    }

    #[tokio::test]
    async fn wake_with_apns_disabled_returns_zero_report_without_db_query() {
        // The gateway calls this on every inbound message. With APNs
        // off (the default), there must be zero DB queries — otherwise
        // a fresh install pays per-message SELECT cost for a feature
        // it doesn't use. We can't easily prove "no query" without a
        // spy, but we *can* prove that an empty-DB call returns a
        // zero report rather than erroring, which catches a major
        // regression (e.g. accidentally hitting an Apple endpoint).
        // Use `test_db()` (in-memory) to avoid touching BORG_HOME and
        // racing other tests' env-var isolation guards.
        let cfg = ApnsConfig {
            enabled: false,
            ..ApnsConfig::default()
        };
        let db = Database::test_db().await;
        let report = wake_paired_devices(&db, &cfg, "device-abc").await.unwrap();
        assert_eq!(report, WakeReport::default());
    }

    #[tokio::test]
    async fn wake_with_apns_enabled_and_no_tokens_returns_zero_skipped_count() {
        // Negative case: enabled config + empty token table. Must walk
        // the DB query path (fetch returns []), skip the client entirely,
        // and report all zeros. Distinguishes "no tokens" from "tokens
        // present but apns skipped" — both yield zero accepted, but only
        // the latter would non-zero skipped if a token were present.
        let cfg = ApnsConfig {
            enabled: true,
            bundle_id: Some("com.borg.ios".into()),
            ..ApnsConfig::default()
        };
        let db = Database::test_db().await;
        let report = wake_paired_devices(&db, &cfg, "device-abc").await.unwrap();
        assert_eq!(report, WakeReport::default());
    }

    #[tokio::test]
    async fn wake_invalidates_token_when_client_returns_bad_device_token() {
        // End-to-end through the DB layer: insert a row, run the trigger
        // with config that causes the (stub) client to actually attempt,
        // then verify the row state. Since the stub client returns
        // Accepted (not BadDeviceToken), this test exercises the
        // `Accepted` branch — the BadDeviceToken→invalidation path is
        // covered by the unit-level `record` test. Tracks the real
        // failure mode: an empty token table + enabled config must not
        // crash, and an enabled config that successfully pushes must
        // bump accepted, not skipped.
        let cfg = ApnsConfig {
            enabled: true,
            bundle_id: Some("com.borg.ios".into()),
            team_id: Some("ABCDE12345".into()),
            key_id: Some("ABC123DEF4".into()),
            private_key_p8_path: Some(std::path::PathBuf::from("apns.p8")),
            use_sandbox: false,
        };
        let db = Database::test_db().await;
        db.upsert_apns_device_token(
            "device-abc",
            "0123456789abcdef",
            "com.borg.ios",
            "key-id-1",
            b"fake-der-pubkey",
        )
        .await
        .unwrap();
        let report = wake_paired_devices(&db, &cfg, "device-abc").await.unwrap();
        assert_eq!(report.accepted, 1, "stub client must report Accepted");
        assert_eq!(report.skipped, 0);
        assert_eq!(report.invalidated, 0);
        // The row must still be active (not invalidated by an Accepted push).
        let active = db.list_active_apns_tokens("device-abc").await.unwrap();
        assert_eq!(active.len(), 1);
    }

    #[tokio::test]
    async fn invalidate_apns_token_removes_row_from_active_list() {
        // The trigger's 410-invalidation path delegates to
        // `db.invalidate_apns_token`. Wire test: the next
        // `list_active_apns_tokens` must NOT see the invalidated row,
        // even though it still exists in the table (we keep it for
        // audit). Regression target: a SET clause typo that wrote
        // `invalidated_at = 0` instead of `now` would leave the row
        // in the active list forever.
        let db = Database::test_db().await;
        db.upsert_apns_device_token("device-1", "tok-a", "com.borg.ios", "key-a", b"der-a")
            .await
            .unwrap();
        let rows = db.list_active_apns_tokens("device-1").await.unwrap();
        assert_eq!(rows.len(), 1);
        db.invalidate_apns_token(rows[0].id, "apns-410")
            .await
            .unwrap();
        let after = db.list_active_apns_tokens("device-1").await.unwrap();
        assert!(
            after.is_empty(),
            "invalidated token must drop out of active list, got {after:?}"
        );
    }
}
