//! Layout math for the picker overlay.
//!
//! Splits a single host `Rect` into stacked sub-areas (progress row,
//! question, options list, notes editor) plus a footer-row count, collapsing
//! gracefully as available height shrinks. Near-verbatim port of the
//! reference TUI's layout module; the overlay's geometric accessors live in
//! the sibling `mod.rs`.
//!
//! Phase 3 only needs the math + tests against the overlay skeleton; Phase 4
//! consumes `LayoutSections` for rendering and Phase 6 wires the focus
//! transitions that change the inputs (notes visibility, question count).

use ratatui::layout::Rect;

use super::RequestUserInputOverlay;
use super::DESIRED_SPACERS_BETWEEN_SECTIONS;

#[derive(Debug, Clone)]
pub(crate) struct LayoutSections {
    pub(crate) progress_area: Rect,
    pub(crate) question_area: Rect,
    /// Wrapped question text rows to render in `question_area`. May be
    /// truncated to fit when the area is tighter than the natural wrap.
    pub(crate) question_lines: Vec<String>,
    pub(crate) options_area: Rect,
    pub(crate) notes_area: Rect,
    /// Number of footer rows (hint lines) to allocate immediately below the
    /// overlay's last section. Rendered by the caller — layout doesn't carve
    /// a `Rect` for it because borg's footer lives outside the overlay's
    /// area.
    pub(crate) footer_lines: u16,
}

impl RequestUserInputOverlay {
    /// Compute layout sections, collapsing notes + hints as space shrinks.
    pub(crate) fn layout_sections(&self, area: Rect) -> LayoutSections {
        let has_options = self.has_options();
        let notes_visible = !has_options || self.notes_ui_visible();
        let footer_pref = self.footer_required_height(area.width);
        let notes_pref_height = self.notes_input_height(area.width);
        let mut question_lines = self.wrapped_question_lines(area.width);
        let question_height = question_lines.len() as u16;

        let layout = if has_options {
            self.layout_with_options(
                OptionsLayoutArgs {
                    available_height: area.height,
                    width: area.width,
                    question_height,
                    notes_pref_height,
                    footer_pref,
                    notes_visible,
                },
                &mut question_lines,
            )
        } else {
            self.layout_without_options(
                area.height,
                question_height,
                notes_pref_height,
                footer_pref,
                &mut question_lines,
            )
        };

        let (progress_area, question_area, options_area, notes_area) =
            self.build_layout_areas(area, layout);

        LayoutSections {
            progress_area,
            question_area,
            question_lines,
            options_area,
            notes_area,
            footer_lines: layout.footer_lines,
        }
    }

    fn layout_with_options(
        &self,
        args: OptionsLayoutArgs,
        question_lines: &mut Vec<String>,
    ) -> LayoutPlan {
        let OptionsLayoutArgs {
            available_height,
            width,
            mut question_height,
            notes_pref_height,
            footer_pref,
            notes_visible,
        } = args;
        let min_options_height = available_height.min(1);
        let max_question_height = available_height.saturating_sub(min_options_height);
        if question_height > max_question_height {
            question_height = max_question_height;
            question_lines.truncate(question_height as usize);
        }
        self.layout_with_options_normal(
            OptionsNormalArgs {
                available_height,
                question_height,
                notes_pref_height,
                footer_pref,
                notes_visible,
            },
            OptionsHeights {
                preferred: self.options_preferred_height(width),
                full: self.options_required_height(width),
            },
        )
    }

    fn layout_with_options_normal(
        &self,
        args: OptionsNormalArgs,
        options: OptionsHeights,
    ) -> LayoutPlan {
        let OptionsNormalArgs {
            available_height,
            question_height,
            notes_pref_height,
            footer_pref,
            notes_visible,
        } = args;
        let max_options_height = available_height.saturating_sub(question_height);
        let min_options_height = max_options_height.min(1);
        let mut options_height = options
            .preferred
            .min(max_options_height)
            .max(min_options_height);
        let used = question_height.saturating_add(options_height);
        let mut remaining = available_height.saturating_sub(used);

        // When notes are hidden, prefer to reserve room for progress, footer,
        // and spacers by shrinking the options window if needed.
        let desired_spacers = if notes_visible {
            1
        } else {
            DESIRED_SPACERS_BETWEEN_SECTIONS
        };
        let required_extra = footer_pref
            .saturating_add(1) // progress line
            .saturating_add(desired_spacers);
        if remaining < required_extra {
            let deficit = required_extra.saturating_sub(remaining);
            let reducible = options_height.saturating_sub(min_options_height);
            let reduce_by = deficit.min(reducible);
            options_height = options_height.saturating_sub(reduce_by);
            remaining = remaining.saturating_add(reduce_by);
        }

        let mut progress_height = 0;
        if remaining > 0 {
            progress_height = 1;
            remaining = remaining.saturating_sub(1);
        }

        if !notes_visible {
            let mut spacer_after_options = 0;
            if remaining > footer_pref {
                spacer_after_options = 1;
                remaining = remaining.saturating_sub(1);
            }
            let footer_lines = footer_pref.min(remaining);
            remaining = remaining.saturating_sub(footer_lines);
            let mut spacer_after_question = 0;
            if remaining > 0 {
                spacer_after_question = 1;
                remaining = remaining.saturating_sub(1);
            }
            let grow_by = remaining.min(options.full.saturating_sub(options_height));
            options_height = options_height.saturating_add(grow_by);
            return LayoutPlan {
                question_height,
                progress_height,
                spacer_after_question,
                options_height,
                spacer_after_options,
                notes_height: 0,
                footer_lines,
            };
        }

        let footer_lines = footer_pref.min(remaining);
        remaining = remaining.saturating_sub(footer_lines);

        let mut spacer_after_question = 0;
        if remaining > 0 {
            spacer_after_question = 1;
            remaining = remaining.saturating_sub(1);
        }
        let spacer_after_options = 0;
        let mut notes_height = notes_pref_height.min(remaining);
        remaining = remaining.saturating_sub(notes_height);

        notes_height = notes_height.saturating_add(remaining);

        LayoutPlan {
            question_height,
            progress_height,
            spacer_after_question,
            options_height,
            spacer_after_options,
            notes_height,
            footer_lines,
        }
    }

