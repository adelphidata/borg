//! ASCII share-card rendering for the `/card` surface.
//!
//! Pure text rendering — no I/O. Produces a boxed card summarizing the
//! current evolution (stage, level, name, dominant archetype, description)
//! suitable for terminal output, channel replies, or writing to a file.

use super::{capitalize_first, Archetype, EvolutionState, Stage};

/// Inner width (characters between the box borders).
const INNER_WIDTH: usize = 48;

/// Render a boxed ASCII share card summarizing the current evolution.
///
/// Layout (48-char inner width, `╭── … ──╮` border):
///   - Header:      `BORG · {name} Lv.{level}`
///   - Stage:       `Stage: Base I|Evolved II|Final III`
///   - Archetype:   `Archetype: {Display}` or `Archetype: —`
///   - Description: one-line tagline (truncated to fit)
///
/// Name fallback: `evolution_name` if set, otherwise a stage-based default
/// (`Base Borg` / `Evolved Borg` / `Final Borg`).
///
/// Description fallback: `evolution_description` if set, otherwise a short
/// stage-based tagline.
pub fn render_ascii_card(evo: &EvolutionState) -> String {
    let name = card_display_name(evo);
    let stage_label = stage_label(evo.stage);
    let archetype_label = archetype_label(evo.dominant_archetype);
    let description = card_description(evo);

    let header = format!("BORG \u{00B7} {name} Lv.{}", evo.level);
    let stage_line = format!("Stage: {stage_label}");
    let archetype_line = format!("Archetype: {archetype_label}");
    let desc_line = truncate_to(&description, INNER_WIDTH.saturating_sub(4));

    let mut out = String::new();
    out.push_str(&top_border());
    out.push('\n');
    out.push_str(&box_line(&header));
    out.push('\n');
    out.push_str(&box_line(&stage_line));
    out.push('\n');
    out.push_str(&box_line(&archetype_line));
    out.push('\n');
    out.push_str(&box_line(&desc_line));
    out.push('\n');
    out.push_str(&bottom_border());
    out.push('\n');
    out
}

fn card_display_name(evo: &EvolutionState) -> String {
    match &evo.evolution_name {
        Some(n) => n.clone(),
        None => match evo.stage {
            Stage::Base => "Base Borg".to_string(),
            Stage::Evolved => "Evolved Borg".to_string(),
            Stage::Final => "Final Borg".to_string(),
        },
    }
}

fn stage_label(stage: Stage) -> &'static str {
    match stage {
        Stage::Base => "Base I",
        Stage::Evolved => "Evolved II",
        Stage::Final => "Final III",
    }
}

fn archetype_label(archetype: Option<Archetype>) -> String {
    match archetype {
        Some(a) => capitalize_first(&format!("{a}")),
        None => "\u{2014}".to_string(), // em dash
    }
}

fn card_description(evo: &EvolutionState) -> String {
    if let Some(d) = &evo.evolution_description {
        return d.clone();
    }
    match evo.stage {
        Stage::Base => "Still discovering its specialization.".to_string(),
        Stage::Evolved => "Specialization taking shape.".to_string(),
        Stage::Final => "Fully realized specialization.".to_string(),
    }
}

/// Truncate `s` to at most `max` display chars, appending `…` when truncated.
fn truncate_to(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let char_count = s.chars().count();
    if char_count <= max {
        return s.to_string();
    }
    let take = max.saturating_sub(1);
    let mut out: String = s.chars().take(take).collect();
    out.push('\u{2026}');
    out
}

fn top_border() -> String {
    format!("\u{256D}{}\u{256E}", "\u{2500}".repeat(INNER_WIDTH))
}

fn bottom_border() -> String {
    format!("\u{2570}{}\u{256F}", "\u{2500}".repeat(INNER_WIDTH))
}

