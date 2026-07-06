//! Regression tests for the `browser.*` settings registry entries.
//!
//! These keys drive the browser-automation backend/engine picker and the cloud
//! provider credentials surfaced in the `/settings` TUI. If a macro arm is
//! dropped in a refactor, `apply_setting`/extract silently stops round-tripping
//! and the TUI can no longer configure a backend. The round-trips below fail
//! loudly if that happens.

use super::*;
use crate::config::{BrowserBackend, BrowserEngine};
use crate::settings::SETTING_REGISTRY;

fn extract(cfg: &Config, key: &str) -> String {
    let ext = SETTING_REGISTRY
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, ext)| ext)
        .unwrap_or_else(|| panic!("key `{key}` must be in SETTING_REGISTRY"));
    ext(cfg)
}

#[test]
fn backend_round_trips_through_apply_setting() {
    let mut cfg = Config::default();
    for wire in [
        "browserbase",
        "browser_use",
        "firecrawl",
        "cdp_attach",
        "local",
    ] {
        cfg.apply_setting("browser.backend", wire)
            .expect("registered enum key must apply cleanly");
        assert_eq!(extract(&cfg, "browser.backend"), wire);
    }
    assert_eq!(cfg.browser.backend, BrowserBackend::Local);
}

#[test]
fn engine_round_trips_through_apply_setting() {
    let mut cfg = Config::default();
    for wire in [
        "brave",
        "edge",
        "chromium",
        "lightpanda",
        "custom",
        "chrome",
    ] {
        cfg.apply_setting("browser.engine", wire)
            .expect("registered enum key must apply cleanly");
        assert_eq!(extract(&cfg, "browser.engine"), wire);
    }
    assert_eq!(cfg.browser.engine, BrowserEngine::Chrome);
}

#[test]
fn malformed_backend_errors_via_registered_path() {
    let mut cfg = Config::default();
    let err = cfg
        .apply_setting("browser.backend", "aws_lambda")
        .expect_err("unknown backend must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("browser backend") && !msg.contains("Unknown setting"),
        "expected a typed enum parse error, got: {msg}"
    );
}

#[test]
fn browserbase_api_key_sets_and_clears() {
    let mut cfg = Config::default();
    assert!(cfg.browser.browserbase_api_key.is_none());

    // A JSON SecretRef sets the key; extract round-trips it as JSON.
    let secret = r#"{"source":"env","var":"BROWSERBASE_API_KEY"}"#;
    cfg.apply_setting("browser.browserbase_api_key", secret)
        .expect("valid SecretRef JSON must apply");
    assert!(cfg.browser.browserbase_api_key.is_some());
    let round = extract(&cfg, "browser.browserbase_api_key");
    assert!(round.contains("BROWSERBASE_API_KEY"), "got: {round}");

    // Empty string clears back to None.
    cfg.apply_setting("browser.browserbase_api_key", "")
        .expect("empty value clears");
    assert!(cfg.browser.browserbase_api_key.is_none());
    assert_eq!(extract(&cfg, "browser.browserbase_api_key"), "");
}

#[test]
fn browser_use_and_firecrawl_keys_are_wired() {
    let mut cfg = Config::default();
    let secret = r#"{"source":"env","var":"K"}"#;
    for key in ["browser.browser_use_api_key", "browser.firecrawl_api_key"] {
        cfg.apply_setting(key, secret)
            .unwrap_or_else(|e| panic!("{key} must apply: {e:#}"));
        assert!(!extract(&cfg, key).is_empty(), "{key} must round-trip");
    }
    assert!(cfg.browser.browser_use_api_key.is_some());
    assert!(cfg.browser.firecrawl_api_key.is_some());
}

#[test]
fn scalar_browser_fields_round_trip() {
    let mut cfg = Config::default();

    cfg.apply_setting("browser.browserbase_project_id", "proj-42")
        .expect("opt_string applies");
    assert_eq!(
        cfg.browser.browserbase_project_id.as_deref(),
        Some("proj-42")
    );

    cfg.apply_setting("browser.browserbase_proxies", "false")
        .expect("bool applies");
    assert!(!cfg.browser.browserbase_proxies);

    cfg.apply_setting("browser.firecrawl_browser_ttl_secs", "900")
        .expect("u64 applies");
    assert_eq!(cfg.browser.firecrawl_browser_ttl_secs, 900);

    cfg.apply_setting("browser.firecrawl_base_url", "https://fc.example")
        .expect("opt_string applies");
    assert_eq!(
        cfg.browser.firecrawl_base_url.as_deref(),
        Some("https://fc.example")
    );
}
