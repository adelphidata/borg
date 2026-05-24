//! Theme system for the TUI.
//!
//! A single `Palette` + `Branding` + `SpinnerStyle` + `BannerArt` bundle
//! owns every user-tunable visual choice. `theme.rs` reads `active()` at
//! render time so swaps via `/theme` are live without a restart.
//!
//! Layout:
//! - `palette` — the flat semantic-color struct (one field per token that
//!   used to be a hardcoded constant in `theme.rs`).
//! - `builtins` — ships `default`, `mono`, `slate`, `daylight` as `static`
//!   `Theme`s. `default` reproduces today's exact look (regression-guarded
//!   by `builtins::tests::default_matches_legacy_palette`).
//! - `loader` — parses `~/.borg/themes/<name>.toml` user overrides. Invalid
//!   hex / unknown keys log + skip; never breaks boot.
//! - `active` — `OnceLock<RwLock<Arc<Theme>>>` hot-swap. `set_active` is the
//!   only writer; readers go through `active()`.

pub mod active;
pub mod builtins;
pub mod loader;
pub mod palette;

pub use active::{active, set_active};
pub use builtins::{builtin, builtin_names};
pub use loader::{list_user_themes, user_themes_dir};
pub use palette::{Theme, ThemeSource};

use std::path::Path;
use std::sync::Arc;

/// Resolve a theme name to a concrete `Arc<Theme>`. Lookup order:
/// 1. User theme file at `<dir>/<name>.toml`
/// 2. Builtin registry
/// 3. Builtin `default` (with a `tracing::warn!` for unknown names)
///
/// Never returns Err — the agent must always start, even with a stale or
/// fat-fingered `ui.theme` value in the DB.
pub fn resolve(name: &str, user_dir: &Path) -> Arc<Theme> {
    match loader::load_user_theme(user_dir, name) {
        Ok(theme) => return Arc::new(theme),
        Err(loader::LoadError::NotFound) => {}
        Err(err) => {
            tracing::warn!(
                target: "borg::tui::themes",
                theme = name,
                error = %err,
                "user theme failed to load; falling back to builtin lookup",
            );
        }
    }

    if let Some(theme) = builtins::builtin(name) {
        return Arc::new(theme.clone());
    }

    if name != "default" {
        tracing::warn!(
            target: "borg::tui::themes",
            theme = name,
            "unknown theme name; falling back to 'default'",
        );
    }
    Arc::new(builtins::default_theme_static().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn resolve_unknown_falls_back_to_default_without_panicking() {
        let dir = TempDir::new().unwrap();
        let theme = resolve("nonexistent-theme-xyz", dir.path());
        assert_eq!(theme.name, "default");
    }

    #[test]
    fn resolve_builtin_returns_that_builtin() {
        let dir = TempDir::new().unwrap();
        let theme = resolve("mono", dir.path());
        assert_eq!(theme.name, "mono");
    }

    #[test]
    fn resolve_default_returns_default_when_no_user_dir() {
        let dir = TempDir::new().unwrap();
        let theme = resolve("default", dir.path());
        assert_eq!(theme.name, "default");
    }

    #[test]
    fn resolve_user_file_overrides_builtin() {
        let dir = TempDir::new().unwrap();
        // Drop a "mono" override that gives a distinctive accent.
        std::fs::write(
            dir.path().join("mono.toml"),
            r##"
name = "mono"
description = "user override"
[palette]
accent = "#FF00FF"
"##,
        )
        .unwrap();
        let theme = resolve("mono", dir.path());
        assert_eq!(theme.name, "mono");
        assert_eq!(theme.description, "user override");
        // The accent should match the override hex, NOT the builtin mono accent.
        assert_eq!(
            theme.palette.accent,
            ratatui::style::Color::Rgb(0xFF, 0x00, 0xFF)
        );
    }
}