/// Render a single padded content line `│ {text} │`.
/// `text` is truncated to fit if it exceeds the inner width.
fn box_line(text: &str) -> String {
    let inner = INNER_WIDTH.saturating_sub(2); // space on each side
    let truncated = truncate_to(text, inner);
    let visible_len = truncated.chars().count();
    let pad = inner.saturating_sub(visible_len);
    format!("\u{2502} {}{} \u{2502}", truncated, " ".repeat(pad))
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fixture(
        stage: Stage,
        name: Option<&str>,
        desc: Option<&str>,
        arch: Option<Archetype>,
    ) -> EvolutionState {
        EvolutionState {
            stage,
            level: match stage {
                Stage::Base => 7,
                Stage::Evolved => 42,
                Stage::Final => 99,
            },
            total_xp: 1234,
            xp_to_next_level: 100,
            dominant_archetype: arch,
            evolution_name: name.map(str::to_string),
            evolution_description: desc.map(str::to_string),
            archetype_scores: HashMap::new(),
            lifetime_scores: HashMap::new(),
            last_30d_scores: HashMap::new(),
            dominant_history: Vec::new(),
            total_events: 0,
            chain_valid: true,
            momentum: HashMap::new(),
            level_up_events_recent: Vec::new(),
            mood: None,
            readiness: None,
        }
    }

    fn assert_box_shape(card: &str) {
        // Every line should be exactly INNER_WIDTH + 2 (for the two borders) chars wide.
        let expected = INNER_WIDTH + 2;
        for line in card.lines() {
            assert_eq!(
                line.chars().count(),
                expected,
                "line width mismatch for {line:?}"
            );
        }
        assert!(card.lines().next().unwrap().starts_with('\u{256D}'));
        assert!(card.lines().last().unwrap().starts_with('\u{2570}'));
    }

    #[test]
    fn render_base_stage_no_name_no_archetype() {
        let evo = fixture(Stage::Base, None, None, None);
        let card = render_ascii_card(&evo);
        assert_box_shape(&card);
        assert!(card.contains("BORG \u{00B7} Base Borg Lv.7"));
        assert!(card.contains("Stage: Base I"));
        assert!(card.contains("Archetype: \u{2014}"));
        assert!(card.contains("Still discovering"));
    }

    #[test]
    fn render_evolved_stage_with_name_and_archetype() {
        let evo = fixture(
            Stage::Evolved,
            Some("Pipeline Warden"),
            Some("A vigilant guardian of pipelines."),
            Some(Archetype::Ops),
        );
        let card = render_ascii_card(&evo);
        assert_box_shape(&card);
        assert!(card.contains("BORG \u{00B7} Pipeline Warden Lv.42"));
        assert!(card.contains("Stage: Evolved II"));
        assert!(card.contains("Archetype: Ops"));
        assert!(card.contains("A vigilant guardian of pipelines."));
    }

    #[test]
    fn render_final_stage_snapshot() {
        let evo = fixture(
            Stage::Final,
            Some("Infrastructure Sovereign"),
            Some("Commanding the full stack with quiet authority."),
            Some(Archetype::Ops),
        );
        let card = render_ascii_card(&evo);
        assert_box_shape(&card);
        assert!(card.contains("Lv.99"));
        assert!(card.contains("Stage: Final III"));
        assert!(card.contains("Archetype: Ops"));
    }

    #[test]
    fn description_truncates_to_fit() {
        let long = "x".repeat(200);
        let evo = fixture(Stage::Base, Some("n"), Some(&long), None);
        let card = render_ascii_card(&evo);
        assert_box_shape(&card);
        // Ellipsis indicates truncation occurred.
        assert!(card.contains('\u{2026}'));
    }

    #[test]
    fn fallback_stage_names() {
        let base = render_ascii_card(&fixture(Stage::Base, None, None, None));
        let evolved = render_ascii_card(&fixture(Stage::Evolved, None, None, None));
        let final_ = render_ascii_card(&fixture(Stage::Final, None, None, None));
        assert!(base.contains("Base Borg"));
        assert!(evolved.contains("Evolved Borg"));
        assert!(final_.contains("Final Borg"));
    }
}
