//! Stage-transition celebration payload and ASCII-art renderer.
//!
//! Extracted from `evolution/mod.rs` — this module owns the presentation of
//! an evolution event: the data payload captured when a stage transition
//! fires, the ASCII art templates, and the boxed message formatter used by
//! the CLI and service delivery paths.

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
