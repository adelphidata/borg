use super::custom_terminal::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use anyhow::Result;

use cortex::config::Config;
use plugins::catalog::{PluginDef, CATALOG};
use plugins::Category;

use super::app::{AppAction, PopupHandler};
use super::popup_utils;
use super::theme;

/// The kind of a unified plugin item.
enum ItemKind {
    /// A messaging channel from the catalog (install/uninstall with credentials).
    Channel { def: &'static PluginDef },
    /// A skill (enable/disable toggle).
    Skill,
    /// A signed WASM plugin (toggle disable/enable; quarantined rows are
    /// read-only and only clearable via `borg plugin verify`).
    WasmPlugin {
        /// True if the plugin row is in `'quarantined'` status — toggling
        /// such rows is a no-op and the render shows a `[quarantined]` tag.
        quarantined: bool,
    },
}

/// Section heading for the popup grouping. Wraps `plugins::Category` so the
/// WASM section can sit alongside the catalog categories without polluting a
/// public enum that other crates consume.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Section {
    Catalog(Category),
    Wasm,
}

impl Section {
    fn label(self) -> String {
        match self {
            Self::Catalog(c) => c.to_string(),
            Self::Wasm => "WASM PLUGINS".to_string(),
        }
    }

    fn order(self) -> usize {
        match self {
            Self::Catalog(Category::Channels) => 0,
            Self::Catalog(Category::Core) => 1,
            Self::Catalog(Category::Developer) => 2,
            Self::Catalog(Category::Productivity) => 3,
            Self::Catalog(Category::Utilities) => 4,
            Self::Catalog(Category::Email) => 5,
            Self::Catalog(Category::Payments) => 6,
            // WASM items group at the end — they sit visually downstream of
            // the curated marketplace surface so operators read the trusted
            // built-ins first.
            Self::Wasm => 7,
        }
    }
}

/// State for a single item in the unified plugins list.
struct PluginItem {
    id: String,
    name: String,
    section: Section,
    kind: ItemKind,
    /// Current persisted state (installed/enabled / not-disabled).
    original_enabled: bool,
    /// Current selection state after user interaction.
    toggled: bool,
}

