//! Settings resolution integration tests.
//!
//! Tests the two-layer resolution (DB → defaults) using
//! in-memory SQLite. Validates set/get/unset lifecycle, type validation,
//! and config application.

#![allow(
    clippy::approx_constant,
    clippy::assertions_on_constants,
    clippy::const_is_empty,
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::identity_op,
    clippy::items_after_test_module,
    clippy::len_zero,
    clippy::manual_range_contains,
    clippy::needless_borrow,
    clippy::needless_collect,
    clippy::redundant_clone,
    clippy::redundant_closure_for_method_calls,
    clippy::uninlined_format_args,
    clippy::unnecessary_cast,
    clippy::unnecessary_map_or,
    clippy::unwrap_used,
    clippy::useless_format,
    clippy::useless_vec
)]

use cortex::settings::{SettingSource, SettingsResolver, ALL_SETTING_KEYS};

mod common;
use common::test_db;

async fn test_resolver() -> SettingsResolver {
    SettingsResolver::new(test_db().await)
}

// ── Test: defaults resolve without DB overrides ──

#[tokio::test]
async fn defaults_resolve_without_db() {
    let resolver = test_resolver().await;
    let config = resolver.resolve().await.expect("resolve");
    // Default temperature should be set
    assert!(config.llm.temperature > 0.0);
    assert!(config.llm.max_tokens > 0);
}

// ── Test: DB override takes precedence ──

#[tokio::test]
async fn db_override_takes_precedence() {
    let resolver = test_resolver().await;

    // Set temperature via DB
    resolver
        .set("temperature", "0.42")
        .await
        .expect("set temperature");

    let config = resolver.resolve().await.expect("resolve");
    assert!(
        (config.llm.temperature - 0.42).abs() < f32::EPSILON,
        "DB override should take precedence, got {}",
        config.llm.temperature
    );
}

// ── Test: unset reverts to default ──

#[tokio::test]
async fn unset_reverts_to_default() {
    let resolver = test_resolver().await;
    let default_config = resolver.resolve().await.expect("resolve default");
    let default_temp = default_config.llm.temperature;

    // Override, then unset
    resolver.set("temperature", "0.1").await.expect("set");
    resolver.unset("temperature").await.expect("unset");

    let config = resolver.resolve().await.expect("resolve after unset");
    assert!(
        (config.llm.temperature - default_temp).abs() < f32::EPSILON,
        "Should revert to default after unset"
    );
}

// ── Test: get_with_source reports correct source ──

#[tokio::test]
async fn get_with_source_reports_correctly() {
    let resolver = test_resolver().await;

    // Default source
    let (_, source) = resolver
        .get_with_source("temperature")
        .await
        .expect("get default");
    assert_eq!(source, SettingSource::Default);

    // DB source after set
    resolver.set("temperature", "0.5").await.expect("set");
    let (val, source) = resolver
        .get_with_source("temperature")
        .await
        .expect("get db");
    assert_eq!(source, SettingSource::Database);
    assert_eq!(val, "0.5");
}

// ── Test: invalid key rejected ──

#[tokio::test]
async fn invalid_key_rejected() {
    let resolver = test_resolver().await;
    let result = resolver.set("nonexistent_key", "value").await;
    assert!(result.is_err(), "Invalid key should be rejected");
}

// ── Test: invalid value rejected ──

#[tokio::test]
async fn invalid_temperature_rejected() {
    let resolver = test_resolver().await;
    let result = resolver.set("temperature", "not_a_number").await;
    assert!(
        result.is_err(),
        "Non-numeric temperature should be rejected"
    );
}

// ── Test: list_all returns all known keys ──

#[tokio::test]
async fn list_all_returns_all_keys() {
    let resolver = test_resolver().await;
    let all = resolver.list_all().await.expect("list all");
    assert_eq!(
        all.len(),
        ALL_SETTING_KEYS.len(),
        "list_all should return all known settings"
    );
}

// ── Test: boolean settings round-trip ──

#[tokio::test]
async fn boolean_settings_round_trip() {
    let resolver = test_resolver().await;

    resolver
        .set("sandbox.enabled", "false")
        .await
        .expect("set false");
    let config = resolver.resolve().await.expect("resolve");
    assert!(!config.sandbox.enabled, "Should be false");

    resolver
        .set("sandbox.enabled", "true")
        .await
        .expect("set true");
    let config = resolver.resolve().await.expect("resolve");
    assert!(config.sandbox.enabled, "Should be true");
}

// ── Test: integer settings round-trip ──

#[tokio::test]
async fn integer_settings_round_trip() {
    let resolver = test_resolver().await;

    resolver
        .set("max_tokens", "2048")
        .await
        .expect("set max_tokens");
    let config = resolver.resolve().await.expect("resolve");
    assert_eq!(config.llm.max_tokens, 2048);
}

// ── Test: multiple overrides compose correctly ──

#[tokio::test]
async fn multiple_overrides_compose() {
    let resolver = test_resolver().await;

    resolver.set("temperature", "0.3").await.expect("set temp");
    resolver
        .set("max_tokens", "1024")
        .await
        .expect("set tokens");
    resolver
        .set("sandbox.enabled", "false")
        .await
        .expect("set sandbox");

    let config = resolver.resolve().await.expect("resolve");
    assert!((config.llm.temperature - 0.3).abs() < f32::EPSILON);
    assert_eq!(config.llm.max_tokens, 1024);
    assert!(!config.sandbox.enabled);
}

// ── Test: DB persistence across resolver operations ──

#[tokio::test]
async fn db_persistence_across_operations() {
    let resolver = test_resolver().await;

    // Set, then verify it persists across get calls
    resolver.set("temperature", "0.99").await.expect("set");
    let (val, source) = resolver.get_with_source("temperature").await.expect("get");
    assert_eq!(val, "0.99");
    assert_eq!(source, SettingSource::Database);

    // Verify it survives resolve()
    let config = resolver.resolve().await.expect("resolve");
    assert!((config.llm.temperature - 0.99).abs() < f32::EPSILON);
}
