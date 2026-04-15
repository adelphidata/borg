//! Settings resolution integration tests.
//!
//! Tests the two-layer resolution (DB → defaults) using
//! in-memory SQLite. Validates set/get/unset lifecycle, type validation,
//! and config application.

use borg_core::settings::{SettingSource, SettingsResolver, ALL_SETTING_KEYS};

mod common;
use common::test_db;

fn test_resolver() -> SettingsResolver {
    SettingsResolver::new(test_db())
}

// ── Test: defaults resolve without DB overrides ──

#[test]
fn defaults_resolve_without_db() {
    let resolver = test_resolver();
    let config = resolver.resolve().expect("resolve");
    // Default temperature should be set
    assert!(config.llm.temperature > 0.0);
    assert!(config.llm.max_tokens > 0);
}

// ── Test: DB override takes precedence ──

#[test]
fn db_override_takes_precedence() {
    let resolver = test_resolver();

    // Set temperature via DB
    resolver
        .set("temperature", "0.42")
        .expect("set temperature");

    let config = resolver.resolve().expect("resolve");
    assert!(
        (config.llm.temperature - 0.42).abs() < f32::EPSILON,
        "DB override should take precedence, got {}",
        config.llm.temperature
    );
}

// ── Test: unset reverts to default ──

#[test]
fn unset_reverts_to_default() {
    let resolver = test_resolver();
    let default_config = resolver.resolve().expect("resolve default");
    let default_temp = default_config.llm.temperature;

    // Override, then unset
    resolver.set("temperature", "0.1").expect("set");
    resolver.unset("temperature").expect("unset");

    let config = resolver.resolve().expect("resolve after unset");
    assert!(
        (config.llm.temperature - default_temp).abs() < f32::EPSILON,
        "Should revert to default after unset"
    );
}

// ── Test: get_with_source reports correct source ──

#[test]
fn get_with_source_reports_correctly() {
    let resolver = test_resolver();

    // Default source
    let (_, source) = resolver
        .get_with_source("temperature")
        .expect("get default");
    assert_eq!(source, SettingSource::Default);

    // DB source after set
    resolver.set("temperature", "0.5").expect("set");
    let (val, source) = resolver.get_with_source("temperature").expect("get db");
    assert_eq!(source, SettingSource::Database);
    assert_eq!(val, "0.5");
}

// ── Test: invalid key rejected ──

#[test]
fn invalid_key_rejected() {
    let resolver = test_resolver();
    let result = resolver.set("nonexistent_key", "value");
    assert!(result.is_err(), "Invalid key should be rejected");
}

// ── Test: invalid value rejected ──

#[test]
fn invalid_temperature_rejected() {
    let resolver = test_resolver();
    let result = resolver.set("temperature", "not_a_number");
    assert!(
        result.is_err(),
        "Non-numeric temperature should be rejected"
    );
}

// ── Test: list_all returns all known keys ──

#[test]
fn list_all_returns_all_keys() {
    let resolver = test_resolver();
    let all = resolver.list_all().expect("list all");
    assert_eq!(
        all.len(),
        ALL_SETTING_KEYS.len(),
        "list_all should return all known settings"
    );
}

// ── Test: boolean settings round-trip ──

#[test]
fn boolean_settings_round_trip() {
    let resolver = test_resolver();

    resolver.set("sandbox.enabled", "false").expect("set false");
    let config = resolver.resolve().expect("resolve");
    assert!(!config.sandbox.enabled, "Should be false");

    resolver.set("sandbox.enabled", "true").expect("set true");
    let config = resolver.resolve().expect("resolve");
    assert!(config.sandbox.enabled, "Should be true");
}

// ── Test: integer settings round-trip ──

#[test]
fn integer_settings_round_trip() {
    let resolver = test_resolver();

    resolver.set("max_tokens", "2048").expect("set max_tokens");
    let config = resolver.resolve().expect("resolve");
    assert_eq!(config.llm.max_tokens, 2048);
}

// ── Test: multiple overrides compose correctly ──

#[test]
fn multiple_overrides_compose() {
    let resolver = test_resolver();

    resolver.set("temperature", "0.3").expect("set temp");
    resolver.set("max_tokens", "1024").expect("set tokens");
    resolver
        .set("sandbox.enabled", "false")
        .expect("set sandbox");

    let config = resolver.resolve().expect("resolve");
    assert!((config.llm.temperature - 0.3).abs() < f32::EPSILON);
    assert_eq!(config.llm.max_tokens, 1024);
    assert!(!config.sandbox.enabled);
}

// ── Test: DB persistence across resolver operations ──

#[test]
fn db_persistence_across_operations() {
    let resolver = test_resolver();

    // Set, then verify it persists across get calls
    resolver.set("temperature", "0.99").expect("set");
    let (val, source) = resolver.get_with_source("temperature").expect("get");
    assert_eq!(val, "0.99");
    assert_eq!(source, SettingSource::Database);

    // Verify it survives resolve()
    let config = resolver.resolve().expect("resolve");
    assert!((config.llm.temperature - 0.99).abs() < f32::EPSILON);
}
