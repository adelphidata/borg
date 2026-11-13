//! Google Calendar, read-only, through the user's own OAuth client.
//!
//! Borg ships no Google client id: the user creates a "Desktop app" OAuth
//! client in their own Google Cloud project and pastes its id and secret.
//! The sign-in is the installed-app flow (PKCE S256, loopback redirect, the
//! daemon's [`oauth_loopback`] listener catches the code). Only the refresh
//! token is kept; access tokens live for one sync.
//!
//! [`oauth_loopback`]: ../../../../borgd/src/meetings/oauth_loopback.rs

use std::collections::HashSet;

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{join_url, CalendarEvent, CalendarSource};
use crate::config::Config;
use crate::meetings::Attendee;
use crate::security::secrets_resolve::SecretRef;

/// The settings row holding the sign-in.
pub const SETTING_KEY: &str = "meetings.calendar.google";

/// Read events and calendar metadata, nothing else.
pub const SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";

/// Pages of events read per sync. 250 events a page; a week of even the
/// busiest calendar fits in two.
const MAX_PAGES: usize = 8;

/// The saved sign-in.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleCalendarCredential {
    /// The user's OAuth client id.
    pub client_id: String,
    /// Its secret. Google calls a desktop client's secret non-confidential,
    /// but it is still not something to print.
    pub client_secret: String,
    /// Long-lived; trades for an access token each sync.
    pub refresh_token: String,
    /// The account, as Google names the primary calendar (its email).
    pub account_label: String,
}

impl std::fmt::Debug for GoogleCalendarCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleCalendarCredential")
            .field("client_id", &self.client_id)
            .field("account_label", &self.account_label)
            .finish_non_exhaustive()
    }
}

impl GoogleCalendarCredential {
    /// The sign-in saved in `config`, or `None` when there is none or it
    /// cannot be read (logged).
    pub fn from_config(config: &Config) -> Option<Self> {
        let secret = config.meetings.calendar.google.as_ref()?;
        let json = match secret.resolve() {
            Ok(v) if !v.trim().is_empty() => v,
            Ok(_) => return None,
            Err(e) => {
                tracing::warn!("{SETTING_KEY} could not be resolved: {e}");
                return None;
            }
        };
        match serde_json::from_str::<Self>(&json) {
            Ok(c) if !c.refresh_token.is_empty() => Some(c),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!("{SETTING_KEY} is not a Google sign-in: {e}");
                None
            }
        }
    }

    /// The value to store under [`SETTING_KEY`].
    pub fn to_setting_value(&self) -> Result<String> {
        let secret = SecretRef::Literal {
            value: serde_json::to_string(self)?,
        };
        Ok(serde_json::to_string(&secret)?)
    }
}

/// Google's endpoints. Swappable so tests can stand up a fake.
#[derive(Debug, Clone)]
pub struct GoogleEndpoints {
    /// Consent page.
    pub auth: String,
    /// Token exchange and refresh.
    pub token: String,
    /// Calendar API v3 root, no trailing slash.
    pub api: String,
}

impl Default for GoogleEndpoints {
    fn default() -> Self {
        Self {
            auth: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token: "https://oauth2.googleapis.com/token".into(),
            api: "https://www.googleapis.com/calendar/v3".into(),
        }
    }
}

/// A sign-in in progress: the consent URL to open, and what the callback
/// must be checked against.
#[derive(Clone)]
pub struct PendingAuth {
    /// Open this in the user's browser.
    pub url: String,
    /// Echoed back on the callback; anything else is a forgery.
    pub state: String,
    /// The PKCE secret sent with the code exchange.
    pub verifier: String,
    /// The loopback address the code comes back to.
    pub redirect_uri: String,
}

