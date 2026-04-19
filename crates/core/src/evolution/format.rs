//! Renderers for the `/xp` surface.
//!
//! Pure string builders shared across CLI, TUI, and channel gateways so every
//! surface sees identical output. Consumes the aggregates produced by
//! [`super::feed::xp_summary`] and [`super::feed::recent_xp_feed`].

use super::{capitalize_first, render_bar, Archetype, FeedEntry, XpSummary};

/// Width used for the archetype breakdown bars.
const BAR_WIDTH: usize = 10;

/// Render the `/xp` summary: today/week totals, top sources, archetype
/// breakdown. Uses the same 2-space indent + Unicode box aesthetic as
/// `format_status_section`.
pub fn format_xp_summary(summary: &XpSummary) -> String {
    let mut out = String::new();
    out.push_str("  XP Summary\n");
    out.push_str(&format!("  today        {:>5}\n", summary.today));
    out.push_str(&format!("  week         {:>5}\n", summary.week));

    if !summary.top_sources.is_empty() {
        out.push('\n');
        out.push_str("  Top sources (7d)\n");
        for (src, xp) in &summary.top_sources {
            out.push_str(&format!("    {src:<20} {xp:>5}\n"));
        }
    }

    if !summary.archetype_breakdown.is_empty() {
        let max = summary
            .archetype_breakdown
            .values()
            .copied()
            .max()
            .unwrap_or(1)
            .max(1);

        // Sort by XP desc (stable on archetype variant order) so repeated
        // renders don't jitter.
        let mut rows: Vec<(Archetype, u32)> = Archetype::ALL
            .iter()
            .filter_map(|a| summary.archetype_breakdown.get(a).map(|xp| (*a, *xp)))
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1));

        out.push('\n');
        out.push_str("  Archetypes (7d)\n");
        for (arch, xp) in &rows {
            let bar = render_bar(*xp, max, BAR_WIDTH);
            let label = capitalize_first(&arch.to_string());
            out.push_str(&format!("    {label:<15} {xp:>5}  {bar}\n"));
        }
    }

    out
}

/// Render the recent XP feed with relative-time prefixes.
///
/// `now` is passed implicitly via `chrono::Utc::now()` — the renderer owns
/// clock reads here because entries carry absolute timestamps.
pub fn format_xp_feed(entries: &[FeedEntry]) -> String {
    let now = chrono::Utc::now().timestamp();
    let mut out = String::new();
    out.push_str("  Recent XP\n");
    if entries.is_empty() {
        out.push_str("    (no recent activity)\n");
        return out;
    }
    for e in entries {
        let when = relative_time(now, e.ts);
        out.push_str(&format!("    {when:<10} {}\n", e.display));
    }
    out
}

/// Human-friendly relative time label (e.g. `"just now"`, `"2h ago"`,
/// `"yesterday"`, `"3d ago"`).
fn relative_time(now: i64, ts: i64) -> String {
    let delta = now.saturating_sub(ts);
    if delta < 60 {
        "just now".to_string()
    } else if delta < 3_600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3_600)
    } else if delta < 2 * 86_400 {
        "yesterday".to_string()
    } else {
        format!("{}d ago", delta / 86_400)
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evolution::{FeedKind, XpSummary};
    use std::collections::HashMap;

    fn entry(ts: i64, kind: FeedKind, display: &str) -> FeedEntry {
        FeedEntry {
            ts,
            kind,
            display: display.to_string(),
        }
    }

    #[test]
    fn summary_empty_renders_zero_totals_only() {
        let s = XpSummary::default();
        let out = format_xp_summary(&s);
        assert!(out.contains("XP Summary"));
        assert!(out.contains("today"));
        assert!(out.contains("week"));
        // No optional sections when empty.
        assert!(!out.contains("Top sources"));
        assert!(!out.contains("Archetypes"));
    }

    #[test]
    fn summary_single_day_shows_today_and_sources() {
        let mut archs = HashMap::new();
        archs.insert(Archetype::Builder, 6);
        let s = XpSummary {
            today: 6,
            week: 6,
            top_sources: vec![("apply_patch".to_string(), 6)],
            archetype_breakdown: archs,
        };
        let out = format_xp_summary(&s);
        assert!(out.contains("today"));
        assert!(out.contains("    6"));
        assert!(out.contains("Top sources (7d)"));
        assert!(out.contains("apply_patch"));
        assert!(out.contains("Archetypes (7d)"));
        assert!(out.contains("Builder"));
    }

    #[test]
    fn summary_week_spanning_multiple_archetypes() {
        let mut archs = HashMap::new();
        archs.insert(Archetype::Ops, 10);
        archs.insert(Archetype::Builder, 4);
        archs.insert(Archetype::Analyst, 2);
        let s = XpSummary {
            today: 4,
            week: 16,
            top_sources: vec![
                ("run_shell".to_string(), 10),
                ("apply_patch".to_string(), 4),
                ("read_file".to_string(), 2),
            ],
            archetype_breakdown: archs,
        };
        let out = format_xp_summary(&s);
        // Archetype rows should appear ordered by XP desc.
        let ops_idx = out.find("Ops").expect("Ops row");
        let builder_idx = out.find("Builder").expect("Builder row");
        let analyst_idx = out.find("Analyst").expect("Analyst row");
        assert!(ops_idx < builder_idx);
        assert!(builder_idx < analyst_idx);
        assert!(out.contains("run_shell"));
    }

    #[test]
    fn feed_empty_shows_placeholder() {
        let out = format_xp_feed(&[]);
        assert!(out.contains("Recent XP"));
        assert!(out.contains("no recent activity"));
    }

    #[test]
    fn feed_formats_entries_with_relative_time() {
        let now = chrono::Utc::now().timestamp();
        let entries = vec![
            entry(now - 30, FeedKind::XpGain, "+2 XP · run_shell · ops"),
            entry(now - 3_600, FeedKind::LevelUp, "Level up · Lvl.9 → Lvl.10"),
            entry(
                now - 3 * 86_400,
                FeedKind::Milestone,
                "Milestone · first_evolution",
            ),
        ];
        let out = format_xp_feed(&entries);
        assert!(out.contains("Recent XP"));
        assert!(out.contains("just now"));
        assert!(out.contains("1h ago"));
        assert!(out.contains("3d ago"));
        assert!(out.contains("Level up"));
        assert!(out.contains("Milestone"));
    }

    #[test]
    fn relative_time_labels() {
        let now = 1_000_000i64;
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now, now - 59), "just now");
        assert_eq!(relative_time(now, now - 60), "1m ago");
        assert_eq!(relative_time(now, now - 3_599), "59m ago");
        assert_eq!(relative_time(now, now - 3_600), "1h ago");
        assert_eq!(relative_time(now, now - 86_400), "yesterday");
        assert_eq!(relative_time(now, now - 2 * 86_400), "2d ago");
    }
}