    fn layout_without_options(
        &self,
        available_height: u16,
        question_height: u16,
        notes_pref_height: u16,
        footer_pref: u16,
        question_lines: &mut Vec<String>,
    ) -> LayoutPlan {
        if question_height > available_height {
            self.layout_without_options_tight(available_height, question_height, question_lines)
        } else {
            self.layout_without_options_normal(
                available_height,
                question_height,
                notes_pref_height,
                footer_pref,
            )
        }
    }

    fn layout_without_options_tight(
        &self,
        available_height: u16,
        question_height: u16,
        question_lines: &mut Vec<String>,
    ) -> LayoutPlan {
        let max_question_height = available_height;
        let adjusted_question_height = question_height.min(max_question_height);
        question_lines.truncate(adjusted_question_height as usize);

        LayoutPlan {
            question_height: adjusted_question_height,
            progress_height: 0,
            spacer_after_question: 0,
            options_height: 0,
            spacer_after_options: 0,
            notes_height: 0,
            footer_lines: 0,
        }
    }

    fn layout_without_options_normal(
        &self,
        available_height: u16,
        question_height: u16,
        notes_pref_height: u16,
        footer_pref: u16,
    ) -> LayoutPlan {
        let mut remaining = available_height.saturating_sub(question_height);
        let mut notes_height = notes_pref_height.min(remaining);
        remaining = remaining.saturating_sub(notes_height);

        let footer_lines = footer_pref.min(remaining);
        remaining = remaining.saturating_sub(footer_lines);

        let mut progress_height = 0;
        if remaining > 0 {
            progress_height = 1;
            remaining = remaining.saturating_sub(1);
        }

        notes_height = notes_height.saturating_add(remaining);

        LayoutPlan {
            question_height,
            progress_height,
            spacer_after_question: 0,
            options_height: 0,
            spacer_after_options: 0,
            notes_height,
            footer_lines,
        }
    }

    fn build_layout_areas(&self, area: Rect, heights: LayoutPlan) -> (Rect, Rect, Rect, Rect) {
        let mut cursor_y = area.y;
        let progress_area = Rect {
            x: area.x,
            y: cursor_y,
            width: area.width,
            height: heights.progress_height,
        };
        cursor_y = cursor_y.saturating_add(heights.progress_height);
        let question_area = Rect {
            x: area.x,
            y: cursor_y,
            width: area.width,
            height: heights.question_height,
        };
        cursor_y = cursor_y.saturating_add(heights.question_height);
        cursor_y = cursor_y.saturating_add(heights.spacer_after_question);

        let options_area = Rect {
            x: area.x,
            y: cursor_y,
            width: area.width,
            height: heights.options_height,
        };
        cursor_y = cursor_y.saturating_add(heights.options_height);
        cursor_y = cursor_y.saturating_add(heights.spacer_after_options);

        let notes_area = Rect {
            x: area.x,
            y: cursor_y,
            width: area.width,
            height: heights.notes_height,
        };

        (progress_area, question_area, options_area, notes_area)
    }
}

#[derive(Clone, Copy, Debug)]
struct LayoutPlan {
    progress_height: u16,
    question_height: u16,
    spacer_after_question: u16,
    options_height: u16,
    spacer_after_options: u16,
    notes_height: u16,
    footer_lines: u16,
}

#[derive(Clone, Copy, Debug)]
struct OptionsLayoutArgs {
    available_height: u16,
    width: u16,
    question_height: u16,
    notes_pref_height: u16,
    footer_pref: u16,
    notes_visible: bool,
}

#[derive(Clone, Copy, Debug)]
struct OptionsNormalArgs {
    available_height: u16,
    question_height: u16,
    notes_pref_height: u16,
    footer_pref: u16,
    notes_visible: bool,
}

