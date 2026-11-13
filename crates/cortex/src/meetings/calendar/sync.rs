//! Connecting, refreshing and forgetting the calendar. The daemon's
//! `calendar-sync` worker and the `Meetings.*Calendar` RPCs call these; the
//! loop, the timer and the event bus stay in `borgd`.
//!
//! One calendar at a time: connecting a source forgets the other, so the
//! cache never mixes a stale feed with a live account.

use anyhow::{Context, Result};

use super::google::{self, GoogleCalendarCredential, GoogleEndpoints, GoogleError, PendingAuth};
use super::ics;
use crate::config::Config;
use crate::db::Database;
use crate::security::secrets_resolve::SecretRef;

/// How far back the cache reaches: a meeting that started late last night
/// is still matched to its event.
pub const SYNC_PAST_SECS: i64 = 86_400;
/// How far ahead: two weeks, so "Coming up" can page forward a week.
pub const SYNC_AHEAD_SECS: i64 = 14 * 86_400;

/// The settings row holding the ICS address.
pub const ICS_SETTING_KEY: &str = "meetings.calendar.ics_url";

/// The window a sync covers.
pub fn sync_window(now: i64) -> (i64, i64) {
    (now - SYNC_PAST_SECS, now + SYNC_AHEAD_SECS)
}

/// The zone date-only and floating calendar times are read in: the user's
/// `user.timezone`, else this machine's, else UTC.
pub fn local_tz(config: &Config) -> chrono_tz::Tz {
    config
        .user
        .timezone
        .as_deref()
        .and_then(|s| s.parse().ok())
        .or_else(|| Config::detect_host_timezone().and_then(|s| s.parse().ok()))
        .unwrap_or(chrono_tz::UTC)
}

/// What a sync found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Nothing is connected; the cache was emptied.
    NotConnected,
    /// The cache now holds `count` events from the connected source.
    Synced {
        /// Events cached.
        count: usize,
    },
    /// Google refused the saved sign-in; it was removed.
    Revoked,
}

/// Refresh the cache from whatever calendar is connected. A failed fetch
/// leaves the cache as it was: stale events still name a meeting better than
/// none. Errors never carry the ICS address or a token.
pub async fn sync_once(
    db: &Database,
    config: &Config,
    http: &reqwest::Client,
    endpoints: &GoogleEndpoints,
    now: i64,
) -> Result<SyncOutcome> {
    let window = sync_window(now);
    if let Some(cred) = GoogleCalendarCredential::from_config(config) {
        let token = match google::refresh_access_token(http, endpoints, &cred).await {
            Ok(t) => t,
            Err(GoogleError::Revoked) => {
                tracing::warn!("Google Calendar sign-in was revoked; disconnecting it");
                db.delete_setting(google::SETTING_KEY).await?;
                db.clear_calendar_events().await?;
                return Ok(SyncOutcome::Revoked);
            }
            Err(GoogleError::Other(e)) => return Err(e),
        };
        let events =
            google::fetch_events(http, endpoints, &token, window, local_tz(config)).await?;
        db.replace_calendar_events("google", &events, now).await?;
        db.replace_calendar_events("ics", &[], now).await?;
        return Ok(SyncOutcome::Synced {
            count: events.len(),
        });
    }
    if let Some(raw) = super::ics_url(config) {
        let url = ics::validate_ics_url(&raw)?;
        let text = ics::fetch_ics(&ics::feed_client()?, &url).await?;
        let events = ics::parse_ics(&text, window, local_tz(config))?;
        db.replace_calendar_events("ics", &events, now).await?;
        db.replace_calendar_events("google", &[], now).await?;
        return Ok(SyncOutcome::Synced {
            count: events.len(),
        });
    }
    db.clear_calendar_events().await?;
    Ok(SyncOutcome::NotConnected)
}

/// Connect an ICS feed: fetch and parse it once (so a wrong address fails
/// here, in front of the person, not silently in the background), store the
/// address as a secret, and fill the cache. Returns how many events fall in
/// the next seven days.
pub async fn connect_ics(db: &Database, config: &Config, raw_url: &str, now: i64) -> Result<usize> {
    let url = ics::validate_ics_url(raw_url)?;
    let text = ics::fetch_ics(&ics::feed_client()?, &url).await?;
    let events = ics::parse_ics(&text, sync_window(now), local_tz(config))?;
    let secret = SecretRef::Literal {
        value: url.to_string(),
    };
    db.set_setting(ICS_SETTING_KEY, &serde_json::to_string(&secret)?)
        .await?;
    db.delete_setting(google::SETTING_KEY).await?;
    db.replace_calendar_events("ics", &events, now).await?;
    db.replace_calendar_events("google", &[], now).await?;
    Ok(events
        .iter()
        .filter(|e| e.start_unix >= now && e.start_unix < now + 7 * 86_400)
        .count())
}

