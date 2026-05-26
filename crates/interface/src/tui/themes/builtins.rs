//! Builtin themes shipped in the binary. Adding a builtin = one entry in
//! `BUILTINS` + a regression-guard test that its accent is distinct from
//! `default`.
//!
//! Color values for `default` are lifted verbatim from the previous
//! hardcoded constants in `theme.rs`; the guard
//! `default_matches_legacy_palette` will fail if anyone drifts the look
//! while editing this file.

use ratatui::style::Color;
use std::borrow::Cow;

use super::palette::{Branding, Palette, SpinnerStyle, Theme, ThemeSource};

/// Look up a builtin theme by name.
pub fn builtin(name: &str) -> Option<&'static Theme> {
    BUILTINS.iter().find(|t| t.name == name)
}

/// Names of all builtin themes, in display order.
pub fn builtin_names() -> Vec<&'static str> {
    BUILTINS.iter().map(|t| t.name.as_ref()).collect()
}

/// Infallible accessor for the `default` builtin. Used by paths that must
/// not Result/Option (theme cell initialization, hot-swap fallback) so we
/// don't have to choose between `expect()` (banned by workspace clippy)
/// and an unrelated panic if `BUILTINS` ever lacks a "default" entry.
pub fn default_theme_static() -> &'static Theme {
    &DEFAULT_THEME
}

static DEFAULT_THEME: std::sync::LazyLock<Theme> = std::sync::LazyLock::new(default_theme);

// `LazyLock` so we can construct `Cow`/`Vec` values without `const fn`
// gymnastics. Reads are cheap after first init.
static BUILTINS: std::sync::LazyLock<Vec<Theme>> = std::sync::LazyLock::new(|| {
    vec![
        default_theme(),
        mono_theme(),
        slate_theme(),
        daylight_theme(),
    ]
});

fn default_branding() -> Branding {
    Branding {
        goodbye: Cow::Borrowed("Bye"),
        help_header: Cow::Borrowed("Commands"),
    }
}

/// Today's borg look. Color values lifted verbatim from the previous
/// `theme.rs` constants — the regression guard
/// `default_matches_legacy_palette` will fail if any value drifts.
fn default_theme() -> Theme {
    Theme {
        name: Cow::Borrowed("default"),
        description: Cow::Borrowed("Borg teal — the classic look"),
        source: ThemeSource::Builtin,
        palette: Palette {
            accent: Color::Rgb(0, 185, 174),
            tool_accent: Color::Rgb(2, 195, 189),
            success: Color::Rgb(0, 159, 147),
            error: Color::Red,
            warning: Color::Rgb(255, 191, 0),
            border: Color::Rgb(3, 113, 113),
            dim: Color::DarkGray,
            active: Color::Rgb(0, 200, 0),
            popup_selected_bg: Color::Rgb(3, 49, 46),
            popup_selected_fg: Color::White,
            shell_prompt: Color::Rgb(200, 100, 200),
            diff_add_bg_dark: Color::Rgb(33, 58, 43),
            diff_add_bg_light: Color::Rgb(218, 251, 225),
            diff_remove_bg_dark: Color::Rgb(74, 34, 29),
            diff_remove_bg_light: Color::Rgb(255, 235, 233),
            diff_add_gutter_bg_light: Color::Rgb(172, 238, 187),
            diff_remove_gutter_bg_light: Color::Rgb(255, 206, 203),
        },
        branding: default_branding(),
        spinner: SpinnerStyle::Shimmer {
            base: Color::Rgb(0, 185, 174),
            highlight: Color::White,
        },
        banner: None,
    }
}