/// Start a sign-in: a fresh PKCE pair and state, and the consent URL asking
/// for offline (refresh-token) read-only calendar access.
pub fn begin_auth(endpoints: &GoogleEndpoints, client_id: &str, redirect_uri: &str) -> PendingAuth {
    let verifier = random_token(64);
    let state = random_token(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        // A refresh token, every time: without `prompt=consent` Google only
        // issues one on the first sign-in for this client.
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .finish();
    PendingAuth {
        url: format!("{}?{query}", endpoints.auth),
        state,
        verifier,
        redirect_uri: redirect_uri.to_string(),
    }
}

/// Compare the callback's `state` without leaking how much of it matched.
pub fn state_matches(expected: &str, got: &str) -> bool {
    let (a, b) = (expected.as_bytes(), got.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// URL-safe random text with `bytes` bytes of entropy.
fn random_token(bytes: usize) -> String {
    use rand::Rng as _;
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

/// Why a refresh failed.
#[derive(Debug, thiserror::Error)]
pub enum GoogleError {
    /// The user revoked access, the client was deleted, or (for an app left
    /// in "Testing") the token's 7 days ran out. Only signing in again helps.
    #[error(
        "Google no longer accepts this sign-in; connect the calendar again \
         (apps left in \"Testing\" lose access after 7 days)"
    )]
    Revoked,
    /// Anything else, likely transient.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: Option<String>,
    refresh_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn post_token(
    http: &reqwest::Client,
    endpoints: &GoogleEndpoints,
    form: &[(&str, &str)],
) -> Result<(reqwest::StatusCode, TokenReply)> {
    let resp = http
        .post(&endpoints.token)
        .form(form)
        .send()
        .await
        .map_err(|e| anyhow!("could not reach Google: {}", e.without_url()))?;
    let status = resp.status();
    let body: TokenReply = resp.json().await.map_err(|e| {
        anyhow!(
            "Google sent an unreadable token reply ({status}): {}",
            e.without_url()
        )
    })?;
    Ok((status, body))
}

/// Trade the callback's code for tokens; returns the refresh token. Google
/// only sends one when consent was given for offline access.
pub async fn exchange_code(
    http: &reqwest::Client,
    endpoints: &GoogleEndpoints,
    client_id: &str,
    client_secret: &str,
    code: &str,
    pending: &PendingAuth,
) -> Result<(String, String)> {
    let (status, reply) = post_token(
        http,
        endpoints,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", &pending.redirect_uri),
            ("code_verifier", &pending.verifier),
        ],
    )
    .await?;
    if !status.is_success() {
        bail!(
            "Google refused the sign-in: {}",
            reply
                .error_description
                .or(reply.error)
                .unwrap_or_else(|| status.to_string())
        );
    }
    let access = reply
        .access_token
        .ok_or_else(|| anyhow!("Google's reply had no access token"))?;
    let refresh = reply
        .refresh_token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "Google did not grant offline access; sign in again and approve every permission"
            )
        })?;
    Ok((access, refresh))
}

/// A fresh access token for `cred`.
pub async fn refresh_access_token(
    http: &reqwest::Client,
    endpoints: &GoogleEndpoints,
    cred: &GoogleCalendarCredential,
) -> Result<String, GoogleError> {
    let (status, reply) = post_token(
        http,
        endpoints,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &cred.refresh_token),
            ("client_id", &cred.client_id),
            ("client_secret", &cred.client_secret),
        ],
    )
    .await?;
    if matches!(
        reply.error.as_deref(),
        Some("invalid_grant" | "invalid_client" | "unauthorized_client")
    ) {
        return Err(GoogleError::Revoked);
    }
    if !status.is_success() {
        return Err(anyhow!(
            "Google token refresh failed: {}",
            reply
                .error_description
                .or(reply.error)
                .unwrap_or_else(|| status.to_string())
        )
        .into());
    }
    reply
        .access_token
        .ok_or_else(|| GoogleError::Other(anyhow!("Google's reply had no access token")))
}

async fn get_json(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    query: &[(&str, String)],
) -> Result<Value> {
    let resp = http
        .get(url)
        .bearer_auth(token)
        .query(query)
        .send()
        .await
        .map_err(|e| anyhow!("could not reach Google Calendar: {}", e.without_url()))?;
    let status = resp.status();
    if !status.is_success() {
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        let message = body["error"]["message"].as_str().unwrap_or("").to_string();
        bail!(
            "Google Calendar answered {status}{}",
            if message.is_empty() {
                String::new()
            } else {
                format!(": {message}")
            }
        );
    }
    resp.json()
        .await
        .map_err(|e| anyhow!("Google Calendar sent unreadable JSON: {}", e.without_url()))
}

/// The signed-in account's name for itself: the primary calendar's id, which
/// is the account email.
pub async fn account_label(
    http: &reqwest::Client,
    endpoints: &GoogleEndpoints,
    access_token: &str,
) -> Result<String> {
    let body = get_json(
        http,
        &format!("{}/calendars/primary", endpoints.api),
        access_token,
        &[],
    )
    .await?;
    body["id"]
        .as_str()
        .or_else(|| body["summary"].as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("Google Calendar did not name the account"))
}