/// Finish a Google sign-in once the consent page sent the code back: trade
/// it for tokens, name the account, save the sign-in, and fill the cache.
/// Returns the account label.
#[allow(clippy::too_many_arguments)]
pub async fn complete_google(
    db: &Database,
    config: &Config,
    http: &reqwest::Client,
    endpoints: &GoogleEndpoints,
    client_id: &str,
    client_secret: &str,
    code: &str,
    pending: &PendingAuth,
    now: i64,
) -> Result<String> {
    let (access, refresh) =
        google::exchange_code(http, endpoints, client_id, client_secret, code, pending).await?;
    let account = google::account_label(http, endpoints, &access)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("Google Calendar account name unavailable: {e}");
            "Google Calendar".to_string()
        });
    let cred = GoogleCalendarCredential {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        refresh_token: refresh,
        account_label: account.clone(),
    };
    db.set_setting(google::SETTING_KEY, &cred.to_setting_value()?)
        .await?;
    db.delete_setting(ICS_SETTING_KEY).await?;
    // The sign-in is saved; a failed first fetch only delays the cache to
    // the next sync.
    match google::fetch_events(http, endpoints, &access, sync_window(now), local_tz(config)).await {
        Ok(events) => {
            db.replace_calendar_events("google", &events, now).await?;
            db.replace_calendar_events("ics", &[], now).await?;
        }
        Err(e) => tracing::warn!("first Google Calendar fetch failed: {e}"),
    }
    Ok(account)
}

