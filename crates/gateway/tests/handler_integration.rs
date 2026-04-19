//! Gateway handler integration tests.
//!
//! Tests the `check_activation` function — the branch point that decides
//! whether the agent should reply to an incoming message. The real logic under
//! test is: (a) DMs always active, (b) group + `Mention` mode requires the bot
//! handle (case-insensitive) and strips it from the text, (c) group + `Always`
//! bypasses the mention check, (d) group + `Mention` without a mention drops.

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

/// Table-driven coverage of the whole (peer_kind × activation × mention)
/// decision matrix. Each row is a full `check_activation` call with the bot
/// handle `@BorgBot`; the expected fields describe what the gateway should
/// forward to the agent (or not).
#[test]
fn activation_matrix() {
    struct Case {
        name: &'static str,
        raw_text: &'static str,
        peer_kind: Option<&'static str>,
        activation: Option<ActivationMode>,
        expect_active: bool,
        expect_text: &'static str,
    }

    let cases: &[Case] = &[
        Case {
            name: "dm_without_peer_kind_activates",
            raw_text: "hello bot",
            peer_kind: None,
            activation: None,
            expect_active: true,
            expect_text: "hello bot",
        },
        Case {
            name: "dm_with_direct_peer_kind_activates",
            raw_text: "hello",
            peer_kind: Some("direct"),
            activation: None,
            expect_active: true,
            expect_text: "hello",
        },
        Case {
            name: "group_mention_activates_and_strips",
            raw_text: "@BorgBot what's the weather?",
            peer_kind: Some("group"),
            activation: Some(ActivationMode::Mention),
            expect_active: true,
            expect_text: "what's the weather?",
        },
        Case {
            name: "group_mention_case_insensitive",
            raw_text: "@borgbot do something",
            peer_kind: Some("group"),
            activation: Some(ActivationMode::Mention),
            expect_active: true,
            expect_text: "do something",
        },
        Case {
            name: "group_without_mention_does_not_activate",
            raw_text: "hey everyone, who's around?",
            peer_kind: Some("group"),
            activation: Some(ActivationMode::Mention),
            expect_active: false,
            expect_text: "hey everyone, who's around?",
        },
        Case {
            name: "group_always_mode_activates_without_mention",
            raw_text: "random message",
            peer_kind: Some("group"),
            activation: Some(ActivationMode::Always),
            expect_active: true,
            expect_text: "random message",
        },
    ];

    let config = Config::default();
    for case in cases {
        let route = route_with_activation(case.activation.clone());
        let (active, text) = check_activation(
            case.raw_text,
            case.peer_kind,
            &route,
            &config,
            Some("@BorgBot"),
        );
        assert_eq!(
            active, case.expect_active,
            "{}: active = {active}, expected {}",
            case.name, case.expect_active
        );
        assert_eq!(
            text, case.expect_text,
            "{}: text = {text:?}, expected {:?}",
            case.name, case.expect_text
        );
    }
}
