//! Tests for provider registry resolution and air-gap policy.

use super::category::ProviderCategory;
use super::registry::{ProviderRegistry, ResolveError};
use super::row::ProviderRow;
use crate::db::Database;

#[test]
fn builtin_resolves_to_compile_time_category() {
    // No row in `providers` — the registry must resolve "openai" to the
    // built-in PublicCloud variant via the compile-time map.
    let db = Database::test_db();
    let resolved = ProviderRegistry::resolve(db.conn(), "openai", true).unwrap();
    assert_eq!(resolved.category, ProviderCategory::PublicCloud);
    assert_eq!(resolved.id, "openai");
    assert!(resolved.row.is_none(), "built-ins have no row");
    assert!(resolved.builtin.is_some());
}

#[test]
fn byo_row_resolves_to_byo_endpoint() {
    let db = Database::test_db();
    ProviderRow {
        id: "my-vllm".into(),
        category: ProviderCategory::BYOEndpoint,
        display_name: "Internal vLLM".into(),
        endpoint_url: "https://vllm.example.com/v1/chat/completions".into(),
        api_key_env: Some("MY_VLLM_KEY".into()),
        enabled: true,
    }
    .insert(db.conn())
    .unwrap();

    let resolved = ProviderRegistry::resolve(db.conn(), "my-vllm", true).unwrap();
    assert_eq!(resolved.category, ProviderCategory::BYOEndpoint);
    assert_eq!(
        resolved.row.as_ref().unwrap().endpoint_url,
        "https://vllm.example.com/v1/chat/completions"
    );
    assert!(resolved.builtin.is_none());
}

#[test]
fn unknown_id_errors() {
    let db = Database::test_db();
    let err = ProviderRegistry::resolve(db.conn(), "definitely-not-a-provider", true).unwrap_err();
    assert!(matches!(err, ResolveError::Unknown(_)));
}

#[test]
fn air_gap_refuses_public_cloud_built_in() {
    // outbound_allowed = false; resolving a built-in PublicCloud provider
    // must return AirGapPolicyViolation, not Allow.
    let db = Database::test_db();
    let err = ProviderRegistry::resolve(db.conn(), "openai", false).unwrap_err();
    match err {
        ResolveError::AirGapPolicyViolation { id, category } => {
            assert_eq!(id, "openai");
            assert_eq!(category, ProviderCategory::PublicCloud);
        }
        other => panic!("expected AirGapPolicyViolation, got {other:?}"),
    }
}

#[test]
fn air_gap_allows_local_provider() {
    let db = Database::test_db();
    let resolved = ProviderRegistry::resolve(db.conn(), "ollama", false).unwrap();
    assert_eq!(resolved.category, ProviderCategory::Local);
}

#[test]
fn air_gap_refuses_byo_unless_marked_local() {
    let db = Database::test_db();
    // BYO endpoint marked as BYOEndpoint — not air-gap-safe.
    ProviderRow {
        id: "external-byo".into(),
        category: ProviderCategory::BYOEndpoint,
        display_name: "External".into(),
        endpoint_url: "https://corp.example.com/v1/chat/completions".into(),
        api_key_env: None,
        enabled: true,
    }
    .insert(db.conn())
    .unwrap();
    assert!(matches!(
        ProviderRegistry::resolve(db.conn(), "external-byo", false).unwrap_err(),
        ResolveError::AirGapPolicyViolation { .. }
    ));

    // Same endpoint registered as Local (operator's call) — air-gap allows it.
    ProviderRow {
        id: "internal-byo".into(),
        category: ProviderCategory::Local,
        display_name: "Tailscale node".into(),
        endpoint_url: "http://10.0.0.5:8000/v1/chat/completions".into(),
        api_key_env: None,
        enabled: true,
    }
    .insert(db.conn())
    .unwrap();
    let resolved = ProviderRegistry::resolve(db.conn(), "internal-byo", false).unwrap();
    assert_eq!(resolved.category, ProviderCategory::Local);
}

#[test]
fn category_db_round_trip_covers_all_variants() {
    // Catches a future variant addition that forgets to update either
    // `as_db_str` or `from_db_str`. Without this, an unknown variant
    // would silently fall through to `None` in `from_db_str`.
    for cat in [
        ProviderCategory::PublicCloud,
        ProviderCategory::Local,
        ProviderCategory::BYOEndpoint,
    ] {
        let s = cat.as_db_str();
        assert_eq!(ProviderCategory::from_db_str(s), Some(cat));
    }
}
