//! User theme loader: parses `<dir>/<name>.toml` and merges over the builtin
//! `default` so partial overrides keep unspecified fields at the default.
//!
//! Failure policy: invalid hex / unknown keys log a warning and inherit
//! from default for that field. A missing file returns `LoadError::NotFound`
//! so the resolver can fall through to the builtin lookup. Any other
//! filesystem / parse error becomes `LoadError::Invalid`. None of these
//! states ever break boot.

use ratatui::style::Color;
use serde::Deserialize;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

use super::builtins;
use super::palette::{BannerArt, Branding, Palette, SpinnerStyle, Theme, ThemeSource};

/// Result of attempting to load a user-theme file. The resolver in
/// `themes::mod.rs` translates these into a falling-back lookup chain.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("user theme file not found")]
    NotFound,
    #[error("invalid user theme file: {0}")]
    Invalid(String),
}

/// Metadata for the picker's user-theme rows.
#[derive(Debug, Clone)]
pub struct UserThemeMeta {
    pub name: String,
    pub description: String,
}

/// Convention path: `~/.borg/themes/`. Wrapper exists so callers don't
/// repeat the `join("themes")` everywhere — and so tests can substitute a
/// TempDir without monkey-patching the data dir.
pub fn user_themes_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("themes")
}

fn user_theme_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.toml"))
}

/// Load and parse a user theme. Returns `LoadError::NotFound` if the file is
/// absent (let the caller fall back); `LoadError::Invalid` for parse/IO
/// failures with a human message.
pub fn load_user_theme(dir: &Path, name: &str) -> Result<Theme, LoadError> {
    let path = user_theme_path(dir, name);
    if !path.is_file() {
        return Err(LoadError::NotFound);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| LoadError::Invalid(format!("read {}: {e}", path.display())))?;
    let raw: RawTheme = toml::from_str(&text)
        .map_err(|e| LoadError::Invalid(format!("parse {}: {e}", path.display())))?;
    Ok(raw.into_theme(name))
}

