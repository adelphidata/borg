use ratatui::layout::{Constraint, Layout, Rect};

pub struct AppLayout {
    pub transcript: Rect,
    pub status: Rect,
    pub queue_preview: Rect,
    pub composer: Rect,
    pub footer: Rect,
}

pub fn compute_layout(
    area: Rect,
    composer_height: u16,
    status_height: u16,
    queue_preview_height: u16,
) -> AppLayout {
    let chunks = Layout::vertical([
        Constraint::Min(3),                       // transcript
        Constraint::Length(status_height),        // status bar
        Constraint::Length(queue_preview_height), // queue preview
        Constraint::Length(composer_height),      // composer
        Constraint::Length(1),                    // footer
    ])
    .split(area);

    AppLayout {
        transcript: chunks[0],
        status: chunks[1],
        queue_preview: chunks[2],
        composer: chunks[3],
        footer: chunks[4],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    #[test]
    fn basic_layout_with_status() {
        let layout = compute_layout(area(80, 40), 3, 1, 0);
        assert_eq!(layout.status.height, 1);
        assert_eq!(layout.composer.height, 3);
        assert_eq!(layout.footer.height, 1);
        assert_eq!(layout.queue_preview.height, 0);
        // transcript gets remaining space
        assert_eq!(layout.transcript.height, 40 - 3 - 1 - 1);
    }

    #[test]
    fn layout_without_status() {
        let layout = compute_layout(area(80, 40), 3, 0, 0);
        assert_eq!(layout.status.height, 0);
        assert_eq!(layout.transcript.height, 40 - 3 - 1);
    }

    #[test]
    fn layout_with_queue_preview() {
        let layout = compute_layout(area(80, 40), 3, 1, 2);
        assert_eq!(layout.queue_preview.height, 2);
        assert_eq!(layout.transcript.height, 40 - 3 - 1 - 2 - 1);
    }

    #[test]
    fn layout_with_two_line_status() {
        // Streaming with a details line asks for status_height=2.
        let layout = compute_layout(area(80, 40), 3, 2, 0);
        assert_eq!(layout.status.height, 2);
        assert_eq!(layout.transcript.height, 40 - 3 - 2 - 1);
    }

    #[test]
    fn layout_positions_are_contiguous() {
        let layout = compute_layout(area(100, 50), 5, 1, 3);
        assert_eq!(layout.transcript.y, 0);
        assert_eq!(
            layout.status.y,
            layout.transcript.y + layout.transcript.height
        );
        assert_eq!(
            layout.queue_preview.y,
            layout.status.y + layout.status.height
        );
        assert_eq!(
            layout.composer.y,
            layout.queue_preview.y + layout.queue_preview.height
        );
        assert_eq!(layout.footer.y, layout.composer.y + layout.composer.height);
    }
}
