use super::custom_terminal::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use super::theme;

/// Vertical padding (rows) inside the menu surface.
const MENU_SURFACE_INSET_V: u16 = 1;
/// Horizontal padding (cols) inside the menu surface.
const MENU_SURFACE_INSET_H: u16 = 2;

/// Popup width as a percentage of the host terminal width.
pub const POPUP_WIDTH_PCT: u16 = 60;
/// Popup height as a percentage of the host terminal height.
pub const POPUP_HEIGHT_PCT: u16 = 80;
/// Minimum popup width in cells; below this, lists wrap unreadably.
pub const POPUP_MIN_WIDTH: u16 = 44;
/// Minimum popup height in cells; below this, header + footer crowd the body out.
pub const POPUP_MIN_HEIGHT: u16 = 12;
/// Horizontal breathing room reserved either side of the popup.
const POPUP_H_MARGIN: u16 = 4;
/// Vertical breathing room reserved above and below the popup.
const POPUP_V_MARGIN: u16 = 2;
/// Hard floor on inner popup width; popups narrower than this skip render.
const POPUP_RENDER_MIN_INNER_WIDTH: u16 = 12;

/// Compute centered popup geometry (60% width, 80% height, clamped).
///
/// The result is always fully contained within `area` — including its
/// `x`/`y` origin. In inline-viewport mode the frame buffer's `area.y` is the
/// terminal-bottom offset (e.g. `{x:0, y:7, ...}`), so a centered popup MUST be
/// anchored relative to that origin, not `(0, 0)`. Omitting the offset writes
/// above the buffer and panics ratatui (`index outside of buffer`). Centered
/// overlays therefore render on the full-screen alt surface (`area.y == 0`),
/// but honoring the offset here keeps `popup_area` correct on any surface.
pub fn popup_area(area: Rect) -> Rect {
    let popup_width = (area.width * POPUP_WIDTH_PCT / 100)
        .max(POPUP_MIN_WIDTH)
        .min(area.width.saturating_sub(POPUP_H_MARGIN));
    let popup_height = (area.height * POPUP_HEIGHT_PCT / 100)
        .max(POPUP_MIN_HEIGHT)
        .min(area.height.saturating_sub(POPUP_V_MARGIN));
    let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_height)) / 2;
    Rect::new(x, y, popup_width, popup_height)
}

/// Paint a borderless "menu surface": clear the area, fill it with a subtle
/// `user_message_style()` background tint (no border), and return the content
/// area inset by the shared menu padding (V=1, H=2). Callers render all inner
/// content in the returned rect.
pub fn render_menu_surface(frame: &mut Frame, area: Rect) -> Rect {
    // Wipe whatever was behind the surface, then paint the tint. The tint alone
    // only restyles cells, so the `Clear` is what removes glyphs.
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(theme::user_message_style()), area);
    Rect {
        x: area.x.saturating_add(MENU_SURFACE_INSET_H),
        y: area.y.saturating_add(MENU_SURFACE_INSET_V),
        width: area
            .width
            .saturating_sub(MENU_SURFACE_INSET_H.saturating_mul(2)),
        height: area
            .height
            .saturating_sub(MENU_SURFACE_INSET_V.saturating_mul(2)),
    }
}

/// Render a popup as a borderless menu surface with the title shown as a bold
/// header on the first inner row. Returns the content area below the title.
pub fn render_popup_frame(frame: &mut Frame, popup_area: Rect, title: &str) -> Rect {
    let inset = render_menu_surface(frame, popup_area);

    if inset.width == 0 || inset.height == 0 {
        return inset;
    }

    // Title as a bold header on the first inner row, content below it.
    if !title.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                title.to_string(),
                theme::header_style(),
            ))),
            Rect::new(inset.x, inset.y, inset.width, 1),
        );
        Rect {
            x: inset.x,
            y: inset.y.saturating_add(1),
            width: inset.width,
            height: inset.height.saturating_sub(1),
        }
    } else {
        inset
    }
}