/// Phase of the plugins popup.
#[derive(Clone)]
enum PluginPhase {
    Browsing,
    CredentialInput {
        action_queue: Vec<(String, &'static [plugins::CredentialSpec])>,
        current_def_idx: usize,
        current_cred_idx: usize,
        buffer: String,
        collected: Vec<(String, String)>,
        all_credentials: Vec<(String, Vec<(String, String)>)>,
    },
}

pub struct PluginsPopup {
    visible: bool,
    items: Vec<PluginItem>,
    cursor: usize,
    phase: PluginPhase,
    status_message: Option<(String, bool)>,
}

/// Actions that the plugins popup can request from the event loop.
pub enum PluginAction {
    /// Install a channel plugin with the provided credentials.
    Install {
        id: String,
        credentials: Vec<(String, String)>,
    },
    /// Uninstall a channel plugin.
    Uninstall { id: String },
    /// Enable or disable a skill.
    SetSkillEnabled { name: String, enabled: bool },
    /// Disable or enable an installed WASM plugin. Quarantined rows are not
    /// emitted via this action — the popup filters those at toggle time.
    SetWasmPluginDisabled { plugin_id: String, disabled: bool },
}

impl PluginsPopup {
    pub fn new() -> Self {
        Self {
            visible: false,
            items: Vec::new(),
            cursor: 0,
            phase: PluginPhase::Browsing,
            status_message: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Show the popup, loading both channel plugins and skills.
    pub fn show(&mut self, config: &cortex::config::Config, data_dir: &std::path::Path) {
        self.visible = true;
        self.cursor = 0;
        self.phase = PluginPhase::Browsing;
        self.status_message = None;

        let mut items = Vec::new();

        // Load channel plugins from catalog
        for def in CATALOG {
            let installed = plugins::installer::is_installed(def, data_dir);
            items.push(PluginItem {
                id: def.id.to_string(),
                name: def.name.to_string(),
                section: Section::Catalog(def.category),
                kind: ItemKind::Channel { def },
                original_enabled: installed,
                toggled: installed,
            });
        }

        // Load skills
        let resolved_creds = config.resolve_credentials();
        if let Ok(skills) = cortex::skills::load_all_skills(&resolved_creds, &config.skills) {
            for skill in skills {
                if skill.is_hidden() {
                    continue;
                }
                let cat = match skill.category() {
                    "core" => Category::Core,
                    "developer" => Category::Developer,
                    "email" => Category::Email,
                    "productivity" => Category::Productivity,
                    "channels" => Category::Channels,
                    "payments" => Category::Payments,
                    _ => Category::Utilities,
                };
                let enabled = !skill.disabled;
                items.push(PluginItem {
                    id: skill.manifest.name.clone(),
                    name: title_case(&skill.manifest.name),
                    section: Section::Catalog(cat),
                    kind: ItemKind::Skill,
                    original_enabled: enabled,
                    toggled: enabled,
                });
            }
        }

        // Load WASM plugins — read-only listing of installed packages with
        // toggle support for verified/disabled status. The popup must still
        // render the catalog if the WASM subsystem is down, but log so the
        // operator can correlate an empty WASM section with a real error
        // rather than "nothing installed".
        match cortex::db::Database::open() {
            Ok(db) => match db.list_wasm_plugins() {
                Ok(rows) => {
                    for row in rows {
                        let quarantined = row.status == "quarantined";
                        let enabled_now = row.status == "verified";
                        items.push(PluginItem {
                            id: row.plugin_id.clone(),
                            name: row.plugin_id.clone(),
                            section: Section::Wasm,
                            kind: ItemKind::WasmPlugin { quarantined },
                            original_enabled: enabled_now,
                            toggled: enabled_now,
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!("plugins popup: list_wasm_plugins failed: {e}");
                }
            },
            Err(e) => {
                tracing::warn!("plugins popup: Database::open failed: {e}");
            }
        }

        items.sort_by(|a, b| {
            a.section
                .order()
                .cmp(&b.section.order())
                .then(a.name.cmp(&b.name))
        });

        self.items = items;
    }

    /// Test helpers for inspecting/manipulating popup state.
    #[cfg(test)]
    pub fn items_for_test(&self) -> Vec<(&str, bool)> {
        self.items
            .iter()
            .map(|i| (i.id.as_str(), i.original_enabled))
            .collect()
    }

    #[cfg(test)]
    pub fn set_cursor_for_test(&mut self, idx: usize) {
        self.cursor = idx;
    }

    #[cfg(test)]
    pub fn force_uninstalled_for_test(&mut self, idx: usize) {
        if let Some(item) = self.items.get_mut(idx) {
            item.original_enabled = false;
            item.toggled = false;
        }
    }

    pub fn dismiss(&mut self) {
        self.visible = false;
        self.phase = PluginPhase::Browsing;
        self.status_message = None;
    }

    /// Handle a bracketed paste event. Returns `true` if consumed.
    pub fn handle_paste(&mut self, text: &str) -> bool {
        if !self.visible {
            return false;
        }
        if let PluginPhase::CredentialInput { ref mut buffer, .. } = self.phase {
            buffer.push_str(text);
            return true;
        }
        false
    }

    /// Handle a key event. Returns actions to execute if Enter is pressed.
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Option<Vec<PluginAction>> {
        use crossterm::event::KeyCode;

        if !self.visible {
            return None;
        }

        match &mut self.phase {
            PluginPhase::Browsing => match key.code {
                KeyCode::Esc => {
                    self.dismiss();
                    None
                }
                KeyCode::Up => {
                    if self.items.is_empty() {
                        return None;
                    }
                    if self.cursor == 0 {
                        self.cursor = self.items.len() - 1;
                    } else {
                        self.cursor -= 1;
                    }
                    self.status_message = None;
                    None
                }
                KeyCode::Down => {
                    if self.items.is_empty() {
                        return None;
                    }
                    self.cursor = (self.cursor + 1) % self.items.len();
                    self.status_message = None;
                    None
                }
                KeyCode::Char(' ') => {
                    if let Some(item) = self.items.get_mut(self.cursor) {
                        // Check platform availability for channels
                        if let ItemKind::Channel { def } = &item.kind {
                            if !def.platform.is_available() {
                                self.status_message = Some((
                                    format!(
                                        "{} requires {}",
                                        item.name,
                                        def.platform.label().unwrap_or("a different platform")
                                    ),
                                    false,
                                ));
                                return None;
                            }
                        }
                        // Quarantined WASM plugins are read-only — toggling
                        // would silently emit a no-op action; surface the
                        // CLI escape hatch instead.
                        if let ItemKind::WasmPlugin { quarantined: true } = &item.kind {
                            self.status_message = Some((
                                format!(
                                    "{} is quarantined; run `borg plugin verify {}` after restoring on-disk files",
                                    item.name, item.id
                                ),
                                false,
                            ));
                            return None;
                        }
                        item.toggled = !item.toggled;
                        self.status_message = None;
                    }
                    None
                }
                KeyCode::Enter => {
                    let (skill_actions, channel_actions) = self.compute_pending_actions();

                    if skill_actions.is_empty() && channel_actions.is_empty() {
                        self.status_message = Some(("No changes".to_string(), false));
                        return None;
                    }

                    // Build final actions list
                    let mut all_actions: Vec<PluginAction> = skill_actions;

                    // Process channel actions — separate into needs-creds and immediate
                    let mut needs_creds: Vec<(String, &'static [plugins::CredentialSpec])> =
                        Vec::new();

                    for (id, is_install) in &channel_actions {
                        if *is_install {
                            if let Some(def) = plugins::catalog::find_by_id(id) {
                                let has_required =
                                    def.required_credentials.iter().any(|c| !c.is_optional);
                                if !has_required {
                                    all_actions.push(PluginAction::Install {
                                        id: id.clone(),
                                        credentials: Vec::new(),
                                    });
                                } else {
                                    needs_creds.push((id.clone(), def.required_credentials));
                                }
                            }
                        } else {
                            all_actions.push(PluginAction::Uninstall { id: id.clone() });
                        }
                    }

                    if needs_creds.is_empty() {
                        self.dismiss();
                        return Some(all_actions);
                    }

                    // Transition to credential input phase
                    let mut all_credentials: Vec<(String, Vec<(String, String)>)> = Vec::new();
                    for action in &all_actions {
                        if let PluginAction::Install { id, credentials } = action {
                            all_credentials.push((id.clone(), credentials.clone()));
                        }
                    }

                    self.phase = PluginPhase::CredentialInput {
                        action_queue: needs_creds,
                        current_def_idx: 0,
                        current_cred_idx: 0,
                        buffer: String::new(),
                        collected: Vec::new(),
                        all_credentials,
                    };
                    self.status_message = None;
                    None
                }
                _ => None,
            },
            PluginPhase::CredentialInput {
                ref mut action_queue,
                ref mut current_def_idx,
                ref mut current_cred_idx,
                ref mut buffer,
                ref mut collected,
                ref mut all_credentials,
            } => match key.code {
                KeyCode::Esc => {
                    self.phase = PluginPhase::Browsing;
                    self.status_message = None;
                    None
                }
                KeyCode::Backspace => {
                    buffer.pop();
                    None
                }
                KeyCode::Char(c) => {
                    buffer.push(c);
                    None
                }
                KeyCode::Enter => {
                    if buffer.is_empty() {
                        self.status_message =
                            Some(("Credential value cannot be empty.".to_string(), false));
                        return None;
                    }

                    let cred_specs = action_queue[*current_def_idx].1;
                    let key = cred_specs[*current_cred_idx].key.to_string();
                    collected.push((key, buffer.clone()));
                    buffer.clear();
                    self.status_message = None;

                    *current_cred_idx += 1;

                    if *current_cred_idx >= cred_specs.len() {
                        let id = action_queue[*current_def_idx].0.clone();
                        all_credentials.push((id, collected.clone()));
                        collected.clear();
                        *current_def_idx += 1;
                        *current_cred_idx = 0;
                    }

                    if *current_def_idx >= action_queue.len() {
                        let mut final_actions: Vec<PluginAction> = Vec::new();

                        // Add skill actions
                        for item in &self.items {
                            if matches!(item.kind, ItemKind::Skill)
                                && item.toggled != item.original_enabled
                            {
                                final_actions.push(PluginAction::SetSkillEnabled {
                                    name: item.id.clone(),
                                    enabled: item.toggled,
                                });
                            }
                        }

                        // Add WASM disable/enable actions. Rebuilding from
                        // item state (not the cached immediate list) keeps
                        // the credential-input branch in lockstep with the
                        // no-credential one — a regression where WASM
                        // toggles were silently dropped if combined with a
                        // credentialed channel install must not be possible.
                        for item in &self.items {
                            if let ItemKind::WasmPlugin { quarantined: false } = item.kind {
                                if item.toggled != item.original_enabled {
                                    final_actions.push(PluginAction::SetWasmPluginDisabled {
                                        plugin_id: item.id.clone(),
                                        disabled: !item.toggled,
                                    });
                                }
                            }
                        }

                        // Add channel uninstall actions
                        for item in &self.items {
                            if matches!(item.kind, ItemKind::Channel { .. })
                                && !item.toggled
                                && item.original_enabled
                            {
                                final_actions.push(PluginAction::Uninstall {
                                    id: item.id.clone(),
                                });
                            }
                        }

                        // Add install actions with collected credentials
                        for (id, creds) in all_credentials.drain(..) {
                            final_actions.push(PluginAction::Install {
                                id,
                                credentials: creds,
                            });
                        }

                        self.dismiss();
                        return Some(final_actions);
                    }

                    None
                }
                _ => None,
            },
        }
    }

    /// Returns (immediate_actions, channel_actions) for pending changes.
    /// `immediate_actions` covers skills and WASM-plugin status flips that
    /// don't need credential capture; `channel_actions` is the list of
    /// catalog channels that need install (true) or uninstall (false).
    fn compute_pending_actions(&self) -> (Vec<PluginAction>, Vec<(String, bool)>) {
        let mut immediate = Vec::new();
        let mut channel_actions = Vec::new();

        for item in &self.items {
            if item.toggled == item.original_enabled {
                continue;
            }
            match &item.kind {
                ItemKind::Skill => {
                    immediate.push(PluginAction::SetSkillEnabled {
                        name: item.id.clone(),
                        enabled: item.toggled,
                    });
                }
                ItemKind::WasmPlugin { quarantined: false } => {
                    // toggled == true → re-enable; toggled == false → disable.
                    immediate.push(PluginAction::SetWasmPluginDisabled {
                        plugin_id: item.id.clone(),
                        disabled: !item.toggled,
                    });
                }
                ItemKind::WasmPlugin { quarantined: true } => {
                    // Render path prevents toggling a quarantined item, but
                    // double-check here so a future keybind change cannot
                    // sneak an action through.
                }
                ItemKind::Channel { .. } => {
                    if item.toggled && !item.original_enabled {
                        channel_actions.push((item.id.clone(), true));
                    } else if !item.toggled && item.original_enabled {
                        channel_actions.push((item.id.clone(), false));
                    }
                }
            }
        }

        (immediate, channel_actions)
    }

    pub fn render(&self, frame: &mut Frame) {
        let Some((inner, content_height)) =
            popup_utils::begin_popup_render(frame, self.visible, "Plugins", 5, 2)
        else {
            return;
        };

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut row_indices: Vec<usize> = Vec::new();

        let mut last_section: Option<Section> = None;
        for (i, item) in self.items.iter().enumerate() {
            if last_section != Some(item.section) {
                if last_section.is_some() {
                    lines.push(Line::default());
                }
                lines.push(Line::from(Span::styled(
                    format!(" {}", item.section.label()),
                    theme::bold(),
                )));
                last_section = Some(item.section);
            }

            row_indices.push(lines.len());

            let check = if item.toggled { "x" } else { " " };

            // Tag quarantined WASM rows so the operator sees the read-only
            // state at a glance — toggling does nothing for those.
            let status = match &item.kind {
                ItemKind::WasmPlugin { quarantined: true } => "  [quarantined]",
                _ => "",
            };

            // Platform note for channels
            let platform_note = if let ItemKind::Channel { def } = &item.kind {
                if let Some(label) = def.platform.label() {
                    if !def.platform.is_available() {
                        format!("  ({label})")
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            let label = format!("  [{check}] {}{status}{platform_note}", item.name,);

            let is_selected = i == self.cursor;
            let style = if is_selected {
                theme::popup_selected()
            } else {
                ratatui::style::Style::default()
            };

            lines.push(Line::from(Span::styled(label, style)));
        }

        // Scroll to keep cursor visible
        let selected_line = row_indices.get(self.cursor).copied().unwrap_or(0);
        let scroll_offset = if selected_line >= content_height {
            selected_line - content_height + 1
        } else {
            0
        };

        let visible_lines: Vec<Line<'static>> = lines
            .into_iter()
            .skip(scroll_offset)
            .take(content_height)
            .collect();

        let content_area = Rect::new(inner.x, inner.y, inner.width, content_height as u16);
        frame.render_widget(Paragraph::new(visible_lines), content_area);

        // Status line
        popup_utils::render_status_message(frame, inner, self.status_message.as_ref(), 2);

        // Credential input overlay
        if let PluginPhase::CredentialInput {
            ref action_queue,
            current_def_idx,
            current_cred_idx,
            ref buffer,
            ..
        } = self.phase
        {
            if let Some((ref id, cred_specs)) = action_queue.get(current_def_idx) {
                if let Some(cred) = cred_specs.get(current_cred_idx) {
                    let label_line = Line::from(vec![
                        Span::styled(" Enter ", theme::bold()),
                        Span::styled(cred.label.to_string(), theme::popup_selected()),
                        Span::styled(format!(" for {id}:"), theme::dim()),
                    ]);
                    let help_line = Line::from(Span::styled(
                        format!("   Get it at: {}", cred.help_url),
                        theme::dim(),
                    ));
                    let masked: String = "*".repeat(buffer.len()) + "_";
                    let input_line = Line::from(Span::styled(
                        format!("   > {masked}"),
                        theme::popup_selected(),
                    ));

                    let cred_y = inner.y + inner.height.saturating_sub(5);
                    let cred_area = Rect::new(inner.x, cred_y, inner.width, 3);
                    frame.render_widget(Clear, cred_area);
                    frame.render_widget(
                        Paragraph::new(vec![label_line, help_line, input_line]),
                        cred_area,
                    );
                }
            }
        }

        // Footer hint
        let hint = if matches!(self.phase, PluginPhase::CredentialInput { .. }) {
            " Enter: submit  Esc: cancel"
        } else {
            " Space: toggle  Enter: apply  Esc: close"
        };
        popup_utils::render_footer(frame, inner, hint);
    }
}

/// Convert a kebab-case name to Title Case for display.
fn title_case(s: &str) -> String {
    s.split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => {
                    let upper: String = c.to_uppercase().collect();
                    upper + chars.as_str()
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl PopupHandler for PluginsPopup {
    fn is_visible(&self) -> bool {
        self.visible
    }

    fn handle_key_event(
        &mut self,
        key: crossterm::event::KeyEvent,
        _config: &mut Config,
    ) -> Result<Option<AppAction>> {
        Ok(self
            .handle_key(key)
            .map(|actions| AppAction::RunPlugins { actions }))
    }

    fn handle_paste_event(&mut self, text: &str) -> bool {
        self.handle_paste(text)
    }

    fn wants_text_input(&self) -> bool {
        matches!(self.phase, PluginPhase::CredentialInput { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn press(popup: &mut PluginsPopup, code: KeyCode) -> Option<Vec<PluginAction>> {
        popup.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(popup: &mut PluginsPopup, text: &str) {
        for c in text.chars() {
            popup.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    fn make_test_popup() -> PluginsPopup {
        let mut popup = PluginsPopup::new();
        let tmp = std::env::temp_dir().join("borg-plugins-unified-test");
        let config = cortex::config::Config::default();
        popup.show(&config, &tmp);
        popup
    }

    #[test]
    fn new_popup_not_visible() {
        let popup = PluginsPopup::new();
        assert!(!popup.is_visible());
    }

    #[test]
    fn show_and_dismiss() {
        let mut popup = make_test_popup();
        assert!(popup.is_visible());
        assert!(!popup.items.is_empty());
        popup.dismiss();
        assert!(!popup.is_visible());
    }

    #[test]
    fn unified_popup_shows_skills_and_channels() {
        let popup = make_test_popup();
        let has_channel = popup
            .items
            .iter()
            .any(|i| matches!(i.kind, ItemKind::Channel { .. }));
        let has_skill = popup
            .items
            .iter()
            .any(|i| matches!(i.kind, ItemKind::Skill));
        assert!(has_channel, "should have channel items");
        assert!(has_skill, "should have skill items");
    }

    #[test]
    fn hidden_skills_not_shown() {
        let popup = make_test_popup();
        for hidden in cortex::skills::HIDDEN_SKILLS {
            assert!(
                !popup.items.iter().any(|i| i.id == *hidden),
                "hidden skill {hidden} should not appear"
            );
        }
    }

    #[test]
    fn default_enabled_state() {
        let popup = make_test_popup();
        for name in cortex::skills::DEFAULT_ENABLED_SKILLS {
            if let Some(item) = popup.items.iter().find(|i| i.id == *name) {
                assert!(
                    item.toggled,
                    "default-enabled skill {name} should be toggled on"
                );
            }
        }
    }

    #[test]
    fn category_grouping_order() {
        let popup = make_test_popup();
        let sections: Vec<Section> = {
            let mut cats = Vec::new();
            for item in &popup.items {
                if cats.last() != Some(&item.section) {
                    cats.push(item.section);
                }
            }
            cats
        };
        // Channels should come first, Core second
        if let Some(channels_pos) = sections
            .iter()
            .position(|s| *s == Section::Catalog(Category::Channels))
        {
            assert_eq!(channels_pos, 0, "Channels should be the first category");
        }
        if let Some(core_pos) = sections
            .iter()
            .position(|s| *s == Section::Catalog(Category::Core))
        {
            assert_eq!(core_pos, 1, "Core should be the second category");
        }
    }

    #[test]
    fn core_skills_grouped_under_core_category() {
        let popup = make_test_popup();
        let core_skill_ids = ["browser", "calendar", "email", "search"];
        for id in &core_skill_ids {
            if let Some(item) = popup.items.iter().find(|i| i.id == *id) {
                assert_eq!(
                    item.section,
                    Section::Catalog(Category::Core),
                    "skill {id} should be in Core category"
                );
            }
        }
    }

    #[test]
    fn navigation_wraps() {
        let mut popup = make_test_popup();
        let count = popup.items.len();
        assert_eq!(popup.cursor, 0);

        press(&mut popup, KeyCode::Up);
        assert_eq!(popup.cursor, count - 1);

        press(&mut popup, KeyCode::Down);
        assert_eq!(popup.cursor, 0);
    }

    #[test]
    fn skill_toggle_produces_action() {
        let mut popup = make_test_popup();
        // Find a skill that's enabled by default
        let skill_idx = popup
            .items
            .iter()
            .position(|i| matches!(i.kind, ItemKind::Skill) && i.original_enabled)
            .expect("should have an enabled skill");

        popup.cursor = skill_idx;
        press(&mut popup, KeyCode::Char(' '));
        assert!(!popup.items[skill_idx].toggled);

        let (skill_actions, _) = popup.compute_pending_actions();
        assert_eq!(skill_actions.len(), 1);
        assert!(matches!(
            &skill_actions[0],
            PluginAction::SetSkillEnabled { enabled: false, .. }
        ));
    }

    #[test]
    fn channel_toggle_and_credential_flow() {
        let mut popup = make_test_popup();
        let telegram_idx = popup
            .items
            .iter()
            .position(|i| i.id == "messaging/telegram")
            .expect("telegram should be in list");

        // Force uninstalled state
        popup.items[telegram_idx].original_enabled = false;
        popup.items[telegram_idx].toggled = false;

        popup.cursor = telegram_idx;
        press(&mut popup, KeyCode::Char(' '));
        assert!(popup.items[telegram_idx].toggled);

        // Enter triggers credential input
        let result = press(&mut popup, KeyCode::Enter);
        assert!(result.is_none());
        assert!(matches!(popup.phase, PluginPhase::CredentialInput { .. }));

        // Type credential and submit
        type_text(&mut popup, "test-token");
        let result = press(&mut popup, KeyCode::Enter);
        assert!(result.is_some());
    }

    #[test]
    fn enter_no_changes_shows_status() {
        let mut popup = make_test_popup();
        let result = press(&mut popup, KeyCode::Enter);
        assert!(result.is_none());
        assert!(popup.status_message.is_some());
    }

    #[test]
    fn esc_closes_popup() {
        let mut popup = make_test_popup();
        assert!(popup.is_visible());
        press(&mut popup, KeyCode::Esc);
        assert!(!popup.is_visible());
    }

    #[test]
    fn handle_paste_consumed_in_credential_input() {
        let mut popup = make_test_popup();
        assert!(!popup.handle_paste("text"));

        // Enter credential input for telegram
        let idx = popup
            .items
            .iter()
            .position(|i| i.id == "messaging/telegram")
            .unwrap();
        popup.items[idx].original_enabled = false;
        popup.items[idx].toggled = false;
        popup.cursor = idx;
        press(&mut popup, KeyCode::Char(' '));
        press(&mut popup, KeyCode::Enter);

        assert!(popup.handle_paste("pasted-token"));
        if let PluginPhase::CredentialInput { ref buffer, .. } = popup.phase {
            assert_eq!(buffer, "pasted-token");
        }
    }

    #[test]
    fn handle_paste_not_consumed_when_hidden() {
        let popup = &mut PluginsPopup::new();
        assert!(!popup.handle_paste("anything"));
    }

    #[test]
    fn native_plugins_appear_in_list() {
        let popup = make_test_popup();
        let native_ids = [
            "messaging/telegram",
            "messaging/slack",
            "messaging/discord",
            "messaging/teams",
            "messaging/google-chat",
        ];
        for id in &native_ids {
            assert!(
                popup.items.iter().any(|i| i.id == *id),
                "native plugin {id} should appear in list"
            );
        }
    }

    #[test]
    fn title_case_works() {
        assert_eq!(title_case("git"), "Git");
        assert_eq!(title_case("skill-creator"), "Skill Creator");
        assert_eq!(title_case("1password"), "1password");
        assert_eq!(title_case("google-calendar"), "Google Calendar");
    }

    /// Tokens/credentials commonly contain `/`. The global slash-interceptor
    /// must defer to this predicate so a `/` mid-credential is NOT eaten by
    /// the dispatcher (which would also throw away `collected` credentials
    /// from prior services in the same install batch).
    #[test]
    fn wants_text_input_only_during_credential_input() {
        use super::super::app::PopupHandler;
        let mut popup = make_test_popup();
        assert!(
            !popup.wants_text_input(),
            "Browsing the plugin list does not consume `/`"
        );
        // Drive into CredentialInput via the same key path users take.
        let idx = popup
            .items
            .iter()
            .position(|i| i.id == "messaging/telegram")
            .expect("telegram plugin");
        popup.items[idx].original_enabled = false;
        popup.items[idx].toggled = false;
        popup.cursor = idx;
        press(&mut popup, KeyCode::Char(' '));
        press(&mut popup, KeyCode::Enter);
        assert!(matches!(popup.phase, PluginPhase::CredentialInput { .. }));
        assert!(
            popup.wants_text_input(),
            "CredentialInput must claim text input — `/` belongs in tokens"
        );
    }
}
