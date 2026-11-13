//! ICS feeds: the secret address every calendar app can export (Google's
//! "Secret address in iCal format", Outlook's published calendar, Fastmail,
//! iCloud). The feed is fetched whole, then each event and every occurrence
//! of a recurring one inside the sync window becomes a [`CalendarEvent`].
//!
//! Recurrence is expanded here rather than through `icalendar`'s helper
//! because that helper hands the raw `TZID` to `rrule`, and Outlook feeds
//! name zones the Windows way ("Pacific Standard Time"), which `rrule`
//! cannot resolve. Zones go through [`resolve_tzid`] first.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone};
use icalendar::{Calendar, Component, Event, Property};
use rrule::{RRule, RRuleSet, Tz, Unvalidated};
use url::Url;

use super::{join_url, CalendarEvent, CalendarSource};
use crate::meetings::Attendee;

/// The largest feed accepted. A busy work calendar with years of history is
/// a few megabytes; anything past this is not a calendar.
pub const MAX_ICS_BYTES: usize = 5 * 1024 * 1024;

/// Most occurrences one recurring event may produce inside a window. A
/// minutely rule would otherwise expand without bound.
const MAX_OCCURRENCES: u16 = 1000;

const DAY_SECS: i64 = 86_400;

/// Check a feed address the user pasted. Only `https://` and `webcal://`
/// (rewritten to `https://`) are accepted, and never a cloud-metadata,
/// link-local or private-network host: the address is fetched by the daemon
/// on a timer, so it must not be a way to reach the local network.
pub fn validate_ics_url(raw: &str) -> Result<Url> {
    let raw = raw.trim();
    let rewritten;
    let raw = match raw.get(..9) {
        Some(prefix) if prefix.eq_ignore_ascii_case("webcal://") => {
            rewritten = format!("https://{}", &raw[9..]);
            rewritten.as_str()
        }
        _ => raw,
    };
    let url = Url::parse(raw).map_err(|_| {
        anyhow!("That doesn't look like a web address. Paste the calendar's secret address.")
    })?;
    if url.scheme() != "https" {
        bail!("Use the calendar's https:// or webcal:// address.");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("That address has no host. Paste the calendar's full address."))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if crate::url_policy::ssrf::is_always_blocked_host(&host) || host == "localhost" {
        bail!("That address points at this computer or its network. Paste the calendar's public address.");
    }
    let blocked_ip = host.parse::<std::net::IpAddr>().is_ok_and(|ip| {
        crate::url_policy::ssrf::is_always_blocked_ip(&ip)
            || crate::url_policy::is_private_class(&ip)
    });
    if blocked_ip {
        bail!("That address points at this computer or its network. Paste the calendar's public address.");
    }
    Ok(url)
}

/// How many redirects a feed may take before Borg gives up.
const MAX_REDIRECTS: usize = 5;

/// Every hop of a redirect must itself be an address [`validate_ics_url`]
/// accepts, so a public feed cannot bounce the daemon to the local network.
fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        match validate_ics_url(attempt.url().as_str()) {
            Ok(_) => attempt.follow(),
            Err(e) => attempt.error(e.to_string()),
        }
    })
}

/// The client feeds are fetched with. Not [`crate::http::shared`]: the
/// daemon fetches a pasted address every few minutes, so the address check
/// must hold where the connection is made, not only on the text. The
/// resolver refuses a name that resolves to a private, loopback or metadata
/// address (on the first request and on every redirect), and the redirect
/// policy re-checks each hop.
pub fn feed_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(10))
        .redirect(redirect_policy())
        .dns_resolver(crate::web::ssrf_guard_resolver(false))
        .build()
        .context("could not set up the calendar client")
}

