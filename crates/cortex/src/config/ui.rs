//! User-interface presentation settings (TUI theme, etc).
//!
//! Kept narrow on purpose: this is the surface the `/theme` picker writes to,
//! plus anything else that's pure presentation and has no daemon-side effect.

use serde::{Deserialize, Serialize};

fn default_theme() -> String {
    "default".to_string()
}

/// UI presentation settings. Currently just the active TUI theme name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// Active theme name. Resolved against the user themes dir first
    /// (`~/.borg/themes/<name>.toml`), then the builtin registry. Unknown
    /// names fall back to `"default"` with a warning rather than failing
    /// boot — the agent must always start.
    #[serde(default = "default_theme")]
    pub theme: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: default_theme(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::settings::SETTING_REGISTRY;

    #[test]
    fn default_theme_round_trips_through_registry_without_explicit_set() {
        // Regression guard: a fresh install must (a) have `ui.theme =
        // "default"` and (b) round-trip that value through the registry
        // extractor that `/settings` uses to read the current value. If
        // the extractor disagrees with the field default, /settings shows
        // a different value than what's actually persisted.
        let cfg = Config::default();
        assert_eq!(cfg.ui.theme, "default");
        let extractor = SETTING_REGISTRY
            .iter()
            .find(|(k, _)| *k == "ui.theme")
            .map(|(_, f)| f)
            .expect("ui.theme must be in SETTING_REGISTRY");
        assert_eq!(extractor(&cfg), "default");
    }

    #[test]
    fn apply_setting_writes_theme_name() {
        let mut cfg = Config::default();
        cfg.apply_setting("ui.theme", "mono")
            .expect("ui.theme = mono must apply");
        assert_eq!(cfg.ui.theme, "mono");
        cfg.apply_setting("ui.theme", "slate").unwrap();
        assert_eq!(cfg.ui.theme, "slate");
    }

    #[test]
    fn apply_setting_accepts_user_theme_names() {
        // User themes (`~/.borg/themes/<name>.toml`) are unknown to cortex
        // — the TUI's resolver falls back to `default` for truly unknown
        // names. Rejecting unknown names here would block users from
        // setting a custom theme via `borg config set ui.theme my-skin`.
        let mut cfg = Config::default();
        cfg.apply_setting("ui.theme", "my-custom-user-skin")
            .expect("user theme names must be settable");
        assert_eq!(cfg.ui.theme, "my-custom-user-skin");
    }

    #[test]
    fn ui_theme_round_trips_through_settings_registry() {
        // The /settings popup needs the registry extractor to mirror what
        // apply_setting just wrote. If this fails the popup will show a
        // stale value after a write.
        let mut cfg = Config::default();
        cfg.apply_setting("ui.theme", "slate").unwrap();
        let extractor = SETTING_REGISTRY
            .iter()
            .find(|(k, _)| *k == "ui.theme")
            .map(|(_, f)| f)
            .expect("ui.theme must be in SETTING_REGISTRY");
        assert_eq!(extractor(&cfg), "slate");
    }

    #[test]
    fn missing_ui_section_in_serde_falls_back_to_default() {
        // Older configs serialized before the ui section existed must
        // still deserialize cleanly — drop-in upgrade story.
        let toml_str = r#"[llm]
model = "claude-sonnet-4""#;
        let cfg: Config = toml::from_str(toml_str).expect("must parse legacy config");
        assert_eq!(cfg.ui.theme, "default");
    }
}
