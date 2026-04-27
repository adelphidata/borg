use ratatui::style::{Color, Modifier, Style};

pub const CYAN: Color = Color::Rgb(0, 185, 174);
pub const YELLOW: Color = Color::Rgb(2, 195, 189);
pub const GREEN: Color = Color::Rgb(0, 159, 147);
pub const RED: Color = Color::Red;
pub const AMBER: Color = Color::Rgb(255, 191, 0);
pub const DIM_WHITE: Color = Color::DarkGray;
pub const BORDER: Color = Color::Rgb(3, 113, 113);

pub const BULLET: &str = "●";
pub const CHEVRON: &str = "❯";
pub const TREE_MID: &str = "├";
pub const TREE_END: &str = "└";

pub const TOOL_ACTIVE_GREEN: Color = Color::Rgb(0, 200, 0);
pub const INPUT_PROMPT: &str = "❯ ";

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

pub fn thinking_border_style() -> Style {
    if super::colors::is_light_terminal() {
        Style::default().fg(Color::Rgb(150, 150, 150))
    } else {
        Style::default().fg(Color::Rgb(80, 80, 80))
    }
}

/// Italic + dim, used for reasoning/thinking bodies to visually separate
/// chain-of-thought content from assistant responses. Shared so future
/// cells can reuse the same look.
pub fn reasoning_style() -> Style {
    dim().add_modifier(Modifier::ITALIC)
}

pub fn diff_add_style() -> Style {
    Style::default().fg(TOOL_ACTIVE_GREEN)
}

pub fn diff_remove_style() -> Style {
    Style::default().fg(RED)
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
