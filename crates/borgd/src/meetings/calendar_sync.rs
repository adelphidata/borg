//! The `calendar-sync` worker: refreshes the cached calendar every five
//! minutes, or at once when poked after a connect or disconnect.
//!
//! It fetches only while meetings are on, so turning the feature off stops
//! all calendar traffic. `CalendarChanged` goes out only when "connected"
//! flips (a revoked Google sign-in, say), not on every refresh.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cortex::config::Config;
use cortex::db::Database;
use cortex::meetings::calendar::connection;
use cortex::meetings::calendar::google::GoogleEndpoints;
use cortex::meetings::calendar::sync::{sync_once, SyncOutcome};
use cortex::meetings::now_unix;
use protocols::meetings::meeting_event::Kind;
use protocols::meetings::CalendarChanged;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::MeetingEventBus;

/// Time between refreshes.
pub const SYNC_EVERY: Duration = Duration::from_secs(300);

/// Run until cancelled.
pub async fn run(
    bus: MeetingEventBus,
    poke: Arc<Notify>,
    liveness: Arc<AtomicU64>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let endpoints = GoogleEndpoints::default();
    let mut last: Option<bool> = None;
    loop {
        liveness.fetch_add(1, Ordering::Relaxed);
        match (Config::load_from_db().await, Database::open().await) {
            (Ok(config), Ok(db)) => {
                let connected = cycle(&db, &config, &endpoints, now_unix()).await;
                announce(&bus, &mut last, connected);
            }
            (Err(e), _) | (_, Err(e)) => tracing::warn!("calendar sync skipped: {e:#}"),
        }
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            () = poke.notified() => {}
            () = tokio::time::sleep(SYNC_EVERY) => {}
        }
    }
}

/// One refresh. Returns whether a calendar is connected afterwards.
pub async fn cycle(db: &Database, config: &Config, endpoints: &GoogleEndpoints, now: i64) -> bool {
    let connected = connection(config).connected;
    if !config.meetings.enabled {
        return connected;
    }
    match sync_once(db, config, cortex::http::shared(), endpoints, now).await {
        Ok(SyncOutcome::Synced { count }) => {
            tracing::debug!("calendar synced: {count} events");
            true
        }
        Ok(SyncOutcome::NotConnected) => false,
        Ok(SyncOutcome::Revoked) => false,
        // The cache stays; the calendar is still connected.
        Err(e) => {
            tracing::warn!("calendar sync failed: {e:#}");
            connected
        }
    }
}

/// Publish `CalendarChanged` when `connected` differs from the last cycle.
/// The first cycle only records the state: clients read it from
/// `GetMeetingsStatus` when they open.
pub fn announce(bus: &MeetingEventBus, last: &mut Option<bool>, connected: bool) {
    if last.is_some_and(|was| was != connected) {
        bus.publish(Kind::CalendarChanged(CalendarChanged {
            connected,
            error: String::new(),
        }));
    }
    *last = Some(connected);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_changed_goes_out_only_on_a_flip() {
        let bus = MeetingEventBus::default();
        let mut rx = bus.subscribe();
        let mut last = None;
        for connected in [true, true, false, false, true] {
            announce(&bus, &mut last, connected);
        }
        let mut got = Vec::new();
        while let Ok(e) = rx.try_recv() {
            if let Some(Kind::CalendarChanged(c)) = e.kind {
                got.push(c.connected);
            }
        }
        assert_eq!(got, [false, true]);
    }

    #[tokio::test]
    async fn nothing_is_fetched_while_meetings_are_off() {
        use wiremock::matchers::any;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let endpoints = GoogleEndpoints {
            auth: format!("{}/auth", server.uri()),
            token: format!("{}/token", server.uri()),
            api: format!("{}/api", server.uri()),
        };
        let cred = cortex::meetings::calendar::google::GoogleCalendarCredential {
            client_id: "cid".into(),
            client_secret: "cs".into(),
            refresh_token: "rt".into(),
            account_label: "me@example.com".into(),
        };
        let mut config = Config::default();
        config
            .apply_setting(
                cortex::meetings::calendar::google::SETTING_KEY,
                &cred.to_setting_value().unwrap(),
            )
            .unwrap();
        let db = Database::test_db().await;
        assert!(!config.meetings.enabled);
        assert!(
            cycle(&db, &config, &endpoints, 0).await,
            "connected, though off"
        );
        // `expect(0)` is verified when the server drops.
    }
}
