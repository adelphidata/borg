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

// ── Test: data_version() runs against real backend and shifts on writes ──
//
// Regression guard for the `PRAGMA data_version` failure. The original bug
// shipped on the turso backend: turso's parser rejected `PRAGMA data_version`,
// so the config-watcher's poll loop logged "Not a valid pragma name" every
// 3 seconds on a real install. The pre-existing unit test was useless
// because it lived under a disabled `#[cfg(feature = "phase9-tests-disabled")]`
// block AND opened `rusqlite::Connection::open_in_memory`, which silently
// accepts the pragma. It would never have caught the bug against the real
// backend. libsql's fixed the parser (PRAGMA data_version works), but the
// regression guard stays — exercising the real backend is the whole point.
#[tokio::test]
async fn data_version_runs_against_real_sqlite_backend() {
    let db = test_db().await;
    let v = db.data_version().await.expect("data_version must succeed");
    // The exact value isn't meaningful — the contract is that the call
    // doesn't error out against the real SQLite parser.
    assert!(v >= 0, "version indicator must be non-negative, got {v}");
}

/// SQLite's `PRAGMA data_version` is connection-local: it only shifts when
/// *another* connection commits a write to the same database. Same-connection
/// writes do not bump it. That semantics is what the config-watcher needs —
/// the watcher's whole point is detecting writes from the daemon while the
/// CLI is polling, or vice versa. The regression guard must therefore use
/// two connections to a real file-backed DB; a single in-memory handle can't
/// observe the cross-connection shift.
async fn open_two_handles_to_same_file() -> (
    tempfile::TempDir,
    cortex::db::Database,
    cortex::db::Database,
) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("borg-test.db");
    let writer = cortex::db::Database::open_at(&path)
        .await
        .expect("open writer");
    let reader = cortex::db::Database::open_at(&path)
        .await
        .expect("open reader");
    (tmp, writer, reader)
}

#[tokio::test]
async fn data_version_shifts_after_cross_connection_write() {
    let (_tmp, writer, reader) = open_two_handles_to_same_file().await;
    let before = reader.data_version().await.expect("read before");

    writer
        .set_setting("temperature", "0.42")
        .await
        .expect("set temperature");

    let after = reader.data_version().await.expect("read after");
    assert_ne!(
        before, after,
        "data_version must change after a cross-connection write (before={before}, after={after})"
    );
}

#[tokio::test]
async fn data_version_shifts_after_cross_connection_delete() {
    let (_tmp, writer, reader) = open_two_handles_to_same_file().await;
    writer
        .set_setting("temperature", "0.42")
        .await
        .expect("seed setting");
    let before = reader.data_version().await.expect("read before");

    let removed = writer.delete_setting("temperature").await.expect("delete");
    assert!(removed, "delete should remove the row");

    let after = reader.data_version().await.expect("read after");
    assert_ne!(
        before, after,
        "data_version must change after a cross-connection delete (before={before}, after={after})"
    );
}

// ── Test: WAL multi-process — the backend swap's primary motivation ──
//
// The bug that drove the turso→libsql swap: the prior backend's WAL
// implementation took an exclusive lock on `borg.db-wal`, so a second
// process opening the same DB hit a fatal `Locking error`. That broke the
// design assumption that the standalone CLI could coexist with `borgd`.
// libsql's WAL is multi-process, real-sqlite behaviour — two writers and
// any number of readers can share one file. This guard locks that down:
// any future regression that re-introduces exclusive WAL locking surfaces
// here instead of as a runtime crash in `borg --listen` startup.
#[tokio::test]
async fn two_handles_to_same_file_can_both_write() {
    let (_tmp, writer_a, writer_b) = open_two_handles_to_same_file().await;
    writer_a
        .set_setting("temperature", "0.10")
        .await
        .expect("writer A: set temperature");
    writer_b
        .set_setting("max_tokens", "2048")
        .await
        .expect("writer B: set max_tokens — would fail on a single-writer WAL backend");

    let temp = writer_b
        .get_setting("temperature")
        .await
        .expect("read temperature from writer B")
        .expect("temperature row present");
    let max_tokens = writer_a
        .get_setting("max_tokens")
        .await
        .expect("read max_tokens from writer A")
        .expect("max_tokens row present");
    assert_eq!(temp, "0.10");
    assert_eq!(max_tokens, "2048");
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
