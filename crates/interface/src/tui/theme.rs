use ratatui::style::{Color, Modifier, Style};

pub const CYAN: Color = Color::Rgb(0, 185, 174);
pub const YELLOW: Color = Color::Rgb(2, 195, 189);
pub const GREEN: Color = Color::Rgb(0, 159, 147);
pub const RED: Color = Color::Red;
pub const AMBER: Color = Color::Rgb(255, 191, 0);
pub const DIM_WHITE: Color = Color::DarkGray;
pub const BORDER: Color = Color::Rgb(3, 113, 113);

// glyphs: lighter bullet (•) and single chevron (›) for message prefixes.
pub const BULLET: &str = "•";
pub const CHEVRON: &str = "›";
pub const TREE_MID: &str = "├";
pub const TREE_END: &str = "└";

pub const TOOL_ACTIVE_GREEN: Color = Color::Rgb(0, 200, 0);

pub const CHECK: &str = "✓";
pub const CROSS: &str = "✗";
pub const SEPARATOR: &str = "─";
pub const BOX_TOP_LEFT: &str = "╭";
pub const BOX_TOP_RIGHT: &str = "╮";
pub const BOX_BOTTOM_LEFT: &str = "╰";
pub const BOX_BOTTOM_RIGHT: &str = "╯";
pub const BOX_VERTICAL: &str = "│";
pub const ELLIPSIS: &str = "…";

pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn dim() -> Style {
    // On light terminals, DarkGray blends into the background; use a mid-grey
    // so dim text still reads. `super::colors::is_light_terminal()` returns
    // false when the terminal bg was not detected, keeping the legacy behavior.
    if super::colors::is_light_terminal() {
        Style::default().fg(Color::Rgb(90, 90, 90))
    } else {
        Style::default().fg(DIM_WHITE)
    }
}

pub fn code_style() -> Style {
    Style::default().fg(CYAN)
}

pub fn tool_style() -> Style {
    Style::default().fg(YELLOW)
}

pub fn success_style() -> Style {
    Style::default().fg(GREEN)
}

pub fn error_style() -> Style {
    Style::default().fg(RED)
}

pub fn tool_bullet_active() -> Style {
    Style::default()
        .fg(TOOL_ACTIVE_GREEN)
        .add_modifier(Modifier::BOLD)
}

pub fn tool_bullet_done() -> Style {
    dim()
}

pub fn popup_selected() -> Style {
    Style::default().bg(Color::Rgb(3, 49, 46)).fg(Color::White)
}

pub fn file_mention_style() -> Style {
    Style::default().fg(CYAN).add_modifier(Modifier::UNDERLINED)
}

pub fn check_style() -> Style {
    Style::default()
        .fg(TOOL_ACTIVE_GREEN)
        .add_modifier(Modifier::BOLD)
}

pub fn cross_style() -> Style {
    Style::default().fg(RED).add_modifier(Modifier::BOLD)
}

