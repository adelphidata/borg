use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::theme;

/// Compute centered popup geometry (60% width, 80% height, clamped).
pub fn popup_area(area: Rect) -> Rect {
    let popup_width = (area.width * 60 / 100)
        .max(44)
        .min(area.width.saturating_sub(4));
    let popup_height = (area.height * 80 / 100)
        .max(12)
        .min(area.height.saturating_sub(2));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    Rect::new(x, y, popup_width, popup_height)
}

/// Render a popup frame with a bordered block and title. Returns the inner area.
/// Clears the background, draws the border, and returns the usable inner rect.
pub fn render_popup_frame(frame: &mut Frame, popup_area: Rect, title: &str) -> Rect {
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::dim())
        .title(format!(" {title} "));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);
    inner
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
    if inner.height < min_height || inner.width < 12 {
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
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {msg}"), style))),
            status_area,
        );
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
    fn popup_area_centering_with_odd_dimensions() {
        let area = Rect::new(0, 0, 101, 51);
        let popup = popup_area(area);
        // Should not panic and should be approximately centered
        assert!(popup.x + popup.width <= area.width);
        assert!(popup.y + popup.height <= area.height);
    }
}
