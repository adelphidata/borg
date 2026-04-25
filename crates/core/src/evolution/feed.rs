//! XP aggregation helpers for the `/xp` surface.
//!
//! These helpers bucket raw `EvolutionEvent`s into a user-facing summary
//! (today / week totals, top sources, archetype breakdown) and a unified
//! feed of `xp_gain` + `level_up` + `milestone_unlocked` rows.
//!
//! All functions are pure: they accept events and a `now` timestamp and
//! return owned data. No DB access, no clock reads.

use std::collections::HashMap;

use super::{Archetype, EvolutionEvent};

/// One day in seconds.
const DAY_SECS: i64 = 86_400;

/// Aggregate XP totals and breakdowns over the last 24h / 7d.
#[derive(Debug, Clone, Default)]
pub struct XpSummary {
    /// XP earned in the last 24 hours.
    pub today: u32,
    /// XP earned in the last 7 days.
    pub week: u32,
    /// Top sources over the last 7 days, descending by XP.
    pub top_sources: Vec<(String, u32)>,
    /// XP per archetype over the last 7 days.
    pub archetype_breakdown: HashMap<Archetype, u32>,
}

/// Kind of a feed entry — discriminates which metadata to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedKind {
    /// An `xp_gain` event.
    XpGain,
    /// A `level_up` event.
    LevelUp,
    /// A `milestone_unlocked` event.
    Milestone,
}

/// One pre-formatted row for the `/xp` feed.
#[derive(Debug, Clone)]
pub struct FeedEntry {
    /// Unix timestamp of the event.
    pub ts: i64,
    /// Discriminator for renderers.
    pub kind: FeedKind,
    /// Pre-formatted display string (e.g. `"+2 XP · apply_patch · builder"`).
    pub display: String,
}

/// Compute a 24h/7d XP summary from the event stream.
pub fn xp_summary(events: &[EvolutionEvent], now: i64) -> XpSummary {
    let cutoff_today = now - DAY_SECS;
    let cutoff_week = now - 7 * DAY_SECS;

    let mut today: u32 = 0;
    let mut week: u32 = 0;
    let mut sources: HashMap<String, u32> = HashMap::new();
    let mut archs: HashMap<Archetype, u32> = HashMap::new();

    for e in events {
        if e.event_type != "xp_gain" || e.created_at < cutoff_week {
            continue;
        }
        let xp = e.xp_delta.max(0) as u32;
        week = week.saturating_add(xp);
        if e.created_at >= cutoff_today {
            today = today.saturating_add(xp);
        }
        *sources.entry(e.source.clone()).or_insert(0) += xp;
        if let Some(arch_str) = e.archetype.as_deref() {
            if let Some(arch) = Archetype::parse(arch_str) {
                *archs.entry(arch).or_insert(0) += xp;
            }
        }
    }

    let mut top_sources: Vec<(String, u32)> = sources.into_iter().collect();
    top_sources.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top_sources.truncate(5);

    XpSummary {
        today,
        week,
        top_sources,
        archetype_breakdown: archs,
    }
}

/// Unify `xp_gain` + `level_up` + `milestone_unlocked` into a single timeline.
///
/// Newest first; capped at `limit`. Display strings are pre-formatted so
/// channel renderers and TUI tabs can emit them verbatim.
pub fn recent_xp_feed(events: &[EvolutionEvent], limit: usize) -> Vec<FeedEntry> {
    let mut entries: Vec<FeedEntry> = Vec::new();

    for e in events {
        let kind = match e.event_type.as_str() {
            "xp_gain" => FeedKind::XpGain,
            "level_up" => FeedKind::LevelUp,
            "milestone_unlocked" => FeedKind::Milestone,
            _ => continue,
        };

        let display = match kind {
            FeedKind::XpGain => {
                let arch = e.archetype.as_deref().unwrap_or("-");
                format!("+{} XP · {} · {}", e.xp_delta.max(0), e.source, arch)
            }
            FeedKind::LevelUp => {
                let (from, to) = parse_level_transition(e);
                match (from, to) {
                    (Some(f), Some(t)) => format!("Level up · Lvl.{f} → Lvl.{t}"),
                    _ => "Level up".to_string(),
                }
            }
            FeedKind::Milestone => {
                let title = parse_string_field(e, "title").unwrap_or_else(|| {
                    parse_string_field(e, "milestone_id").unwrap_or_else(|| "milestone".into())
                });
                format!("Milestone · {title}")
            }
        };

        entries.push(FeedEntry {
            ts: e.created_at,
            kind,
            display,
        });
    }

    entries.sort_by(|a, b| b.ts.cmp(&a.ts));
    entries.truncate(limit);
    entries
}

