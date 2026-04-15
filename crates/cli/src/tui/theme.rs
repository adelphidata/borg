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
    Style::default().fg(DIM_WHITE)
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
    Style::default().fg(Color::Rgb(80, 80, 80))
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

    // -- style functions return expected modifiers/colors --

    #[test]
    fn bold_has_bold_modifier() {
        let style = bold();
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn dim_has_dim_white_fg() {
        let style = dim();
        assert_eq!(style.fg, Some(DIM_WHITE));
    }

    #[test]
    fn code_style_has_cyan_fg() {
        assert_eq!(code_style().fg, Some(CYAN));
    }

    #[test]
    fn tool_style_has_yellow_fg() {
        assert_eq!(tool_style().fg, Some(YELLOW));
    }

    #[test]
    fn success_style_has_green_fg() {
        assert_eq!(success_style().fg, Some(GREEN));
    }

    #[test]
    fn error_style_has_red_fg() {
        assert_eq!(error_style().fg, Some(RED));
    }

    #[test]
    fn tool_bullet_active_is_bold_green() {
        let style = tool_bullet_active();
        assert_eq!(style.fg, Some(TOOL_ACTIVE_GREEN));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn tool_bullet_done_matches_dim() {
        assert_eq!(tool_bullet_done(), dim());
    }

    #[test]
    fn popup_selected_has_bg_and_fg() {
        let style = popup_selected();
        assert!(style.bg.is_some());
        assert_eq!(style.fg, Some(Color::White));
    }

    #[test]
    fn file_mention_style_is_underlined_cyan() {
        let style = file_mention_style();
        assert_eq!(style.fg, Some(CYAN));
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn check_style_is_bold_green() {
        let style = check_style();
        assert_eq!(style.fg, Some(TOOL_ACTIVE_GREEN));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn cross_style_is_bold_red() {
        let style = cross_style();
        assert_eq!(style.fg, Some(RED));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn header_style_is_bold_white() {
        let style = header_style();
        assert_eq!(style.fg, Some(Color::White));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn warning_style_has_amber_fg() {
        assert_eq!(warning_style().fg, Some(AMBER));
    }

    #[test]
    fn icon_style_has_cyan_fg() {
        assert_eq!(icon_style().fg, Some(CYAN));
    }

    #[test]
    fn thinking_border_has_gray_fg() {
        let style = thinking_border_style();
        assert_eq!(style.fg, Some(Color::Rgb(80, 80, 80)));
    }

    #[test]
    fn diff_add_style_is_green() {
        assert_eq!(diff_add_style().fg, Some(TOOL_ACTIVE_GREEN));
    }

    #[test]
    fn diff_remove_style_is_red() {
        assert_eq!(diff_remove_style().fg, Some(RED));
    }

    #[test]
    fn diff_header_style_is_bold_cyan() {
        let style = diff_header_style();
        assert_eq!(style.fg, Some(CYAN));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn toast_variant_borders_distinct() {
        assert_ne!(toast_info_border().fg, toast_success_border().fg);
        assert_ne!(toast_warning_border().fg, toast_error_border().fg);
        assert_eq!(toast_error_border().fg, Some(RED));
        assert_eq!(toast_success_border().fg, Some(TOOL_ACTIVE_GREEN));
    }
}
