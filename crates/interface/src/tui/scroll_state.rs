//! Selection + scroll bookkeeping for vertical list pickers.
//!
//! Encapsulates the common behaviour of a selectable list that supports:
//!
//! - Optional selection (`None` when the list is empty)
//! - Wrap-around navigation on up/down
//! - Maintaining a scroll window (`scroll_top`) so the selected row stays
//!   visible inside the caller's viewport
//!
//! Callers own the filtered row count and the visible window size. Every
//! mutation takes those as arguments rather than caching them here, so list
//! views can apply filters, pagination, or density changes without this helper
//! knowing about their data model. Passing a stale length after filtering
//! would leave the selection pointing at the wrong row, so callers should
//! clamp or move through this type immediately after changing their visible
//! row set.

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ScrollState {
    pub selected_idx: Option<usize>,
    pub scroll_top: usize,
}

impl ScrollState {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Clamp selection into `[0, len-1]`, or `None` when empty.
    #[cfg(test)]
    pub fn clamp_selection(&mut self, len: usize) {
        if self.clear_if_empty(len) {
            return;
        }
        self.selected_idx = Some(self.selected_idx.unwrap_or(0).min(len - 1));
    }

    pub fn move_up_wrap(&mut self, len: usize) {
        if self.clear_if_empty(len) {
            return;
        }
        self.selected_idx = Some(match self.selected_idx {
            Some(idx) if idx > 0 => idx - 1,
            Some(_) => len - 1,
            None => 0,
        });
    }

    pub fn move_down_wrap(&mut self, len: usize) {
        if self.clear_if_empty(len) {
            return;
        }
        self.selected_idx = Some(match self.selected_idx {
            Some(idx) if idx + 1 < len => idx + 1,
            _ => 0,
        });
    }

    /// Move selection up by one visible page, clamping at the first row.
    ///
    /// Page movement intentionally does not wrap — repeated page-up converges
    /// at the top edge while still keeping the selected row visible.
    pub fn page_up_clamped(&mut self, len: usize, visible_rows: usize) {
        if self.clear_if_empty(len) {
            return;
        }
        let step = visible_rows.max(1);
        let current = self.selected_idx.unwrap_or(0).min(len - 1);
        self.selected_idx = Some(current.saturating_sub(step));
        self.ensure_visible(len, visible_rows);
    }

    /// Move selection down by one visible page, clamping at the last row.
    pub fn page_down_clamped(&mut self, len: usize, visible_rows: usize) {
        if self.clear_if_empty(len) {
            return;
        }
        let step = visible_rows.max(1);
        let current = self.selected_idx.unwrap_or(0).min(len - 1);
        self.selected_idx = Some(current.saturating_add(step).min(len - 1));
        self.ensure_visible(len, visible_rows);
    }

    pub fn jump_top(&mut self, len: usize, visible_rows: usize) {
        if self.clear_if_empty(len) {
            return;
        }
        self.selected_idx = Some(0);
        self.ensure_visible(len, visible_rows);
    }

    pub fn jump_bottom(&mut self, len: usize, visible_rows: usize) {
        if self.clear_if_empty(len) {
            return;
        }
        self.selected_idx = Some(len - 1);
        self.ensure_visible(len, visible_rows);
    }

    fn clear_if_empty(&mut self, len: usize) -> bool {
        if len != 0 {
            return false;
        }
        self.selected_idx = None;
        self.scroll_top = 0;
        true
    }

