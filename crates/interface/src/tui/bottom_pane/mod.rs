//! Bottom pane host: a screen-bottom region that owns whichever modal "view"
//! is currently displacing the composer (picker overlay, list selection, …).
//!
//! The host is intentionally tiny — it stores a single optional boxed View and
//! routes input/render to it. The composer continues to render in its own
//! `Rect`; when a View is active it claims height ABOVE the composer (the
//! transcript shrinks to accommodate). Existing popups in
//! `crates/interface/src/tui/` are not migrated through this host — they keep
//! their existing surfaces.
//!
//! The View stack is flattened to one slot (borg never stacks
//! request_user_input overlays inside another View) and key dispatch is
//! routed directly from `App::handle_key`.

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::widgets::{Clear, Widget};

use crate::tui::custom_terminal::Frame;

pub(crate) mod request_user_input;
mod text_line;

/// Outcome of routing a cancellation key (Esc / Ctrl-C) to the active View.
///
/// `Handled` means the View consumed the cancellation (it may have closed,
/// cleared a sub-state, etc.). `NotHandled` means the caller should fall back
/// to its own cancellation path (cancel turn, exit pager, etc.).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CancellationEvent {
    Handled,
    NotHandled,
}

/// A modal pane hosted by [`BottomPane`].
///
/// Implementors own their own state (selection, scroll, notes draft, …) and
/// expose only the small surface the host needs to route input + render +
/// arbitrate height.
pub(crate) trait BottomPaneView {
    /// Total rows the view wants to occupy for the given pane width.
    ///
    /// The host clamps this against available transcript space; views must
    /// tolerate rendering into a shorter `Rect` than they asked for.
    fn desired_height(&self, width: u16) -> u16;

    /// Render into `area`. The caller has already issued a `Clear` over the
    /// region.
    fn render(&self, frame: &mut Frame<'_>, area: Rect);

    /// Handle a single key event while the view is active. The host always
    /// requests a redraw after this call.
    fn handle_key_event(&mut self, key: KeyEvent);

    /// `true` once the view has finished and the host should drop it. The
    /// host checks this after every input dispatch.
    fn is_complete(&self) -> bool {
        false
    }

    /// Handle Esc / Ctrl-C. Default: not handled (host falls back).
    fn on_ctrl_c(&mut self) -> CancellationEvent {
        CancellationEvent::NotHandled
    }
}

/// Owner of the active `BottomPaneView`. Lives in `App` and is rendered after
/// the composer.
pub(crate) struct BottomPane {
    view: Option<Box<dyn BottomPaneView>>,
}

impl BottomPane {
    pub fn new() -> Self {
        Self { view: None }
    }

    /// Install a new view. Replaces any view currently active — borg's flow
    /// never nests overlays, so swapping is the safe default. The previously
    /// active view is dropped on the spot.
    pub fn set_view(&mut self, view: Box<dyn BottomPaneView>) {
        self.view = Some(view);
    }

    /// Drop the current view without giving it a chance to handle anything.
    /// Reserved for an LLM-side cancel that needs to tear the overlay down
    /// without dispatching a key event; currently unused in production but
    /// kept as the host's "force-close" surface.
    #[allow(dead_code)]
    pub fn clear_view(&mut self) {
        self.view = None;
    }

    pub fn is_active(&self) -> bool {
        self.view.is_some()
    }

    /// Height the active view wants, or `0` when idle. Layout code uses this
    /// to displace the transcript above the composer.
    pub fn desired_height(&self, width: u16) -> u16 {
        self.view.as_ref().map_or(0, |v| v.desired_height(width))
    }

    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        if let Some(view) = self.view.as_ref() {
            // Wipe the rect before delegating so the view never sees stale
            // pixels from the transcript or composer underneath. Views
            // document this expectation (see `request_user_input::render`).
            Clear.render(area, frame.buffer_mut());
            view.render(frame, area);
        }
    }

    /// Route a key to the active view. Returns `true` if a view was present
    /// (so the caller should NOT forward to the composer). Drops the view if
    /// it reported `is_complete()` after handling.
    pub fn handle_key_event(&mut self, key: KeyEvent) -> bool {
        let Some(view) = self.view.as_mut() else {
            return false;
        };
        view.handle_key_event(key);
        if view.is_complete() {
            self.view = None;
        }
        true
    }

    /// Route Esc / Ctrl-C to the active view. The view may consume it (return
    /// `Handled`) or pass it through (`NotHandled`), in which case the caller
    /// runs its own cancel path.
    pub fn on_ctrl_c(&mut self) -> CancellationEvent {
        let Some(view) = self.view.as_mut() else {
            return CancellationEvent::NotHandled;
        };
        let event = view.on_ctrl_c();
        if view.is_complete() {
            self.view = None;
        }
        event
    }
}