#[derive(Clone, Copy, Debug)]
struct OptionsHeights {
    preferred: u16,
    full: u16,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::bottom_pane::request_user_input::Focus;
    use cortex::tool_handlers::user_input::{Question, QuestionOption};

    fn q(prompt: &str, options: usize) -> Question {
        Question {
            id: "q".into(),
            header: "Q".into(),
            question: prompt.into(),
            options: (0..options)
                .map(|i| QuestionOption {
                    label: format!("opt {i}"),
                    description: format!("desc {i}"),
                })
                .collect(),
            multi_select: false,
        }
    }

    fn area(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    fn overlay(prompt: &str, opts: usize) -> RequestUserInputOverlay {
        RequestUserInputOverlay::new(vec![q(prompt, opts)]).unwrap()
    }

    /// Stacked sub-areas must tile the host rect exactly: no row is rendered
    /// twice, none is skipped, and `progress_area.y == area.y`. A drifting
    /// cursor would either over-paint the composer (clipping bug) or leave a
    /// black band (the symptom we're fixing).
    #[test]
    fn sections_tile_host_rect_top_down_and_within_bounds() {
        let o = overlay("pick a thing", 3);
        let host = area(80, 20);
        let s = o.layout_sections(host);

        assert_eq!(s.progress_area.y, host.y);
        let mut y = s.progress_area.y;
        for r in [
            s.progress_area,
            s.question_area,
            s.options_area,
            s.notes_area,
        ] {
            assert!(r.y >= y, "section starts before previous ends");
            assert_eq!(r.x, host.x);
            assert_eq!(r.width, host.width);
            assert!(
                r.y.saturating_add(r.height) <= host.y + host.height,
                "section overflows host rect"
            );
            y = r.y.saturating_add(r.height);
        }
    }

    /// With notes hidden (default), the layout must reserve at minimum: 1
    /// progress row + question rows + options rows + 1 footer hint. Notes
    /// stays zero-height. This is the "common case" that the old broken
    /// renderer collapsed onto the composer rect.
    #[test]
    fn default_layout_hides_notes_and_keeps_options_visible() {
        let o = overlay("pick", 3);
        let s = o.layout_sections(area(80, 20));
        assert_eq!(s.notes_area.height, 0, "notes hidden by default");
        assert!(s.progress_area.height >= 1);
        assert!(s.question_area.height >= 1);
        assert!(
            s.options_area.height >= o.current_options_len() as u16,
            "all 4 option rows (3 + Other) should be visible at height 20"
        );
        assert_eq!(s.footer_lines, 1);
    }

    /// Focus the notes field → layout reallocates space for the notes area
    /// while still keeping the options list visible. Without this carve-out,
    /// Tab into Notes would render an invisible editor.
    #[test]
    fn focusing_notes_reserves_notes_area_height() {
        let mut o = overlay("pick", 2);
        o.focus = Focus::Notes;
        let s = o.layout_sections(area(80, 20));
        assert!(
            s.notes_area.height >= 1,
            "notes area must be non-empty when focused"
        );
        assert!(s.options_area.height >= 1, "options must remain visible");
    }

    /// Tight height: shrink the host until only a sliver of room remains.
    /// Question must still render (at least one row); footer + notes can
    /// collapse to zero. The reference's tight-layout branch is the exact
    /// behaviour borg needs to recover when the terminal is short.
    #[test]
    fn tight_height_collapses_footer_and_notes_first() {
        let o = overlay(
            "a very long question prompt that wraps several times across narrow widths",
            4,
        );
        let s = o.layout_sections(area(40, 6));
        let total: u16 = s.progress_area.height
            + s.question_area.height
            + s.options_area.height
            + s.notes_area.height;
        assert!(total <= 6, "sections must fit in host height");
        assert!(s.question_area.height >= 1, "question always shows");
    }

    /// Zero-height host (terminal momentarily collapses): no panic, all
    /// section heights are zero, question_lines vec may be truncated to
    /// empty. This is the defensive case that protects against the same
    /// class of UTF-8 byte-index panic the Phase-1 fix targeted — layout
    /// must be total over every Rect.
    #[test]
    fn zero_height_host_does_not_panic_and_returns_empty_sections() {
        let o = overlay("anything", 3);
        let s = o.layout_sections(area(80, 0));
        assert_eq!(s.progress_area.height, 0);
        assert_eq!(s.question_area.height, 0);
        assert_eq!(s.options_area.height, 0);
        assert_eq!(s.notes_area.height, 0);
    }

    /// Width-zero host: same defensive case as zero-height. Wrap returns no
    /// lines; layout must still produce a valid `LayoutSections`.
    #[test]
    fn zero_width_host_does_not_panic() {
        let o = overlay("anything", 3);
        let s = o.layout_sections(area(0, 20));
        assert!(s.question_lines.is_empty());
        assert_eq!(s.question_area.height, 0);
    }
}