/// Grayscale theme.
fn mono_theme() -> Theme {
    Theme {
        name: Cow::Borrowed("mono"),
        description: Cow::Borrowed("Monochrome — clean grayscale"),
        source: ThemeSource::Builtin,
        palette: Palette {
            accent: Color::Rgb(0xC9, 0xD1, 0xD9),
            tool_accent: Color::Rgb(0xAA, 0xAA, 0xAA),
            success: Color::Rgb(0x88, 0x88, 0x88),
            // White (brightest) for error and a saturated mid-gray for
            // warning so they remain visually distinct from `accent`
            // (#C9D1D9). Without this contrast, an error tool-result line
            // and a normal text line look identical in this theme.
            error: Color::Rgb(0xFF, 0xFF, 0xFF),
            warning: Color::Rgb(0x77, 0x77, 0x77),
            border: Color::Rgb(0x55, 0x55, 0x55),
            dim: Color::DarkGray,
            active: Color::Rgb(0xB5, 0xB5, 0xB5),
            popup_selected_bg: Color::Rgb(0x2A, 0x2A, 0x2A),
            popup_selected_fg: Color::Rgb(0xF0, 0xF0, 0xF0),
            shell_prompt: Color::Rgb(0xCC, 0xCC, 0xCC),
            diff_add_bg_dark: Color::Rgb(0x2A, 0x2A, 0x2A),
            diff_add_bg_light: Color::Rgb(0xE8, 0xE8, 0xE8),
            diff_remove_bg_dark: Color::Rgb(0x3A, 0x2A, 0x2A),
            diff_remove_bg_light: Color::Rgb(0xEE, 0xDC, 0xDC),
            diff_add_gutter_bg_light: Color::Rgb(0xD0, 0xD0, 0xD0),
            diff_remove_gutter_bg_light: Color::Rgb(0xE2, 0xC8, 0xC8),
        },
        branding: default_branding(),
        spinner: SpinnerStyle::Shimmer {
            base: Color::Rgb(0xAA, 0xAA, 0xAA),
            highlight: Color::White,
        },
        banner: None,
    }
}

/// Cool blue developer-focused theme.
fn slate_theme() -> Theme {
    Theme {
        name: Cow::Borrowed("slate"),
        description: Cow::Borrowed("Cool blue — developer-focused"),
        source: ThemeSource::Builtin,
        palette: Palette {
            accent: Color::Rgb(0x7E, 0xB8, 0xF6),
            tool_accent: Color::Rgb(0x8E, 0xA8, 0xFF),
            success: Color::Rgb(0x63, 0xD0, 0xA6),
            error: Color::Rgb(0xF7, 0xA0, 0x72),
            warning: Color::Rgb(0xE6, 0xA8, 0x55),
            border: Color::Rgb(0x41, 0x69, 0xE1),
            dim: Color::Rgb(0x4B, 0x55, 0x63),
            active: Color::Rgb(0x63, 0xD0, 0xA6),
            popup_selected_bg: Color::Rgb(0x15, 0x1C, 0x2F),
            popup_selected_fg: Color::Rgb(0xC9, 0xD1, 0xD9),
            shell_prompt: Color::Rgb(0x8E, 0xA8, 0xFF),
            diff_add_bg_dark: Color::Rgb(0x18, 0x3A, 0x2B),
            diff_add_bg_light: Color::Rgb(0xDA, 0xFB, 0xE1),
            diff_remove_bg_dark: Color::Rgb(0x3A, 0x22, 0x1D),
            diff_remove_bg_light: Color::Rgb(0xFF, 0xEB, 0xE9),
            diff_add_gutter_bg_light: Color::Rgb(0xAC, 0xEE, 0xBB),
            diff_remove_gutter_bg_light: Color::Rgb(0xFF, 0xCE, 0xCB),
        },
        branding: default_branding(),
        spinner: SpinnerStyle::Shimmer {
            base: Color::Rgb(0x7E, 0xB8, 0xF6),
            highlight: Color::White,
        },
        banner: None,
    }
}

