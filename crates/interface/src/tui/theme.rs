use ratatui::style::{Color, Modifier, Style};

use super::themes;

// Color accessors read from the active theme's palette every render. There are
// deliberately no module-level color constants, so a stale reference is a
// compile-time error rather than a silent legacy color.

#[inline]
pub fn cyan() -> Color {
    themes::active().palette.accent
}

#[inline]
pub fn yellow() -> Color {
    themes::active().palette.tool_accent
}

#[inline]
pub fn green() -> Color {
    themes::active().palette.success
}

#[inline]
pub fn red() -> Color {
    themes::active().palette.error
}

#[inline]
pub fn amber() -> Color {
    themes::active().palette.warning
}

#[inline]
pub fn dim_color() -> Color {
    themes::active().palette.dim
}

#[inline]
pub fn tool_active_green() -> Color {
    themes::active().palette.active
}

// glyphs: lighter bullet (•) and single chevron (›) for message prefixes.
pub const BULLET: &str = "•";
pub const CHEVRON: &str = "›";
pub const TREE_END: &str = "└";

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
    // NB: this branch is independent of theme. The theme's `dim` color is
    // used on dark terminals; the light-terminal fallback is a fixed mid-grey
    // so even a light theme's `dim` setting can't render unreadably bright
    // against a bright terminal background.
    if super::colors::is_light_terminal() {
        Style::default().fg(Color::Rgb(90, 90, 90))
    } else {
        Style::default().fg(dim_color())
    }
}

pub fn code_style() -> Style {
    Style::default().fg(cyan())
}

pub fn tool_style() -> Style {
    Style::default().fg(yellow())
}

pub fn success_style() -> Style {
    Style::default().fg(green())
}

pub fn error_style() -> Style {
    Style::default().fg(red())
}

pub fn tool_bullet_active() -> Style {
    Style::default()
        .fg(tool_active_green())
        .add_modifier(Modifier::BOLD)
}

pub fn tool_bullet_done() -> Style {
    dim()
}

pub fn popup_selected() -> Style {
    let p = themes::active();
    Style::default()
        .bg(p.palette.popup_selected_bg)
        .fg(p.palette.popup_selected_fg)
}

pub fn file_mention_style() -> Style {
    Style::default()
        .fg(cyan())
        .add_modifier(Modifier::UNDERLINED)
}

pub fn check_style() -> Style {
    Style::default()
        .fg(tool_active_green())
        .add_modifier(Modifier::BOLD)
}

pub fn cross_style() -> Style {
    Style::default().fg(red()).add_modifier(Modifier::BOLD)
}

