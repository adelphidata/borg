#[allow(unused_imports)]
use super::*;

/// Guard: gateway resolve_credential must delegate to config.resolve_credential_or_env
#[test]
fn gateway_resolve_credential_delegates_to_core() {
    let source = include_str!("../../../../gateway/src/channel_init.rs");
    assert!(
        source.contains("config.resolve_credential_or_env(key)"),
        "gateway resolve_credential must delegate to core's resolve_credential_or_env"
    );
}

/// Guard: KEY_PLUGIN_MAP must cover all credential keys used in gateway channel_init.rs.
/// If a new credential is added to channel_init.rs, it must also be added to KEY_PLUGIN_MAP
/// in config/mod.rs so the keychain fallback can resolve it.
#[test]
fn key_plugin_map_covers_all_gateway_credentials() {
    let gateway_source = include_str!("../../../../gateway/src/channel_init.rs");
    let config_source = include_str!("../mod.rs");

    // Extract all credential keys from resolve_credential() calls in gateway
    for line in gateway_source.lines() {
        let trimmed = line.trim();
        // Match lines like: resolve_credential(config, "messaging/foo", "SOME_KEY");
        if let Some(start) = trimmed.find("resolve_credential(") {
            let after = &trimmed[start..];
            // Extract the third argument (the key)
            let parts: Vec<&str> = after.split('"').collect();
            // Pattern: resolve_credential(config, "plugin_id", "KEY")
            // parts[0] = resolve_credential(config,
            // parts[1] = plugin_id
            // parts[2] = ,
            // parts[3] = KEY
            if parts.len() >= 4 {
                let key = parts[3];
                if key.contains("BORG_TEST") || key.is_empty() {
                    continue; // skip test-only keys
                }
                assert!(
                    config_source.contains(&format!("\"{key}\"")),
                    "KEY_PLUGIN_MAP in config/mod.rs is missing credential key '{key}' \
                     used in gateway/channel_init.rs. Add it to resolve_keychain_fallback()."
                );
            }
        }
    }
}

/// Guard: TUI must reload config after plugin install
#[test]
fn tui_reloads_config_after_plugin_install() {
    let source = include_str!("../../../../cli/src/tui/mod.rs");
    // Find the Install block and verify it reloads config
    let install_section: String = source
        .lines()
        .skip_while(|l| !l.contains("PluginAction::Install"))
        .take(80)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        install_section.contains("Config::load_from_db()"),
        "TUI must reload config after plugin install to pick up new credentials"
    );
}
