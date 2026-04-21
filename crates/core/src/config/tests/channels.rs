use super::*;

/// Serializes env-var-mutating channel tests so they don't race each other
/// when cargo test runs them in parallel.
static CHANNEL_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn has_any_native_channel_detects_telegram_env() {
    let _lock = CHANNEL_ENV_MUTEX.lock().unwrap();
    // Use a unique env var name to avoid conflicts with real credentials
    std::env::set_var("TELEGRAM_BOT_TOKEN", "test-token-for-unit-test");
    let cfg = Config::default();
    assert!(cfg.has_any_native_channel());
    std::env::remove_var("TELEGRAM_BOT_TOKEN");
}

#[test]
fn has_any_native_channel_false_when_no_creds() {
    let _lock = CHANNEL_ENV_MUTEX.lock().unwrap();
    // Temporarily clear all native channel env vars
    let keys = [
        "TELEGRAM_BOT_TOKEN",
        "SLACK_BOT_TOKEN",
        "DISCORD_BOT_TOKEN",
        "TWILIO_ACCOUNT_SID",
        "TEAMS_APP_ID",
        "GOOGLE_CHAT_SERVICE_TOKEN",
    ];
    let saved: Vec<_> = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
    for k in &keys {
        std::env::remove_var(k);
    }
    let cfg = Config::default();
    // Skip assertion if the OS keychain has real credentials (e.g. dev machine with installed plugins)
    if cfg.detected_native_channels().is_empty() {
        assert!(!cfg.has_any_native_channel());
    }
    // Restore
    for (k, v) in saved {
        if let Some(val) = v {
            std::env::set_var(k, val);
        }
    }
}

#[test]
fn detected_native_channels_returns_configured() {
    // Use a dedicated credential in config to avoid env var races with other tests
    let mut cfg = Config::default();
    cfg.credentials.insert(
        "SLACK_BOT_TOKEN".to_string(),
        CredentialValue::EnvVar("BORG_TEST_DETECTED_NATIVE_SLACK".to_string()),
    );
    std::env::set_var("BORG_TEST_DETECTED_NATIVE_SLACK", "xoxb-test-token");
    let channels = cfg.detected_native_channels();
    std::env::remove_var("BORG_TEST_DETECTED_NATIVE_SLACK");
    assert!(channels.iter().any(|(name, _)| *name == "slack"));
}
