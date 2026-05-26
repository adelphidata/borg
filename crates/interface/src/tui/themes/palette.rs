//! Theme data types. A `Theme` is a flat bundle of `Palette` (colors),
//! `Branding` (user-facing strings), `SpinnerStyle` (loading animation), and
//! optional `BannerArt`. Every render-time read in `theme.rs` resolves
//! against the active `Theme`.
//!
//! Field set on `Palette` is intentionally limited to semantic tokens that
//! existed as hardcoded constants in the previous `theme.rs`. Adding a new
//! token is a deliberate act — every builtin must then supply a value for
//! it, and `default` must keep the legacy color for regression-guard
//! reasons.

use ratatui::style::Color;
use std::borrow::Cow;

/// Where a theme came from. Drives the picker's `[builtin]` / `[user]` tag
/// and lets the popup describe whether a row was shipped in-binary or
/// dropped into `~/.borg/themes/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeSource {
    /// One of the themes compiled into the borg binary.
    Builtin,
    /// Parsed from a user TOML file at `~/.borg/themes/<name>.toml`.
    User,
}

/// Semantic color palette. Every token maps to one (or more) of the previous
/// hardcoded `Color` constants in `theme.rs`. Field names are descriptive of
/// the *role*, not the legacy color name (so a "mono" theme can map `accent`
/// to a gray without the field being called `cyan`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    /// Primary accent (legacy `CYAN`). Used for code spans, icons, file
    /// mentions, composer cursor when focused, diff header.
    pub accent: Color,
    /// Tool-call accent (legacy `YELLOW`). Used for `tool_style`.
    pub tool_accent: Color,
    /// Success / OK accent (legacy `GREEN`). Used for `success_style`.
    pub success: Color,
    /// Error accent (legacy `RED`). Used for `error_style`, cross marks,
    /// diff removals.
    pub error: Color,
    /// Warning accent (legacy `AMBER`). Used for `warning_style`, toast
    /// warning border.
    pub warning: Color,
    /// Frame / border color (legacy `BORDER`).
    pub border: Color,
    /// Dim text color (legacy `DIM_WHITE`). Used by `dim()`; the light-
    /// terminal fallback inside `dim()` is independent of theme.
    pub dim: Color,
    /// Brighter "active tool" green (legacy `TOOL_ACTIVE_GREEN`). Used for
    /// active tool bullets, check marks, diff additions, success toast
    /// borders.
    pub active: Color,
    /// Background for the currently-selected row in popups (legacy hardcoded
    /// `Color::Rgb(3, 49, 46)`).
    pub popup_selected_bg: Color,
    /// Foreground for the currently-selected row in popups.
    pub popup_selected_fg: Color,
    /// Shell prompt color for run_shell echoes (legacy magenta
    /// `Color::Rgb(200, 100, 200)`).
    pub shell_prompt: Color,
    /// Background for diff "add" rows on dark terminals.
    pub diff_add_bg_dark: Color,
    /// Background for diff "add" rows on light terminals.
    pub diff_add_bg_light: Color,
    /// Background for diff "remove" rows on dark terminals.
    pub diff_remove_bg_dark: Color,
    /// Background for diff "remove" rows on light terminals.
    pub diff_remove_bg_light: Color,
    /// Saturated gutter bg for diff "add" rows on light terminals.
    pub diff_add_gutter_bg_light: Color,
    /// Saturated gutter bg for diff "remove" rows on light terminals.
    pub diff_remove_gutter_bg_light: Color,
}

/// User-facing identity strings the theme can swap. Intentionally minimal
/// — only fields with real, wired-up use sites live here so a user editing
/// a theme TOML never writes a value that quietly does nothing. Add a
/// field here only when adding the consumer in the same PR.
///
/// Currently wired:
/// - `goodbye` → `goodbye.rs` subtitle on `/quit`
/// - `help_header` → `command_popup.rs` title
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branding {
    /// Sign-off line shown on `/quit` (legacy hardcoded in `goodbye.rs`).
    pub goodbye: Cow<'static, str>,
    /// `/help` / command picker header text.
    pub help_header: Cow<'static, str>,
}

/// Spinner rendering style. The existing `shimmer.rs` is a color-sweep
/// over already-rendered text. Themes can re-color the sweep here; a
/// frame-rotation `faces` style is not yet implemented — the loader
/// rejects `kind = "faces"` with a warn rather than silently mis-rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpinnerStyle {
    /// Color-sweep highlight over text (current borg behavior).
    Shimmer { base: Color, highlight: Color },
}

/// Optional ratatui-styled ASCII banner. `None` means "use the default
/// onboarding logo rendered in `onboarding_tui::render_logo`".
pub type BannerArt = Option<Cow<'static, str>>;

/// A complete theme: name + description + every visual surface the user can
/// customize. Built from a `static` table in `builtins.rs` or parsed from a
/// user TOML in `loader.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: Cow<'static, str>,
    pub description: Cow<'static, str>,
    pub source: ThemeSource,
    pub palette: Palette,
    pub branding: Branding,
    pub spinner: SpinnerStyle,
    pub banner: BannerArt,
}