/// Events on the primary calendar overlapping `window` (Unix seconds),
/// recurring ones expanded by Google. Date-only (all-day) events are days,
/// so they start at midnight in `default_tz`, the person's own zone.
pub async fn fetch_events(
    http: &reqwest::Client,
    endpoints: &GoogleEndpoints,
    access_token: &str,
    window: (i64, i64),
    default_tz: chrono_tz::Tz,
) -> Result<Vec<CalendarEvent>> {
    let rfc = |ts: i64| -> Result<String> {
        Ok(Utc
            .timestamp_opt(ts, 0)
            .single()
            .context("window out of range")?
            .to_rfc3339())
    };
    let url = format!("{}/calendars/primary/events", endpoints.api);
    let mut out = Vec::new();
    let mut page_token: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut query = vec![
            ("singleEvents", "true".to_string()),
            ("orderBy", "startTime".to_string()),
            ("timeMin", rfc(window.0)?),
            ("timeMax", rfc(window.1)?),
            ("maxResults", "250".to_string()),
        ];
        if let Some(t) = &page_token {
            query.push(("pageToken", t.clone()));
        }
        let body = get_json(http, &url, access_token, &query).await?;
        for item in body["items"].as_array().into_iter().flatten() {
            if let Some(event) = to_event(item, default_tz) {
                out.push(event);
            }
        }
        page_token = body["nextPageToken"].as_str().map(str::to_string);
        if page_token.is_none() {
            return Ok(out);
        }
    }
    tracing::warn!("Google Calendar returned more than {MAX_PAGES} pages; the rest were skipped");
    Ok(out)
}