pub fn header_style() -> Style {
    Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

pub fn warning_style() -> Style {
    Style::default().fg(AMBER)
}

pub fn icon_style() -> Style {
    Style::default().fg(CYAN)
}

/// Italic + dim, used for reasoning/thinking bodies to visually separate
/// chain-of-thought content from assistant responses. Shared so future
/// cells can reuse the same look.
pub fn reasoning_style() -> Style {
    dim().add_modifier(Modifier::ITALIC)
}

// --- markdown semantic styles ---

/// Heading level 1: bold + underlined.
pub fn md_h1_style() -> Style {
    Style::default()
        .add_modifier(Modifier::BOLD)
        .add_modifier(Modifier::UNDERLINED)
}

/// Heading level 2: bold.
pub fn md_h2_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Heading levels 3–6: italic (3 also bold).
pub fn md_h3_style() -> Style {
    Style::default()
        .add_modifier(Modifier::BOLD)
        .add_modifier(Modifier::ITALIC)
}

pub fn md_h4_style() -> Style {
    Style::default().add_modifier(Modifier::ITALIC)
}

/// Ordered-list markers (e.g. `1.`): light blue.
pub fn md_ordered_marker_style() -> Style {
    Style::default().fg(Color::LightBlue)
}

/// Key-hint footer text ("ctrl + c" labels).
pub fn key_hint_style() -> Style {
    dim()
}

pub fn diff_add_style() -> Style {
    Style::default().fg(TOOL_ACTIVE_GREEN)
}

pub fn diff_remove_style() -> Style {
    Style::default().fg(RED)
}

/// adaptive backgrounds for diff add/remove rows.
///
/// Dark terminals: muted green (`#213A2B`) / red (`#4A221D`).
/// Light terminals: pastel green (`#dafbe1`) / red (`#ffebe9`).
pub fn diff_add_bg() -> Color {
    if super::colors::is_light_terminal() {
        Color::Rgb(218, 251, 225)
    } else {
        Color::Rgb(33, 58, 43)
    }
}

pub fn diff_remove_bg() -> Color {
    if super::colors::is_light_terminal() {
        Color::Rgb(255, 235, 233)
    } else {
        Color::Rgb(74, 34, 29)
    }
}

/// More-saturated diff gutter backgrounds for line-number / sign columns.
/// On dark terminals the gutter shares the row bg (returns the row bg). On
/// light terminals the gutter is more saturated (`#aceebb` / `#ffcecb`) so
/// the line numbers still read against the pastel row bg.
pub fn diff_add_gutter_bg() -> Color {
    if super::colors::is_light_terminal() {
        Color::Rgb(172, 238, 187)
    } else {
        diff_add_bg()
    }
}

pub fn diff_remove_gutter_bg() -> Color {
    if super::colors::is_light_terminal() {
        Color::Rgb(255, 206, 203)
    } else {
        diff_remove_bg()
    }
}

/// Foreground for the diff gutter (line numbers + sign), tuned for contrast
/// against the gutter bg.
pub fn diff_gutter_fg() -> Color {
    if super::colors::is_light_terminal() {
        Color::Rgb(31, 35, 40)
    } else {
        Color::Rgb(140, 140, 140)
    }
}

/// Magenta `$` shell prompt for exec/run_shell command echoes.
pub fn shell_prompt_style() -> Style {
    Style::default()
        .fg(Color::Rgb(200, 100, 200))
        .add_modifier(Modifier::BOLD)
}

/// Composer cursor chevron (`›`) — cyan when focused, dim when blurred.
pub fn composer_cursor_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(DIM_WHITE)
    }
}

pub fn diff_header_style() -> Style {
    Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
}

pub fn diff_hunk_style() -> Style {
    Style::default().fg(Color::Rgb(140, 140, 140))
}

pub fn toast_info_border() -> Style {
    Style::default().fg(CYAN)
}

pub fn toast_success_border() -> Style {
    Style::default().fg(TOOL_ACTIVE_GREEN)
}

pub fn toast_warning_border() -> Style {
    Style::default().fg(AMBER)
}

pub fn toast_error_border() -> Style {
    Style::default().fg(RED)
}

pub fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m {s}s")
        }
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    }
}

/// Style for user message lines — subtle background tint when terminal bg is known.
pub fn user_message_style() -> Style {
    match super::colors::user_message_bg() {
        Some(bg) => Style::default().bg(bg),
        None => Style::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- format_elapsed --

    #[test]
    fn format_elapsed_seconds() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(1), "1s");
        assert_eq!(format_elapsed(59), "59s");
    }

    #[test]
    fn format_elapsed_minutes() {
        assert_eq!(format_elapsed(60), "1m");
        assert_eq!(format_elapsed(61), "1m 1s");
        assert_eq!(format_elapsed(90), "1m 30s");
        assert_eq!(format_elapsed(120), "2m");
        assert_eq!(format_elapsed(3599), "59m 59s");
    }

    #[test]
    fn format_elapsed_hours() {
        assert_eq!(format_elapsed(3600), "1h");
        assert_eq!(format_elapsed(3660), "1h 1m");
        assert_eq!(format_elapsed(7200), "2h");
        assert_eq!(format_elapsed(7260), "2h 1m");
        assert_eq!(format_elapsed(86400), "24h");
    }
}
