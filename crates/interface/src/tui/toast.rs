//! Non-blocking toast notifications rendered in the bottom-right corner.
//!
//! Toasts are fire-and-forget: push one, it auto-dismisses after a short
//! duration. Multiple toasts stack vertically. The stack evicts the oldest
//! toast when a new one would exceed `max`.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::theme;

const DEFAULT_TTL: Duration = Duration::from_secs(3);
const DEFAULT_MAX: usize = 4;
const MIN_WIDTH: u16 = 18;
const MAX_WIDTH: u16 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Success/Warning/Error only constructed in tests; variants kept for border styling coverage.
pub enum ToastVariant {
    Info,
    Success,
    Warning,
    Error,
}

impl ToastVariant {
    fn border_style(self) -> ratatui::style::Style {
        match self {
            ToastVariant::Info => theme::toast_info_border(),
            ToastVariant::Success => theme::toast_success_border(),
            ToastVariant::Warning => theme::toast_warning_border(),
            ToastVariant::Error => theme::toast_error_border(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub variant: ToastVariant,
    pub expires_at: Instant,
}

impl Toast {
    pub fn new(text: impl Into<String>, variant: ToastVariant, ttl: Duration) -> Self {
        Self {
            text: text.into(),
            variant,
            expires_at: Instant::now() + ttl,
        }
    }
}

#[derive(Debug)]
pub struct ToastStack {
    items: VecDeque<Toast>,
    max: usize,
    ttl: Duration,
}

impl Default for ToastStack {
    fn default() -> Self {
        Self {
            items: VecDeque::new(),
            max: DEFAULT_MAX,
            ttl: DEFAULT_TTL,
        }
    }
}

impl ToastStack {
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(dead_code)] // used in tests
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[allow(dead_code)] // used in tests
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn push(&mut self, text: impl Into<String>, variant: ToastVariant) {
        while self.items.len() >= self.max {
            self.items.pop_front();
        }
        self.items.push_back(Toast::new(text, variant, self.ttl));
    }

    /// Drop toasts whose expiry has passed. Returns the count removed.
    pub fn prune_expired(&mut self) -> usize {
        let now = Instant::now();
        let before = self.items.len();
        self.items.retain(|t| t.expires_at > now);
        before - self.items.len()
    }

    /// Render the active toasts anchored to the bottom-right of `area`.
    /// No-op when the stack is empty or the area is too small.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if self.items.is_empty() || area.width < MIN_WIDTH + 2 || area.height < 3 {
            return;
        }
        let width = {
            let longest = self
                .items
                .iter()
                .map(|t| t.text.len() as u16)
                .max()
                .unwrap_or(0);
            (longest + 4).clamp(MIN_WIDTH, MAX_WIDTH.min(area.width.saturating_sub(2)))
        };
        let max_height = area.height.saturating_sub(2);
        let height = (self.items.len() as u16 * 3).min(max_height);
        if height < 3 {
            return;
        }
        let x = area.x + area.width.saturating_sub(width + 1);
        let y = area.y + area.height.saturating_sub(height + 1);

        let mut cursor_y = y;
        for toast in self.items.iter() {
            if cursor_y + 3 > area.y + area.height {
                break;
            }
            let rect = Rect::new(x, cursor_y, width, 3);
            frame.render_widget(Clear, rect);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(toast.variant.border_style());
            let inner_width = width.saturating_sub(4) as usize;
            let text = if toast.text.chars().count() > inner_width {
                let truncated: String = toast
                    .text
                    .chars()
                    .take(inner_width.saturating_sub(1))
                    .collect();
                format!("{truncated}…")
            } else {
                toast.text.clone()
            };
            let line = Line::from(Span::styled(text, theme::header_style()));
            frame.render_widget(Paragraph::new(line).block(block), rect);
            cursor_y += 3;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_appends_up_to_max() {
        let mut stack = ToastStack::new();
        stack.push("a", ToastVariant::Info);
        stack.push("b", ToastVariant::Success);
        assert_eq!(stack.len(), 2);
    }

    #[test]
    fn push_over_max_evicts_oldest() {
        let mut stack = ToastStack::new();
        for i in 0..(DEFAULT_MAX + 2) {
            stack.push(format!("toast-{i}"), ToastVariant::Info);
        }
        assert_eq!(stack.len(), DEFAULT_MAX);
        // Newest retained, oldest gone
        let texts: Vec<_> = stack.items.iter().map(|t| t.text.as_str()).collect();
        assert!(texts.contains(&"toast-5"));
        assert!(!texts.contains(&"toast-0"));
    }

    #[test]
    fn prune_expired_removes_stale() {
        let mut stack = ToastStack::new();
        stack.push("stale", ToastVariant::Warning);
        // Manually expire
        if let Some(t) = stack.items.back_mut() {
            t.expires_at = Instant::now() - Duration::from_millis(1);
        }
        let removed = stack.prune_expired();
        assert_eq!(removed, 1);
        assert!(stack.is_empty());
    }

    #[test]
    fn prune_keeps_fresh() {
        let mut stack = ToastStack::new();
        stack.push("fresh", ToastVariant::Info);
        let removed = stack.prune_expired();
        assert_eq!(removed, 0);
        assert_eq!(stack.len(), 1);
    }

    #[test]
    fn variant_border_styles_distinct() {
        // Use the theme helpers directly — assert all variants map to distinct colors
        let a = ToastVariant::Info.border_style();
        let b = ToastVariant::Success.border_style();
        let c = ToastVariant::Warning.border_style();
        let d = ToastVariant::Error.border_style();
        let fgs = [a.fg, b.fg, c.fg, d.fg];
        let distinct: std::collections::HashSet<_> = fgs.iter().collect();
        assert_eq!(distinct.len(), 4, "variant borders should all differ");
    }

    #[test]
    fn render_on_empty_area_is_noop() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let stack = ToastStack::new();
        let backend = TestBackend::new(40, 10);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|frame| {
            // Empty stack, area is fine — just ensure no panic.
            let area = frame.area();
            stack.render(frame, area);
        })
        .unwrap();
    }

    #[test]
    fn render_with_toast_is_noop_when_tiny_area() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut stack = ToastStack::new();
        stack.push("hello", ToastVariant::Info);
        let backend = TestBackend::new(10, 2); // too small for 3-row toast
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|frame| {
            let area = frame.area();
            stack.render(frame, area);
        })
        .unwrap();
    }

    #[test]
    fn render_with_toast_draws_without_panic() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut stack = ToastStack::new();
        stack.push("skill installed", ToastVariant::Success);
        let backend = TestBackend::new(60, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|frame| {
            let area = frame.area();
            stack.render(frame, area);
        })
        .unwrap();
    }
}