/// Forget the calendar: both sources' settings and every cached event.
pub async fn disconnect(db: &Database) -> Result<()> {
    db.delete_setting(google::SETTING_KEY)
        .await
        .context("forget the Google sign-in")?;
    db.delete_setting(ICS_SETTING_KEY)
        .await
        .context("forget the calendar feed")?;
    db.clear_calendar_events().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const NOW: i64 = 1_794_784_800; // 2026-11-16T00:00:00Z

    fn endpoints(server: &MockServer) -> GoogleEndpoints {
        GoogleEndpoints {
            auth: format!("{}/auth", server.uri()),
            token: format!("{}/token", server.uri()),
            api: format!("{}/calendar/v3", server.uri()),
        }
    }

    async fn config_of(db: &Database) -> Config {
        let rows = db.list_settings().await.unwrap();
        Config::from_setting_rows(rows.into_iter().map(|(k, v, _)| (k, v)))
    }

    fn ics_feed() -> String {
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Sync\r\nDTSTART:20261116T150000Z\r\nDTEND:20261116T153000Z\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:b\r\nSUMMARY:Far\r\nDTSTART:20261127T150000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".into()
    }

    /// A settings row's value; the test database seeds every key, so an
    /// unset one reads as empty rather than missing.
    async fn stored(db: &Database, key: &str) -> Option<String> {
        db.get_setting(key).await.unwrap().filter(|v| !v.is_empty())
    }

    async fn seed_ics(db: &Database, url: &str) {
        let secret = SecretRef::Literal { value: url.into() };
        db.set_setting(ICS_SETTING_KEY, &serde_json::to_string(&secret).unwrap())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn connect_ics_refuses_a_bad_address_and_stores_nothing() {
        let db = Database::test_db().await;
        let config = Config::default();
        let err = connect_ics(&db, &config, "http://cal.example.com/a.ics", NOW)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("https://"), "{err}");
        assert_eq!(stored(&db, ICS_SETTING_KEY).await, None);
    }

    #[tokio::test]
    async fn sync_refuses_a_stored_feed_address_that_is_not_https() {
        // A row written by hand (or by an older build) is re-validated
        // before every fetch, not trusted because it is already stored.
        let db = Database::test_db().await;
        seed_ics(&db, "http://192.168.1.10/feed.ics").await;
        let config = config_of(&db).await;
        let err = sync_once(
            &db,
            &config,
            crate::http::shared(),
            &GoogleEndpoints::default(),
            NOW,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("https://"), "{err}");
    }

    #[tokio::test]
    async fn google_sign_in_saves_the_account_fills_the_cache_and_replaces_ics() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token": "at", "refresh_token": "rt"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/calendar/v3/calendars/primary"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "me@example.com"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/calendar/v3/calendars/primary/events"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [
                {"id": "g1", "summary": "Standup", "start": {"dateTime": "2026-11-16T15:00:00Z"}, "end": {"dateTime": "2026-11-16T15:15:00Z"}, "hangoutLink": "https://meet.google.com/abc-defg-hij"}
            ]})))
            .mount(&server)
            .await;
        let db = Database::test_db().await;
        seed_ics(&db, "https://cal.example.com/a.ics").await;
        let stale = ics::parse_ics(&ics_feed(), sync_window(NOW), chrono_tz::UTC).unwrap();
        db.replace_calendar_events("ics", &stale, NOW)
            .await
            .unwrap();

        let ep = endpoints(&server);
        let pending = google::begin_auth(&ep, "cid", "http://127.0.0.1:1/callback");
        let account = complete_google(
            &db,
            &Config::default(),
            crate::http::shared(),
            &ep,
            "cid",
            "cs",
            "code",
            &pending,
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(account, "me@example.com");

        let config = config_of(&db).await;
        let c = super::super::connection(&config);
        assert_eq!(
            (c.kind, c.account_label.as_str()),
            ("google", "me@example.com")
        );
        assert_eq!(
            stored(&db, ICS_SETTING_KEY).await,
            None,
            "one calendar at a time"
        );
        let cached = db
            .calendar_events_between(NOW, NOW + 30 * 86_400)
            .await
            .unwrap();
        let ids: Vec<&str> = cached.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["google:g1"], "the old feed's events are gone");

        // A later sync refreshes with the stored refresh token.
        let outcome = sync_once(&db, &config, crate::http::shared(), &ep, NOW)
            .await
            .unwrap();
        assert_eq!(outcome, SyncOutcome::Synced { count: 1 });
    }

    #[tokio::test]
    async fn a_revoked_sign_in_disconnects_and_empties_the_cache() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})),
            )
            .mount(&server)
            .await;
        let db = Database::test_db().await;
        let cred = GoogleCalendarCredential {
            client_id: "cid".into(),
            client_secret: "cs".into(),
            refresh_token: "rt".into(),
            account_label: "me@example.com".into(),
        };
        db.set_setting(google::SETTING_KEY, &cred.to_setting_value().unwrap())
            .await
            .unwrap();
        let events = ics::parse_ics(&ics_feed(), sync_window(NOW), chrono_tz::UTC).unwrap();
        db.replace_calendar_events("google", &events, NOW)
            .await
            .unwrap();

        let config = config_of(&db).await;
        let outcome = sync_once(
            &db,
            &config,
            crate::http::shared(),
            &endpoints(&server),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(outcome, SyncOutcome::Revoked);
        assert_eq!(stored(&db, google::SETTING_KEY).await, None);
        assert!(db
            .calendar_events_between(0, i64::MAX)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn a_transient_google_failure_keeps_the_cache() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(503).set_body_json(json!({"error": "backend_error"})),
            )
            .mount(&server)
            .await;
        let db = Database::test_db().await;
        let cred = GoogleCalendarCredential {
            client_id: "cid".into(),
            client_secret: "cs".into(),
            refresh_token: "rt".into(),
            account_label: "me@example.com".into(),
        };
        db.set_setting(google::SETTING_KEY, &cred.to_setting_value().unwrap())
            .await
            .unwrap();
        let events = ics::parse_ics(&ics_feed(), sync_window(NOW), chrono_tz::UTC).unwrap();
        db.replace_calendar_events("google", &events, NOW)
            .await
            .unwrap();

        let config = config_of(&db).await;
        assert!(sync_once(
            &db,
            &config,
            crate::http::shared(),
            &endpoints(&server),
            NOW
        )
        .await
        .is_err());
        assert_eq!(
            db.calendar_events_between(0, i64::MAX).await.unwrap().len(),
            2,
            "stale beats empty"
        );
        assert!(stored(&db, google::SETTING_KEY).await.is_some());
    }

    #[tokio::test]
    async fn disconnect_forgets_both_sources_and_the_cache() {
        let db = Database::test_db().await;
        seed_ics(&db, "https://cal.example.com/a.ics").await;
        let events = ics::parse_ics(&ics_feed(), sync_window(NOW), chrono_tz::UTC).unwrap();
        db.replace_calendar_events("ics", &events, NOW)
            .await
            .unwrap();
        disconnect(&db).await.unwrap();
        let config = config_of(&db).await;
        assert!(!super::super::connection(&config).connected);
        assert!(db
            .calendar_events_between(0, i64::MAX)
            .await
            .unwrap()
            .is_empty());
        let outcome = sync_once(
            &db,
            &config,
            crate::http::shared(),
            &GoogleEndpoints::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(outcome, SyncOutcome::NotConnected);
    }
}