/// Enumerate user themes in a directory. Returns sorted (by name), with
/// shadowing rules: a user file that re-uses a builtin name is included
/// (it overrides). Files that fail to parse are skipped with a `warn!`.
pub fn list_user_themes(dir: &Path) -> Vec<UserThemeMeta> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| toml::from_str::<RawTheme>(&text).map_err(|e| e.to_string()))
        {
            Ok(raw) => {
                let description = raw.description.unwrap_or_default();
                out.push(UserThemeMeta {
                    name: stem,
                    description,
                });
            }
            Err(err) => tracing::warn!(
                target: "borg::tui::themes",
                path = %path.display(),
                error = %err,
                "skipping unparseable user theme file",
            ),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

// ---------------------------------------------------------------------------
// TOML deserialization helpers.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct RawTheme {
    /// Accepted on input but intentionally ignored — see `into_theme`
    /// for the file-stem-wins rationale.
    #[serde(default, rename = "name")]
    _name: Option<String>,
    description: Option<String>,
    palette: Option<RawPalette>,
    branding: Option<RawBranding>,
    spinner: Option<RawSpinner>,
    /// Inline string for ratatui-styled ASCII banner.
    banner: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawPalette {
    accent: Option<String>,
    tool_accent: Option<String>,
    success: Option<String>,
    error: Option<String>,
    warning: Option<String>,
    border: Option<String>,
    dim: Option<String>,
    active: Option<String>,
    popup_selected_bg: Option<String>,
    popup_selected_fg: Option<String>,
    shell_prompt: Option<String>,
    diff_add_bg_dark: Option<String>,
    diff_add_bg_light: Option<String>,
    diff_remove_bg_dark: Option<String>,
    diff_remove_bg_light: Option<String>,
    diff_add_gutter_bg_light: Option<String>,
    diff_remove_gutter_bg_light: Option<String>,
}

/// Mirrors the (intentionally small) `Branding` struct. Unknown TOML keys
/// in a `[branding]` table are silently dropped by serde with `#[serde(default)]`
/// — that's fine, but it means adding a field here without adding a real
/// consumer would silently accept user input that does nothing. Don't.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawBranding {
    goodbye: Option<String>,
    help_header: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawSpinner {
    /// "shimmer" (the only currently-implemented kind). Defaults to
    /// shimmer when omitted; `kind = "faces"` is rejected by `merge_spinner`
    /// with a `warn!` rather than silently disabling the spinner.
    kind: Option<String>,
    base: Option<String>,
    highlight: Option<String>,
}

impl RawTheme {
    /// `name_from_file` is the file stem (e.g. `mono` for `mono.toml`). It
    /// is *always* the authoritative name — the in-file `name` field is
    /// ignored on purpose so `list_user_themes` (which reads the stem) and
    /// `load_user_theme` (which reads the file) can never disagree about
    /// what a theme is called. The TOML still accepts a `name` key so
    /// foreign theme files round-trip cleanly, but it's a no-op here.
    fn into_theme(self, name_from_file: &str) -> Theme {
        // Inherit from builtin "default" for every unset field so a partial
        // override only changes the fields the operator actually set.
        let base = builtins::default_theme_static().clone();

        let theme_name = name_from_file.to_string();
        let description = self
            .description
            .unwrap_or_else(|| base.description.to_string());

        let palette = merge_palette(self.palette.unwrap_or_default(), &base.palette, &theme_name);
        let branding = merge_branding(self.branding.unwrap_or_default(), &base.branding);
        let spinner = merge_spinner(self.spinner, &base.spinner, &theme_name);
        let banner: BannerArt = self.banner.map(Cow::Owned);

        Theme {
            name: Cow::Owned(theme_name),
            description: Cow::Owned(description),
            source: ThemeSource::User,
            palette,
            branding,
            spinner,
            banner: banner.or(base.banner),
        }
    }
}

/// Parse `#RRGGBB` (case-insensitive, optional leading `#`). Three-digit
/// shorthand (`#abc`) is intentionally NOT supported — explicit-only keeps
/// the parser tiny and avoids surprises.
fn parse_hex(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

fn pick_color(field: &str, raw: Option<String>, fallback: Color, theme_name: &str) -> Color {
    match raw {
        None => fallback,
        Some(s) => match parse_hex(&s) {
            Some(c) => c,
            None => {
                tracing::warn!(
                    target: "borg::tui::themes",
                    theme = theme_name,
                    field,
                    value = %s,
                    "invalid hex color in user theme; using fallback",
                );
                fallback
            }
        },
    }
}

fn merge_palette(raw: RawPalette, base: &Palette, theme_name: &str) -> Palette {
    Palette {
        accent: pick_color("accent", raw.accent, base.accent, theme_name),
        tool_accent: pick_color("tool_accent", raw.tool_accent, base.tool_accent, theme_name),
        success: pick_color("success", raw.success, base.success, theme_name),
        error: pick_color("error", raw.error, base.error, theme_name),
        warning: pick_color("warning", raw.warning, base.warning, theme_name),
        border: pick_color("border", raw.border, base.border, theme_name),
        dim: pick_color("dim", raw.dim, base.dim, theme_name),
        active: pick_color("active", raw.active, base.active, theme_name),
        popup_selected_bg: pick_color(
            "popup_selected_bg",
            raw.popup_selected_bg,
            base.popup_selected_bg,
            theme_name,
        ),
        popup_selected_fg: pick_color(
            "popup_selected_fg",
            raw.popup_selected_fg,
            base.popup_selected_fg,
            theme_name,
        ),
        shell_prompt: pick_color(
            "shell_prompt",
            raw.shell_prompt,
            base.shell_prompt,
            theme_name,
        ),
        diff_add_bg_dark: pick_color(
            "diff_add_bg_dark",
            raw.diff_add_bg_dark,
            base.diff_add_bg_dark,
            theme_name,
        ),
        diff_add_bg_light: pick_color(
            "diff_add_bg_light",
            raw.diff_add_bg_light,
            base.diff_add_bg_light,
            theme_name,
        ),
        diff_remove_bg_dark: pick_color(
            "diff_remove_bg_dark",
            raw.diff_remove_bg_dark,
            base.diff_remove_bg_dark,
            theme_name,
        ),
        diff_remove_bg_light: pick_color(
            "diff_remove_bg_light",
            raw.diff_remove_bg_light,
            base.diff_remove_bg_light,
            theme_name,
        ),
        diff_add_gutter_bg_light: pick_color(
            "diff_add_gutter_bg_light",
            raw.diff_add_gutter_bg_light,
            base.diff_add_gutter_bg_light,
            theme_name,
        ),
        diff_remove_gutter_bg_light: pick_color(
            "diff_remove_gutter_bg_light",
            raw.diff_remove_gutter_bg_light,
            base.diff_remove_gutter_bg_light,
            theme_name,
        ),
    }
}

fn merge_branding(raw: RawBranding, base: &Branding) -> Branding {
    Branding {
        goodbye: raw
            .goodbye
            .map(Cow::Owned)
            .unwrap_or_else(|| base.goodbye.clone()),
        help_header: raw
            .help_header
            .map(Cow::Owned)
            .unwrap_or_else(|| base.help_header.clone()),
    }
}

fn merge_spinner(raw: Option<RawSpinner>, base: &SpinnerStyle, theme_name: &str) -> SpinnerStyle {
    let Some(raw) = raw else {
        return base.clone();
    };
    let kind = raw
        .kind
        .as_deref()
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "shimmer".to_string());
    match kind.as_str() {
        "shimmer" => {
            let SpinnerStyle::Shimmer {
                base: base_color,
                highlight,
            } = base;
            SpinnerStyle::Shimmer {
                base: pick_color("spinner.base", raw.base, *base_color, theme_name),
                highlight: pick_color("spinner.highlight", raw.highlight, *highlight, theme_name),
            }
        }
        "faces" => {
            // Faces are defined in the type but borg's spinner is `shimmer.rs`
            // — a color sweep, not a frame ticker. Accepting `kind = "faces"`
            // here would silently disable spinner animation. Reject explicitly
            // so users get a clear signal in the log and fall back to the
            // inherited shimmer rather than getting a frozen spinner.
            tracing::warn!(
                target: "borg::tui::themes",
                theme = theme_name,
                "spinner.kind = 'faces' is parsed but not yet rendered; falling back to inherited shimmer",
            );
            base.clone()
        }
        other => {
            tracing::warn!(
                target: "borg::tui::themes",
                theme = theme_name,
                kind = other,
                "unknown spinner kind; using default",
            );
            base.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_theme(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(format!("{name}.toml")), body).unwrap();
    }

    // -- parse_hex --

    #[test]
    fn parse_hex_accepts_with_and_without_hash() {
        assert_eq!(parse_hex("#FF0080"), Some(Color::Rgb(0xFF, 0x00, 0x80)));
        assert_eq!(parse_hex("ff0080"), Some(Color::Rgb(0xFF, 0x00, 0x80)));
    }

    #[test]
    fn parse_hex_rejects_invalid() {
        assert_eq!(parse_hex("#GGGGGG"), None);
        assert_eq!(parse_hex("#FF"), None); // wrong length, no shorthand support
        assert_eq!(parse_hex(""), None);
        assert_eq!(parse_hex("notahex"), None);
    }

    // -- load_user_theme --

    #[test]
    fn missing_file_returns_not_found() {
        let dir = TempDir::new().unwrap();
        match load_user_theme(dir.path(), "nope") {
            Err(LoadError::NotFound) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn valid_override_replaces_field_and_inherits_rest() {
        let dir = TempDir::new().unwrap();
        write_theme(
            dir.path(),
            "custom",
            r##"
description = "user override"
[palette]
accent = "#123456"
"##,
        );
        let theme = load_user_theme(dir.path(), "custom").unwrap();
        // overridden:
        assert_eq!(theme.palette.accent, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(theme.description, "user override");
        // inherited from default:
        assert_eq!(theme.palette.warning, Color::Rgb(255, 191, 0));
        assert_eq!(theme.source, ThemeSource::User);
    }

    #[test]
    fn invalid_hex_falls_back_to_default_without_poisoning_other_fields() {
        let dir = TempDir::new().unwrap();
        write_theme(
            dir.path(),
            "bad-hex",
            r##"
[palette]
accent = "#ZZZZZZ"
warning = "#112233"
"##,
        );
        let theme = load_user_theme(dir.path(), "bad-hex").unwrap();
        // accent fell back to default's accent (legacy CYAN)
        assert_eq!(theme.palette.accent, Color::Rgb(0, 185, 174));
        // warning was valid and applied
        assert_eq!(theme.palette.warning, Color::Rgb(0x11, 0x22, 0x33));
    }

    #[test]
    fn in_file_name_field_is_ignored_in_favor_of_file_stem() {
        // Regression guard: a theme TOML with an in-file `name` key must
        // NOT make `Theme.name` disagree with the filename stem, because
        // `list_user_themes` keys on the stem. If they could diverge, the
        // picker would list a name the resolver couldn't find.
        let dir = TempDir::new().unwrap();
        write_theme(
            dir.path(),
            "stem-wins",
            r##"
name = "in-file-name"
description = "test"
"##,
        );
        let theme = load_user_theme(dir.path(), "stem-wins").unwrap();
        assert_eq!(theme.name, "stem-wins");
    }

    #[test]
    fn malformed_toml_returns_invalid() {
        let dir = TempDir::new().unwrap();
        write_theme(dir.path(), "broken", "this is = not [valid toml");
        match load_user_theme(dir.path(), "broken") {
            Err(LoadError::Invalid(_)) => {}
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn branding_overrides_apply_and_inherit() {
        let dir = TempDir::new().unwrap();
        write_theme(
            dir.path(),
            "branded",
            r#"
[branding]
goodbye = "see ya"
"#,
        );
        let theme = load_user_theme(dir.path(), "branded").unwrap();
        assert_eq!(theme.branding.goodbye, "see ya");
        // help_header inherited from default
        assert_eq!(theme.branding.help_header, "Commands");
    }

    // -- list_user_themes --

    #[test]
    fn list_user_themes_skips_non_toml_and_sorts() {
        let dir = TempDir::new().unwrap();
        write_theme(dir.path(), "zebra", "name = \"zebra\"");
        write_theme(dir.path(), "alpha", "name = \"alpha\"");
        std::fs::write(dir.path().join("notes.txt"), "ignore me").unwrap();
        let metas = list_user_themes(dir.path());
        let names: Vec<_> = metas.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zebra"]);
    }

    #[test]
    fn list_user_themes_skips_unparseable_files() {
        let dir = TempDir::new().unwrap();
        write_theme(dir.path(), "good", r#"description = "ok""#);
        write_theme(dir.path(), "broken", "this is = not [valid toml");
        let metas = list_user_themes(dir.path());
        let names: Vec<_> = metas.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["good"]);
    }

    #[test]
    fn list_user_themes_missing_dir_returns_empty() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(list_user_themes(&missing).is_empty());
    }
}