impl Default for BottomPane {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// Minimal test view: counts key dispatches, optionally completes after N
    /// keys, optionally consumes Ctrl-C. Lets each test wire up exactly the
    /// behaviour it wants to exercise without the noise of a real overlay.
    struct CountingView {
        keys_seen: usize,
        complete_after: Option<usize>,
        ctrl_c_handled: bool,
        height: u16,
    }

    impl CountingView {
        fn new(height: u16) -> Self {
            Self {
                keys_seen: 0,
                complete_after: None,
                ctrl_c_handled: false,
                height,
            }
        }
    }

    impl BottomPaneView for CountingView {
        fn desired_height(&self, _width: u16) -> u16 {
            self.height
        }
        fn render(&self, _frame: &mut Frame<'_>, _area: Rect) {}
        fn handle_key_event(&mut self, _key: KeyEvent) {
            self.keys_seen += 1;
        }
        fn is_complete(&self) -> bool {
            self.complete_after.is_some_and(|n| self.keys_seen >= n)
        }
        fn on_ctrl_c(&mut self) -> CancellationEvent {
            if self.ctrl_c_handled {
                CancellationEvent::Handled
            } else {
                CancellationEvent::NotHandled
            }
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Idle host: no view installed → desired_height 0, key dispatch is a
    /// no-op the caller knows about (returns false so App can route to
    /// composer instead).
    #[test]
    fn idle_pane_reports_zero_height_and_passes_keys_through() {
        let mut pane = BottomPane::new();
        assert!(!pane.is_active());
        assert_eq!(pane.desired_height(80), 0);
        assert!(!pane.handle_key_event(key(KeyCode::Char('x'))));
        assert_eq!(pane.on_ctrl_c(), CancellationEvent::NotHandled);
    }

    /// Active view: keys are routed to it and the host reports active. Caller
    /// using the bool return value can short-circuit composer routing.
    #[test]
    fn active_pane_routes_keys_to_view_and_reports_active() {
        let mut pane = BottomPane::new();
        pane.set_view(Box::new(CountingView::new(5)));
        assert!(pane.is_active());
        assert_eq!(pane.desired_height(80), 5);
        assert!(pane.handle_key_event(key(KeyCode::Char('a'))));
        assert!(pane.handle_key_event(key(KeyCode::Char('b'))));
        // Caller still sees `active` because the view didn't complete.
        assert!(pane.is_active());
    }

    /// A view that reports `is_complete()` after handling a key must be
    /// dropped by the host on the spot — otherwise the next key would land
    /// on a finished view and the composer would stay locked out.
    #[test]
    fn completed_view_is_dropped_after_dispatch() {
        let mut pane = BottomPane::new();
        let mut view = CountingView::new(1);
        view.complete_after = Some(1);
        pane.set_view(Box::new(view));
        assert!(pane.handle_key_event(key(KeyCode::Enter)));
        assert!(!pane.is_active(), "view should be dropped after completion");
        // Next key falls through.
        assert!(!pane.handle_key_event(key(KeyCode::Char('z'))));
    }

    /// Ctrl-C propagation: if the view says Handled, the host returns Handled
    /// AND drops the view (mirrors the reference: a handled cancel always
    /// dismisses the overlay). If NotHandled, the view stays installed.
    #[test]
    fn ctrl_c_handled_dismisses_view_not_handled_keeps_it() {
        let mut pane = BottomPane::new();
        let mut view = CountingView::new(2);
        view.ctrl_c_handled = false;
        pane.set_view(Box::new(view));
        assert_eq!(pane.on_ctrl_c(), CancellationEvent::NotHandled);
        assert!(pane.is_active(), "NotHandled keeps the view installed");

        let mut view = CountingView::new(2);
        view.ctrl_c_handled = true;
        view.complete_after = Some(0); // completes immediately on the Handled path
        pane.set_view(Box::new(view));
        assert_eq!(pane.on_ctrl_c(), CancellationEvent::Handled);
        assert!(
            !pane.is_active(),
            "Handled cancel should drop the view when it completes"
        );
    }

    /// `clear_view` forcibly drops the active view regardless of its state.
    /// Used by App when the LLM cancels a tool request mid-overlay.
    #[test]
    fn clear_view_unconditionally_drops_active_view() {
        let mut pane = BottomPane::new();
        pane.set_view(Box::new(CountingView::new(3)));
        assert!(pane.is_active());
        pane.clear_view();
        assert!(!pane.is_active());
        assert_eq!(pane.desired_height(80), 0);
    }
}
