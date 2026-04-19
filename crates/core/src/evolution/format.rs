//! Renderers for the `/xp` and `/evolution` surfaces.
//!
//! Pure string builders shared across CLI, TUI, and channel gateways so every
//! surface sees identical output. The `/xp` helpers consume
//! [`super::feed::xp_summary`] / [`super::feed::recent_xp_feed`]; the
//! `/evolution` helpers consume an [`EvolutionState`] with momentum / mood /
//! readiness filled in by the caller. No DB access, no clock reads (except
//! [`format_xp_feed`], which owns the relative-time clock).

use super::{
    capitalize_first, format_status_section, render_bar, Archetype, BlockingGate, EvolutionState,
    FeedEntry, ReadinessReport, XpSummary,
};

/// Width used for the archetype breakdown bars in the XP summary.
const BAR_WIDTH: usize = 10;

/// Width (chars) of the progress bars in the evolution overview surface.
const OVERVIEW_BAR_WIDTH: usize = 20;

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

/// Render the full `/evolution` overview.
///
/// Layout:
/// 1. Compact header reusing `format_status_section` (stage/xp bars).
/// 2. Archetype scores with momentum arrows.
/// 3. Readiness block (only if `state.readiness` has blocking gates).
///
/// Next-step hints are appended by the caller (dispatcher) — they require
/// `VitalsState` + `BondState` inputs that this pure renderer does not take.
pub fn format_evolution_overview(state: &EvolutionState) -> String {
    let mut out = String::new();
    out.push_str(&format_status_section(state));
    out.push('\n');
    out.push_str(&format_archetype_scores_with_momentum(state));

    if let Some(report) = state.readiness.as_ref() {
        if !report.blocking.is_empty() {
            out.push('\n');
            out.push_str(&format_readiness(report));
        }
    }

    out
}

/// Render archetype scores with momentum arrows appended where known.
///
/// Mirrors the existing `format_archetype_scores` layout but appends one of
/// `↑ / → / ↓` from [`super::Trend::arrow`] per archetype with a momentum
/// entry. Archetypes without a momentum entry get no arrow.
pub fn format_archetype_scores_with_momentum(state: &EvolutionState) -> String {
    let mut out = String::from("Archetype Scores\n");

    let mut sorted: Vec<(Archetype, u32)> = Archetype::ALL
        .iter()
        .map(|a| (*a, *state.archetype_scores.get(a).unwrap_or(&0)))
        .collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));

    let max_score = sorted.first().map(|(_, s)| *s).unwrap_or(1).max(1);

    for (arch, score) in &sorted {
        let arch_display = format!("{arch}");
        let capitalized = capitalize_first(&arch_display);
        let bar_len = (*score as usize * 10) / max_score as usize;
        let bar = format!(
            "{}{}",
            "\u{2588}".repeat(bar_len),
            "\u{2591}".repeat(10 - bar_len)
        );
        let marker = if Some(*arch) == state.dominant_archetype {
            " *"
        } else {
            ""
        };
        let arrow = state
            .momentum
            .get(arch)
            .map(|t| format!(" {}", t.arrow()))
            .unwrap_or_default();
        out.push_str(&format!(
            "  {capitalized:<15} {score:>5}  {bar}{marker}{arrow}\n"
        ));
    }

    out
}

/// Render the readiness block: title, overall progress bar, then one line
/// per [`BlockingGate`].
///
/// Callers should only call this when `report.blocking` is non-empty —
/// [`format_evolution_overview`] already guards on that.
pub fn format_readiness(report: &ReadinessReport) -> String {
    let mut out = String::from("Readiness\n");

    let pct = (report.progress.clamp(0.0, 1.0) * 100.0).round() as u32;
    let bar = render_bar(pct, 100, OVERVIEW_BAR_WIDTH);
    out.push_str(&format!("  overall       {bar}  {pct}%\n"));

    for gate in &report.blocking {
        out.push_str(&format_gate_line(gate));
    }

    out
}

/// Format a single blocking-gate line:
/// `  name  current/target  [bar]  hint`.
fn format_gate_line(gate: &BlockingGate) -> String {
    let pct = if gate.target > 0.0 {
        ((gate.current / gate.target).clamp(0.0, 1.0) * 100.0).round() as u32
    } else {
        0
    };
    let bar = render_bar(pct, 100, OVERVIEW_BAR_WIDTH);
    let cur = format_gate_number(gate.current);
    let tgt = format_gate_number(gate.target);
    format!(
        "  {:<14} {}/{}  {}  {}\n",
        gate.name, cur, tgt, bar, gate.hint
    )
}

/// Format a gate value: integer when whole, `NaN` as `?`, otherwise one decimal.
fn format_gate_number(n: f64) -> String {
    if n.is_nan() {
        return "?".to_string();
    }
    if (n - n.round()).abs() < f64::EPSILON {
        format!("{}", n.round() as i64)
    } else {
        format!("{n:.1}")
    }
}

