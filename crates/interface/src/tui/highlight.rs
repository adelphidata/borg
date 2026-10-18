//! Syntax highlighting for fenced code blocks and diff context lines.
//!
//! Wraps [`syntect`] with two bundled themes (one per terminal background) +
//! default syntax set. Lookups and highlight state are created per-call but
//! the syntax set and themes are loaded once via `OnceLock` to amortize
//! parsing cost.

use std::sync::OnceLock;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use super::theme;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
/// `[dark, light]` background themes.
static THEMES: OnceLock<[Theme; 2]> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// The bundled theme that reads on the terminal's background.
///
/// Only foreground colors are painted, so the theme must suit the terminal's
/// own background. `base16-ocean.dark` draws plain code in a light gray that
/// almost vanishes on white. Chosen on each call, like the other light/dark
/// styles, so a block drawn before the background probe answers is not
/// stuck with the dark theme.
fn active_theme() -> &'static Theme {
    theme_for(super::colors::is_light_terminal())
}

fn theme_for(light_background: bool) -> &'static Theme {
    let [dark, light] = THEMES.get_or_init(|| {
        let mut themes = ThemeSet::load_defaults().themes;
        ["base16-ocean.dark", "InspiredGitHub"].map(|name| {
            themes.remove(name).unwrap_or_else(|| {
                tracing::warn!(
                    theme = name,
                    "bundled syntax theme missing; using the default"
                );
                Theme::default()
            })
        })
    });
    if light_background {
        light
    } else {
        dark
    }
}

fn find_syntax<'a>(ss: &'a SyntaxSet, lang: &str) -> Option<&'a SyntaxReference> {
    if lang.is_empty() {
        return None;
    }
    ss.find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_extension(lang))
        .or_else(|| ss.find_syntax_by_name(lang))
}

/// Convert a syntect color to a ratatui Color. syntect supplies 8-bit RGB.
fn conv_color(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

fn conv_style(s: SynStyle) -> Style {
    let mut style = Style::default().fg(conv_color(s.foreground));
    use syntect::highlighting::FontStyle;
    if s.font_style.contains(FontStyle::BOLD) {
        style = style.add_modifier(ratatui::style::Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        style = style.add_modifier(ratatui::style::Modifier::ITALIC);
    }
    if s.font_style.contains(FontStyle::UNDERLINE) {
        style = style.add_modifier(ratatui::style::Modifier::UNDERLINED);
    }
    style
}

/// Whether `code` is small enough to hand to `syntect`.
///
/// See the guardrail constants in [`crate::tui::constants`] for why each limit
/// exists. Returning `false` costs only syntax colors — the text is preserved.
fn within_highlight_limits(code: &str) -> bool {
    use crate::tui::constants::{
        HIGHLIGHT_MAX_LINES, HIGHLIGHT_MAX_LINE_BYTES, HIGHLIGHT_MAX_TOTAL_BYTES,
    };
    if code.len() > HIGHLIGHT_MAX_TOTAL_BYTES {
        return false;
    }
    let mut lines = 0usize;
    for line in code.split_inclusive('\n') {
        lines += 1;
        if lines > HIGHLIGHT_MAX_LINES || line.len() > HIGHLIGHT_MAX_LINE_BYTES {
            return false;
        }
    }
    true
}

/// Highlight a multi-line code block. Returns one [`Line`] per source line.
///
/// For unknown languages, oversized input, or when highlighting fails, falls
/// back to a single cyan span per line (same as the existing code-style
/// treatment).
pub fn highlight_code_block(code: &str, lang: &str) -> Vec<Line<'static>> {
    highlight_with_theme(code, lang, active_theme())
}

fn highlight_with_theme(code: &str, lang: &str, theme_ref: &Theme) -> Vec<Line<'static>> {
    if !within_highlight_limits(code) {
        return fallback_lines(code);
    }
    let ss = syntax_set();
    let Some(syntax) = find_syntax(ss, lang) else {
        return fallback_lines(code);
    };
    let mut h = HighlightLines::new(syntax, theme_ref);

    let mut out = Vec::new();
    for line in code.split_inclusive('\n') {
        match h.highlight_line(line, ss) {
            Ok(regions) => {
                let spans: Vec<Span<'static>> = regions
                    .into_iter()
                    .map(|(style, text)| {
                        Span::styled(text.trim_end_matches('\n').to_string(), conv_style(style))
                    })
                    .filter(|s| !s.content.is_empty())
                    .collect();
                out.push(Line::from(spans));
            }
            Err(e) => {
                tracing::warn!(%e, "syntect highlight failed; falling back to plain line");
                out.push(Line::from(Span::styled(
                    line.trim_end_matches('\n').to_string(),
                    theme::code_style(),
                )));
            }
        }
    }
    if out.is_empty() {
        out.push(Line::default());
    }
    out
}

