//! Fan an event out to every active subscription that wants it.
//!
//! Endpoint events name their subscription in the URL. Channel messages and
//! Borg's own events do not: one Slack message may wake two subscriptions, or
//! none. Those are matched here, against each subscription's filter, *before*
//! anything is stored, so a busy channel costs nothing until a message
//! matches.

use anyhow::Result;
use serde_json::Value;

use super::filter::{EventView, Filter};
use super::{cap_payload, is_subscription_session, presets, wake, EventOrigin, Source};
use crate::config::SubscriptionsConfig;
use crate::db::{Database, IngestLimits, InsertOutcome, NewSubscriptionEvent};

/// The ingest budget from config.
pub fn limits(cfg: &SubscriptionsConfig) -> IngestLimits {
    IngestLimits {
        per_minute: cfg.ingest_burst,
        storm_threshold: cfg.storm_threshold,
        max_pending: cfg.max_pending,
    }
}

/// One event looking for subscriptions.
#[derive(Debug, Clone)]
pub struct FanoutEvent<'a> {
    /// Channel or internal.
    pub source: Source,
    /// Preset id the subscription must have (`slack`, `borg`, …).
    pub provider: &'a str,
    /// Payload in the shape the preset reads.
    pub payload: &'a Value,
    /// Where it came from, for replies.
    pub origin: Option<&'a EventOrigin>,
    /// Session the event was born in, if any. An event born in a
    /// subscription's own session is never fanned out: that is the loop
    /// guard that stops a run from waking itself.
    pub born_in_session: Option<&'a str>,
}

/// Store the event for every matching active subscription. Returns how many
/// subscriptions got it.
pub async fn fan_out(
    db: &Database,
    ev: &FanoutEvent<'_>,
    cfg: &SubscriptionsConfig,
) -> Result<usize> {
    if !cfg.enabled || ev.born_in_session.is_some_and(is_subscription_session) {
        return Ok(0);
    }
    let Some(preset) = presets::get(ev.provider) else {
        return Ok(0);
    };
    let subs = db
        .active_subscriptions_for_source(ev.source.as_str())
        .await?;
    let normalized = preset.normalize(&|_| None, ev.payload);
    let text = preset.text(ev.payload);
    let view = EventView {
        kind: normalized.kind.as_deref(),
        summary: &normalized.summary,
        text: text.as_deref(),
        severity: preset.severity(ev.payload),
        payload: ev.payload,
    };
    let (stored, truncated) = cap_payload(ev.payload, cfg.payload_max_bytes);
    let origin = ev.origin.and_then(|o| serde_json::to_string(o).ok());
    let now = chrono::Utc::now().timestamp();

    let mut delivered = 0;
    let mut first_error = None;
    for sub in subs.iter().filter(|s| s.provider == ev.provider) {
        let Some(filter) = Filter::parse(&sub.filter) else {
            report_unreadable_filter(db, &sub.id).await;
            continue;
        };
        if !filter.matches(&view) {
            continue;
        }
        let new = NewSubscriptionEvent {
            subscription_id: &sub.id,
            kind: normalized.kind.as_deref(),
            dedup_key: normalized.dedup_key.as_deref(),
            summary: &normalized.summary,
            payload: &stored,
            payload_truncated: truncated,
            origin: origin.as_deref(),
            received_at: now,
        };
        // One subscription's failure must not cost the others the event.
        match db.insert_subscription_event(&new, limits(cfg)).await {
            Ok(InsertOutcome::Inserted(_)) => delivered += 1,
            Ok(InsertOutcome::StormTripped { events_in_window }) => {
                wake::report_storm(&sub.id, events_in_window);
            }
            Ok(
                InsertOutcome::Duplicate
                | InsertOutcome::RateLimited
                | InsertOutcome::QueueFull(_)
                | InsertOutcome::NotAccepting,
            ) => {}
            Err(e) => {
                tracing::warn!(subscription = %sub.id, "subscription event insert failed: {e:#}");
                first_error.get_or_insert(e);
            }
        }
    }
    if delivered > 0 {
        wake::poke();
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(delivered),
    }
}

/// What the app shows when a stored filter cannot be read.
pub const UNREADABLE_FILTER: &str =
    "The filter could not be read, so events are being ignored. Edit the subscription to fix it.";

