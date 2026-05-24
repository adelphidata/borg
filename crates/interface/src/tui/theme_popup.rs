//! `/theme` popup — pick the TUI theme and persist the choice.
//!
//! Live preview is the killer feature here: every cursor move applies the
//! highlighted theme to `themes::active()` immediately, so the operator
//! sees the new look without having to commit. Esc reverts to whatever was
//! active when the popup opened; Enter commits via `AppAction::UpdateSetting`
//! and the central handler in `tui/mod.rs` persists to the DB.
//!
//! Builds entries from `themes::builtin_names()` ∪ user files in
//! `~/.borg/themes/` (resolved via `themes::user_themes_dir`).

use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use cortex::config::Config;

use super::app::{AppAction, PopupHandler};
use super::custom_terminal::Frame;
use super::popup_utils;
use super::theme;
use super::themes::{self, ThemeSource};

#[derive(Debug, Clone)]
struct ThemeEntry {
    name: String,
    description: String,
    source: ThemeSource,
}

pub struct ThemePopup {
    visible: bool,
    entries: Vec<ThemeEntry>,
    cursor: usize,
    /// Theme name that was active when `show` was called. Esc rolls back to
    /// this so a live-preview scroll doesn't permanently change the look
    /// when the user backs out.
    original_name: String,
    /// User themes dir (`~/.borg/themes/`). Resolved once at `show` so the
    /// popup's view of disk doesn't shift mid-scroll if the user is
    /// dropping files in concurrently.
    user_dir: PathBuf,
}

impl ThemePopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            entries: Vec::new(),
            cursor: 0,
            original_name: "default".to_string(),
            user_dir: PathBuf::new(),
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Open the picker. Seeds the cursor onto the row matching the current
    /// `ui.theme` value, then snapshots that name into `original_name` so
    /// Esc can revert. The popup persists via `AppAction::UpdateSetting` and
    /// the central handler in `tui/mod.rs` does the DB write, so we don't
    /// need a `DaemonClient` here.
    pub fn show(&mut self, config: &Config) {
        self.visible = true;

        // Resolve user themes dir from the daemon's data dir. Failing to
        // resolve is non-fatal — we just won't list user themes. Logged
        // because silent data-dir errors mask configuration problems
        // (the same warn fires at startup wire-up, so this catches
        // post-startup degradations like a deleted $HOME).
        self.user_dir = match Config::data_dir() {
            Ok(dir) => themes::user_themes_dir(&dir),
            Err(e) => {
                tracing::warn!(
                    target: "borg::tui::themes",
                    error = %e,
                    "could not resolve borg data dir; user themes won't appear in /theme picker",
                );
                PathBuf::new()
            }
        };

        self.entries = collect_entries(&self.user_dir);
        self.original_name = config.ui.theme.clone();
        self.cursor = self
            .entries
            .iter()
            .position(|e| e.name == self.original_name)
            .unwrap_or(0);

        // Apply the current selection to the active cell. In the common
        // case this is a no-op (config.ui.theme == themes::active().name)
        // but if they ever diverge, the popup should reflect what's
        // selected, not what was last persisted.
        self.apply_cursor_preview();
    }

    fn close(&mut self) {
        self.visible = false;
        self.entries.clear();
        self.cursor = 0;
    }

    /// Forced close from outside (e.g. `App::close_all_popups`). Reverts
    /// the live-preview theme so the user doesn't get stuck with a half-
    /// applied selection when another command yanks the popup away.
    pub fn dismiss(&mut self) {
        if self.visible {
            self.revert_preview();
        }
        self.close();
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len() as isize;
        let mut new_cursor = (self.cursor as isize + delta) % len;
        if new_cursor < 0 {
            new_cursor += len;
        }
        self.cursor = new_cursor as usize;
        self.apply_cursor_preview();
    }

    fn apply_cursor_preview(&self) {
        if let Some(entry) = self.entries.get(self.cursor) {
            let resolved = themes::resolve(&entry.name, &self.user_dir);
            themes::set_active(resolved);
        }
    }

    /// Restore the theme that was active when `show` was called. Called by
    /// Esc and by error paths.
    fn revert_preview(&self) {
        let resolved = themes::resolve(&self.original_name, &self.user_dir);
        themes::set_active(resolved);
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<Option<AppAction>> {
        match key.code {
            KeyCode::Esc => {
                self.revert_preview();
                self.close();
                Ok(Some(AppAction::Continue))
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_cursor(-1);
                Ok(Some(AppAction::Continue))
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_cursor(1);
                Ok(Some(AppAction::Continue))
            }
            KeyCode::PageUp => {
                self.move_cursor(-5);
                Ok(Some(AppAction::Continue))
            }
            KeyCode::PageDown => {
                self.move_cursor(5);
                Ok(Some(AppAction::Continue))
            }
            KeyCode::Home => {
                self.cursor = 0;
                self.apply_cursor_preview();
                Ok(Some(AppAction::Continue))
            }
            KeyCode::End => {
                if !self.entries.is_empty() {
                    self.cursor = self.entries.len() - 1;
                    self.apply_cursor_preview();
                }
                Ok(Some(AppAction::Continue))
            }
            KeyCode::Enter => {
                let Some(entry) = self.entries.get(self.cursor).cloned() else {
                    return Ok(Some(AppAction::Continue));
                };
                self.close();
                Ok(Some(AppAction::UpdateSetting {
                    key: "ui.theme".to_string(),
                    value: entry.name,
                }))
            }
            _ => Ok(Some(AppAction::Continue)),
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        let Some((inner, content_height)) =
            popup_utils::begin_popup_render(frame, self.visible, "Themes", 5, 2)
        else {
            return;
        };

        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(Span::styled(
            " Pick a theme — live preview, Enter to keep, Esc to revert",
            theme::dim(),
        )));
        lines.push(Line::from(""));

        for (i, entry) in self.entries.iter().enumerate() {
            let is_cursor = i == self.cursor;
            let marker = if is_cursor { "▶ " } else { "  " };
            let star = if entry.name == self.original_name {
                "★ "
            } else {
                "  "
            };
            let tag = match entry.source {
                ThemeSource::Builtin => "[builtin]",
                ThemeSource::User => "[user]   ",
            };
            let label = format!(
                "{marker}{star}{name:<16} {tag}  {desc}",
                name = entry.name,
                desc = entry.description,
            );
            let style = if is_cursor {
                theme::popup_selected()
            } else {
                theme::dim()
            };
            lines.push(Line::from(Span::styled(label, style)));
        }

        // Keep the cursor row visible by skipping rows above the viewport.
        let scroll_offset = if self.cursor + 2 >= content_height {
            self.cursor + 3 - content_height
        } else {
            0
        };
        let visible: Vec<Line<'static>> = lines.into_iter().skip(scroll_offset).collect();
        let area = Rect::new(inner.x, inner.y, inner.width, content_height as u16);
        frame.render_widget(Paragraph::new(visible), area);
        popup_utils::render_footer(
            frame,
            inner,
            " ↑↓: preview  Enter: keep  Esc: revert & close",
        );
    }
}

