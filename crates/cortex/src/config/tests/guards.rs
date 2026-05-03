//! Cross-crate invariants for credential resolution.
//!
//! The previous `include_str!`-based source-grep guards have been replaced
//! with behaviour assertions:
//!
//! * gateway↔config key-map alignment is asserted in
//!   `crates/gateway/src/channel_init.rs` (test `every_gateway_key_maps_to_a_plugin`),
//!   which iterates the typed `GATEWAY_CREDENTIAL_KEYS` const against
//!   `Config::keychain_plugin_for_key`.
//! * the TUI plugin-install reload path is exercised end-to-end by the TUI
//!   tests; a regex on its source no longer adds signal here.

use super::*;

/// `KEY_PLUGIN_MAP` is the source of truth used by `resolve_keychain_fallback`
/// to derive service names. A duplicate key would silently mask a later entry.
#[test]
fn key_plugin_map_has_no_duplicate_keys() {
    let mut seen = std::collections::HashSet::new();
    for (key, _) in Config::KEY_PLUGIN_MAP {
        assert!(seen.insert(*key), "duplicate credential key in map: {key}");
    }
}

/// Every plugin ID referenced by the keychain fallback must produce a non-empty
/// keychain service name (`borg-{plugin_id with / → -}`). An empty plugin ID
/// would collapse the namespace and let one channel's keychain entry resolve
/// for another.
#[test]
fn key_plugin_map_plugin_ids_are_well_formed() {
    for (key, plugin_id) in Config::KEY_PLUGIN_MAP {
        assert!(!plugin_id.is_empty(), "empty plugin id for key {key}");
        assert!(
            plugin_id.contains('/'),
            "plugin id {plugin_id} (key {key}) must be namespaced (contain '/')"
        );
    }
}
