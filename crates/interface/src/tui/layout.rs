use ratatui::layout::Rect;

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
    let narrow = area.width < 72;
    let very_narrow = area.width < 56;

    let mut composer = composer_height.min(area.height);
    let composer_min = 3.min(composer.max(1));
    if composer > composer_min && area.height < composer + status_height + queue_preview_height + 4
    {
        composer = composer_min;
    }

    let mut status = if narrow {
        status_height.min(1)
    } else {
        status_height
    };
    let mut queue_preview = if narrow { 0 } else { queue_preview_height };
    let mut footer = if very_narrow { 0 } else { 1 };

    let transcript_min = 3.min(area.height);
    let mut chrome = composer
        .saturating_add(status)
        .saturating_add(queue_preview)
        .saturating_add(footer);

    if chrome.saturating_add(transcript_min) > area.height {
        let overflow = chrome.saturating_add(transcript_min) - area.height;
        let reduce = queue_preview.min(overflow);
        queue_preview -= reduce;
        chrome -= reduce;
    }
    if chrome.saturating_add(transcript_min) > area.height {
        let overflow = chrome.saturating_add(transcript_min) - area.height;
        let reduce = footer.min(overflow);
        footer -= reduce;
        chrome -= reduce;
    }
    if chrome.saturating_add(transcript_min) > area.height {
        let overflow = chrome.saturating_add(transcript_min) - area.height;
        let reduce = status.min(overflow);
        status -= reduce;
        chrome -= reduce;
    }
    if chrome.saturating_add(transcript_min) > area.height {
        let overflow = chrome.saturating_add(transcript_min) - area.height;
        let reduce = composer.saturating_sub(composer_min).min(overflow);
        composer -= reduce;
        chrome -= reduce;
    }

    let transcript = area.height.saturating_sub(chrome);

    let mut y = area.y;
    let transcript_rect = Rect::new(area.x, y, area.width, transcript);
    y = y.saturating_add(transcript);
    let status_rect = Rect::new(area.x, y, area.width, status);
    y = y.saturating_add(status);
    let queue_preview_rect = Rect::new(area.x, y, area.width, queue_preview);
    y = y.saturating_add(queue_preview);
    let composer_rect = Rect::new(area.x, y, area.width, composer);
    y = y.saturating_add(composer);
    let footer_rect = Rect::new(area.x, y, area.width, footer);

    AppLayout {
        transcript: transcript_rect,
        status: status_rect,
        queue_preview: queue_preview_rect,
        composer: composer_rect,
        footer: footer_rect,
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

    #[test]
    fn narrow_layout_drops_queue_preview_first() {
        let layout = compute_layout(area(60, 18), 5, 2, 4);
        assert_eq!(layout.queue_preview.height, 0);
        assert_eq!(layout.status.height, 1);
        assert_eq!(layout.footer.height, 1);
        assert!(layout.transcript.height >= 3);
    }

    #[test]
    fn tiny_layout_drops_footer_and_shrinks_composer() {
        let layout = compute_layout(area(50, 7), 5, 2, 3);
        assert_eq!(layout.footer.height, 0);
        assert_eq!(layout.queue_preview.height, 0);
        assert_eq!(layout.status.height, 1);
        assert_eq!(layout.composer.height, 3);
        assert_eq!(layout.transcript.height, 3);
    }
}