impl Default for ThemePopup {
    fn default() -> Self {
        Self::new()
    }
}

impl PopupHandler for ThemePopup {
    fn is_visible(&self) -> bool {
        self.visible
    }

    fn handle_key_event(
        &mut self,
        key: KeyEvent,
        _config: &mut Config,
    ) -> Result<Option<AppAction>> {
        self.handle_key(key)
    }
}

/// Build the picker rows. Builtins first (in `builtin_names` order), then
/// user themes alphabetically. A user file that shares a builtin's name
/// shadows the builtin's row — `themes::resolve` will pick the user file
/// at apply time, so the picker shows the user description.
fn collect_entries(user_dir: &std::path::Path) -> Vec<ThemeEntry> {
    let user_metas = themes::list_user_themes(user_dir);

    let mut entries = Vec::new();
    for name in themes::builtin_names() {
        // If a user file shadows this builtin, prefer the user row (it's
        // what `resolve` will actually load).
        if let Some(meta) = user_metas.iter().find(|m| m.name == name) {
            entries.push(ThemeEntry {
                name: meta.name.clone(),
                description: if meta.description.is_empty() {
                    "(user override)".to_string()
                } else {
                    meta.description.clone()
                },
                source: ThemeSource::User,
            });
        } else if let Some(t) = themes::builtin(name) {
            entries.push(ThemeEntry {
                name: t.name.to_string(),
                description: t.description.to_string(),
                source: ThemeSource::Builtin,
            });
        }
    }
    // User-only themes (no builtin shadow).
    let builtin_names: std::collections::HashSet<&str> =
        themes::builtin_names().into_iter().collect();
    for meta in user_metas {
        if !builtin_names.contains(meta.name.as_str()) {
            entries.push(ThemeEntry {
                name: meta.name,
                description: meta.description,
                source: ThemeSource::User,
            });
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Snapshot+restore active theme around a test AND hold the shared
    /// `themes::active::TEST_LOCK` so sibling tests in `tui::theme` /
    /// `tui::themes::active` can't race on the same process-global cell
    /// (cargo test parallelizes within a binary).
    fn with_clean_theme<F: FnOnce()>(body: F) {
        let _guard = super::themes::active::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = themes::active();
        body();
        themes::set_active(before);
    }

    fn popup_with_entries(entries: Vec<ThemeEntry>, current: &str) -> ThemePopup {
        ThemePopup {
            visible: true,
            entries,
            cursor: 0,
            original_name: current.to_string(),
            user_dir: PathBuf::new(),
        }
    }

    fn builtin_entry(name: &str) -> ThemeEntry {
        let t = themes::builtin(name).expect("builtin must exist");
        ThemeEntry {
            name: t.name.to_string(),
            description: t.description.to_string(),
            source: ThemeSource::Builtin,
        }
    }

    #[test]
    fn show_seeds_cursor_on_current_theme() {
        // Exercise the real `show` path against a real Config so a future
        // refactor that breaks cursor-seeding fails this test. Earlier
        // versions of this test re-implemented the position() seek in the
        // test body and asserted on the re-implementation, which only
        // caught typos in the test — not behavior regressions.
        with_clean_theme(|| {
            let mut cfg = Config::default();
            cfg.ui.theme = "mono".to_string();
            let mut p = ThemePopup::new();
            p.show(&cfg);
            assert!(p.is_visible());
            let cursor_name = p
                .entries
                .get(p.cursor)
                .map(|e| e.name.as_str())
                .expect("cursor must point at an entry");
            assert_eq!(
                cursor_name, "mono",
                "show() must seed the cursor onto the row matching config.ui.theme"
            );
        });
    }

    #[test]
    fn down_arrow_live_previews_next_theme() {
        with_clean_theme(|| {
            let mut p = popup_with_entries(
                vec![
                    builtin_entry("default"),
                    builtin_entry("mono"),
                    builtin_entry("slate"),
                ],
                "default",
            );
            p.move_cursor(1); // → mono
            assert_eq!(themes::active().name, "mono");
            p.move_cursor(1); // → slate
            assert_eq!(themes::active().name, "slate");
        });
    }

    #[test]
    fn enter_returns_update_setting_action() {
        with_clean_theme(|| {
            let mut p = popup_with_entries(
                vec![builtin_entry("default"), builtin_entry("slate")],
                "default",
            );
            p.move_cursor(1);
            let action = p.handle_key(key(KeyCode::Enter)).unwrap().unwrap();
            match action {
                AppAction::UpdateSetting { key, value } => {
                    assert_eq!(key, "ui.theme");
                    assert_eq!(value, "slate");
                }
                _ => panic!("expected UpdateSetting, got other action"),
            }
        });
    }

    #[test]
    fn esc_reverts_live_preview_to_original_theme() {
        with_clean_theme(|| {
            // Start with default active.
            themes::set_active(Arc::new(themes::builtin("default").unwrap().clone()));
            let mut p = popup_with_entries(
                vec![
                    builtin_entry("default"),
                    builtin_entry("mono"),
                    builtin_entry("slate"),
                ],
                "default",
            );
            p.move_cursor(2); // preview slate
            assert_eq!(
                themes::active().name,
                "slate",
                "precondition: preview should be active"
            );
            let _ = p.handle_key(key(KeyCode::Esc)).unwrap();
            assert_eq!(
                themes::active().name,
                "default",
                "Esc must revert active theme to the original",
            );
        });
    }

    #[test]
    fn up_down_wrap_around_list() {
        with_clean_theme(|| {
            let mut p = popup_with_entries(
                vec![builtin_entry("default"), builtin_entry("mono")],
                "default",
            );
            assert_eq!(p.cursor, 0);
            p.move_cursor(-1);
            assert_eq!(p.cursor, 1, "Up at top wraps to bottom");
            p.move_cursor(1);
            assert_eq!(p.cursor, 0, "Down at bottom wraps to top");
        });
    }

    #[test]
    fn collect_entries_includes_all_builtins_when_no_user_dir() {
        let dir = TempDir::new().unwrap();
        let entries = collect_entries(dir.path());
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        for builtin in ["default", "mono", "slate", "daylight"] {
            assert!(
                names.contains(&builtin),
                "missing builtin '{builtin}' in picker entries: {names:?}"
            );
        }
        for e in entries {
            assert_eq!(e.source, ThemeSource::Builtin);
        }
    }

    #[test]
    fn collect_entries_marks_user_override_as_user_source() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("mono.toml"),
            r##"description = "user override"
"##,
        )
        .unwrap();
        let entries = collect_entries(dir.path());
        let mono = entries
            .iter()
            .find(|e| e.name == "mono")
            .expect("mono must appear");
        assert_eq!(mono.source, ThemeSource::User);
        assert_eq!(mono.description, "user override");
    }

    #[test]
    fn collect_entries_appends_user_only_themes_after_builtins() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("custom.toml"),
            r##"description = "my custom"
"##,
        )
        .unwrap();
        let entries = collect_entries(dir.path());
        let custom = entries
            .iter()
            .find(|e| e.name == "custom")
            .expect("user-only theme must appear");
        assert_eq!(custom.source, ThemeSource::User);
        // Builtins come first.
        let first_user_idx = entries.iter().position(|e| e.source == ThemeSource::User);
        let last_builtin_idx = entries
            .iter()
            .rposition(|e| e.source == ThemeSource::Builtin);
        if let (Some(u), Some(b)) = (first_user_idx, last_builtin_idx) {
            assert!(u > b, "user-only entries must follow all builtins");
        }
    }
}