fn fallback_lines(code: &str) -> Vec<Line<'static>> {
    code.lines()
        .map(|l| Line::from(Span::styled(l.to_string(), theme::code_style())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::constants::{
        HIGHLIGHT_MAX_LINES, HIGHLIGHT_MAX_LINE_BYTES, HIGHLIGHT_MAX_TOTAL_BYTES,
    };

    /// A single very long line is the worst case for syntect's regex engine and
    /// slips past both the total-size and line-count limits — a minified bundle
    /// or long unterminated string in a tool result could wedge the render
    /// thread. The content must survive verbatim; only the colors are dropped.
    #[test]
    fn long_single_line_falls_back_to_plain_text_preserving_content() {
        let long = format!("let x = \"{}\";", "a".repeat(HIGHLIGHT_MAX_LINE_BYTES));
        assert!(long.len() > HIGHLIGHT_MAX_LINE_BYTES);
        let lines = highlight_code_block(&long, "rust");
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        assert_eq!(
            spans.len(),
            1,
            "oversized line must render as one unstyled span, got {} spans",
            spans.len()
        );
        assert_eq!(
            spans[0].content.as_ref(),
            long.as_str(),
            "text must be preserved byte-for-byte"
        );
    }

    #[test]
    fn oversized_total_input_falls_back_to_plain_text() {
        // Every individual line is short, so only the total-size limit can fire.
        let line = "let x = 1;\n";
        let code = line.repeat(HIGHLIGHT_MAX_TOTAL_BYTES / line.len() + 1);
        assert!(code.len() > HIGHLIGHT_MAX_TOTAL_BYTES);
        let lines = highlight_code_block(&code, "rust");
        let distinct: std::collections::HashSet<_> =
            lines[0].spans.iter().map(|s| s.style).collect();
        assert_eq!(
            distinct.len(),
            1,
            "oversized input must not be syntax-highlighted"
        );
    }

    #[test]
    fn too_many_lines_falls_back_to_plain_text() {
        // Short total per line keeps the byte limit clear of the line limit.
        let code = "x\n".repeat(HIGHLIGHT_MAX_LINES + 1);
        assert!(code.len() < HIGHLIGHT_MAX_TOTAL_BYTES);
        let lines = highlight_code_block(&code, "rust");
        let distinct: std::collections::HashSet<_> =
            lines[0].spans.iter().map(|s| s.style).collect();
        assert_eq!(
            distinct.len(),
            1,
            "input over the line limit must not be syntax-highlighted"
        );
    }

    /// The guards must not fire on ordinary input — otherwise they would silently
    /// turn off highlighting for every code block.
    #[test]
    fn input_just_under_the_limits_is_still_highlighted() {
        let body = "fn main() { let x = 1; }\n";
        let code = body.repeat(50);
        assert!(code.len() < HIGHLIGHT_MAX_TOTAL_BYTES);
        assert!(body.len() < HIGHLIGHT_MAX_LINE_BYTES);
        let lines = highlight_code_block(&code, "rust");
        let distinct: std::collections::HashSet<_> =
            lines[0].spans.iter().map(|s| s.style).collect();
        assert!(
            distinct.len() > 1,
            "normal input must still be syntax-highlighted"
        );
    }

    #[test]
    fn highlights_rust_code_block_has_multiple_styles() {
        let code = "fn main() { let x = 1; }";
        let lines = highlight_code_block(code, "rust");
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        assert!(!spans.is_empty(), "should produce at least one span");
        // Rust highlighting should produce more than one distinct style
        let distinct: std::collections::HashSet<_> = spans.iter().map(|s| s.style).collect();
        assert!(
            distinct.len() > 1,
            "expected multiple distinct styles for highlighted rust, got {}",
            distinct.len()
        );
    }

    #[test]
    fn unknown_language_falls_back_without_panic() {
        let lines = highlight_code_block("some text", "totally-not-a-language");
        assert_eq!(lines.len(), 1);
        // Fallback: single cyan span matching code_style
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].style, theme::code_style());
    }

    #[test]
    fn multi_line_code_produces_one_line_each() {
        let code = "let a = 1;\nlet b = 2;\nlet c = 3;";
        let lines = highlight_code_block(code, "rust");
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn empty_code_is_safe() {
        let lines = highlight_code_block("", "rust");
        // Either no lines or a single empty line — never panic, never crash.
        assert!(lines.len() <= 1);
    }

    #[test]
    fn syntax_set_is_cached() {
        // Two calls should not panic and should return pointer-equal references
        let a = syntax_set() as *const _;
        let b = syntax_set() as *const _;
        assert_eq!(a, b);
    }

    #[test]
    fn common_langs_resolve_to_expected_syntax() {
        // Each token (name, alias, or extension) must map to the *correct*
        // syntax, not merely some syntax — a misrouted extension would
        // highlight Python as JS while still returning Some.
        let ss = syntax_set();
        let cases = [
            ("rust", "Rust"),
            ("python", "Python"),
            ("rs", "Rust"), // extension lookup
            ("py", "Python"),
        ];
        for (lang, expected_name) in cases {
            let syntax = find_syntax(ss, lang).unwrap_or_else(|| panic!("{lang} should resolve"));
            assert_eq!(
                syntax.name, expected_name,
                "{lang} resolved to the wrong syntax"
            );
        }
        // JS may be named "JavaScript" or "JavaScript (Babel)" depending on
        // the bundled syntax set — assert it at least resolves to a JS family.
        let js = find_syntax(ss, "js").expect("js should resolve");
        assert!(
            js.name.contains("JavaScript"),
            "js resolved to {:?}",
            js.name
        );
    }

    /// WCAG contrast ratio of two sRGB colors.
    fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
        fn lum((r, g, b): (u8, u8, u8)) -> f64 {
            let lin = |c: u8| {
                let c = f64::from(c) / 255.0;
                if c <= 0.04045 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
        }
        let (hi, lo) = {
            let (x, y) = (lum(a), lum(b));
            if x > y {
                (x, y)
            } else {
                (y, x)
            }
        };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Only the foreground is painted, so code must read on the terminal's
    /// own background. The floor sits under the dimmest comment gray of the
    /// chosen themes (2.55:1 on the light gray `COLORFGBG` implies) and well
    /// over a dark theme's plain text on white (about 1.7:1), the failure
    /// this guards.
    #[test]
    fn code_colors_contrast_with_the_terminal_background() {
        const FLOOR: f64 = 2.5;
        let code = "// say hi\nfn main() {\n    let name = \"borg\";\n    println!(\"{name} {}\", 42);\n}\n";
        for (light, background) in [
            (true, (255, 255, 255)),
            (true, (240, 240, 240)),
            (false, (30, 30, 30)),
        ] {
            for line in highlight_with_theme(code, "rust", theme_for(light)) {
                for span in &line.spans {
                    if span.content.trim().is_empty() {
                        continue;
                    }
                    let Some(Color::Rgb(r, g, b)) = span.style.fg else {
                        panic!("span {:?} has no RGB foreground", span.content);
                    };
                    let ratio = contrast((r, g, b), background);
                    assert!(
                        ratio >= FLOOR,
                        "{:?} in #{r:02x}{g:02x}{b:02x} is {ratio:.2}:1 on {background:?}",
                        span.content
                    );
                }
            }
        }
    }
}