    /// Adjust `scroll_top` so the current `selected_idx` sits inside the
    /// `visible_rows`-tall window.
    pub fn ensure_visible(&mut self, len: usize, visible_rows: usize) {
        if len == 0 || visible_rows == 0 {
            self.scroll_top = 0;
            return;
        }
        if let Some(sel) = self.selected_idx {
            if sel < self.scroll_top {
                self.scroll_top = sel;
            } else {
                let bottom = self.scroll_top + visible_rows - 1;
                if sel > bottom {
                    self.scroll_top = sel + 1 - visible_rows;
                }
            }
        } else {
            self.scroll_top = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ScrollState;

    /// Wrap behaviour at both ends + scroll window follows selection so the
    /// caller never has to scroll-fix manually after a wrap.
    #[test]
    fn wrap_navigation_keeps_selection_visible() {
        let mut s = ScrollState::new();
        let len = 10;
        let vis = 5;

        s.clamp_selection(len);
        assert_eq!(s.selected_idx, Some(0));
        s.ensure_visible(len, vis);
        assert_eq!(s.scroll_top, 0);

        // up from row 0 wraps to last row; window must follow.
        s.move_up_wrap(len);
        s.ensure_visible(len, vis);
        assert_eq!(s.selected_idx, Some(len - 1));
        let sel = s.selected_idx.expect("selected after wrap");
        assert!(s.scroll_top <= sel && sel < s.scroll_top + vis);

        // down from last row wraps to 0; window resets to top.
        s.move_down_wrap(len);
        s.ensure_visible(len, vis);
        assert_eq!(s.selected_idx, Some(0));
        assert_eq!(s.scroll_top, 0);
    }

    /// Page movement converges at edges (no wrap); jump_top/bottom snap.
    #[test]
    fn page_and_jump_navigation_clamps_at_edges() {
        let mut s = ScrollState::new();
        let len = 10;
        let vis = 4;

        s.clamp_selection(len);
        s.page_down_clamped(len, vis);
        assert_eq!(s.selected_idx, Some(4));
        assert_eq!(s.scroll_top, 1);

        s.page_down_clamped(len, vis);
        assert_eq!(s.selected_idx, Some(8));
        assert_eq!(s.scroll_top, 5);

        // Third page-down clamps at the last row instead of wrapping.
        s.page_down_clamped(len, vis);
        assert_eq!(s.selected_idx, Some(9));
        assert_eq!(s.scroll_top, 6);

        s.page_up_clamped(len, vis);
        assert_eq!(s.selected_idx, Some(5));
        assert_eq!(s.scroll_top, 5);

        s.jump_top(len, vis);
        assert_eq!(s.selected_idx, Some(0));
        assert_eq!(s.scroll_top, 0);

        s.jump_bottom(len, vis);
        assert_eq!(s.selected_idx, Some(9));
        assert_eq!(s.scroll_top, 6);
    }

    /// Mutations against an empty list MUST clear selection rather than panic.
    /// Lists shrink dynamically (filter typing, async refresh) and the picker
    /// would otherwise be unable to recover when the user types past the last
    /// match.
    #[test]
    fn empty_list_clears_selection_for_every_mutation() {
        let mut s = ScrollState {
            selected_idx: Some(3),
            scroll_top: 2,
        };
        s.clamp_selection(0);
        assert_eq!(s.selected_idx, None);
        assert_eq!(s.scroll_top, 0);

        s.selected_idx = Some(3);
        s.move_up_wrap(0);
        assert_eq!(s.selected_idx, None);

        s.selected_idx = Some(3);
        s.move_down_wrap(0);
        assert_eq!(s.selected_idx, None);

        s.selected_idx = Some(3);
        s.page_up_clamped(0, 5);
        assert_eq!(s.selected_idx, None);

        s.selected_idx = Some(3);
        s.page_down_clamped(0, 5);
        assert_eq!(s.selected_idx, None);
    }

    /// `ensure_visible` with a zero-row viewport (host dropped the picker
    /// area to nothing) must not divide-by-zero — it resets the window to 0.
    #[test]
    fn ensure_visible_handles_zero_row_viewport() {
        let mut s = ScrollState {
            selected_idx: Some(4),
            scroll_top: 7,
        };
        s.ensure_visible(10, 0);
        assert_eq!(s.scroll_top, 0);
    }
}
