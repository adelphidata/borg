//! Calendar sources for meetings: Google Calendar (OAuth, the user's own
//! client) and any calendar's secret ICS address. Both normalise to one
//! [`CalendarEvent`], cached in the `calendar_events` table by the daemon's
//! sync worker.

pub mod google;
pub mod ics;
pub mod join_url;
pub mod sync;

use crate::config::Config;
use crate::meetings::Attendee;

/// Which video service an event's join link belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinProvider {
    /// Google Meet.
    Meet,
    /// Zoom.
    Zoom,
    /// Microsoft Teams.
    Teams,
}

impl JoinProvider {
    /// The stored and wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Meet => "meet",
            Self::Zoom => "zoom",
            Self::Teams => "teams",
        }
    }
}

/// Where an event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarSource {
    /// Google Calendar API.
    Google,
    /// An ICS feed.
    Ics,
}

impl CalendarSource {
    /// The stored and wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Ics => "ics",
        }
    }
}

/// One calendar event, whichever source it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarEvent {
    /// `google:<id>`, `ics:<uid>`, or `ics:<uid>:<start>` for one occurrence
    /// of a recurring event.
    pub id: String,
    /// `google` or `ics`.
    pub source: String,
    /// Summary line.
    pub title: String,
    /// Start, Unix seconds.
    pub start_unix: i64,
    /// End, Unix seconds.
    pub end_unix: i64,
    /// A date-only event.
    pub all_day: bool,
    /// Invitees, organiser included.
    pub attendees: Vec<Attendee>,
    /// Meet, Zoom or Teams link; empty when none was found.
    pub join_url: String,
    /// `meet`, `zoom`, `teams`, or empty.
    pub provider: String,
}

/// What calendar is connected, as the status line shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CalendarConnection {
    /// Something is connected.
    pub connected: bool,
    /// `google`, `ics`, or empty.
    pub kind: &'static str,
    /// The Google account, or the feed's host.
    pub account_label: String,
}

/// The connected calendar. Google wins when both are set: it carries
/// attendees and conference links an ICS export often drops.
pub fn connection(config: &Config) -> CalendarConnection {
    if let Some(cred) = google::GoogleCalendarCredential::from_config(config) {
        return CalendarConnection {
            connected: true,
            kind: "google",
            account_label: cred.account_label,
        };
    }
    if let Some(url) = ics_url(config) {
        let host = url::Url::parse(&url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        return CalendarConnection {
            connected: true,
            kind: "ics",
            account_label: host,
        };
    }
    CalendarConnection::default()
}

/// The configured ICS address, resolved; `None` when unset or unresolvable.
pub fn ics_url(config: &Config) -> Option<String> {
    let secret = config.meetings.calendar.ics_url.as_ref()?;
    match secret.resolve() {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("meetings.calendar.ics_url could not be resolved: {e}");
            None
        }
    }
}

/// The event that a call starting at `now` most plausibly is: one with a join
/// link whose start is within `window_secs` of now (or that is under way),
/// the closest start first.
pub fn nearest_joinable_event(
    events: &[CalendarEvent],
    now: i64,
    window_secs: i64,
) -> Option<&CalendarEvent> {
    events
        .iter()
        .filter(|e| !e.join_url.is_empty() && !e.all_day)
        .filter(|e| {
            let near_start = (e.start_unix - now).abs() <= window_secs;
            let under_way = e.start_unix <= now && now < e.end_unix;
            near_start || under_way
        })
        .min_by_key(|e| (e.start_unix - now).abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str, start: i64, end: i64, join: &str) -> CalendarEvent {
        CalendarEvent {
            id: id.into(),
            source: "ics".into(),
            title: id.into(),
            start_unix: start,
            end_unix: end,
            all_day: false,
            attendees: vec![],
            join_url: join.into(),
            provider: String::new(),
        }
    }

    #[test]
    fn nearest_joinable_event_needs_a_link_and_a_nearby_or_running_start() {
        let now = 10_000;
        let link = "https://meet.google.com/abc-defg-hij";
        // (start offset, end offset, link, expected match)
        let cases = [
            (90, 1800, link, true),    // starts in 90 s
            (200, 1800, link, false),  // too far ahead
            (-600, 1200, link, true),  // already under way
            (-1800, -60, link, false), // finished
            (60, 1800, "", false),     // no link
        ];
        for (start, end, join, want) in cases {
            let events = [event("e", now + start, now + end, join)];
            assert_eq!(
                nearest_joinable_event(&events, now, 120).is_some(),
                want,
                "start {start:+} end {end:+} link {join:?}"
            );
        }
    }

    #[test]
    fn nearest_joinable_event_prefers_the_closest_start() {
        let now = 10_000;
        let link = "https://zoom.us/j/123456789";
        let events = [
            event("running", now - 1500, now + 300, link),
            event("next", now + 30, now + 1800, link),
        ];
        assert_eq!(
            nearest_joinable_event(&events, now, 120).unwrap().id,
            "next"
        );
    }

    #[test]
    fn connection_reports_the_ics_host_and_nothing_when_unset() {
        let mut config = Config::default();
        assert_eq!(connection(&config), CalendarConnection::default());
        config.meetings.calendar.ics_url =
            Some(crate::security::secrets_resolve::SecretRef::Literal {
                value: "https://calendar.example.com/private/abc/basic.ics".into(),
            });
        let c = connection(&config);
        assert!(c.connected);
        assert_eq!(c.kind, "ics");
        assert_eq!(c.account_label, "calendar.example.com");
    }
}