/// Compute a popup rectangle anchored above the composer, clamped to fit
/// within the frame's viewport.
///
/// In inline-viewport mode the frame buffer's `area.y` may be > 0; popups
/// must size against the gap between the composer and the viewport top, not
/// against the absolute composer y. Returns `None` when there is not enough
/// space above the composer for `min_height` rows.
pub fn popup_rect_above(
    viewport: Rect,
    composer: Rect,
    desired_height: u16,
    min_height: u16,
    width: u16,
) -> Option<Rect> {
    let available_above = composer.y.saturating_sub(viewport.y);
    if available_above < min_height {
        return None;
    }
    let height = desired_height.min(available_above).max(min_height);
    let popup_y = composer.y - height;
    Some(Rect::new(composer.x, popup_y, width, height))
}

/// Render a footer hint bar at the bottom of the inner area.
pub fn render_footer(frame: &mut Frame, inner: Rect, hint: &str) {
    let footer_y = inner.y + inner.height - 1;
    let footer_area = Rect::new(inner.x, footer_y, inner.width, 1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint.to_string(), theme::dim()))),
        footer_area,
    );
}

/// Common popup render preamble: visibility check, centered area, frame, size
/// guard, and content height calculation. Returns `None` if the popup should
/// not be rendered (invisible or too small). `min_height` is the minimum inner
/// height required to render. `footer_lines` is the number of rows reserved at
/// the bottom (footer + status bar).
pub fn begin_popup_render(
    frame: &mut Frame,
    visible: bool,
    title: &str,
    min_height: u16,
    footer_lines: usize,
) -> Option<(Rect, usize)> {
    if !visible {
        return None;
    }
    let area = popup_area(frame.area());
    let inner = render_popup_frame(frame, area, title);
    if inner.height < min_height || inner.width < POPUP_RENDER_MIN_INNER_WIDTH {
        return None;
    }
    let content_height = (inner.height as usize).saturating_sub(footer_lines);
    Some((inner, content_height))
}

