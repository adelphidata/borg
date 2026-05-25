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
fn show_and_dismiss() {
    // A freshly-constructed popup is invisible; `show` (via the test
    // helper) makes it visible with a non-empty item list; `dismiss`
    // hides it again. Folds three behaviors into one check.
    let popup_fresh = PluginsPopup::new();
    assert!(!popup_fresh.is_visible(), "fresh popup must start hidden");

    let mut popup = make_test_popup();
    assert!(popup.is_visible(), "show must make popup visible");
    assert!(!popup.items.is_empty(), "items must be loaded on show");
    popup.dismiss();
    assert!(!popup.is_visible());
}

/// When an MCP server is configured, `/plugins` must surface it
/// under the "MCP SERVERS" section as a read-only row — operators
/// expect "things that add tools to the agent" to live in one
/// place. The section sits below WASM plugins so the trusted
/// built-ins read first; the popup MUST NOT generate a toggle
/// action for MCP rows (those flow through `borg mcp` to keep the
/// schema gauntlet on the install path).
#[test]
fn mcp_section_appears_when_servers_are_configured() {
    use cortex::config::{McpServerConfig, TransportSpec};
    use std::collections::HashMap;

    let mut config = cortex::config::Config::default();
    config.mcp.servers.insert(
        "time".to_string(),
        McpServerConfig {
            enabled: true,
            transport: TransportSpec::Stdio {
                command: "uvx".to_string(),
                args: vec!["mcp-server-time".to_string()],
                env: HashMap::new(),
            },
            connect_timeout_ms: 60_000,
            call_timeout_ms: 120_000,
            rate_limit_per_minute: 60,
            rate_limit_burst: 10,
            tools: Default::default(),
        },
    );

    let mut popup = PluginsPopup::new();
    let tmp = std::env::temp_dir().join("borg-plugins-mcp-section-test");
    popup.show(&config, &tmp);

    let mcp_item = popup
        .items
        .iter()
        .find(|i| matches!(i.kind, ItemKind::Mcp { .. }))
        .expect("an MCP server in config should produce an Mcp item");
    assert_eq!(mcp_item.section, Section::Mcp);
    assert_eq!(mcp_item.name, "time");

    // Toggling an MCP row must not produce any action — the popup
    // is intentionally read-only for MCP, and any drift would let
    // operators bypass the schema gauntlet by flipping rows here
    // instead of in the CLI.
    let mcp_idx = popup
        .items
        .iter()
        .position(|i| matches!(i.kind, ItemKind::Mcp { .. }))
        .unwrap();
    popup.cursor = mcp_idx;
    press(&mut popup, KeyCode::Char(' '));
    let (immediate, channels) = popup.compute_pending_actions();
    assert!(
        immediate.iter().all(|a| !matches!(
            a,
            PluginAction::SetSkillEnabled { name, .. } if name == "time"
        )),
        "MCP row must not emit a skill-toggle action"
    );
    assert!(
        channels
            .iter()
            .all(|(id, _)| id != "time" && id != "mcp/time"),
        "MCP row must not be queued for channel install/uninstall"
    );
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
