use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanOption {
    ClearAndProceed,
    ProceedWithContext,
    TypeFeedback,
}

pub struct PlanOverlay {
    pub visible: bool,
    selected: PlanOption,
    context_pct: u8,
    agent_name: String,
}

impl PlanOverlay {
    pub fn new() -> Self {
        Self {
            visible: false,
            selected: PlanOption::ClearAndProceed,
            context_pct: 0,
            agent_name: String::new(),
        }
    }

    pub fn show(&mut self, context_pct: u8, agent_name: String) {
        self.visible = true;
        self.selected = PlanOption::ClearAndProceed;
        self.context_pct = context_pct;
        self.agent_name = agent_name;
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
    }

    pub fn cycle(&mut self) {
        self.selected = match self.selected {
            PlanOption::ClearAndProceed => PlanOption::ProceedWithContext,
            PlanOption::ProceedWithContext => PlanOption::TypeFeedback,
            PlanOption::TypeFeedback => PlanOption::ClearAndProceed,
        };
    }

    pub fn select(&mut self, opt: PlanOption) {
        self.selected = opt;
    }

    pub fn selected(&self) -> PlanOption {
        self.selected
    }

    pub fn render(&self, frame: &mut Frame, composer_area: Rect) {
        if !self.visible {
            return;
        }

        let popup_height: u16 = 5;
        let available_above = composer_area.y;
        if available_above < popup_height {
            return;
        }

        let popup_width = composer_area.width.min(60);
        let popup_y = composer_area.y - popup_height;
        let popup_area = Rect::new(composer_area.x, popup_y, popup_width, popup_height);

        frame.render_widget(Clear, popup_area);

        let options = [
            (
                PlanOption::ClearAndProceed,
                format!("1. Clear context ({}% used) and proceed", self.context_pct),
            ),
            (
                PlanOption::ProceedWithContext,
                "2. Proceed with context".to_string(),
            ),
            (PlanOption::TypeFeedback, "3. Give feedback".to_string()),
        ];

        let lines: Vec<Line<'_>> = options
            .iter()
            .map(|(opt, label)| {
                let is_selected = *opt == self.selected;
                let prefix = if is_selected { " \u{276f} " } else { "   " };
                let style = if is_selected {
                    theme::popup_selected()
                } else {
                    theme::dim()
                };
                Line::from(Span::styled(format!("{prefix}{label}"), style))
            })
            .collect();

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme::dim())
            .title(" Plan Review ");

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, popup_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_overlay_not_visible() {
        let overlay = PlanOverlay::new();
        assert!(!overlay.visible);
        assert_eq!(overlay.selected(), PlanOption::ClearAndProceed);
    }

    #[test]
    fn show_sets_visible_and_context() {
        let mut overlay = PlanOverlay::new();
        overlay.show(75, "Borg".to_string());
        assert!(overlay.visible);
        assert_eq!(overlay.context_pct, 75);
        assert_eq!(overlay.agent_name, "Borg");
        assert_eq!(overlay.selected(), PlanOption::ClearAndProceed);
    }

    #[test]
    fn dismiss_hides() {
        let mut overlay = PlanOverlay::new();
        overlay.show(50, "Bot".to_string());
        overlay.dismiss();
        assert!(!overlay.visible);
    }

    #[test]
    fn cycle_wraps_around() {
        let mut overlay = PlanOverlay::new();
        assert_eq!(overlay.selected(), PlanOption::ClearAndProceed);
        overlay.cycle();
        assert_eq!(overlay.selected(), PlanOption::ProceedWithContext);
        overlay.cycle();
        assert_eq!(overlay.selected(), PlanOption::TypeFeedback);
        overlay.cycle();
        assert_eq!(overlay.selected(), PlanOption::ClearAndProceed);
    }

    #[test]
    fn select_sets_option() {
        let mut overlay = PlanOverlay::new();
        overlay.select(PlanOption::TypeFeedback);
        assert_eq!(overlay.selected(), PlanOption::TypeFeedback);
    }

    #[test]
    fn show_resets_selection() {
        let mut overlay = PlanOverlay::new();
        overlay.select(PlanOption::TypeFeedback);
        overlay.show(80, "Agent".to_string());
        assert_eq!(overlay.selected(), PlanOption::ClearAndProceed);
    }
}