/// Put an unreadable filter where the user sees it. A stored filter is
/// validated on save, so this means an older build wrote it.
pub async fn report_unreadable_filter(db: &Database, subscription_id: &str) {
    tracing::warn!(subscription = %subscription_id, "subscription filter is unreadable; skipping event");
    if let Err(e) = db
        .set_subscription_error(subscription_id, UNREADABLE_FILTER)
        .await
    {
        tracing::warn!(subscription = %subscription_id, "could not record the unreadable filter: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NewSubscription;
    use serde_json::json;

    async fn channel_sub(db: &Database, provider: &str, filter: &str, status: &str) -> String {
        db.create_subscription(&NewSubscription {
            name: "ops",
            source: "channel",
            provider,
            auth_scheme: "none",
            secret_ref: None,
            filter,
            instruction: "x",
            delivery_channel: Some("origin"),
            delivery_target: None,
            status,
            rate_limit_per_hour: 60,
            timeout_secs: 60,
        })
        .await
        .unwrap()
        .id
    }

    fn message(text: &str, id: &str) -> Value {
        json!({"kind": "message", "text": text, "channel_id": "C1", "sender": "U1", "mentioned": false, "message_id": id})
    }

    #[tokio::test]
    async fn only_matching_active_subscriptions_of_that_provider_get_the_event() {
        let db = Database::test_db().await;
        let repro = channel_sub(&db, "slack", r#"{"phrase":"needs repro"}"#, "active").await;
        let all = channel_sub(&db, "slack", "{}", "active").await;
        let paused = channel_sub(&db, "slack", "{}", "paused").await;
        let discord = channel_sub(&db, "discord", "{}", "active").await;
        let cfg = SubscriptionsConfig::default();
        let origin = EventOrigin {
            channel: "slack".into(),
            sender: "U1".into(),
            chat: Some("C1".into()),
            thread_id: Some("t1".into()),
            session_key: None,
        };

        let payload = message("hello", "m1");
        let ev = FanoutEvent {
            source: Source::Channel,
            provider: "slack",
            payload: &payload,
            origin: Some(&origin),
            born_in_session: None,
        };
        assert_eq!(
            fan_out(&db, &ev, &cfg).await.unwrap(),
            1,
            "only the unfiltered one"
        );

        let payload = message("this NEEDS REPRO please", "m2");
        let ev = FanoutEvent {
            payload: &payload,
            ..ev
        };
        assert_eq!(fan_out(&db, &ev, &cfg).await.unwrap(), 2);

        assert_eq!(
            db.list_subscription_events(&repro, 10).await.unwrap().len(),
            1
        );
        assert_eq!(
            db.list_subscription_events(&all, 10).await.unwrap().len(),
            2
        );
        assert!(db
            .list_subscription_events(&paused, 10)
            .await
            .unwrap()
            .is_empty());
        assert!(db
            .list_subscription_events(&discord, 10)
            .await
            .unwrap()
            .is_empty());

        let stored = &db.list_subscription_events(&repro, 10).await.unwrap()[0];
        let back: EventOrigin = serde_json::from_str(stored.origin.as_deref().unwrap()).unwrap();
        assert_eq!(back, origin, "the origin is kept for replies in the thread");
    }

    #[tokio::test]
    async fn events_born_in_a_subscription_session_never_fan_out() {
        let db = Database::test_db().await;
        let id = db
            .create_subscription(&NewSubscription {
                name: "borg",
                source: "internal",
                provider: "borg",
                auth_scheme: "none",
                secret_ref: None,
                filter: "{}",
                instruction: "x",
                delivery_channel: None,
                delivery_target: None,
                status: "active",
                rate_limit_per_hour: 60,
                timeout_secs: 60,
            })
            .await
            .unwrap()
            .id;
        let cfg = SubscriptionsConfig::default();
        let payload = json!({"id": "goal:1", "kind": "goal.blocked", "summary": "stuck"});
        let own = crate::subscriptions::session_key(&id);
        let looped = FanoutEvent {
            source: Source::Internal,
            provider: "borg",
            payload: &payload,
            origin: None,
            born_in_session: Some(&own),
        };
        assert_eq!(fan_out(&db, &looped, &cfg).await.unwrap(), 0);
        let normal = FanoutEvent {
            born_in_session: Some("main-session"),
            ..looped
        };
        assert_eq!(fan_out(&db, &normal, &cfg).await.unwrap(), 1);

        let off = SubscriptionsConfig {
            enabled: false,
            ..Default::default()
        };
        let payload2 = json!({"id": "goal:2", "kind": "goal.blocked", "summary": "stuck"});
        let again = FanoutEvent {
            payload: &payload2,
            ..normal
        };
        assert_eq!(
            fan_out(&db, &again, &off).await.unwrap(),
            0,
            "the master switch wins"
        );
    }
}
