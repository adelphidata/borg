//! Popup routing: the `PopupHandler` trait that every routed popup implements,
//! the `for_each_routed_popup!` registry macro, and the `App` methods that
//! dispatch keys/paste to the visible popup or query aggregate popup state.
//!
//! Adding a popup is a one-line change in `for_each_routed_popup!` plus
//! a `PopupHandler` impl on the popup itself.

use anyhow::Result;

use cortex::config::Config;

use super::{App, AppAction, AppState};

/// Single source of truth for the list of popups that participate in
/// key/paste/visibility routing. Every dispatch site uses this macro, so
/// adding a popup is a one-line change.
///
/// `$body!($self, $popup)` is invoked once per popup field. Callers define
/// an inner `macro_rules!` for the per-popup step and pass its name.
macro_rules! for_each_routed_popup {
    ($self:ident, $body:ident) => {
        $body!($self, settings_popup);
        $body!($self, model_popup);
        $body!($self, plugins_popup);
        $body!($self, pairing_popup);
        $body!($self, sessions_popup);
        $body!($self, schedule_popup);
        $body!($self, migrate_popup);
        $body!($self, status_popup);
        $body!($self, usage_popup);
        $body!($self, subprocesses_popup);
        $body!($self, btw_popup);
    };
}
pub(super) use for_each_routed_popup;

/// Trait for popup windows that handle keyboard and paste events.
/// Each popup converts its domain-specific actions into `AppAction` variants internally.
///
/// Dispatch is currently macro-driven on concrete types (see
/// `for_each_routed_popup!`), so the trait is not invoked through `dyn`; it
/// documents the shared contract and makes accidental drift at the signature
/// level a compile error.
pub(crate) trait PopupHandler {
    #[allow(dead_code)]
    fn is_visible(&self) -> bool;
    /// Handle a key event. `config` is provided for popups that need to mutate settings.
    /// Returns `Ok(Some(action))` if an action should be dispatched, `Ok(None)` to absorb
    /// the event (caller will map to `Continue`).
    fn handle_key_event(
        &mut self,
        key: crossterm::event::KeyEvent,
        config: &mut Config,
    ) -> Result<Option<AppAction>>;
    /// Handle a paste event. Returns `true` if consumed.
    fn handle_paste_event(&mut self, _text: &str) -> bool {
        false
    }
    /// True when the popup is currently in a free-text-input mode (API key,
    /// cron expression, project name, etc.). Used by the global `/` fall-through
    /// in `dispatch_key_to_popup` so we don't hijack `/` from legitimate input
    /// like cron `*/5 * * * *` or paths/URLs in setting values.
    fn wants_text_input(&self) -> bool {
        false
    }
}

impl<'a> App<'a> {
    /// Returns `true` if any popup overlay is currently visible.
    ///
    /// Used to suppress input routing to the composer and to prevent
    /// `drain_queued_if_idle` from starting a new streaming turn while the
    /// user is interacting with a popup.
    pub fn any_popup_visible(&self) -> bool {
        macro_rules! check {
            ($self:ident, $popup:ident) => {
                if $self.$popup.is_visible() {
                    return true;
                }
            };
        }
        for_each_routed_popup!(self, check);
        false
    }

    /// Dismiss every routed popup. Used when a global key (e.g. `/`) needs to
    /// reach the composer regardless of current popup state.
    pub fn close_all_popups(&mut self) {
        macro_rules! close {
            ($self:ident, $popup:ident) => {
                if $self.$popup.is_visible() {
                    $self.$popup.dismiss();
                }
            };
        }
        for_each_routed_popup!(self, close);
    }

    /// True when any visible popup is currently in a free-text-input mode
    /// (settings value editing, model API key entry, plugin credentials,
    /// project name/description, schedule cron). The global `/` fall-through
    /// must NOT fire while one of these is active — `/` is a valid character
    /// in cron expressions, paths, URLs, and many credential formats.
    pub fn any_popup_wants_text_input(&self) -> bool {
        macro_rules! check {
            ($self:ident, $popup:ident) => {
                if $self.$popup.is_visible() && $self.$popup.wants_text_input() {
                    return true;
                }
            };
        }
        for_each_routed_popup!(self, check);
        false
    }

    /// Whether the TUI currently wants an alt-screen surface.
    ///
    /// True when any of the large popups in the spec's "alt-screen wrap audit"
    /// is visible, or while the full-screen transcript pager is open. The
    /// event loop reconciles this against [`super::super::tui_lifecycle::Tui::is_alt_screen_active`]
    /// after every iteration; smaller popups (`command_popup`, `model_popup`,
    /// `file_popup`, plan overlay, file search) stay inline.
    pub fn wants_alt_screen(&self) -> bool {
        if matches!(self.state, AppState::TranscriptPager) {
            return true;
        }
        self.settings_popup.is_visible()
            || self.pairing_popup.is_visible()
            || self.sessions_popup.is_visible()
            || self.schedule_popup.is_visible()
            || self.migrate_popup.is_visible()
            || self.status_popup.is_visible()
            || self.usage_popup.is_visible()
            || self.subprocesses_popup.is_visible()
            || self.btw_popup.is_visible()
            || self.plugins_popup.is_visible()
    }

    /// Route a paste event to the first visible popup that accepts paste input.
    /// Returns `true` if a popup consumed the paste (caller should return Continue).
    pub(super) fn dispatch_paste_to_popup(&mut self, text: &str) -> bool {
        macro_rules! route {
            ($self:ident, $popup:ident) => {
                if $self.$popup.is_visible() {
                    $self.$popup.handle_paste_event(text);
                    return true;
                }
            };
        }
        for_each_routed_popup!(self, route);
        false
    }

    /// Route a key event to the first visible popup.
    /// Returns `Some(AppAction)` if a popup handled the key, `None` if no popup is visible.
    pub(super) fn dispatch_key_to_popup(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Result<Option<AppAction>> {
        // Slash is the global command-palette entry. When a non-text-input
        // popup is open (e.g. Settings browsing list, Status overview),
        // typing `/stats` would otherwise be silently dropped because popups
        // have modal capture. Dismiss the popup and let `/` fall through to
        // the composer.
        //
        // Skipped when a popup is in a text-input mode (cron `*/5`, API keys,
        // paths, project names) — those need `/` as a literal character.
        if matches!(key.code, crossterm::event::KeyCode::Char('/'))
            && key.modifiers.is_empty()
            && self.any_popup_visible()
            && !self.any_popup_wants_text_input()
        {
            self.close_all_popups();
            return Ok(None);
        }

        // The macro expands to a chain of `if popup.is_visible() { return ... }`
        // blocks — one per popup. Writing it this way avoids the split-borrow
        // problem that blocks iterating an `&mut dyn PopupHandler` array when
        // `handle_key_event` also needs `&mut self.config`.
        macro_rules! route {
            ($self:ident, $popup:ident) => {
                if $self.$popup.is_visible() {
                    let action = $self.$popup.handle_key_event(key, &mut $self.config)?;
                    return Ok(Some(action.unwrap_or(AppAction::Continue)));
                }
            };
        }
        for_each_routed_popup!(self, route);
        Ok(None)
    }
}
