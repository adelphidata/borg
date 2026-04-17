//! Gateway handler integration tests.
//!
//! Tests the `check_activation` function which determines whether the bot
//! should respond to a message based on peer kind, activation mode, and
//! bot mention.

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

use borg_core::config::{ActivationMode, Config};
use borg_gateway::handler::check_activation;
use borg_gateway::routing::ResolvedRoute;

/// Build a minimal `ResolvedRoute` with the given activation mode override.
fn route_with_activation(activation: Option<ActivationMode>) -> ResolvedRoute {
    ResolvedRoute {
        config: Config::default(),
        binding_id: "test".to_string(),
        memory_scope: None,
        identity_path: None,
        matched_by: "test".to_string(),
        activation,
    }
}

// ── Test: DMs always activate ──

#[test]
fn activation_dm_always_true() {
    let route = route_with_activation(None);
    let config = Config::default();

    // peer_kind = None (DM)
    let (active, text) = check_activation("hello bot", None, &route, &config, Some("@bot"));
    assert!(active, "DMs should always activate");
    assert_eq!(text, "hello bot");

    // peer_kind = Some("direct")
    let (active, text) = check_activation("hello", Some("direct"), &route, &config, Some("@bot"));
    assert!(active, "Direct messages should always activate");
    assert_eq!(text, "hello");
}

// ── Test: group with mention activates and strips mention ──

#[test]
fn activation_group_with_mention() {
    let route = route_with_activation(Some(ActivationMode::Mention));
    let config = Config::default();

    let (active, text) = check_activation(
        "@BorgBot what's the weather?",
        Some("group"),
        &route,
        &config,
        Some("@BorgBot"),
    );
    assert!(active, "Group message with mention should activate");
    assert_eq!(text, "what's the weather?");
    assert!(
        !text.contains("@BorgBot"),
        "Mention should be stripped from text"
    );
}

// ── Test: group without mention does not activate ──

#[test]
fn activation_group_without_mention() {
    let route = route_with_activation(Some(ActivationMode::Mention));
    let config = Config::default();

    let (active, text) = check_activation(
        "hey everyone, who's around?",
        Some("group"),
        &route,
        &config,
        Some("@BorgBot"),
    );
    assert!(!active, "Group message without mention should not activate");
    assert_eq!(text, "hey everyone, who's around?");
}

// ── Test: group with Always activation mode ──

#[test]
fn activation_group_always_mode() {
    let route = route_with_activation(Some(ActivationMode::Always));
    let config = Config::default();

    let (active, text) = check_activation(
        "random message",
        Some("group"),
        &route,
        &config,
        Some("@bot"),
    );
    assert!(active, "Group with Always activation should respond to all");
    assert_eq!(text, "random message");
}

// ── Test: case-insensitive mention matching ──

#[test]
fn activation_case_insensitive_mention() {
    let route = route_with_activation(Some(ActivationMode::Mention));
    let config = Config::default();

    let (active, text) = check_activation(
        "@borgbot do something",
        Some("group"),
        &route,
        &config,
        Some("@BorgBot"),
    );
    assert!(active, "Mention matching should be case-insensitive");
    assert_eq!(text, "do something");
}