fn parse_level_transition(e: &EvolutionEvent) -> (Option<u64>, Option<u64>) {
    let Some(meta) = e.metadata_json.as_deref() else {
        return (None, None);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(meta) else {
        return (None, None);
    };
    let from = v.get("from_level").and_then(serde_json::Value::as_u64);
    let to = v.get("to_level").and_then(serde_json::Value::as_u64);
    (from, to)
}

fn parse_string_field(e: &EvolutionEvent, key: &str) -> Option<String> {
    let meta = e.metadata_json.as_deref()?;
    let v: serde_json::Value = serde_json::from_str(meta).ok()?;
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event_type: &str, source: &str, arch: Option<&str>, xp: i32, ts: i64) -> EvolutionEvent {
        EvolutionEvent {
            id: 0,
            event_type: event_type.to_string(),
            xp_delta: xp,
            archetype: arch.map(String::from),
            source: source.to_string(),
            metadata_json: None,
            created_at: ts,
            hmac: String::new(),
            prev_hmac: String::new(),
            session_id: None,
            pubkey_id: None,
        }
    }

    #[test]
    fn xp_summary_empty() {
        let s = xp_summary(&[], 1_000_000);
        assert_eq!(s.today, 0);
        assert_eq!(s.week, 0);
        assert!(s.top_sources.is_empty());
        assert!(s.archetype_breakdown.is_empty());
    }

    #[test]
    fn xp_summary_bounds_today_and_week() {
        let now = 1_000_000i64;
        let today = now - 3_600;
        let yesterday = now - 2 * DAY_SECS;
        let ancient = now - 30 * DAY_SECS;
        let events = vec![
            ev("xp_gain", "a", Some("ops"), 2, today),
            ev("xp_gain", "b", Some("ops"), 2, yesterday),
            ev("xp_gain", "c", Some("ops"), 2, ancient), // outside 7d
        ];
        let s = xp_summary(&events, now);
        assert_eq!(s.today, 2);
        assert_eq!(s.week, 4);
        assert_eq!(*s.archetype_breakdown.get(&Archetype::Ops).unwrap(), 4);
    }

    #[test]
    fn xp_summary_top_sources_sorted() {
        let now = 1_000_000i64;
        let t = now - 100;
        let events = vec![
            ev("xp_gain", "low", None, 1, t),
            ev("xp_gain", "high", None, 2, t),
            ev("xp_gain", "high", None, 2, t),
        ];
        let s = xp_summary(&events, now);
        assert_eq!(s.top_sources[0].0, "high");
        assert_eq!(s.top_sources[0].1, 4);
    }

    #[test]
    fn recent_xp_feed_newest_first_and_limited() {
        let events = vec![
            ev("xp_gain", "a", Some("ops"), 2, 100),
            ev("xp_gain", "b", Some("ops"), 2, 300),
            ev("xp_gain", "c", Some("ops"), 2, 200),
        ];
        let feed = recent_xp_feed(&events, 2);
        assert_eq!(feed.len(), 2);
        assert_eq!(feed[0].ts, 300);
        assert_eq!(feed[1].ts, 200);
    }

    #[test]
    fn recent_xp_feed_formats_level_up() {
        let mut e = ev("level_up", "hook", None, 0, 500);
        e.metadata_json = Some(r#"{"from_level":9,"to_level":10,"stage":"base"}"#.to_string());
        let feed = recent_xp_feed(&[e], 10);
        assert_eq!(feed.len(), 1);
        assert!(feed[0].display.contains("9"));
        assert!(feed[0].display.contains("10"));
    }
}
