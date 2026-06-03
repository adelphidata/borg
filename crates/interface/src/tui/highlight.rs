//! Syntax highlighting for fenced code blocks and diff context lines.
//!
//! Wraps [`syntect`] with a bundled theme + default syntax set. Lookups and
//! highlight state are created per-call but the `SyntaxSet` / `ThemeSet` are
//! loaded once via `OnceLock` to amortize parsing cost.

use std::sync::OnceLock;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use super::theme;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<Theme> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn active_theme() -> &'static Theme {
    THEME.get_or_init(|| {
        let ts = ThemeSet::load_defaults();
        // base16-ocean.dark reads clearly on a dark terminal; fall back gracefully.
        ts.themes
            .get("base16-ocean.dark")
            .or_else(|| ts.themes.values().next())
            .cloned()
            .unwrap_or_default()
    })
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

/// Highlight a multi-line code block. Returns one [`Line`] per source line.
///
/// For unknown languages or when highlighting fails, falls back to a single
/// cyan span per line (same as the existing code-style treatment).
pub fn highlight_code_block(code: &str, lang: &str) -> Vec<Line<'static>> {
    let ss = syntax_set();
    let Some(syntax) = find_syntax(ss, lang) else {
        return fallback_lines(code);
    };
    let theme_ref = active_theme();
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
}
