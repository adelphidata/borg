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
    /// A configured MCP server. Read-only inside this popup — toggling
    /// shows a pointer to `borg mcp` instead. Lives here so operators
    /// can see "everything that adds tools" in one view, but trust-model
    /// boundaries (signature/HMAC vs env-allowlist+pinned-DNS) stay
    /// owned by their respective install paths.
    Mcp {
        /// Transport label (`stdio`/`http`/`sse`) shown inline.
        transport: &'static str,
        /// Human-readable lifecycle state for the trailing tag.
        state: &'static str,
        /// Tool count from the live registry.
        tool_count: usize,
    },
}

/// Section heading for the popup grouping. Wraps `plugins::Category` so the
/// WASM section can sit alongside the catalog categories without polluting a
/// public enum that other crates consume.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Section {
    Catalog(Category),
    Wasm,
    Mcp,
}

impl Section {
    fn label(self) -> String {
        match self {
            Self::Catalog(c) => c.to_string(),
            Self::Wasm => "WASM PLUGINS".to_string(),
            Self::Mcp => "MCP SERVERS".to_string(),
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
            // MCP servers last: they're operator-configured external code
            // at arm's length, and edits flow through `borg mcp` rather
            // than the popup itself.
            Self::Mcp => 8,
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

        let resolved_creds = config.resolve_credentials();
        if let Ok(skills) = cortex::skills::load_all_skills(&resolved_creds, &config.skills, None) {
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
        match crate::async_bridge::block_on(async {
            let db = cortex::db::Database::open_readonly().await?;
            db.list_wasm_plugins().await
        }) {
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
                tracing::warn!("plugins popup: WASM plugin listing failed: {e}");
            }
        }

        // MCP servers — read-only listing. Toggles in this popup point
        // operators at the `borg mcp` CLI; the trust-boundary checks
        // (env allowlist, DNS pinning, schema gauntlet) only run on the
        // CLI path, so accepting toggles here would be a subtle way to
        // bypass them.
        for snap in cortex::mcp_clients::server_status_snapshot(config) {
            let state = snap.state_label();
            items.push(PluginItem {
                id: format!("mcp/{}", snap.name),
                name: snap.name.clone(),
                section: Section::Mcp,
                kind: ItemKind::Mcp {
                    transport: snap.transport,
                    state,
                    tool_count: snap.tool_count,
                },
                // `original_enabled == toggled` ensures
                // `compute_pending_actions` never produces an action for
                // this row, even if the user mashes Space (the handler
                // also short-circuits Mcp rows for clearer feedback).
                original_enabled: !snap.live_disabled && snap.config_enabled,
                toggled: !snap.live_disabled && snap.config_enabled,
            });
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
                        // MCP server rows are intentionally read-only here:
                        // enable/disable, transport edits, and the schema
                        // gauntlet all live on the `borg mcp` CLI path so
                        // there's one place that owns the trust model.
                        if let ItemKind::Mcp { .. } = &item.kind {
                            self.status_message = Some((
                                format!(
                                    "{} is an MCP server — manage via `borg mcp disable {}` / `enable` / `remove`",
                                    item.name, item.name
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
                ItemKind::Mcp { .. } => {
                    // MCP rows have no popup-emitted action; the
                    // Space handler bounces the user to the CLI.
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

            // MCP rows render without the toggle checkbox (this popup
            // never mutates them); other kinds get the `[x]/[ ]` glyph.
            let label = if let ItemKind::Mcp {
                transport,
                state,
                tool_count,
            } = &item.kind
            {
                let tools = if *tool_count == 1 {
                    "1 tool".to_string()
                } else {
                    format!("{tool_count} tools")
                };
                format!(
                    "  · {name}  ({transport}, {state}, {tools})",
                    name = item.name
                )
            } else {
                let check = if item.toggled { "x" } else { " " };
                let status = match &item.kind {
                    ItemKind::WasmPlugin { quarantined: true } => "  [quarantined]",
                    _ => "",
                };
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
                format!("  [{check}] {}{status}{platform_note}", item.name)
            };

            let is_selected = i == self.cursor;
            let style = if is_selected {
                theme::popup_selected()
            } else {
                ratatui::style::Style::default()
            };

            lines.push(Line::from(Span::styled(label, style)));
        }

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

        popup_utils::render_status_message(frame, inner, self.status_message.as_ref(), 2);

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
mod tests;