pub fn header_style() -> Style {
    Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

pub fn warning_style() -> Style {
    Style::default().fg(amber())
}

pub fn icon_style() -> Style {
    Style::default().fg(cyan())
}

/// Italic + dim, used for reasoning/thinking bodies to visually separate
/// chain-of-thought content from assistant responses. Shared so future
/// cells can reuse the same look.
pub fn reasoning_style() -> Style {
    dim().add_modifier(Modifier::ITALIC)
}

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

pub fn diff_add_style() -> Style {
    Style::default().fg(tool_active_green())
}

pub fn diff_remove_style() -> Style {
    Style::default().fg(red())
}

/// adaptive backgrounds for diff add/remove rows.
///
/// Dark vs light terminal branch is preserved; the actual colors on each
/// branch come from the active theme so themes can tune diff legibility
/// for their accent set (e.g. `daylight` for light terminals).
pub fn diff_add_bg() -> Color {
    let p = themes::active();
    if super::colors::is_light_terminal() {
        p.palette.diff_add_bg_light
    } else {
        p.palette.diff_add_bg_dark
    }
}

pub fn diff_remove_bg() -> Color {
    let p = themes::active();
    if super::colors::is_light_terminal() {
        p.palette.diff_remove_bg_light
    } else {
        p.palette.diff_remove_bg_dark
    }
}

/// More-saturated diff gutter backgrounds for line-number / sign columns.
/// On dark terminals the gutter shares the row bg (returns the row bg). On
/// light terminals the gutter uses the theme's saturated gutter color so
/// line numbers still read against the pastel row bg.
pub fn diff_add_gutter_bg() -> Color {
    if super::colors::is_light_terminal() {
        themes::active().palette.diff_add_gutter_bg_light
    } else {
        diff_add_bg()
    }
}

pub fn diff_remove_gutter_bg() -> Color {
    if super::colors::is_light_terminal() {
        themes::active().palette.diff_remove_gutter_bg_light
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

/// Shell prompt color for exec/run_shell command echoes.
pub fn shell_prompt_style() -> Style {
    Style::default()
        .fg(themes::active().palette.shell_prompt)
        .add_modifier(Modifier::BOLD)
}

/// Composer cursor chevron (`›`) — accent when focused, dim when blurred.
pub fn composer_cursor_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(cyan()).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(dim_color())
    }
}

pub fn diff_header_style() -> Style {
    Style::default().fg(cyan()).add_modifier(Modifier::BOLD)
}

pub fn diff_hunk_style() -> Style {
    Style::default().fg(Color::Rgb(140, 140, 140))
}

pub fn toast_info_border() -> Style {
    Style::default().fg(cyan())
}

pub fn toast_success_border() -> Style {
    Style::default().fg(tool_active_green())
}

pub fn toast_warning_border() -> Style {
    Style::default().fg(amber())
}

pub fn toast_error_border() -> Style {
    Style::default().fg(red())
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
    use std::sync::Arc;

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

    // -- theme integration --
    //
    // These tests verify that the accessor functions in this file actually
    // read from the active theme. If someone re-hardcodes a constant by
    // mistake, the active-theme swap will be ignored and these fail.

    /// Snapshot/restore the active theme around a closure AND hold the
    /// shared `themes::active::TEST_LOCK` for the body so a sibling test
    /// in another module can't swap the cell mid-flight (cargo test
    /// parallelizes within a binary).
    fn with_theme<F: FnOnce()>(name: &str, body: F) {
        let _guard = super::themes::active::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = themes::active();
        let Some(t) = themes::builtin(name) else {
            panic!("builtin '{name}' must exist");
        };
        themes::set_active(Arc::new(t.clone()));
        body();
        themes::set_active(before);
    }

    #[test]
    fn color_accessors_track_active_theme() {
        with_theme("default", || {
            assert_eq!(cyan(), Color::Rgb(0, 185, 174));
            assert_eq!(yellow(), Color::Rgb(2, 195, 189));
            assert_eq!(green(), Color::Rgb(0, 159, 147));
        });
        with_theme("slate", || {
            assert_eq!(cyan(), Color::Rgb(0x7E, 0xB8, 0xF6));
        });
        with_theme("mono", || {
            assert_eq!(cyan(), Color::Rgb(0xC9, 0xD1, 0xD9));
        });
    }

    #[test]
    fn tool_style_follows_theme() {
        with_theme("default", || {
            assert_eq!(tool_style(), Style::default().fg(Color::Rgb(2, 195, 189)));
        });
        with_theme("slate", || {
            assert_eq!(
                tool_style(),
                Style::default().fg(Color::Rgb(0x8E, 0xA8, 0xFF))
            );
        });
    }

    #[test]
    fn shell_prompt_style_follows_theme() {
        with_theme("default", || {
            assert_eq!(
                shell_prompt_style(),
                Style::default()
                    .fg(Color::Rgb(200, 100, 200))
                    .add_modifier(Modifier::BOLD)
            );
        });
        with_theme("slate", || {
            // slate uses a different shell prompt color
            let s = shell_prompt_style();
            assert_ne!(
                s,
                Style::default()
                    .fg(Color::Rgb(200, 100, 200))
                    .add_modifier(Modifier::BOLD),
                "slate must override the magenta shell prompt"
            );
        });
    }
}
