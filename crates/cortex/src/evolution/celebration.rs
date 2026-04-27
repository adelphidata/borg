//! Stage-transition celebration payload and ASCII-art renderer.
//!
//! Extracted from `evolution/mod.rs` — this module owns the presentation of
//! an evolution event: the data payload captured when a stage transition
//! fires, the ASCII art templates, and the boxed message formatter used by
//! the CLI and service delivery paths.
//!
//! Also hosts the `milestone` celebration surface — a compact box emitted for
//! sub-evolution events (level boundaries, bond thresholds, archetype
//! stabilization, etc.).

use super::capitalize_first;

/// Data captured at evolution time for celebration message rendering.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CelebrationPayload {
    /// Stage transitioning from (e.g. "base", "evolved").
    pub from_stage: String,
    /// Stage transitioning to (e.g. "evolved", "final").
    pub to_stage: String,
    /// LLM-generated evolution name, if available.
    pub evolution_name: Option<String>,
    /// LLM-generated evolution description, if available.
    pub evolution_description: Option<String>,
    /// Dominant archetype at evolution time.
    pub dominant_archetype: Option<String>,
    /// Bond score at evolution time.
    pub bond_score: u8,
    /// Vitals at evolution time.
    pub stability: u8,
    /// Vitals at evolution time.
    pub focus: u8,
    /// Vitals at evolution time.
    pub sync_stat: u8,
    /// Vitals at evolution time.
    pub growth: u8,
    /// Vitals at evolution time.
    pub happiness: u8,
}

/// ASCII art for a specific stage transition.
///
/// Each entry is a slice of lines to render inside the celebration box.
/// Extend this by adding new entries keyed on `(from_stage, to_stage)` or
/// `(to_stage, archetype)` combinations.
pub struct CelebrationArt {
    /// Lines of ASCII art to display.
    pub lines: &'static [&'static str],
}

/// Get ASCII art for a stage transition.
///
/// Currently provides base art for each transition type. Add archetype-specific
/// variants by matching on `archetype` in the future.
pub fn celebration_art(to_stage: &str, _archetype: Option<&str>) -> CelebrationArt {
    match to_stage {
        "final" => CelebrationArt {
            lines: &[
                "       /\\_____/\\           __/|__",
                "      (  o . o  )   -->   / o.O  \\___",
                "       > ^ ^ ^ <         |  __    __ \\",
                "       /_______\\         | /  \\  /  ||",
                "                         |_\\__/  \\__/|",
                "                          \\_________/",
            ],
        },
        // Default: base -> evolved
        _ => CelebrationArt {
            lines: &[
                "          .  .",
                "         /(..)\\ ",
                "        ( (\")(\")))          /\\_____/\\",
                "         \\  ~ /    -->    (  o . o  )",
                "          ~~~~             > ^ ^ ^ <",
                "                           /_______\\",
            ],
        },
    }
}