/// Render an optional status message above the footer.
pub fn render_status_message(
    frame: &mut Frame,
    inner: Rect,
    message: Option<&(String, bool)>,
    offset_from_bottom: u16,
) {
    if let Some((msg, is_success)) = message {
        let style = if *is_success {
            theme::success_style()
        } else {
            theme::error_style()
        };
        let status_y = inner.y + inner.height - offset_from_bottom;
        let status_area = Rect::new(inner.x, status_y, inner.width, 1);
        // One row only — elide instead of letting a long confirmation (a key,
        // a path, an error) run off the edge mid-character.
        let line = super::line_utils::truncate_line_with_ellipsis(
            Line::from(Span::styled(format!(" {msg}"), style)),
            inner.width as usize,
        );
        frame.render_widget(Paragraph::new(line), status_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_area_centered_in_large_terminal() {
        let area = Rect::new(0, 0, 200, 50);
        let popup = popup_area(area);
        // 60% of 200 = 120, 80% of 50 = 40
        assert_eq!(popup.width, 120);
        assert_eq!(popup.height, 40);
        // centered
        assert_eq!(popup.x, (200 - 120) / 2);
        assert_eq!(popup.y, (50 - 40) / 2);
    }

    #[test]
    fn popup_area_minimum_width_enforced() {
        let area = Rect::new(0, 0, 50, 30);
        let popup = popup_area(area);
        // 60% of 50 = 30, but min is 44; clamped to 50-4=46
        assert_eq!(popup.width, 44);
    }

    #[test]
    fn popup_area_minimum_height_enforced() {
        let area = Rect::new(0, 0, 100, 14);
        let popup = popup_area(area);
        // 80% of 14 = 11.2 → 11, but min is 12; clamped to 14-2=12
        assert_eq!(popup.height, 12);
    }

    #[test]
    fn popup_area_clamped_to_available_space() {
        // Very small terminal where min exceeds available
        let area = Rect::new(0, 0, 46, 13);
        let popup = popup_area(area);
        // 60% of 46 = 27, max(27, 44) = 44, min(44, 46-4=42) = 42
        assert_eq!(popup.width, 42);
        // 80% of 13 = 10, max(10, 12) = 12, min(12, 13-2=11) = 11
        assert_eq!(popup.height, 11);
    }

    #[test]
    fn popup_area_tiny_terminal() {
        let area = Rect::new(0, 0, 10, 5);
        let popup = popup_area(area);
        // width: 60% of 10 = 6, max(6, 44) = 44, min(44, 10-4=6) = 6
        assert_eq!(popup.width, 6);
        // height: 80% of 5 = 4, max(4, 12) = 12, min(12, 5-2=3) = 3
        assert_eq!(popup.height, 3);
    }

    #[test]
    fn popup_rect_above_inline_viewport_stays_in_bounds() {
        // Inline viewport: buffer occupies y=17..24 (height=7).
        let viewport = Rect::new(0, 17, 102, 7);
        // Composer near the bottom of the viewport.
        let composer = Rect::new(0, 22, 102, 2);
        // Want a tall popup (10 rows) but only 5 rows above the composer.
        let popup = popup_rect_above(viewport, composer, 10, 3, 50).unwrap();
        assert!(
            popup.y >= viewport.y,
            "popup at y={} must be >= viewport top {}",
            popup.y,
            viewport.y,
        );
        assert!(popup.y + popup.height <= composer.y);
        assert_eq!(popup.height, 5); // clamped to available_above
    }

    #[test]
    fn popup_rect_above_returns_none_when_insufficient_space() {
        let viewport = Rect::new(0, 17, 102, 7);
        // Composer one row below viewport top: only 1 row above.
        let composer = Rect::new(0, 18, 102, 2);
        assert!(popup_rect_above(viewport, composer, 10, 3, 50).is_none());
    }

    #[test]
    fn popup_rect_above_full_screen_viewport() {
        let viewport = Rect::new(0, 0, 100, 40);
        let composer = Rect::new(0, 35, 100, 3);
        let popup = popup_rect_above(viewport, composer, 6, 3, 50).unwrap();
        assert_eq!(popup.y, 29);
        assert_eq!(popup.height, 6);
    }

    /// Regression: `/model` panicked ratatui with
    /// `index outside of buffer: the area is Rect { x: 0, y: 7, width: 112,
    /// height: 7 } but index is (22, 1)`. In inline-viewport mode the frame
    /// buffer is offset to the terminal bottom (`y == 7` here), but `popup_area`
    /// centered against `(0, 0)` and produced `y == 1` — above the buffer. The
    /// returned rect MUST be fully contained within the input area's bounds,
    /// origin included, on any surface.
    #[test]
    fn popup_area_contained_within_offset_inline_viewport() {
        // Exact geometry from the panic: a short viewport anchored at y=7.
        let area = Rect::new(0, 7, 112, 7);
        let popup = popup_area(area);
        assert!(
            popup.x >= area.x,
            "popup.x {} escaped left of area.x {}",
            popup.x,
            area.x
        );
        assert!(
            popup.y >= area.y,
            "popup.y {} escaped above area.y {} (the original panic)",
            popup.y,
            area.y
        );
        assert!(
            popup.right() <= area.right(),
            "popup.right {} escaped area.right {}",
            popup.right(),
            area.right()
        );
        assert!(
            popup.bottom() <= area.bottom(),
            "popup.bottom {} escaped area.bottom {}",
            popup.bottom(),
            area.bottom()
        );
    }

    /// Generalizes the containment invariant across a range of offset areas so a
    /// future tweak to the centering math can't reintroduce an out-of-buffer
    /// write on any surface origin.
    #[test]
    fn popup_area_always_contained_for_varied_offsets() {
        for &(x, y, w, h) in &[
            (0u16, 0u16, 200u16, 50u16),
            (0, 7, 112, 7),
            (0, 20, 80, 12),
            (5, 3, 100, 40),
            (0, 38, 120, 2),
        ] {
            let area = Rect::new(x, y, w, h);
            let popup = popup_area(area);
            assert!(
                popup.x >= area.x
                    && popup.y >= area.y
                    && popup.right() <= area.right()
                    && popup.bottom() <= area.bottom(),
                "popup {popup:?} escaped area {area:?}"
            );
        }
    }

    #[test]
    fn popup_area_centering_with_odd_dimensions() {
        let area = Rect::new(0, 0, 101, 51);
        let popup = popup_area(area);
        // Should not panic and should be approximately centered
        assert!(popup.x + popup.width <= area.width);
        assert!(popup.y + popup.height <= area.height);
    }
}
