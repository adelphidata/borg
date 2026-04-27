use std::sync::OnceLock;
use std::time::Instant;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use super::colors;

static SHIMMER_EPOCH: OnceLock<Instant> = OnceLock::new();

fn elapsed_since_start() -> std::time::Duration {
    let start = SHIMMER_EPOCH.get_or_init(Instant::now);
    start.elapsed()
}

/// Produce per-character spans with a sweeping shimmer highlight, defaulting
/// to the detected terminal foreground/background so the sweep matches the
/// user's theme. Falls back to the hardcoded teal pair when the terminal
/// palette is unknown (tests, non-Unix, OSC-unfriendly terminals).
pub(super) fn shimmer_spans_auto(text: &str) -> Vec<Span<'static>> {
    let base = colors::default_fg().unwrap_or((0, 185, 174));
    let highlight = colors::default_bg().unwrap_or((180, 255, 252));
    shimmer_spans(text, base, highlight)
}

/// Produce per-character spans with a sweeping shimmer highlight.
///
/// A cosine-based band sweeps left-to-right over 2 seconds, blending each
/// character from `base_rgb` toward `highlight_rgb`.  Falls back to BOLD/DIM
/// modifiers when true-color is unavailable.
pub(super) fn shimmer_spans(
    text: &str,
    base_rgb: (u8, u8, u8),
    highlight_rgb: (u8, u8, u8),
) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }

    let padding = 10usize;
    let period = chars.len() + padding * 2;
    let sweep_seconds = 2.0f32;
    let pos_f =
        (elapsed_since_start().as_secs_f32() % sweep_seconds) / sweep_seconds * (period as f32);
    let pos = pos_f as usize;
    let band_half_width = 5.0f32;

    let has_truecolor = colors::supports_truecolor();

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(chars.len());
    for (i, ch) in chars.iter().enumerate() {
        let i_pos = i as isize + padding as isize;
        let dist = (i_pos - pos as isize).abs() as f32;

        let t = if dist <= band_half_width {
            let x = std::f32::consts::PI * (dist / band_half_width);
            0.5 * (1.0 + x.cos())
        } else {
            0.0
        };

        let style = if has_truecolor {
            // Cap at 90% to keep the base color visible at peak intensity.
            let highlight = (t * 0.9).clamp(0.0, 1.0);
            let r = colors::blend_channel(highlight_rgb.0, base_rgb.0, highlight as f64);
            let g = colors::blend_channel(highlight_rgb.1, base_rgb.1, highlight as f64);
            let b = colors::blend_channel(highlight_rgb.2, base_rgb.2, highlight as f64);
            Style::default()
                .fg(Color::Rgb(r, g, b))
                .add_modifier(Modifier::BOLD)
        } else {
            color_for_level(t)
        };

        spans.push(Span::styled(ch.to_string(), style));
    }
    spans
}

fn color_for_level(intensity: f32) -> Style {
    if intensity < 0.2 {
        Style::default().add_modifier(Modifier::DIM)
    } else if intensity < 0.6 {
        Style::default()
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}