/// Render a bullet list of next-step hints with a `Next Steps` title.
///
/// Empty-input callers should skip invoking this — the helper still handles
/// the empty case gracefully but the title row is wasted otherwise.
pub fn format_next_step_hints(hints: &[String]) -> String {
    let mut out = String::from("Next Steps\n");
    for h in hints {
        out.push_str(&format!("  - {h}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evolution::{xp_for_level, FeedKind, Stage, Trend};
    use std::collections::HashMap;

    // ── XP renderers ──

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

    // ── Evolution overview renderers ──

    fn mk_state(stage: Stage, level: u8) -> EvolutionState {
        let mut scores: HashMap<Archetype, u32> = HashMap::new();
        scores.insert(Archetype::Ops, 80);
        scores.insert(Archetype::Builder, 40);
        let xp_needed = xp_for_level(&stage, level);
        EvolutionState {
            stage,
            level,
            total_xp: 0,
            xp_to_next_level: xp_needed,
            dominant_archetype: Some(Archetype::Ops),
            evolution_name: None,
            evolution_description: None,
            archetype_scores: scores.clone(),
            lifetime_scores: scores.clone(),
            last_30d_scores: scores,
            dominant_history: Vec::new(),
            total_events: 0,
            chain_valid: true,
            momentum: HashMap::new(),
            level_up_events_recent: Vec::new(),
            mood: None,
            readiness: None,
        }
    }

    #[test]
    fn overview_stage1_no_readiness_renders_header_and_scores() {
        let state = mk_state(Stage::Base, 1);
        let out = format_evolution_overview(&state);
        assert!(out.contains("Base Borg Lvl.1"));
        assert!(out.contains("Archetype Scores"));
        assert!(!out.contains("Readiness\n"));
    }

    #[test]
    fn overview_stage1_near_ready_renders_readiness_block() {
        let mut state = mk_state(Stage::Base, 99);
        state.readiness = Some(ReadinessReport {
            ready: false,
            blocking: vec![BlockingGate {
                name: "bond".to_string(),
                current: 10.0,
                target: 30.0,
                hint: "Spend more time.".to_string(),
            }],
            progress: 0.66,
        });
        let out = format_evolution_overview(&state);
        assert!(out.contains("Readiness"));
        assert!(out.contains("bond"));
        assert!(out.contains("10/30"));
        assert!(out.contains("66%"));
    }

    #[test]
    fn overview_stage3_omits_readiness_block() {
        let state = mk_state(Stage::Final, 99);
        let out = format_evolution_overview(&state);
        assert!(!out.contains("Readiness\n"));
    }

    #[test]
    fn overview_with_empty_blocking_omits_readiness_block() {
        let mut state = mk_state(Stage::Base, 99);
        state.readiness = Some(ReadinessReport {
            ready: true,
            blocking: Vec::new(),
            progress: 1.0,
        });
        let out = format_evolution_overview(&state);
        assert!(!out.contains("Readiness\n"));
    }

    #[test]
    fn archetype_scores_with_momentum_renders_each_arrow() {
        let mut state = mk_state(Stage::Base, 10);
        state.momentum.insert(Archetype::Ops, Trend::Rising);
        state.momentum.insert(Archetype::Builder, Trend::Falling);
        let out = format_archetype_scores_with_momentum(&state);
        assert!(out.contains("Ops"));
        assert!(out.contains("\u{2191}"));
        assert!(out.contains("\u{2193}"));
    }

    #[test]
    fn archetype_scores_stable_momentum_renders_horizontal_arrow() {
        let mut state = mk_state(Stage::Base, 10);
        state.momentum.insert(Archetype::Ops, Trend::Stable);
        let out = format_archetype_scores_with_momentum(&state);
        assert!(out.contains("\u{2192}"));
    }

    #[test]
    fn archetype_scores_without_momentum_omits_arrow() {
        let state = mk_state(Stage::Base, 10);
        let out = format_archetype_scores_with_momentum(&state);
        assert!(!out.contains("\u{2191}"));
        assert!(!out.contains("\u{2192}"));
        assert!(!out.contains("\u{2193}"));
    }

    #[test]
    fn next_step_hints_renders_bulleted_list() {
        let hints = vec!["Do X".to_string(), "Do Y".to_string()];
        let out = format_next_step_hints(&hints);
        assert!(out.starts_with("Next Steps\n"));
        assert!(out.contains("  - Do X\n"));
        assert!(out.contains("  - Do Y\n"));
    }

    #[test]
    fn readiness_handles_nan_current() {
        let report = ReadinessReport {
            ready: false,
            blocking: vec![BlockingGate {
                name: "correction_rate".to_string(),
                current: f64::NAN,
                target: 0.20,
                hint: "low".to_string(),
            }],
            progress: 0.0,
        };
        let out = format_readiness(&report);
        assert!(out.contains("?/"));
    }
}
