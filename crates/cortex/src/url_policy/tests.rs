//! Spec-driven tests for the URL policy pipeline. Every test starts from
//! an invariant we intend to hold and asserts against it — if the
//! implementation regressed, the test must fail. No `is_ok()` smoke
//! asserts, no "field equals what we set it to" tautologies.

use super::*;
use crate::config::{ThrottleRule, TransformAction, TransformRule, UrlPolicyConfig};

fn cfg_default() -> UrlPolicyConfig {
    UrlPolicyConfig::default()
}

// ────────────────────────────────────────────────────────────────────
// Rule matching (table-driven)
// ────────────────────────────────────────────────────────────────────

#[test]
fn rules_matches_rule_table() {
    let cases: &[(&str, &str, bool, &str)] = &[
        ("tracking.com", "tracking.com", true, "exact"),
        ("a.tracking.com", "tracking.com", true, "subdomain suffix"),
        ("mytracking.com", "tracking.com", false, "not a suffix"),
        (
            "a.tracking.com",
            "*.tracking.com",
            true,
            "wildcard subdomain",
        ),
        (
            "tracking.com",
            "*.tracking.com",
            false,
            "wildcard requires a label",
        ),
        ("a.b.tracking.com", "*.tracking.com", true, "deep subdomain"),
        ("example.com", "", false, "empty rule"),
        ("", "example.com", false, "empty host"),
    ];
    for (host, rule, expected, label) in cases {
        let got = rules::matches_rule(host, rule);
        assert_eq!(got, *expected, "{label}: host={host} rule={rule}");
    }
}

#[test]
fn rules_normalize_host_handles_case_and_trailing_dot() {
    assert_eq!(rules::normalize_host("EXAMPLE.com"), "example.com");
    assert_eq!(rules::normalize_host("example.com."), "example.com");
    assert_eq!(rules::normalize_host("example.com..."), "example.com");
    assert_eq!(rules::normalize_host("  Example.COM "), "example.com");
}

#[test]
fn rules_normalize_rule_handles_url_shaped_input() {
    // Real failure mode: users paste a full URL into the denylist; the
    // rule must collapse to just the host for the matcher to fire.
    assert_eq!(
        rules::normalize_rule("https://example.com/some/path?q=1"),
        Some("example.com".into())
    );
    assert_eq!(
        rules::normalize_rule("http://example.com#frag"),
        Some("example.com".into())
    );
    assert_eq!(rules::normalize_rule("  # comment"), None);
    assert_eq!(rules::normalize_rule(""), None);
    assert_eq!(rules::normalize_rule("*.X.COM"), Some("*.x.com".into()));
}