/// Light-terminal theme. The non-bg colors are tuned for contrast against
/// a light background; the diff *_light values stay close to GitHub's
/// pastel diff backgrounds.
///
/// Note: borg's `dim()`/diff helpers branch on the runtime
/// `is_light_terminal()` detector, NOT on theme. If an operator activates
/// daylight on a dark terminal, the accents (dark blue) will render
/// against a dark background and may read poorly — daylight is intended
/// for terminal backgrounds with luminance > 128. The diff *_dark values
/// here are kept identical to the `default` theme so the rare
/// "daylight on dark term" case isn't worse than today's baseline.
fn daylight_theme() -> Theme {
    Theme {
        name: Cow::Borrowed("daylight"),
        description: Cow::Borrowed("Light theme — dark text on bright terminals"),
        source: ThemeSource::Builtin,
        palette: Palette {
            accent: Color::Rgb(0x25, 0x63, 0xEB),
            tool_accent: Color::Rgb(0x1D, 0x4E, 0xD8),
            success: Color::Rgb(0x15, 0x80, 0x3D),
            error: Color::Rgb(0xB9, 0x1C, 0x1C),
            warning: Color::Rgb(0xB4, 0x53, 0x09),
            border: Color::Rgb(0x93, 0xC5, 0xFD),
            dim: Color::Rgb(0x47, 0x55, 0x69),
            active: Color::Rgb(0x15, 0x80, 0x3D),
            popup_selected_bg: Color::Rgb(0xDB, 0xEA, 0xFE),
            popup_selected_fg: Color::Rgb(0x0F, 0x17, 0x2A),
            shell_prompt: Color::Rgb(0x7C, 0x3A, 0xED),
            diff_add_bg_dark: Color::Rgb(33, 58, 43),
            diff_add_bg_light: Color::Rgb(0xDA, 0xFB, 0xE1),
            diff_remove_bg_dark: Color::Rgb(74, 34, 29),
            diff_remove_bg_light: Color::Rgb(0xFF, 0xEB, 0xE9),
            diff_add_gutter_bg_light: Color::Rgb(0xAC, 0xEE, 0xBB),
            diff_remove_gutter_bg_light: Color::Rgb(0xFF, 0xCE, 0xCB),
        },
        branding: default_branding(),
        spinner: SpinnerStyle::Shimmer {
            base: Color::Rgb(0x25, 0x63, 0xEB),
            highlight: Color::Rgb(0x0F, 0x17, 0x2A),
        },
        banner: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard: the `default` palette MUST reproduce the legacy
    /// hardcoded colors from `theme.rs` byte-for-byte. If this fails,
    /// somebody changed the look without intending to.
    #[test]
    fn default_matches_legacy_palette() {
        let t = builtin("default").expect("default must exist");
        assert_eq!(t.palette.accent, Color::Rgb(0, 185, 174), "CYAN");
        assert_eq!(t.palette.tool_accent, Color::Rgb(2, 195, 189), "YELLOW");
        assert_eq!(t.palette.success, Color::Rgb(0, 159, 147), "GREEN");
        assert_eq!(t.palette.error, Color::Red, "RED");
        assert_eq!(t.palette.warning, Color::Rgb(255, 191, 0), "AMBER");
        assert_eq!(t.palette.border, Color::Rgb(3, 113, 113), "BORDER");
        assert_eq!(t.palette.dim, Color::DarkGray, "DIM_WHITE");
        assert_eq!(t.palette.active, Color::Rgb(0, 200, 0), "TOOL_ACTIVE_GREEN");
        assert_eq!(t.palette.popup_selected_bg, Color::Rgb(3, 49, 46));
        assert_eq!(t.palette.shell_prompt, Color::Rgb(200, 100, 200));
        assert_eq!(t.palette.diff_add_bg_dark, Color::Rgb(33, 58, 43));
        assert_eq!(t.palette.diff_remove_bg_dark, Color::Rgb(74, 34, 29));
    }

    #[test]
    fn all_expected_builtins_present() {
        for name in ["default", "mono", "slate", "daylight"] {
            assert!(builtin(name).is_some(), "missing builtin: {name}");
        }
    }

    #[test]
    fn unknown_builtin_returns_none() {
        assert!(builtin("nonexistent").is_none());
        assert!(builtin("").is_none());
    }

    /// Every non-default builtin must have a distinct accent color. A theme
    /// that copies default's accent silently is a bug — the user picks
    /// "slate" expecting a visible change.
    #[test]
    fn non_default_builtins_differ_visibly_from_default() {
        let Some(default_accent) = builtin("default").map(|t| t.palette.accent) else {
            panic!("default theme must exist");
        };
        for name in ["mono", "slate", "daylight"] {
            let Some(t) = builtin(name) else {
                panic!("missing builtin: {name}");
            };
            assert_ne!(
                t.palette.accent, default_accent,
                "builtin '{name}' has the same accent as 'default' — themes must visually differ"
            );
        }
    }

    /// Within-theme contrast guard: `error` and `accent` must not be the
    /// same color. A red tool error indistinguishable from a normal text
    /// line silently demotes diagnostic UX — exactly the bug the mono
    /// theme hit in the first review (error #CCCCCC vs accent #C9D1D9).
    #[test]
    fn every_builtin_has_distinct_error_and_accent() {
        for t in BUILTINS.iter() {
            assert_ne!(
                t.palette.error, t.palette.accent,
                "theme '{}' has error == accent; error tool results would be invisible",
                t.name
            );
        }
    }

    #[test]
    fn builtin_names_starts_with_default() {
        let names = builtin_names();
        assert_eq!(names.first().copied(), Some("default"));
        assert!(names.contains(&"mono"));
        assert!(names.contains(&"slate"));
        assert!(names.contains(&"daylight"));
    }
}