/// Download a feed, refusing one larger than [`MAX_ICS_BYTES`]. The address
/// is a secret, so no error carries it. Production callers pass
/// [`feed_client`].
pub async fn fetch_ics(http: &reqwest::Client, url: &Url) -> Result<String> {
    let host = url.host_str().unwrap_or("the calendar").to_string();
    let mut resp = http
        .get(url.clone())
        .header(reqwest::header::ACCEPT, "text/calendar, */*;q=0.5")
        .send()
        .await
        .map_err(|e| {
            anyhow!(
                "Borg couldn't reach {host}: {}. Check the address and your connection.",
                e.without_url()
            )
        })?;
    let status = resp.status();
    if !status.is_success() {
        bail!("{host} answered {status}. Check that this is the calendar's secret address.");
    }
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_ICS_BYTES as u64)
    {
        bail!("The calendar from {host} is larger than 5 MB, which Borg doesn't read.");
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| {
        anyhow!(
            "Reading the calendar from {host} failed: {}. Try again in a minute.",
            e.without_url()
        )
    })? {
        if body.len() + chunk.len() > MAX_ICS_BYTES {
            bail!("The calendar from {host} is larger than 5 MB, which Borg doesn't read.");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Every event, and every occurrence of a recurring one, that overlaps
/// `window` (Unix seconds, start inclusive, end exclusive). Times with no
/// zone are read in the calendar's own zone (`X-WR-TIMEZONE`), else
/// `default_tz`. All-day dates are days, not moments, so they are read in
/// `default_tz`, the person's own zone: a holiday on the 26th starts at
/// their midnight even when the feed says it is a UTC calendar.
pub fn parse_ics(
    text: &str,
    window: (i64, i64),
    default_tz: chrono_tz::Tz,
) -> Result<Vec<CalendarEvent>> {
    if !text.trim_start().starts_with("BEGIN:VCALENDAR") {
        bail!("That address didn't return a calendar. Paste the calendar's ICS (iCal) address.");
    }
    let calendar: Calendar = text
        .parse()
        .map_err(|e: String| anyhow!("Borg couldn't read that calendar: {e}"))?;
    let zones = Zones {
        cal: calendar
            .get_timezone()
            .and_then(resolve_tzid)
            .unwrap_or(Tz::Tz(default_tz)),
        days: Tz::Tz(default_tz),
    };

    // Occurrences moved or cancelled one at a time: each override names the
    // original start it replaces.
    let mut overridden: HashMap<&str, HashSet<i64>> = HashMap::new();
    for event in calendar.events() {
        let original = event
            .properties()
            .get("RECURRENCE-ID")
            .and_then(|rid| prop_time(rid, zones));
        if let (Some(uid), Some(original)) = (event.get_uid(), original) {
            overridden
                .entry(uid)
                .or_default()
                .insert(original.at.timestamp());
        }
    }

    let mut out = Vec::new();
    for event in calendar.events() {
        let Some(uid) = event.get_uid().filter(|u| !u.trim().is_empty()) else {
            continue;
        };
        if is_cancelled(event) {
            continue;
        }
        let Some(start) = event
            .properties()
            .get("DTSTART")
            .and_then(|p| prop_time(p, zones))
        else {
            tracing::debug!("calendar event {uid} has no readable start; skipped");
            continue;
        };
        let span = Span::of(event, &start, zones);
        let base = Base::of(event);

        if let Some(rid) = event.properties().get("RECURRENCE-ID") {
            let Some(original) = prop_time(rid, zones) else {
                continue;
            };
            let id = format!("ics:{uid}:{}", original.at.timestamp());
            push_if_overlapping(&mut out, &base, id, &start, &span, window);
            continue;
        }

        let recurs = event.property_value("RRULE").is_some()
            || event.multi_properties().contains_key("RDATE");
        if !recurs {
            push_if_overlapping(&mut out, &base, format!("ics:{uid}"), &start, &span, window);
            continue;
        }
        let starts = match expand(event, &start, &span, window, zones) {
            Ok(starts) => starts,
            Err(e) => {
                tracing::warn!(
                    "calendar event {uid}: recurrence unreadable ({e}); using its first date only"
                );
                vec![start.at]
            }
        };
        let skip = overridden.get(uid);
        for at in starts {
            let ts = at.timestamp();
            if skip.is_some_and(|s| s.contains(&ts)) {
                continue;
            }
            let occurrence = IcsTime {
                at,
                all_day: start.all_day,
            };
            push_if_overlapping(
                &mut out,
                &base,
                format!("ics:{uid}:{ts}"),
                &occurrence,
                &span,
                window,
            );
        }
    }
    out.sort_by(|a, b| {
        a.start_unix
            .cmp(&b.start_unix)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(out)
}

/// A zone name as calendars write it: IANA (`Europe/London`, sometimes with
/// a leading `/`), or a Windows name from Outlook and Exchange. `None` when
/// unknown; callers fall back to the calendar's zone.
pub fn resolve_tzid(raw: &str) -> Option<Tz> {
    let name = raw.trim().trim_matches('"').trim_start_matches('/');
    if let Ok(tz) = name.parse::<chrono_tz::Tz>() {
        return Some(Tz::Tz(tz));
    }
    let iana = WINDOWS_ZONES
        .iter()
        .find(|(windows, _)| windows.eq_ignore_ascii_case(name))
        .map(|(_, iana)| *iana)?;
    iana.parse::<chrono_tz::Tz>().ok().map(Tz::Tz)
}

/// The Windows zone names Exchange writes most often, mapped to IANA. Not the
/// full CLDR table: an unknown name falls back to the calendar's own zone,
/// which for a person's calendar is nearly always right.
const WINDOWS_ZONES: &[(&str, &str)] = &[
    ("UTC", "UTC"),
    ("Coordinated Universal Time", "UTC"),
    ("Hawaiian Standard Time", "Pacific/Honolulu"),
    ("Alaskan Standard Time", "America/Anchorage"),
    ("Pacific Standard Time", "America/Los_Angeles"),
    ("US Mountain Standard Time", "America/Phoenix"),
    ("Mountain Standard Time", "America/Denver"),
    ("Central Standard Time", "America/Chicago"),
    ("Central Standard Time (Mexico)", "America/Mexico_City"),
    ("Central America Standard Time", "America/Guatemala"),
    ("Eastern Standard Time", "America/New_York"),
    ("US Eastern Standard Time", "America/Indianapolis"),
    ("SA Pacific Standard Time", "America/Bogota"),
    ("Atlantic Standard Time", "America/Halifax"),
    ("Newfoundland Standard Time", "America/St_Johns"),
    ("E. South America Standard Time", "America/Sao_Paulo"),
    ("Argentina Standard Time", "America/Buenos_Aires"),
    ("Greenwich Standard Time", "Atlantic/Reykjavik"),
    ("GMT Standard Time", "Europe/London"),
    ("W. Europe Standard Time", "Europe/Berlin"),
    ("Romance Standard Time", "Europe/Paris"),
    ("Central Europe Standard Time", "Europe/Budapest"),
    ("Central European Standard Time", "Europe/Warsaw"),
    ("GTB Standard Time", "Europe/Bucharest"),
    ("FLE Standard Time", "Europe/Kiev"),
    ("E. Europe Standard Time", "Europe/Chisinau"),
    ("South Africa Standard Time", "Africa/Johannesburg"),
    ("Israel Standard Time", "Asia/Jerusalem"),
    ("Turkey Standard Time", "Europe/Istanbul"),
    ("Russian Standard Time", "Europe/Moscow"),
    ("Arabian Standard Time", "Asia/Dubai"),
    ("Pakistan Standard Time", "Asia/Karachi"),
    ("India Standard Time", "Asia/Kolkata"),
    ("SE Asia Standard Time", "Asia/Bangkok"),
    ("China Standard Time", "Asia/Shanghai"),
    ("Singapore Standard Time", "Asia/Singapore"),
    ("Taipei Standard Time", "Asia/Taipei"),
    ("Tokyo Standard Time", "Asia/Tokyo"),
    ("Korea Standard Time", "Asia/Seoul"),
    ("AUS Eastern Standard Time", "Australia/Sydney"),
    ("E. Australia Standard Time", "Australia/Brisbane"),
    ("W. Australia Standard Time", "Australia/Perth"),
    ("New Zealand Standard Time", "Pacific/Auckland"),
];

/// A start or end as the feed wrote it, resolved to an instant.
#[derive(Debug, Clone, Copy)]
struct IcsTime {
    at: DateTime<Tz>,
    /// A `VALUE=DATE` day rather than a moment.
    all_day: bool,
}

/// The zones a feed's times are read in.
#[derive(Clone, Copy)]
struct Zones {
    /// Floating date-times: the calendar's own zone.
    cal: Tz,
    /// `VALUE=DATE` days: the person's zone.
    days: Tz,
}

/// How long each occurrence lasts: whole days for all-day events (so a
/// daylight-saving change does not shift the end by an hour), else seconds.
#[derive(Debug, Clone, Copy)]
enum Span {
    Days(i64),
    Secs(i64),
}

impl Span {
    fn of(event: &Event, start: &IcsTime, zones: Zones) -> Self {
        let end = event
            .properties()
            .get("DTEND")
            .and_then(|p| prop_time(p, zones));
        if start.all_day {
            let days = end
                .filter(|e| e.all_day)
                .map(|e| (e.at.date_naive() - start.at.date_naive()).num_days())
                .or_else(|| {
                    event
                        .property_value("DURATION")
                        .and_then(parse_duration)
                        .map(|s| s / DAY_SECS)
                })
                .filter(|d| *d > 0)
                .unwrap_or(1);
            return Self::Days(days);
        }
        let secs = end
            .map(|e| e.at.timestamp() - start.at.timestamp())
            .or_else(|| event.property_value("DURATION").and_then(parse_duration))
            .filter(|s| *s >= 0)
            .unwrap_or(0);
        Self::Secs(secs)
    }

    fn secs_upper_bound(self) -> i64 {
        match self {
            Self::Days(d) => (d + 1) * DAY_SECS,
            Self::Secs(s) => s,
        }
    }

    fn end_of(self, start: &DateTime<Tz>) -> i64 {
        match self {
            Self::Secs(s) => start.timestamp() + s,
            Self::Days(d) => {
                let day = start.date_naive() + chrono::Days::new(d.unsigned_abs());
                local_midnight(start.timezone(), day)
                    .map_or(start.timestamp() + d * DAY_SECS, |m| m.timestamp())
            }
        }
    }
}

/// What every occurrence of an event shares.
struct Base {
    title: String,
    attendees: Vec<Attendee>,
    join: Option<(String, super::JoinProvider)>,
}

impl Base {
    fn of(event: &Event) -> Self {
        let title = event
            .get_summary()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Untitled event")
            .to_string();
        let field = |key: &str| event.property_value(key).unwrap_or("");
        let join = join_url::extract(&[
            field("X-GOOGLE-CONFERENCE"),
            field("X-MICROSOFT-SKYPETEAMSMEETINGURL"),
            field("URL"),
            field("LOCATION"),
            field("DESCRIPTION"),
        ]);
        Self {
            title,
            attendees: attendees(event),
            join,
        }
    }
}

fn push_if_overlapping(
    out: &mut Vec<CalendarEvent>,
    base: &Base,
    id: String,
    start: &IcsTime,
    span: &Span,
    (from, to): (i64, i64),
) {
    let start_unix = start.at.timestamp();
    let end_unix = span.end_of(&start.at).max(start_unix);
    // A zero-length event still occupies its instant.
    if start_unix >= to || end_unix.max(start_unix + 1) <= from {
        return;
    }
    let (join_url, provider) = base
        .join
        .as_ref()
        .map_or((String::new(), String::new()), |(u, p)| {
            (u.clone(), p.as_str().to_string())
        });
    out.push(CalendarEvent {
        id,
        source: CalendarSource::Ics.as_str().to_string(),
        title: base.title.clone(),
        start_unix,
        end_unix,
        all_day: start.all_day,
        attendees: base.attendees.clone(),
        join_url,
        provider,
    });
}

/// The starts of a recurring event that could overlap `window`.
fn expand(
    event: &Event,
    start: &IcsTime,
    span: &Span,
    (from, to): (i64, i64),
    zones: Zones,
) -> Result<Vec<DateTime<Tz>>> {
    let mut set = RRuleSet::new(start.at);
    if let Some(rule) = event.property_value("RRULE") {
        let mut rule: RRule<Unvalidated> = rule.parse().context("RRULE")?;
        // RFC 5545 wants UNTIL in UTC when DTSTART has a zone; many feeds
        // write it floating or as a bare date. Read it in the start's zone.
        if let Some(until) = rule
            .get_until()
            .copied()
            .filter(|u| u.timezone() != Tz::UTC)
        {
            let fixed = from_local(start.at.timezone(), until.naive_local())
                .ok_or_else(|| anyhow!("UNTIL falls in a clock change"))?;
            rule = rule.until(fixed.with_timezone(&Tz::UTC));
        }
        set = set.rrule(rule.validate(start.at).context("RRULE")?);
    } else {
        // An RDATE-only event still occurs at DTSTART.
        set = set.rdate(start.at);
    }
    for (key, include) in [("RDATE", true), ("EXDATE", false)] {
        for prop in event.multi_properties().get(key).into_iter().flatten() {
            for at in prop_times(prop, zones) {
                set = if include {
                    set.rdate(at.at)
                } else {
                    set.exdate(at.at)
                };
            }
        }
    }
    let lo = Tz::UTC
        .timestamp_opt(from - span.secs_upper_bound() - DAY_SECS, 0)
        .single()
        .ok_or_else(|| anyhow!("window start out of range"))?;
    let hi = Tz::UTC
        .timestamp_opt(to + DAY_SECS, 0)
        .single()
        .ok_or_else(|| anyhow!("window end out of range"))?;
    let result = set.after(lo).before(hi).all(MAX_OCCURRENCES);
    if result.limited && result.dates.len() >= usize::from(MAX_OCCURRENCES) {
        tracing::debug!("recurrence capped at {MAX_OCCURRENCES} occurrences");
    }
    Ok(result.dates)
}

/// A single-valued DTSTART/DTEND/RECURRENCE-ID.
fn prop_time(prop: &Property, zones: Zones) -> Option<IcsTime> {
    prop_times(prop, zones).into_iter().next()
}

/// Every value of a date property (RDATE and EXDATE may list several,
/// comma-separated). Periods (`start/end`) are skipped.
fn prop_times(prop: &Property, zones: Zones) -> Vec<IcsTime> {
    let tzid = prop.params().get("TZID").map(icalendar::Parameter::value);
    let date_only = prop
        .params()
        .get("VALUE")
        .is_some_and(|v| v.value().eq_ignore_ascii_case("DATE"));
    prop.value()
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty() && !v.contains('/'))
        .filter_map(|v| parse_time(v, tzid, date_only, zones))
        .collect()
}

fn parse_time(value: &str, tzid: Option<&str>, date_only: bool, zones: Zones) -> Option<IcsTime> {
    if date_only || value.len() == 8 {
        let day = NaiveDate::parse_from_str(value, "%Y%m%d").ok()?;
        return Some(IcsTime {
            at: local_midnight(zones.days, day)?,
            all_day: true,
        });
    }
    if let Some(utc) = value.strip_suffix('Z').or_else(|| value.strip_suffix('z')) {
        let naive = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok()?;
        return Some(IcsTime {
            at: Tz::UTC.from_utc_datetime(&naive),
            all_day: false,
        });
    }
    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
    let tz = tzid.and_then(resolve_tzid).unwrap_or(zones.cal);
    Some(IcsTime {
        at: from_local(tz, naive)?,
        all_day: false,
    })
}

/// A wall-clock time in `tz`. In a spring-forward gap the time does not
/// exist; the event happens an hour later on the clock, as calendar apps do.
fn from_local(tz: Tz, naive: NaiveDateTime) -> Option<DateTime<Tz>> {
    tz.from_local_datetime(&naive).earliest().or_else(|| {
        tz.from_local_datetime(&(naive + chrono::Duration::hours(1)))
            .earliest()
    })
}

fn local_midnight(tz: Tz, day: NaiveDate) -> Option<DateTime<Tz>> {
    from_local(tz, day.and_hms_opt(0, 0, 0)?)
}

/// An RFC 5545 duration (`PT1H30M`, `P1D`, `P2W`) in seconds; negative
/// durations are not event lengths and read as `None`.
fn parse_duration(raw: &str) -> Option<i64> {
    let s = raw.trim().strip_prefix('+').unwrap_or(raw.trim());
    let s = s.strip_prefix('P')?;
    let mut total = 0i64;
    let mut num = String::new();
    let mut in_time = false;
    for c in s.chars() {
        match c {
            '0'..='9' => num.push(c),
            'T' => in_time = true,
            unit => {
                let n: i64 = num.parse().ok()?;
                num.clear();
                total += n * match (unit, in_time) {
                    ('W', false) => 7 * DAY_SECS,
                    ('D', false) => DAY_SECS,
                    ('H', true) => 3600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return None,
                };
            }
        }
    }
    num.is_empty().then_some(total)
}

fn is_cancelled(event: &Event) -> bool {
    event
        .property_value("STATUS")
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("CANCELLED"))
}

/// Organiser first, then invitees; rooms and resources are not people.
fn attendees(event: &Event) -> Vec<Attendee> {
    let organizer = event.properties().get("ORGANIZER");
    let invitees = event
        .multi_properties()
        .get("ATTENDEE")
        .into_iter()
        .flatten();
    let mut seen = HashSet::new();
    organizer
        .into_iter()
        .chain(invitees)
        .filter(|p| {
            !p.params().get("CUTYPE").is_some_and(|c| {
                matches!(c.value().to_ascii_uppercase().as_str(), "ROOM" | "RESOURCE")
            })
        })
        .filter_map(|p| {
            let value = p.value().trim();
            let email = match value.get(..7) {
                Some(prefix) if prefix.eq_ignore_ascii_case("mailto:") => &value[7..],
                _ => "",
            }
            .trim()
            .to_string();
            if email.ends_with("resource.calendar.google.com") {
                return None;
            }
            let name = p
                .params()
                .get("CN")
                .map(|c| c.value().trim().trim_matches('"').to_string())
                .filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case(&email))
                .unwrap_or_default();
            if name.is_empty() && email.is_empty() {
                return None;
            }
            seen.insert(if email.is_empty() {
                name.to_lowercase()
            } else {
                email.to_lowercase()
            })
            .then_some(Attendee { name, email })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use chrono_tz::America::New_York;

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0)
            .unwrap()
            .timestamp()
    }

    fn feed(events: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\n{}END:VCALENDAR\r\n",
            events.replace('\n', "\r\n")
        )
    }

    const NOV: (i64, i64) = (1_793_491_200, 1_795_910_400); // 2026-11-01Z .. 2026-11-29Z

    const WEEKLY: &str = "BEGIN:VEVENT
UID:standup-1
SUMMARY:Weekly sync
DTSTART;TZID=America/New_York:20261102T093000
DTEND;TZID=America/New_York:20261102T100000
RRULE:FREQ=WEEKLY;BYDAY=MO
EXDATE;TZID=America/New_York:20261109T093000
LOCATION:https://meet.google.com/abc-defg-hij
END:VEVENT
";

    #[test]
    fn window_constant_is_november() {
        assert_eq!(NOV, (utc(2026, 11, 1, 0, 0), utc(2026, 11, 29, 0, 0)));
    }

    #[test]
    fn weekly_rule_expands_inside_the_window_and_honours_exdate() {
        let events = parse_ics(&feed(WEEKLY), NOV, New_York).unwrap();
        let starts: Vec<i64> = events.iter().map(|e| e.start_unix).collect();
        // Mondays in November 2026 at 9:30 EST (UTC-5), the 9th excluded,
        // the 30th outside the window.
        assert_eq!(
            starts,
            [
                utc(2026, 11, 2, 14, 30),
                utc(2026, 11, 16, 14, 30),
                utc(2026, 11, 23, 14, 30)
            ]
        );
        for e in &events {
            assert_eq!(
                e.end_unix - e.start_unix,
                1800,
                "duration kept per occurrence"
            );
            assert_eq!(e.id, format!("ics:standup-1:{}", e.start_unix));
            assert_eq!(e.provider, "meet");
            assert_eq!(e.join_url, "https://meet.google.com/abc-defg-hij");
        }
    }

    #[test]
    fn a_moved_occurrence_replaces_the_one_it_overrides() {
        let moved = "BEGIN:VEVENT
UID:standup-1
RECURRENCE-ID;TZID=America/New_York:20261116T093000
SUMMARY:Weekly sync (moved)
DTSTART;TZID=America/New_York:20261116T110000
DTEND;TZID=America/New_York:20261116T113000
END:VEVENT
";
        let cancelled = "BEGIN:VEVENT
UID:standup-1
RECURRENCE-ID;TZID=America/New_York:20261123T093000
STATUS:CANCELLED
DTSTART;TZID=America/New_York:20261123T093000
END:VEVENT
";
        let events =
            parse_ics(&feed(&format!("{WEEKLY}{moved}{cancelled}")), NOV, New_York).unwrap();
        let got: Vec<(i64, &str, &str)> = events
            .iter()
            .map(|e| (e.start_unix, e.title.as_str(), e.id.as_str()))
            .collect();
        let moved_id = format!("ics:standup-1:{}", utc(2026, 11, 16, 14, 30));
        assert_eq!(
            got,
            [
                (
                    utc(2026, 11, 2, 14, 30),
                    "Weekly sync",
                    format!("ics:standup-1:{}", utc(2026, 11, 2, 14, 30)).as_str()
                ),
                (
                    utc(2026, 11, 16, 16, 0),
                    "Weekly sync (moved)",
                    moved_id.as_str()
                ),
            ]
        );
    }

    #[test]
    fn start_times_resolve_whatever_zone_style_the_feed_uses() {
        let want = utc(2026, 11, 12, 15, 0);
        let cases = [
            "DTSTART:20261112T150000Z",
            "DTSTART;TZID=Europe/London:20261112T150000",
            "DTSTART;TZID=/Europe/London:20261112T150000",
            "DTSTART;TZID=Pacific Standard Time:20261112T070000",
            "DTSTART;TZID=\"W. Europe Standard Time\":20261112T160000",
            // Floating: read in the default zone.
            "DTSTART:20261112T100000",
            // Unknown zone: falls back to the calendar's zone.
            "DTSTART;TZID=Somewhere Odd:20261112T100000",
        ];
        for line in cases {
            let ics = feed(&format!(
                "BEGIN:VEVENT\nUID:x\nSUMMARY:t\n{line}\nEND:VEVENT\n"
            ));
            let events = parse_ics(&ics, NOV, New_York).unwrap();
            assert_eq!(events.len(), 1, "{line}");
            assert_eq!(events[0].start_unix, want, "{line}");
            assert!(!events[0].all_day);
        }
    }

    #[test]
    fn calendar_zone_governs_floating_times() {
        let ics = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nX-WR-TIMEZONE:Europe/London\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART:20261112T150000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        let events = parse_ics(&ics, NOV, New_York).unwrap();
        assert_eq!(events[0].start_unix, utc(2026, 11, 12, 15, 0));
    }

    #[test]
    fn all_day_events_span_whole_local_days() {
        let ics = feed(
            "BEGIN:VEVENT\nUID:off\nSUMMARY:Offsite\nDTSTART;VALUE=DATE:20261112\nDTEND;VALUE=DATE:20261114\nEND:VEVENT\nBEGIN:VEVENT\nUID:hol\nSUMMARY:Holiday\nDTSTART;VALUE=DATE:20261126\nEND:VEVENT\n",
        );
        let events = parse_ics(&ics, NOV, New_York).unwrap();
        let offsite = &events[0];
        assert!(offsite.all_day);
        assert_eq!(offsite.start_unix, utc(2026, 11, 12, 5, 0), "midnight EST");
        assert_eq!(
            offsite.end_unix,
            utc(2026, 11, 14, 5, 0),
            "DTEND is exclusive"
        );
        let holiday = &events[1];
        assert_eq!(
            holiday.end_unix - holiday.start_unix,
            DAY_SECS,
            "no DTEND means one day"
        );
    }

    #[test]
    fn all_day_dates_start_at_the_persons_midnight_whatever_zone_the_feed_names() {
        // Google's public holiday feeds say X-WR-TIMEZONE:UTC. Read in UTC,
        // Thanksgiving would start at 7 PM on the 25th in New York and land
        // on the wrong day.
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nX-WR-TIMEZONE:UTC\r\nBEGIN:VEVENT\r\nUID:t\r\nSUMMARY:Thanksgiving\r\nDTSTART;VALUE=DATE:20261126\r\nDTEND;VALUE=DATE:20261127\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:f\r\nDTSTART:20261126T150000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_ics(ics, NOV, New_York).unwrap();
        let day = events.iter().find(|e| e.all_day).unwrap();
        assert_eq!(day.start_unix, utc(2026, 11, 26, 5, 0), "midnight EST");
        assert_eq!(day.end_unix, utc(2026, 11, 27, 5, 0));
        let floating = events.iter().find(|e| !e.all_day).unwrap();
        assert_eq!(
            floating.start_unix,
            utc(2026, 11, 26, 15, 0),
            "a floating time still follows the feed's own zone"
        );
    }

    #[test]
    fn people_join_links_and_cancellations() {
        let ics = feed(
            "BEGIN:VEVENT
UID:p
SUMMARY:PSP7 Tiger Sync
DTSTART:20261112T150000Z
DURATION:PT45M
ORGANIZER;CN=Juan Diaz:mailto:juan@example.com
ATTENDEE;CN=Jae Kim;PARTSTAT=ACCEPTED:mailto:jae@example.com
ATTENDEE;CN=juan@example.com:MAILTO:juan@example.com
ATTENDEE;CUTYPE=ROOM;CN=Room 4:mailto:room4@example.com
ATTENDEE:mailto:c_123@resource.calendar.google.com
ATTENDEE:mailto:ali@example.com
DESCRIPTION:Join Zoom Meeting\\nhttps://example.zoom.us/j/81234567890?pwd=abc
END:VEVENT
BEGIN:VEVENT
UID:gone
SUMMARY:Cancelled
STATUS:CANCELLED
DTSTART:20261112T170000Z
END:VEVENT
BEGIN:VEVENT
UID:later
SUMMARY:Outside
DTSTART:20261212T170000Z
END:VEVENT
",
        );
        let events = parse_ics(&ics, NOV, New_York).unwrap();
        assert_eq!(
            events.len(),
            1,
            "cancelled and out-of-window events dropped"
        );
        let e = &events[0];
        assert_eq!(e.end_unix - e.start_unix, 45 * 60);
        assert_eq!(e.provider, "zoom");
        assert_eq!(e.join_url, "https://example.zoom.us/j/81234567890?pwd=abc");
        let people: Vec<(&str, &str)> = e
            .attendees
            .iter()
            .map(|a| (a.name.as_str(), a.email.as_str()))
            .collect();
        assert_eq!(
            people,
            [
                ("Juan Diaz", "juan@example.com"),
                ("Jae Kim", "jae@example.com"),
                ("", "ali@example.com"),
            ]
        );
    }

    #[test]
    fn a_rule_with_a_floating_until_still_expands() {
        let ics = feed(
            "BEGIN:VEVENT
UID:d
SUMMARY:Daily
DTSTART;TZID=America/New_York:20261116T090000
DTEND;TZID=America/New_York:20261116T091500
RRULE:FREQ=DAILY;UNTIL=20261118T090000
END:VEVENT
",
        );
        let events = parse_ics(&ics, NOV, New_York).unwrap();
        assert_eq!(events.len(), 3, "16th, 17th and 18th, UNTIL inclusive");
    }

    #[test]
    fn garbage_is_an_error_not_an_empty_calendar() {
        assert!(parse_ics("<html>sign in</html>", NOV, New_York).is_err());
    }

    #[test]
    fn duration_parsing() {
        let cases = [
            ("PT1H30M", Some(5400)),
            ("P1D", Some(DAY_SECS)),
            ("P1W", Some(7 * DAY_SECS)),
            ("P1DT2H", Some(DAY_SECS + 7200)),
            ("PT45S", Some(45)),
            ("-PT1H", None),
            ("1H", None),
            ("PT1H5", None),
        ];
        for (raw, want) in cases {
            assert_eq!(parse_duration(raw), want, "{raw}");
        }
    }

    #[test]
    fn feed_addresses_must_be_public_https() {
        let ok = [
            (
                "https://calendar.google.com/calendar/ical/x/private-abc/basic.ics",
                "https://calendar.google.com/calendar/ical/x/private-abc/basic.ics",
            ),
            (
                "webcal://p01-caldav.icloud.com/published/2/abc",
                "https://p01-caldav.icloud.com/published/2/abc",
            ),
            (
                "  WEBCAL://outlook.office365.com/owa/calendar/a/b/calendar.ics ",
                "https://outlook.office365.com/owa/calendar/a/b/calendar.ics",
            ),
        ];
        for (raw, want) in ok {
            assert_eq!(validate_ics_url(raw).unwrap().as_str(), want, "{raw}");
        }
        let bad = [
            "http://calendar.example.com/a.ics",
            "file:///etc/passwd",
            "https://169.254.169.254/latest",
            "https://metadata.google.internal/x",
            "https://localhost/a.ics",
            "https://192.168.1.10/a.ics",
            "https://[::1]/a.ics",
            "not a url",
        ];
        for raw in bad {
            assert!(validate_ics_url(raw).is_err(), "{raw} should be refused");
        }
    }

    #[tokio::test]
    async fn fetch_caps_the_body_and_keeps_the_address_out_of_errors() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/private-secret/basic.ics"))
            .respond_with(ResponseTemplate::new(200).set_body_string(feed(WEEKLY)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/private-secret/huge.ics"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; MAX_ICS_BYTES + 1]))
            .mount(&server)
            .await;
        let http = crate::http::shared();
        let base = Url::parse(&server.uri()).unwrap();

        let ok = fetch_ics(http, &base.join("/private-secret/basic.ics").unwrap())
            .await
            .unwrap();
        assert!(ok.contains("UID:standup-1"));

        let huge = fetch_ics(http, &base.join("/private-secret/huge.ics").unwrap())
            .await
            .unwrap_err()
            .to_string();
        assert!(huge.contains("larger than 5 MB"), "{huge}");

        let missing = fetch_ics(http, &base.join("/private-secret/gone.ics").unwrap())
            .await
            .unwrap_err()
            .to_string();
        assert!(missing.contains("404"), "{missing}");
        assert!(
            !missing.contains("private-secret"),
            "the secret path leaked: {missing}"
        );
    }

    #[tokio::test]
    async fn a_feed_cannot_redirect_the_daemon_to_the_local_network() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        for (from, to) in [
            (
                "/to-metadata.ics",
                "https://169.254.169.254/latest/meta-data/",
            ),
            ("/to-lan.ics", "https://10.0.0.5/feed.ics"),
            ("/to-plain-http.ics", "http://calendar.example.com/feed.ics"),
        ] {
            Mock::given(method("GET"))
                .and(path(from))
                .respond_with(ResponseTemplate::new(302).insert_header("location", to))
                .mount(&server)
                .await;
        }
        // The production redirect policy on a client that can reach the mock.
        let http = reqwest::Client::builder()
            .redirect(redirect_policy())
            .build()
            .unwrap();
        let base = Url::parse(&server.uri()).unwrap();
        for from in ["/to-metadata.ics", "/to-lan.ics", "/to-plain-http.ics"] {
            let err = fetch_ics(&http, &base.join(from).unwrap())
                .await
                .expect_err(from)
                .to_string();
            assert!(
                err.contains("redirect"),
                "refused at the redirect, not attempted: {from}: {err}"
            );
        }
        feed_client().expect("the production client builds");
    }
}