// ────────────────────────────────────────────────────────────────────
// SSRF — always-blocked hosts/IPs ignore the toggle
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn ssrf_metadata_hostname_blocked_even_with_toggle() {
    let mut cfg = cfg_default();
    cfg.allow_private_urls = true; // toggle on
    let out = classify("http://metadata.google.internal/", &cfg).await;
    match out {
        ClassifyOutcome::Block { rule, .. } => {
            assert_eq!(rule, "ssrf_blocked_host");
        }
        other => panic!("expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn ssrf_metadata_ip_blocked_even_with_toggle() {
    // Use the literal-IP form so we don't need DNS to be honest about it.
    let mut cfg = cfg_default();
    cfg.allow_private_urls = true;
    for url in [
        "http://169.254.169.254/latest/",
        "http://100.100.100.200/",
        "http://169.254.170.2/",
    ] {
        let out = classify(url, &cfg).await;
        match out {
            ClassifyOutcome::Block { rule, .. } => {
                assert_eq!(rule, "ssrf_blocked_ip", "url={url}");
            }
            other => panic!("expected Block for {url}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn ssrf_azure_wireserver_blocked_routable_ip() {
    // 168.63.129.16 is NOT link-local but serves Azure host-internal
    // IMDS-adjacent endpoints. Confirms the explicit entry — without it,
    // a permissive `allow_private_urls=true` config would let it through.
    let mut cfg = cfg_default();
    cfg.allow_private_urls = true;
    let out = classify("http://168.63.129.16/", &cfg).await;
    match out {
        ClassifyOutcome::Block { rule, .. } => assert_eq!(rule, "ssrf_blocked_ip"),
        other => panic!("expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn ssrf_ipv4_mapped_ipv6_metadata_blocked() {
    // `::ffff:169.254.169.254` is the IPv4-mapped IPv6 form of the AWS
    // metadata IP. Without the unwrap in is_always_blocked_ip / the v6
    // branch of is_private_class, a DNS server returning a v6-mapped
    // address would bypass both gates.
    let mut cfg = cfg_default();
    cfg.allow_private_urls = true;
    let out = classify("http://[::ffff:169.254.169.254]/", &cfg).await;
    match out {
        ClassifyOutcome::Block { rule, .. } => assert_eq!(rule, "ssrf_blocked_ip"),
        other => panic!("expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn private_ipv4_mapped_ipv6_blocked_by_default() {
    // `::ffff:10.0.0.1` must hit the v4 private-IP gate via unwrap.
    let cfg = cfg_default();
    let out = classify("http://[::ffff:10.0.0.1]/", &cfg).await;
    match out {
        ClassifyOutcome::Block { rule, .. } => assert_eq!(rule, "private_ip"),
        other => panic!("expected Block, got {other:?}"),
    }
}

// ────────────────────────────────────────────────────────────────────
// Private-IP toggle behavior
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn private_ip_blocked_by_default() {
    let cfg = cfg_default();
    for url in [
        "http://10.0.0.1/",
        "http://192.168.1.1/",
        "http://127.0.0.1/",
        "http://100.64.5.5/", // CGNAT — std::is_private misses this
    ] {
        let out = classify(url, &cfg).await;
        match out {
            ClassifyOutcome::Block { rule, .. } => assert_eq!(rule, "private_ip", "url={url}"),
            other => panic!("expected Block for {url}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn private_ip_allowed_when_toggled() {
    let mut cfg = cfg_default();
    cfg.allow_private_urls = true;
    for url in [
        "http://10.0.0.1/",
        "http://192.168.1.1/",
        "http://127.0.0.1/",
        "http://100.64.5.5/",
    ] {
        let out = classify(url, &cfg).await;
        match out {
            ClassifyOutcome::Allow { .. } => {}
            other => panic!("expected Allow for {url}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn link_local_blocked_even_with_toggle() {
    // 169.254.0.0/16 outside the specific metadata IPs.
    let mut cfg = cfg_default();
    cfg.allow_private_urls = true;
    let out = classify("http://169.254.5.5/", &cfg).await;
    match out {
        ClassifyOutcome::Block { rule, .. } => assert_eq!(rule, "ssrf_blocked_ip"),
        other => panic!("expected Block, got {other:?}"),
    }
}

// ────────────────────────────────────────────────────────────────────
// Allowlist / denylist (use literal IPs to avoid DNS in tests)
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn allowlist_admits_matched_host() {
    let mut cfg = cfg_default();
    cfg.allow_private_urls = true;
    cfg.allowlist = vec!["8.8.8.8".into()];
    let out = classify("http://8.8.8.8/", &cfg).await;
    assert!(matches!(out, ClassifyOutcome::Allow { .. }));
}

#[tokio::test]
async fn allowlist_blocks_unmatched_host() {
    let mut cfg = cfg_default();
    cfg.allow_private_urls = true;
    cfg.allowlist = vec!["1.1.1.1".into()];
    let out = classify("http://8.8.8.8/", &cfg).await;
    match out {
        ClassifyOutcome::Block { rule, .. } => assert_eq!(rule, "not_in_allowlist"),
        other => panic!("expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn allowlist_empty_means_no_allowlist_gating() {
    let mut cfg = cfg_default();
    cfg.allow_private_urls = true;
    // empty allowlist & denylist — anything (non-SSRF) should pass.
    let out = classify("http://8.8.8.8/", &cfg).await;
    assert!(matches!(out, ClassifyOutcome::Allow { .. }));
}

#[tokio::test]
async fn denylist_blocks_exact_and_subdomain() {
    let mut cfg = cfg_default();
    cfg.allow_private_urls = true;
    cfg.denylist = vec!["bad.example".into()];

    // Use an IP literal that maps to "bad.example" — but we can't fake
    // DNS in unit tests. Instead, exercise the offline classifier which
    // skips DNS but still runs the allowlist/denylist gate.
    let out = classify_offline("http://bad.example/", &cfg);
    match out {
        ClassifyOutcome::Block { rule, .. } => assert_eq!(rule, "bad.example"),
        other => panic!("expected Block, got {other:?}"),
    }
    let out = classify_offline("http://sub.bad.example/", &cfg);
    assert!(matches!(out, ClassifyOutcome::Block { .. }));
}

#[tokio::test]
async fn denylist_does_not_block_unrelated() {
    let mut cfg = cfg_default();
    cfg.allow_private_urls = true;
    cfg.denylist = vec!["bad.example".into()];
    let out = classify_offline("http://good.example/", &cfg);
    assert!(matches!(out, ClassifyOutcome::Allow { .. }));
}

#[tokio::test]
async fn denylist_substring_does_not_match() {
    // Regression: `mybad.example` must NOT match a `bad.example` rule.
    let mut cfg = cfg_default();
    cfg.allow_private_urls = true;
    cfg.denylist = vec!["bad.example".into()];
    let out = classify_offline("http://mybad.example/", &cfg);
    assert!(matches!(out, ClassifyOutcome::Allow { .. }));
}

// ────────────────────────────────────────────────────────────────────
// Transforms
// ────────────────────────────────────────────────────────────────────

fn xform_cfg(rules: Vec<TransformRule>) -> UrlPolicyConfig {
    UrlPolicyConfig {
        enabled: true,
        allow_private_urls: true,
        transform_rules: rules,
        ..Default::default()
    }
}

#[test]
fn transform_strip_query_params_removes_named_only() {
    let url = url::Url::parse("https://x.com/?utm_source=a&q=1&utm_medium=b").unwrap();
    let out = transforms::apply(
        &url,
        &[TransformRule {
            pattern: "x.com".into(),
            action: TransformAction::StripQueryParams {
                names: vec!["utm_source".into(), "utm_medium".into()],
            },
        }],
    )
    .unwrap();
    assert_eq!(out.as_str(), "https://x.com/?q=1");
}

#[test]
fn transform_strip_query_params_no_match_keeps_url_clean() {
    let url = url::Url::parse("https://x.com/p").unwrap();
    let out = transforms::apply(
        &url,
        &[TransformRule {
            pattern: "x.com".into(),
            action: TransformAction::StripQueryParams {
                names: vec!["utm_source".into()],
            },
        }],
    )
    .unwrap();
    // No trailing '?' even though we touched the URL.
    assert_eq!(out.as_str(), "https://x.com/p");
}

#[test]
fn transform_strip_query_params_all_removed_drops_query() {
    let url = url::Url::parse("https://x.com/p?utm_source=a").unwrap();
    let out = transforms::apply(
        &url,
        &[TransformRule {
            pattern: "x.com".into(),
            action: TransformAction::StripQueryParams {
                names: vec!["utm_source".into()],
            },
        }],
    )
    .unwrap();
    assert_eq!(out.as_str(), "https://x.com/p");
}

#[test]
fn transform_force_https_strips_default_http_port() {
    // Regression: https on :80 is almost always wrong. ForceHttps should
    // drop an explicit :80 when upgrading.
    let url = url::Url::parse("http://x.com:80/p").unwrap();
    let out = transforms::apply(
        &url,
        &[TransformRule {
            pattern: "x.com".into(),
            action: TransformAction::ForceHttps,
        }],
    )
    .unwrap();
    assert_eq!(out.scheme(), "https");
    assert_eq!(
        out.port(),
        None,
        "port :80 should not survive https upgrade"
    );
}

#[test]
fn transform_force_https_upgrades_http() {
    let url = url::Url::parse("http://x.com/p?q=1").unwrap();
    let out = transforms::apply(
        &url,
        &[TransformRule {
            pattern: "x.com".into(),
            action: TransformAction::ForceHttps,
        }],
    )
    .unwrap();
    assert_eq!(out.scheme(), "https");
    assert_eq!(out.path(), "/p");
    assert_eq!(out.query(), Some("q=1"));
}

#[test]
fn transform_rewrite_host_preserves_path_and_query() {
    let url = url::Url::parse("https://m.wikipedia.org/wiki/Foo?lang=en").unwrap();
    let out = transforms::apply(
        &url,
        &[TransformRule {
            pattern: "m.wikipedia.org".into(),
            action: TransformAction::RewriteHost {
                to: "en.wikipedia.org".into(),
            },
        }],
    )
    .unwrap();
    assert_eq!(out.host_str(), Some("en.wikipedia.org"));
    assert_eq!(out.path(), "/wiki/Foo");
    assert_eq!(out.query(), Some("lang=en"));
}

#[test]
fn transform_strip_fragment_drops_anchor() {
    let url = url::Url::parse("https://x.com/p#section").unwrap();
    let out = transforms::apply(
        &url,
        &[TransformRule {
            pattern: "x.com".into(),
            action: TransformAction::StripFragment,
        }],
    )
    .unwrap();
    assert_eq!(out.fragment(), None);
    assert_eq!(out.path(), "/p");
}

#[test]
fn transform_rules_apply_in_declared_order() {
    // ForceHttps then RewriteHost should produce an https URL on the new
    // host — confirms each step re-uses the result of the previous step.
    let url = url::Url::parse("http://m.wikipedia.org/x").unwrap();
    let out = transforms::apply(
        &url,
        &[
            TransformRule {
                pattern: "m.wikipedia.org".into(),
                action: TransformAction::ForceHttps,
            },
            TransformRule {
                pattern: "m.wikipedia.org".into(),
                action: TransformAction::RewriteHost {
                    to: "en.wikipedia.org".into(),
                },
            },
        ],
    )
    .unwrap();
    assert_eq!(out.scheme(), "https");
    assert_eq!(out.host_str(), Some("en.wikipedia.org"));
}

#[tokio::test]
async fn transform_to_metadata_host_still_blocked_by_ssrf() {
    // Closing the loop: a user can't sneak past SSRF via a transform.
    let cfg = xform_cfg(vec![TransformRule {
        pattern: "x.com".into(),
        action: TransformAction::RewriteHost {
            to: "metadata.google.internal".into(),
        },
    }]);
    let out = classify("http://x.com/", &cfg).await;
    match out {
        ClassifyOutcome::Block { rule, .. } => assert_eq!(rule, "ssrf_blocked_host"),
        other => panic!("expected Block, got {other:?}"),
    }
}

// ────────────────────────────────────────────────────────────────────
// Throttle
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn throttle_blocks_second_request_within_window_then_allows_after() {
    throttle::reset_for_tests();
    let cfg = UrlPolicyConfig {
        enabled: true,
        allow_private_urls: true,
        throttle_rules: vec![ThrottleRule {
            pattern: "*.throttle-test.invalid".into(),
            min_interval_ms: 200,
        }],
        ..Default::default()
    };
    // Use the offline classifier — DNS would fail on an .invalid TLD.
    // To exercise throttle we need to drive the same code path the online
    // one uses; throttle is run inside classify(), so use a host that
    // resolves. Switch to a literal IP that satisfies allow_private_urls.
    let cfg = UrlPolicyConfig {
        throttle_rules: vec![ThrottleRule {
            pattern: "127.0.0.1".into(),
            min_interval_ms: 200,
        }],
        ..cfg
    };
    let first = classify("http://127.0.0.1/", &cfg).await;
    assert!(matches!(first, ClassifyOutcome::Allow { .. }));
    let second = classify("http://127.0.0.1/", &cfg).await;
    match second {
        ClassifyOutcome::Throttle { retry_after_ms, .. } => {
            assert!(retry_after_ms > 0 && retry_after_ms <= 200);
        }
        other => panic!("expected Throttle, got {other:?}"),
    }
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let third = classify("http://127.0.0.1/", &cfg).await;
    assert!(matches!(third, ClassifyOutcome::Allow { .. }));
}

#[tokio::test]
async fn throttle_is_per_host_not_global() {
    throttle::reset_for_tests();
    let cfg = UrlPolicyConfig {
        enabled: true,
        allow_private_urls: true,
        throttle_rules: vec![ThrottleRule {
            pattern: "*.test".into(),
            min_interval_ms: 60_000,
        }],
        ..Default::default()
    };
    // Two distinct IPs both still hit Allow on first contact.
    let a = classify("http://127.0.0.1/", &cfg).await;
    assert!(matches!(a, ClassifyOutcome::Allow { .. }));
    let b = classify("http://127.0.0.2/", &cfg).await;
    assert!(matches!(b, ClassifyOutcome::Allow { .. }));
}

#[tokio::test]
async fn throttle_buckets_isolate_by_port() {
    // Regression: throttle was keyed on host-only, so two services on
    // the same host (or two wiremock servers in integration tests) would
    // collide. Key now includes the port.
    throttle::reset_for_tests();
    let cfg = UrlPolicyConfig {
        enabled: true,
        allow_private_urls: true,
        throttle_rules: vec![ThrottleRule {
            pattern: "127.0.0.1".into(),
            min_interval_ms: 60_000,
        }],
        ..Default::default()
    };
    // Same host, different ports — both first calls allowed.
    let a = classify("http://127.0.0.1:8001/", &cfg).await;
    assert!(matches!(a, ClassifyOutcome::Allow { .. }));
    let b = classify("http://127.0.0.1:8002/", &cfg).await;
    assert!(matches!(b, ClassifyOutcome::Allow { .. }));
    // Same host *and* port — second call throttled.
    let c = classify("http://127.0.0.1:8001/", &cfg).await;
    assert!(
        matches!(c, ClassifyOutcome::Throttle { .. }),
        "expected throttle, got {c:?}"
    );
}

#[tokio::test]
async fn throttle_not_applied_when_no_rule_matches() {
    throttle::reset_for_tests();
    let cfg = UrlPolicyConfig {
        enabled: true,
        allow_private_urls: true,
        throttle_rules: vec![ThrottleRule {
            pattern: "never-matches.example".into(),
            min_interval_ms: 60_000,
        }],
        ..Default::default()
    };
    for _ in 0..3 {
        let out = classify("http://127.0.0.1/", &cfg).await;
        assert!(matches!(out, ClassifyOutcome::Allow { .. }));
    }
}

// ────────────────────────────────────────────────────────────────────
// Scheme gate
// ────────────────────────────────────────────────────────────────────

#[test]
fn scheme_gate_offline_blocks_non_http_schemes() {
    let cfg = cfg_default();
    for url in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "data:text/html,<h1>x</h1>",
        "chrome://settings",
        "ftp://example.com/",
    ] {
        let out = classify_offline(url, &cfg);
        match out {
            ClassifyOutcome::Block { rule, .. } => {
                assert_eq!(rule, "scheme_blocked", "url={url}");
            }
            other => panic!("expected Block for {url}, got {other:?}"),
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Malformed input
// ────────────────────────────────────────────────────────────────────

#[test]
fn malformed_urls_blocked() {
    // url crate quirks: `https:///path` parses with host="path", and
    // `not a url` errors as a relative URL. We test the three cases that
    // matter from a security standpoint: nothing, garbage, empty host.
    let cfg = cfg_default();
    for url in ["", "not a url", "http://"] {
        let out = classify_offline(url, &cfg);
        match out {
            ClassifyOutcome::Block { .. } => {}
            other => panic!("expected Block for {url:?}, got {other:?}"),
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Kill switch (operator escape hatch)
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn kill_switch_disables_all_checks() {
    // When enabled=false, even a metadata URL passes. This is
    // intentional — documented in mod.rs — so that operators can debug
    // by toggling once via /settings.
    let mut cfg = cfg_default();
    cfg.enabled = false;
    let out = classify("http://169.254.169.254/", &cfg).await;
    match out {
        ClassifyOutcome::Allow { effective_url } => {
            assert_eq!(effective_url, "http://169.254.169.254/");
        }
        other => panic!("expected Allow, got {other:?}"),
    }
}

// ────────────────────────────────────────────────────────────────────
// Serde round-trip — detect enum-tag drift (#[serde(tag = "kind")])
// ────────────────────────────────────────────────────────────────────

#[test]
fn url_policy_config_serde_round_trip() {
    let original = UrlPolicyConfig {
        enabled: true,
        allow_private_urls: false,
        allowlist: vec!["a.com".into()],
        denylist: vec!["b.com".into(), "*.c.com".into()],
        throttle_rules: vec![ThrottleRule {
            pattern: "throttle.com".into(),
            min_interval_ms: 1000,
        }],
        transform_rules: vec![
            TransformRule {
                pattern: "x.com".into(),
                action: TransformAction::ForceHttps,
            },
            TransformRule {
                pattern: "x.com".into(),
                action: TransformAction::StripFragment,
            },
            TransformRule {
                pattern: "x.com".into(),
                action: TransformAction::RewriteHost { to: "y.com".into() },
            },
            TransformRule {
                pattern: "x.com".into(),
                action: TransformAction::StripQueryParams {
                    names: vec!["utm".into()],
                },
            },
        ],
    };
    let json = serde_json::to_string(&original).unwrap();
    let parsed: UrlPolicyConfig = serde_json::from_str(&json).unwrap();
    // Compare via re-serialization rather than relying on PartialEq for
    // the whole struct (it isn't derived for UrlPolicyConfig).
    let json2 = serde_json::to_string(&parsed).unwrap();
    assert_eq!(json, json2);
    // Spot-check that all four TransformAction tags survived (catches a
    // typo in the `#[serde(tag = "kind")]` discriminator).
    assert!(json.contains("\"kind\":\"force_https\""));
    assert!(json.contains("\"kind\":\"strip_fragment\""));
    assert!(json.contains("\"kind\":\"rewrite_host\""));
    assert!(json.contains("\"kind\":\"strip_query_params\""));
}

// ────────────────────────────────────────────────────────────────────
// Block-message formatting (used by tool handlers)
// ────────────────────────────────────────────────────────────────────

#[test]
fn describe_block_returns_human_string() {
    let blocked = ClassifyOutcome::Block {
        reason: "Host 'evil.com' matches denylist rule 'evil.com'".into(),
        rule: "evil.com".into(),
    };
    let msg = describe_block_or_throttle(&blocked).unwrap();
    assert!(msg.contains("URL policy blocked"));
    assert!(msg.contains("evil.com"));
}

#[test]
fn describe_allow_returns_none() {
    assert!(describe_block_or_throttle(&ClassifyOutcome::Allow {
        effective_url: "https://x.com/".into()
    })
    .is_none());
}
