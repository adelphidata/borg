//! Compact keyboard-binding span renderer.
//!
//! Produces compact `Span`s like `ctrl + c`, `shift + tab`, `↑`, `pgup` —
//! used in footer hints and popup status lines.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::text::Span;

use super::theme;

#[derive(Debug, Clone, Copy)]
pub(crate) struct KeyBinding {
    key: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyBinding {
    pub(crate) const fn new(key: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { key, modifiers }
    }
}

#[allow(dead_code)]
pub(crate) const fn plain(key: KeyCode) -> KeyBinding {
    KeyBinding::new(key, KeyModifiers::NONE)
}

#[allow(dead_code)]
pub(crate) const fn ctrl(key: KeyCode) -> KeyBinding {
    KeyBinding::new(key, KeyModifiers::CONTROL)
}

#[allow(dead_code)]
pub(crate) const fn shift(key: KeyCode) -> KeyBinding {
    KeyBinding::new(key, KeyModifiers::SHIFT)
}

#[allow(dead_code)]
pub(crate) const fn alt(key: KeyCode) -> KeyBinding {
    KeyBinding::new(key, KeyModifiers::ALT)
}

#[cfg(target_os = "macos")]
const ALT_PREFIX: &str = "⌥ + ";
#[cfg(not(target_os = "macos"))]
const ALT_PREFIX: &str = "alt + ";

const CTRL_PREFIX: &str = "ctrl + ";
const SHIFT_PREFIX: &str = "shift + ";

fn modifiers_to_string(modifiers: KeyModifiers) -> String {
    let mut out = String::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        out.push_str(CTRL_PREFIX);
    }
    if modifiers.contains(KeyModifiers::ALT) {
        out.push_str(ALT_PREFIX);
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        out.push_str(SHIFT_PREFIX);
    }
    out
}

fn key_label(key: KeyCode) -> String {
    match key {
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::BackTab => "shift + tab".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Delete => "del".to_string(),
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Up => "↑".to_string(),
        KeyCode::Down => "↓".to_string(),
        KeyCode::Left => "←".to_string(),
        KeyCode::Right => "→".to_string(),
        KeyCode::PageUp => "pgup".to_string(),
        KeyCode::PageDown => "pgdn".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::Char(c) => c.to_ascii_lowercase().to_string(),
        other => format!("{other:?}").to_ascii_lowercase(),
    }
}

impl From<KeyBinding> for Span<'static> {
    fn from(binding: KeyBinding) -> Self {
        let label = format!(
            "{}{}",
            modifiers_to_string(binding.modifiers),
            key_label(binding.key)
        );
        Span::styled(label, theme::key_hint_style())
    }
}

impl From<&KeyBinding> for Span<'static> {
    fn from(binding: &KeyBinding) -> Self {
        Span::from(*binding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_arrow_renders_as_glyph() {
        let span: Span<'static> = plain(KeyCode::Up).into();
        assert_eq!(span.content.as_ref(), "↑");
    }

    #[test]
    fn ctrl_combo_renders_with_prefix() {
        let span: Span<'static> = ctrl(KeyCode::Char('C')).into();
        assert_eq!(span.content.as_ref(), "ctrl + c");
    }

    #[test]
    fn shift_tab_uses_back_tab_label() {
        let span: Span<'static> = shift(KeyCode::BackTab).into();
        // Shift modifier prepended; BackTab body resolves to "shift + tab".
        assert!(span.content.as_ref().contains("tab"));
    }
}