/// One API event, or `None` for a cancelled one, a working-location marker,
/// or one without readable times.
fn to_event(item: &Value, tz: chrono_tz::Tz) -> Option<CalendarEvent> {
    if item["status"].as_str() == Some("cancelled")
        || item["eventType"].as_str() == Some("workingLocation")
    {
        return None;
    }
    let id = item["id"].as_str()?;
    let (start_unix, all_day) = time_of(&item["start"], tz)?;
    let (end_unix, _) = time_of(&item["end"], tz).unwrap_or((start_unix, all_day));
    let title = item["summary"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Untitled event")
        .to_string();
    let video = item["conferenceData"]["entryPoints"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["entryPointType"].as_str() == Some("video"))
        .and_then(|p| p["uri"].as_str())
        .unwrap_or("");
    let field = |k: &str| item[k].as_str().unwrap_or("");
    let join = join_url::extract(&[
        video,
        field("hangoutLink"),
        field("location"),
        field("description"),
    ]);
    let (join_url, provider) = join.map_or((String::new(), String::new()), |(u, p)| {
        (u, p.as_str().to_string())
    });
    Some(CalendarEvent {
        id: format!("google:{id}"),
        source: CalendarSource::Google.as_str().to_string(),
        title,
        start_unix,
        end_unix: end_unix.max(start_unix),
        all_day,
        attendees: attendees_of(item),
        join_url,
        provider,
    })
}

/// `{dateTime}` or `{date}` (all day, read at local midnight).
fn time_of(v: &Value, tz: chrono_tz::Tz) -> Option<(i64, bool)> {
    if let Some(dt) = v["dateTime"].as_str() {
        return DateTime::parse_from_rfc3339(dt)
            .ok()
            .map(|d| (d.timestamp(), false));
    }
    let day = NaiveDate::parse_from_str(v["date"].as_str()?, "%Y-%m-%d").ok()?;
    let midnight = tz
        .from_local_datetime(&day.and_hms_opt(0, 0, 0)?)
        .earliest()?;
    Some((midnight.timestamp(), true))
}

/// Organiser first, then invitees; rooms (`resource: true`) are skipped.
fn attendees_of(item: &Value) -> Vec<Attendee> {
    let mut seen = HashSet::new();
    let organizer = Some(&item["organizer"]).filter(|o| o.is_object());
    let invitees = item["attendees"].as_array().into_iter().flatten();
    organizer
        .into_iter()
        .chain(invitees)
        .filter(|a| !a["resource"].as_bool().unwrap_or(false))
        .filter_map(|a| {
            let email = a["email"].as_str().unwrap_or("").trim().to_string();
            if email.ends_with("calendar.google.com") {
                // Group and resource calendars, not people.
                return None;
            }
            let name = a["displayName"].as_str().unwrap_or("").trim().to_string();
            if email.is_empty() && name.is_empty() {
                return None;
            }
            seen.insert(email.to_lowercase())
                .then_some(Attendee { name, email })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn endpoints(server: &MockServer) -> GoogleEndpoints {
        GoogleEndpoints {
            auth: format!("{}/auth", server.uri()),
            token: format!("{}/token", server.uri()),
            api: format!("{}/calendar/v3", server.uri()),
        }
    }

    fn cred() -> GoogleCalendarCredential {
        GoogleCalendarCredential {
            client_id: "cid.apps.googleusercontent.com".into(),
            client_secret: "csecret".into(),
            refresh_token: "1//refresh".into(),
            account_label: "me@example.com".into(),
        }
    }

    #[test]
    fn consent_url_asks_for_offline_read_only_access_with_pkce() {
        let a = begin_auth(
            &GoogleEndpoints::default(),
            "cid",
            "http://127.0.0.1:5555/callback",
        );
        let url = url::Url::parse(&a.url).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["scope"], SCOPE);
        assert_eq!(q["access_type"], "offline");
        assert_eq!(q["prompt"], "consent");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:5555/callback");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(
            q["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(a.verifier.as_bytes())),
            "challenge is the S256 of the verifier"
        );
        assert_eq!(q["state"], a.state);
        // RFC 7636: 43..=128 characters.
        assert!(
            (43..=128).contains(&a.verifier.len()),
            "{}",
            a.verifier.len()
        );
        let b = begin_auth(
            &GoogleEndpoints::default(),
            "cid",
            "http://127.0.0.1:5555/callback",
        );
        assert_ne!(a.state, b.state, "state is fresh per sign-in");
        assert_ne!(a.verifier, b.verifier);
    }

    #[test]
    fn state_comparison() {
        assert!(state_matches("abc", "abc"));
        assert!(!state_matches("abc", "abd"));
        assert!(!state_matches("abc", "abcd"));
        assert!(!state_matches("abc", ""));
    }

    #[tokio::test]
    async fn exchange_sends_the_verifier_and_needs_a_refresh_token() {
        let server = MockServer::start().await;
        let pending = begin_auth(&endpoints(&server), "cid", "http://127.0.0.1:1/callback");
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains(format!(
                "code_verifier={}",
                pending.verifier
            )))
            .and(body_string_contains("code=good"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token": "at", "refresh_token": "rt"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("code=no-offline"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "at"})))
            .mount(&server)
            .await;
        let http = crate::http::shared();
        let ep = endpoints(&server);
        let (access, refresh) = exchange_code(http, &ep, "cid", "cs", "good", &pending)
            .await
            .unwrap();
        assert_eq!((access.as_str(), refresh.as_str()), ("at", "rt"));
        let err = exchange_code(http, &ep, "cid", "cs", "no-offline", &pending)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("offline access"), "{err}");
    }

    #[tokio::test]
    async fn refresh_distinguishes_a_revoked_grant_from_a_hiccup() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=1%2F%2Frefresh"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"access_token": "fresh"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=revoked"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant", "error_description": "Token has been expired or revoked."})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=flaky"))
            .respond_with(
                ResponseTemplate::new(503).set_body_json(json!({"error": "backend_error"})),
            )
            .mount(&server)
            .await;
        let http = crate::http::shared();
        let ep = endpoints(&server);
        assert_eq!(
            refresh_access_token(http, &ep, &cred()).await.unwrap(),
            "fresh"
        );
        let revoked = GoogleCalendarCredential {
            refresh_token: "revoked".into(),
            ..cred()
        };
        assert!(matches!(
            refresh_access_token(http, &ep, &revoked).await,
            Err(GoogleError::Revoked)
        ));
        let flaky = GoogleCalendarCredential {
            refresh_token: "flaky".into(),
            ..cred()
        };
        assert!(matches!(
            refresh_access_token(http, &ep, &flaky).await,
            Err(GoogleError::Other(_))
        ));
    }

    #[tokio::test]
    async fn events_page_through_and_map_to_calendar_events() {
        let server = MockServer::start().await;
        let page1 = json!({
            "timeZone": "America/New_York",
            "nextPageToken": "p2",
            "items": [
                {
                    "id": "e1", "summary": "PSP7 Tiger Sync",
                    "start": {"dateTime": "2026-11-12T09:45:00-05:00"},
                    "end": {"dateTime": "2026-11-12T10:15:00-05:00"},
                    "organizer": {"email": "juan@example.com", "displayName": "Juan"},
                    "attendees": [
                        {"email": "juan@example.com", "displayName": "Juan", "organizer": true},
                        {"email": "jae@example.com"},
                        {"email": "room@example.com", "displayName": "Room 4", "resource": true},
                    ],
                    "conferenceData": {"entryPoints": [
                        {"entryPointType": "phone", "uri": "tel:+1-555"},
                        {"entryPointType": "video", "uri": "https://meet.google.com/abc-defg-hij"},
                    ]},
                    "location": "https://zoom.us/j/123456789",
                },
                {"id": "gone", "status": "cancelled", "start": {"dateTime": "2026-11-12T11:00:00Z"}},
                {"id": "wl", "eventType": "workingLocation", "start": {"date": "2026-11-12"}, "end": {"date": "2026-11-13"}},
            ]
        });
        let page2 = json!({
            "timeZone": "UTC",
            "items": [
                {"id": "e2", "summary": "Offsite", "start": {"date": "2026-11-13"}, "end": {"date": "2026-11-14"}},
            ]
        });
        Mock::given(method("GET"))
            .and(path("/calendar/v3/calendars/primary/events"))
            .and(header("authorization", "Bearer at"))
            .and(query_param("singleEvents", "true"))
            .and(query_param("pageToken", "p2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page2))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/calendar/v3/calendars/primary/events"))
            .and(header("authorization", "Bearer at"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page1))
            .mount(&server)
            .await;

        let events = fetch_events(
            crate::http::shared(),
            &endpoints(&server),
            "at",
            (1_793_491_200, 1_795_910_400),
            chrono_tz::America::New_York,
        )
        .await
        .unwrap();
        assert_eq!(events.len(), 2, "{events:?}");
        let sync = &events[0];
        assert_eq!(sync.id, "google:e1");
        assert_eq!(
            sync.start_unix,
            Utc.with_ymd_and_hms(2026, 11, 12, 14, 45, 0)
                .unwrap()
                .timestamp()
        );
        assert_eq!(sync.end_unix - sync.start_unix, 1800);
        assert_eq!(
            sync.join_url, "https://meet.google.com/abc-defg-hij",
            "conference video beats location"
        );
        assert_eq!(sync.provider, "meet");
        let people: Vec<_> = sync
            .attendees
            .iter()
            .map(|a| (a.name.as_str(), a.email.as_str()))
            .collect();
        assert_eq!(
            people,
            [("Juan", "juan@example.com"), ("", "jae@example.com")]
        );
        let offsite = &events[1];
        assert!(offsite.all_day);
        // An all-day date is a day, not a moment: it starts at the person's
        // midnight (New York) even though this calendar says it is UTC.
        // Read in UTC it would start at 7 PM the evening before.
        assert_eq!(
            offsite.start_unix,
            Utc.with_ymd_and_hms(2026, 11, 13, 5, 0, 0)
                .unwrap()
                .timestamp()
        );
        assert_eq!(offsite.end_unix - offsite.start_unix, 86_400);
    }

    #[tokio::test]
    async fn account_label_is_the_primary_calendar_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/calendar/v3/calendars/primary"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id": "me@example.com", "summary": "Me"})),
            )
            .mount(&server)
            .await;
        let label = account_label(crate::http::shared(), &endpoints(&server), "at")
            .await
            .unwrap();
        assert_eq!(label, "me@example.com");
    }

    #[test]
    fn credential_round_trips_through_its_setting_and_wins_over_ics() {
        let value = cred().to_setting_value().unwrap();
        let mut config = Config::default();
        config
            .apply_setting(
                "meetings.calendar.ics_url",
                r#"{"source":"literal","value":"https://cal.example.com/a.ics"}"#,
            )
            .unwrap();
        config.apply_setting(SETTING_KEY, &value).unwrap();
        assert_eq!(GoogleCalendarCredential::from_config(&config), Some(cred()));
        let c = super::super::connection(&config);
        assert_eq!(
            (c.kind, c.account_label.as_str()),
            ("google", "me@example.com")
        );
        // Never printed.
        assert!(!format!("{:?}", cred()).contains("csecret"));
        assert!(!format!("{:?}", cred()).contains("refresh"));
    }
}