/// Format a fun ASCII art celebration message for an evolution stage transition.
pub fn format_celebration_message(payload: &CelebrationPayload) -> String {
    let mut out = String::new();

    let is_final = payload.to_stage == "final";

    // Header
    let title = if is_final {
        "* * *  F I N A L   F O R M  * * *"
    } else {
        "* * *  E V O L U T I O N  * * *"
    };

    let w = 45; // inner width
    let border = "\u{2550}".repeat(w);
    let pad_title = format!("{title:^w$}");

    out.push_str(&format!("\u{2554}{border}\u{2557}\n"));
    out.push_str(&format!("\u{2551}{pad_title}\u{2551}\n"));
    out.push_str(&format!("\u{2560}{border}\u{2563}\n"));

    let line =
        |text: &str| -> String { format!("\u{2551} {:<width$}\u{2551}\n", text, width = w - 2) };
    let empty = || -> String { line("") };

    out.push_str(&empty());

    // ASCII art — looked up by stage transition and archetype
    let art = celebration_art(&payload.to_stage, payload.dominant_archetype.as_deref());
    for art_line in art.lines {
        out.push_str(&line(art_line));
    }

    out.push_str(&empty());

    // Stage transition
    let from_label = match payload.from_stage.as_str() {
        "base" => "Base Borg",
        "evolved" => payload.evolution_name.as_deref().unwrap_or("Evolved Borg"),
        _ => "Borg",
    };
    let to_label = payload.evolution_name.as_deref().unwrap_or(if is_final {
        "Final Form"
    } else {
        "Evolved Borg"
    });
    let (from_num, to_num) = if is_final {
        ("Stage 2/3", "Stage 3/3")
    } else {
        ("Stage 1/3", "Stage 2/3")
    };

    out.push_str(&line(&format!("  {from_label}  -->  {to_label}")));
    out.push_str(&line(&format!("  {from_num}  -->  {to_num}")));

    if let Some(ref arch) = payload.dominant_archetype {
        out.push_str(&line(&format!("  Archetype: {}", capitalize_first(arch))));
    }

    out.push_str(&empty());

    // Vitals
    out.push_str(&line("  Vitals"));
    out.push_str(&line(&format!(
        "    STB: {:>3}  FOC: {:>3}  SYN: {:>3}",
        payload.stability, payload.focus, payload.sync_stat
    )));
    out.push_str(&line(&format!(
        "    GRW: {:>3}  HAP: {:>3}",
        payload.growth, payload.happiness
    )));
    out.push_str(&line(&format!("  Bond: {}", payload.bond_score)));

    out.push_str(&empty());

    // Description
    if let Some(ref desc) = payload.evolution_description {
        let max_line = w - 6;
        let words: Vec<&str> = desc.split_whitespace().collect();
        let mut lines = Vec::new();
        let mut current = String::new();
        for word in &words {
            if current.is_empty() {
                current = word.to_string();
            } else if current.len() + 1 + word.len() <= max_line {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(current);
                current = word.to_string();
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        for (i, l) in lines.iter().enumerate() {
            let prefix = if i == 0 { "  \"" } else { "   " };
            let suffix = if i == lines.len() - 1 { "\"" } else { "" };
            out.push_str(&line(&format!("{prefix}{l}{suffix}")));
        }
        out.push_str(&empty());
    }

    // Bottom border
    out.push_str(&format!("\u{255A}{border}\u{255D}\n"));

    out
}

// ── Milestone celebrations ──

/// Compact payload emitted for sub-evolution milestones.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MilestonePayload {
    /// Stable milestone identifier (e.g. `"level_10_base"`, `"first_evolution"`).
    pub milestone_id: String,
    /// User-facing title.
    pub title: String,
    /// Current level at time of unlock.
    pub level: u8,
    /// Stage string (base/evolved/final).
    pub stage: String,
    /// Associated archetype, if any.
    pub archetype: Option<String>,
}

/// Unified celebration surface — evolution stage transitions or sub-evolution
/// milestones. Callers pick the variant and `format_celebration` renders.
pub enum CelebrationKind {
    /// Full stage-transition celebration.
    Evolution(CelebrationPayload),
    /// Sub-evolution milestone (level, bond, archetype, streak).
    Milestone(MilestonePayload),
}

/// Render a celebration regardless of kind.
pub fn format_celebration(kind: &CelebrationKind) -> String {
    match kind {
        CelebrationKind::Evolution(p) => format_celebration_message(p),
        CelebrationKind::Milestone(p) => format_milestone(p),
    }
}

/// Compact ASCII box for a milestone unlock. Intentionally smaller than the
/// full evolution art — milestones fire frequently and shouldn't dominate
/// the transcript.
fn format_milestone(p: &MilestonePayload) -> String {
    let inner = 36; // inner width between borders
    let border = "\u{2500}".repeat(inner);

    let line = |text: &str| -> String {
        // Truncate/pad to `inner - 2` so the two side spaces keep the box aligned.
        let max = inner.saturating_sub(2);
        let body: String = text.chars().take(max).collect();
        format!("\u{2502} {body:<max$} \u{2502}\n")
    };

    let mut out = String::new();
    out.push_str(&format!("\u{256D}{border}\u{256E}\n"));
    out.push_str(&line("MILESTONE UNLOCKED"));
    out.push_str(&line(""));
    out.push_str(&line(&p.title));

    let stage_cap = capitalize_first(&p.stage);
    out.push_str(&line(&format!("Lvl.{} | {}", p.level, stage_cap)));

    if let Some(ref arch) = p.archetype {
        out.push_str(&line(&format!("Archetype: {}", capitalize_first(arch))));
    }
    out.push_str(&format!("\u{2570}{border}\u{256F}\n"));

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_milestone_contains_title() {
        let p = MilestonePayload {
            milestone_id: "level_10_base".into(),
            title: "Lvl.10".into(),
            level: 10,
            stage: "base".into(),
            archetype: Some("ops".into()),
        };
        let s = format_milestone(&p);
        assert!(s.contains("Lvl.10"), "expected title in output: {s}");
        assert!(s.contains("Base"), "expected capitalized stage: {s}");
        assert!(s.contains("Ops"), "expected archetype in output: {s}");
        assert!(s.contains("MILESTONE UNLOCKED"));
    }

    #[test]
    fn format_celebration_dispatches_variants() {
        let milestone = CelebrationKind::Milestone(MilestonePayload {
            milestone_id: "first_evolution".into(),
            title: "First Evolution".into(),
            level: 0,
            stage: "evolved".into(),
            archetype: None,
        });
        assert!(format_celebration(&milestone).contains("First Evolution"));

        let evolution = CelebrationKind::Evolution(CelebrationPayload {
            from_stage: "base".into(),
            to_stage: "evolved".into(),
            evolution_name: Some("Pipeline Warden".into()),
            evolution_description: Some("A vigilant guardian".into()),
            dominant_archetype: Some("guardian".into()),
            bond_score: 30,
            stability: 50,
            focus: 50,
            sync_stat: 50,
            growth: 50,
            happiness: 50,
        });
        assert!(format_celebration(&evolution).contains("Pipeline Warden"));
    }
}
