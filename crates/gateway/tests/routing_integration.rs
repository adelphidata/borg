//! Gateway routing integration tests.
//!
//! Tests route resolution with binding cascades, pattern matching,
//! and config override propagation.

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

use borg_core::config::Config;
use borg_gateway::routing::{matches_pattern, resolve_route};

// ── Test: no bindings returns default route ──

#[test]
fn no_bindings_returns_default() {
    let config = Config::default();
    let route = resolve_route(&config, "telegram", "user123", Some("direct"));
    assert_eq!(route.matched_by, "default");
    assert!(route.memory_scope.is_none());
    assert!(route.identity_path.is_none());
    assert!(route.activation.is_none());
}

// ── Test: exact pattern match ──

#[test]
fn exact_pattern_match() {
    assert!(matches_pattern("telegram", "telegram"));
    assert!(!matches_pattern("telegram", "slack"));
}

// ── Test: wildcard matches everything ──

#[test]
fn wildcard_matches_all() {
    assert!(matches_pattern("*", "telegram"));
    assert!(matches_pattern("*", "slack"));
    assert!(matches_pattern("*", "discord"));
    assert!(matches_pattern("*", ""));
}

// ── Test: prefix glob matching ──

#[test]
fn prefix_glob_matching() {
    assert!(matches_pattern("telegram-*", "telegram-personal"));
    assert!(matches_pattern("telegram-*", "telegram-work"));
    assert!(!matches_pattern("telegram-*", "slack-work"));
}

// ── Test: suffix glob matching ──

#[test]
fn suffix_glob_matching() {
    assert!(matches_pattern("*-work", "telegram-work"));
    assert!(matches_pattern("*-work", "slack-work"));
    assert!(!matches_pattern("*-work", "slack-personal"));
}

// ── Test: default route has clean config ──

#[test]
fn default_route_clean_config() {
    let config = Config::default();
    let route = resolve_route(&config, "any_channel", "any_sender", None);

    // Default route should have the base config
    assert!(route.config.llm.temperature > 0.0);
    assert!(route.config.llm.max_tokens > 0);
}

// ── Test: case-sensitive pattern matching ──

#[test]
fn case_sensitive_patterns() {
    assert!(!matches_pattern("Telegram", "telegram"));
    assert!(matches_pattern("telegram", "telegram"));
}

// ── Test: empty pattern matches nothing (except empty string) ──

#[test]
fn empty_pattern_behavior() {
    assert!(matches_pattern("", ""));
    assert!(!matches_pattern("", "telegram"));
}

// ── Test: resolve with different peer kinds ──

#[test]
fn resolve_different_peer_kinds() {
    let config = Config::default();

    let dm = resolve_route(&config, "telegram", "user1", Some("direct"));
    let group = resolve_route(&config, "telegram", "user1", Some("group"));
    let none_peer = resolve_route(&config, "telegram", "user1", None);

    // All should resolve to default when no bindings
    assert_eq!(dm.matched_by, "default");
    assert_eq!(group.matched_by, "default");
    assert_eq!(none_peer.matched_by, "default");
}
