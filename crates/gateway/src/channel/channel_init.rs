//! Channel client initialization extracted from `GatewayServer::run`.
//!
//! Each function resolves credentials, creates a client, runs a probe/auth check,
//! and returns the client (if successful) plus any metadata fields.

use std::sync::Arc;

use cortex::config::Config;
use tracing::{info, warn};

use crate::discord::api::DiscordClient;
use crate::google_chat::api::GoogleChatClient;
use crate::signal::api::SignalClient;
use crate::slack::api::SlackClient;
use crate::teams::api::TeamsClient;
use crate::telegram::api::TelegramClient;
use crate::twilio::api::TwilioClient;

/// All credential keys this module reads via `resolve_credential`.
/// Every entry must be present in `Config::KEY_PLUGIN_MAP` (asserted in tests
/// below) so the OS-keychain fallback can locate it.
#[allow(dead_code)] // referenced from tests only; documents the contract
pub(crate) const GATEWAY_CREDENTIAL_KEYS: &[&str] = &[
    "TELEGRAM_BOT_TOKEN",
    "SLACK_BOT_TOKEN",
    "SLACK_SIGNING_SECRET",
    "TWILIO_ACCOUNT_SID",
    "TWILIO_AUTH_TOKEN",
    "TWILIO_PHONE_NUMBER",
    "TWILIO_WHATSAPP_NUMBER",
    "DISCORD_BOT_TOKEN",
    "DISCORD_PUBLIC_KEY",
    "TEAMS_APP_ID",
    "TEAMS_APP_SECRET",
    "GOOGLE_CHAT_SERVICE_TOKEN",
    "GOOGLE_CHAT_WEBHOOK_TOKEN",
    "SIGNAL_ACCOUNT",
];

/// Resolve a credential for a channel plugin.
///
/// Delegates to `Config::resolve_credential_or_env` which handles config store,
/// env var, and OS keychain fallback (using the plugin naming convention).
pub(crate) fn resolve_credential(config: &Config, _plugin_id: &str, key: &str) -> Option<String> {
    config.resolve_credential_or_env(key)
}

async fn log_gateway_activity(message: &str) {
    crate::activity::log_gateway_activity("info", message).await;
}

/// Initialize Telegram client. Returns (client, bot_username).
pub(crate) async fn init_telegram(
    config: &Config,
) -> (Option<Arc<TelegramClient>>, Option<String>, Option<String>) {
    let telegram_token = resolve_credential(config, "messaging/telegram", "TELEGRAM_BOT_TOKEN");
    let telegram_secret = config.resolve_credential_or_env("TELEGRAM_WEBHOOK_SECRET");

    let (client, bot_username) = match telegram_token {
        Some(token) => {
            let client = match TelegramClient::new(&token) {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to create Telegram client: {e}");
                    return (None, None, telegram_secret);
                }
            };
            match client.get_me().await {
                Ok(me) => {
                    info!(
                        "Telegram native integration active (bot: @{})",
                        me.username.as_deref().unwrap_or(&me.first_name)
                    );
                    log_gateway_activity("Telegram native integration active").await;
                    let bot_username = me.username.clone();

                    if let Some(ref url) = config.gateway.public_url {
                        let webhook_url = format!("{url}/webhook/telegram");
                        if let Err(e) = client
                            .set_webhook(&webhook_url, telegram_secret.as_deref())
                            .await
                        {
                            warn!("Failed to set Telegram webhook: {e}");
                        } else {
                            info!("Telegram webhook set to {webhook_url}");
                        }
                    }

                    {
                        use crate::commands::{NativeCommandRegistration, GATEWAY_COMMANDS};
                        if let Err(e) = client.register_commands(GATEWAY_COMMANDS).await {
                            warn!("Failed to register Telegram bot commands: {e}");
                        } else {
                            info!("Telegram bot menu commands registered");
                        }
                    }

                    (Some(Arc::new(client)), bot_username)
                }
                Err(e) => {
                    warn!("TELEGRAM_BOT_TOKEN set but getMe failed: {e}");
                    (None, None)
                }
            }
        }
        None => {
            if config.credentials.contains_key("TELEGRAM_BOT_TOKEN") {
                warn!("TELEGRAM_BOT_TOKEN is configured but could not be resolved — check keychain access");
            }
            (None, None)
        }
    };

    (client, bot_username, telegram_secret)
}

/// Initialize Slack client. Returns (client, signing_secret, bot_user_id).
pub(crate) async fn init_slack(
    config: &Config,
) -> (Option<Arc<SlackClient>>, Option<String>, Option<String>) {
    let slack_token = resolve_credential(config, "messaging/slack", "SLACK_BOT_TOKEN");
    let slack_signing_secret =
        resolve_credential(config, "messaging/slack", "SLACK_SIGNING_SECRET");

    let (client, bot_user_id) = match slack_token {
        Some(token) => match SlackClient::new(&token) {
            Ok(mut client) => match client.auth_test().await {
                Ok(resp) => {
                    info!(
                        "Slack native integration active (bot: {}, team: {})",
                        resp.user.as_deref().unwrap_or("unknown"),
                        resp.team.as_deref().unwrap_or("unknown"),
                    );
                    log_gateway_activity("Slack native integration active").await;
                    let bot_uid = client.bot_user_id().map(String::from);
                    (Some(Arc::new(client)), bot_uid)
                }
                Err(e) => {
                    warn!("SLACK_BOT_TOKEN set but auth.test failed: {e}");
                    (None, None)
                }
            },
            Err(e) => {
                warn!("Failed to create Slack HTTP client: {e}");
                (None, None)
            }
        },
        None => (None, None),
    };

    (client, slack_signing_secret, bot_user_id)
}

/// Initialize Twilio client. Returns (client, auth_token, phone_number, whatsapp_number).
pub(crate) fn init_twilio(
    config: &Config,
) -> (
    Option<Arc<TwilioClient>>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let twilio_account_sid = resolve_credential(config, "messaging/whatsapp", "TWILIO_ACCOUNT_SID");
    let twilio_auth_token = resolve_credential(config, "messaging/whatsapp", "TWILIO_AUTH_TOKEN");
    let twilio_phone_number =
        resolve_credential(config, "messaging/whatsapp", "TWILIO_PHONE_NUMBER");
    let twilio_whatsapp_number =
        resolve_credential(config, "messaging/whatsapp", "TWILIO_WHATSAPP_NUMBER");

    let client = match (&twilio_account_sid, &twilio_auth_token) {
        (Some(sid), Some(token)) => {
            info!(
                "Twilio native integration active (account: {}...)",
                &sid[..sid.len().min(8)]
            );
            match TwilioClient::new(sid, token) {
                Ok(client) => Some(Arc::new(client)),
                Err(e) => {
                    warn!("Failed to create Twilio HTTP client: {e}");
                    None
                }
            }
        }
        _ => None,
    };

    (
        client,
        twilio_auth_token,
        twilio_phone_number,
        twilio_whatsapp_number,
    )
}

/// Initialize Discord client. Returns (client, public_key).
pub(crate) async fn init_discord(config: &Config) -> (Option<Arc<DiscordClient>>, Option<String>) {
    let discord_token = resolve_credential(config, "messaging/discord", "DISCORD_BOT_TOKEN");
    let discord_public_key = resolve_credential(config, "messaging/discord", "DISCORD_PUBLIC_KEY");

    let client = match discord_token {
        Some(token) => {
            let mut client = match DiscordClient::new(&token) {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to create Discord client: {e}");
                    return (None, discord_public_key);
                }
            };
            match client.get_current_user().await {
                Ok(user) => {
                    info!("Discord native integration active (bot: {})", user.username);
                    client.set_application_id(user.id.clone());

                    {
                        use crate::commands::{NativeCommandRegistration, GATEWAY_COMMANDS};
                        if let Err(e) = client.register_commands(GATEWAY_COMMANDS).await {
                            warn!("Failed to register Discord slash commands: {e}");
                        } else {
                            info!("Discord slash commands registered");
                        }
                    }

                    Some(Arc::new(client))
                }
                Err(e) => {
                    warn!("DISCORD_BOT_TOKEN set but /users/@me failed: {e}");
                    None
                }
            }
        }
        None => None,
    };

    (client, discord_public_key)
}

/// Initialize Teams client. Returns (client, app_secret).
pub(crate) fn init_teams(
    config: &Config,
) -> anyhow::Result<(Option<Arc<TeamsClient>>, Option<String>)> {
    let teams_app_id = resolve_credential(config, "messaging/teams", "TEAMS_APP_ID");
    let teams_app_secret = resolve_credential(config, "messaging/teams", "TEAMS_APP_SECRET");

    let client = match (&teams_app_id, &teams_app_secret) {
        (Some(app_id), Some(app_secret)) => {
            info!(
                "Teams native integration active (app: {}...)",
                &app_id[..app_id.len().min(8)]
            );
            Some(Arc::new(TeamsClient::new(app_id, app_secret)?))
        }
        _ => None,
    };

    Ok((client, teams_app_secret))
}

/// Initialize Google Chat client. Returns (client, webhook_token).
pub(crate) fn init_google_chat(
    config: &Config,
) -> anyhow::Result<(Option<Arc<GoogleChatClient>>, Option<String>)> {
    let google_chat_service_token =
        resolve_credential(config, "messaging/google-chat", "GOOGLE_CHAT_SERVICE_TOKEN");
    let google_chat_token =
        resolve_credential(config, "messaging/google-chat", "GOOGLE_CHAT_WEBHOOK_TOKEN");

    let client = match google_chat_service_token {
        Some(token) => {
            info!("Google Chat native integration active");
            Some(Arc::new(GoogleChatClient::new(&token)?))
        }
        None => None,
    };

    Ok((client, google_chat_token))
}

/// Initialize Signal client.
pub(crate) async fn init_signal(config: &Config) -> Option<Arc<SignalClient>> {
    let signal_account = resolve_credential(config, "messaging/signal", "SIGNAL_ACCOUNT");
    match signal_account {
        Some(ref account) => {
            let host = config
                .gateway
                .signal_cli_host
                .as_deref()
                .unwrap_or("localhost");
            let port = config
                .gateway
                .signal_cli_port
                .unwrap_or(crate::constants::SIGNAL_CLI_PORT_DEFAULT);
            let base_url = format!("http://{host}:{port}");
            match SignalClient::new(&base_url, account) {
                Ok(client) => match client.probe().await {
                    Ok(version) => {
                        info!(
                            "Signal native integration active (account: {}, daemon: v{})",
                            account, version
                        );
                        Some(Arc::new(client))
                    }
                    Err(e) => {
                        warn!("SIGNAL_ACCOUNT set but signal-cli daemon not reachable: {e}");
                        None
                    }
                },
                Err(e) => {
                    warn!("Failed to create Signal client: {e}");
                    None
                }
            }
        }
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_credential_falls_back_to_env() {
        let config = Config::default();
        // No credential in config, no keychain entry, but env var set
        let key = "BORG_TEST_CHANNEL_INIT_ENV_CRED";
        unsafe { std::env::set_var(key, "test-value") };
        let result = resolve_credential(&config, "messaging/test", key);
        unsafe { std::env::remove_var(key) };
        assert_eq!(result.as_deref(), Some("test-value"));
    }

    #[test]
    fn resolve_credential_returns_none_when_missing() {
        let config = Config::default();
        let result = resolve_credential(
            &config,
            "messaging/nonexistent",
            "BORG_TEST_NONEXISTENT_CRED_12345",
        );
        assert!(result.is_none());
    }

    #[test]
    fn resolve_credential_config_takes_priority_over_keychain() {
        let mut config = Config::default();
        let key = "BORG_TEST_CHANNEL_INIT_CONFIG_CRED";
        unsafe { std::env::set_var(key, "config-value") };
        config.credentials.insert(
            key.to_string(),
            cortex::config::CredentialValue::EnvVar(key.to_string()),
        );
        let result = resolve_credential(&config, "messaging/test", key);
        unsafe { std::env::remove_var(key) };
        assert_eq!(result.as_deref(), Some("config-value"));
    }

    /// Builds a config whose credential store maps each `(credential key,
    /// value)` pair through a uniquely named env var, so parallel tests
    /// never race on the real credential names. Callers hold the
    /// `borg_home_guard` and point `BORG_HOME` at a temp dir first, so the
    /// on-disk credentials-file fallback for *absent* keys stays hermetic.
    fn config_with_creds(pairs: &[(&str, &str)]) -> Config {
        let mut config = Config::default();
        for (key, value) in pairs {
            let env_name = format!("BORG_TEST_CHINIT_{key}");
            unsafe { std::env::set_var(&env_name, value) };
            config.credentials.insert(
                (*key).to_string(),
                cortex::config::CredentialValue::EnvVar(env_name),
            );
        }
        config
    }

    fn clear_cred_env(pairs: &[(&str, &str)]) {
        for (key, _) in pairs {
            unsafe { std::env::remove_var(format!("BORG_TEST_CHINIT_{key}")) };
        }
    }

    struct BorgHomeTemp {
        _tmp: tempfile::TempDir,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    fn hermetic_borg_home() -> BorgHomeTemp {
        let guard = crate::test_env::borg_home_guard();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("BORG_HOME", tmp.path()) };
        BorgHomeTemp {
            _tmp: tmp,
            _guard: guard,
        }
    }

    impl Drop for BorgHomeTemp {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("BORG_HOME") };
        }
    }

    /// With no bot token, no client is built — but the webhook secret must
    /// still pass through, or webhook verification silently disarms for a
    /// user who configured the secret before the token.
    #[tokio::test]
    async fn init_telegram_without_token_still_returns_webhook_secret() {
        let _home = hermetic_borg_home();
        let creds = [("TELEGRAM_WEBHOOK_SECRET", "tg-hook-secret-91")];
        let config = config_with_creds(&creds);

        let (client, bot_username, secret) = init_telegram(&config).await;
        clear_cred_env(&creds);

        assert!(client.is_none(), "no token must mean no client");
        assert!(bot_username.is_none());
        assert_eq!(secret.as_deref(), Some("tg-hook-secret-91"));
    }

    /// Same pass-through contract for Slack: the signing secret gates
    /// webhook verification independently of whether the API client exists.
    #[tokio::test]
    async fn init_slack_without_token_still_returns_signing_secret() {
        let _home = hermetic_borg_home();
        let creds = [("SLACK_SIGNING_SECRET", "slack-sign-77")];
        let config = config_with_creds(&creds);

        let (client, signing_secret, bot_user_id) = init_slack(&config).await;
        clear_cred_env(&creds);

        assert!(client.is_none(), "no token must mean no client");
        assert_eq!(signing_secret.as_deref(), Some("slack-sign-77"));
        assert!(bot_user_id.is_none());
    }

    /// Discord's public key verifies interaction signatures — it must
    /// resolve even when the bot token (and thus the client) is absent.
    #[tokio::test]
    async fn init_discord_without_token_still_returns_public_key() {
        let _home = hermetic_borg_home();
        let creds = [("DISCORD_PUBLIC_KEY", "disc-pub-abc")];
        let config = config_with_creds(&creds);

        let (client, public_key) = init_discord(&config).await;
        clear_cred_env(&creds);

        assert!(client.is_none(), "no token must mean no client");
        assert_eq!(public_key.as_deref(), Some("disc-pub-abc"));
    }

    /// Twilio needs BOTH the account SID and the auth token to build a
    /// client; the three metadata fields pass through regardless so webhook
    /// verification and outbound numbers survive a partial configuration.
    #[test]
    fn init_twilio_requires_sid_and_token_and_passes_metadata_through() {
        let _home = hermetic_borg_home();
        let full = [
            ("TWILIO_ACCOUNT_SID", "ACtestsid1234"),
            ("TWILIO_AUTH_TOKEN", "tw-auth-3"),
            ("TWILIO_PHONE_NUMBER", "+15550001111"),
            ("TWILIO_WHATSAPP_NUMBER", "+15550002222"),
        ];
        let config = config_with_creds(&full);
        let (client, auth, phone, whatsapp) = init_twilio(&config);
        assert!(client.is_some(), "sid + token must build a client");
        assert_eq!(auth.as_deref(), Some("tw-auth-3"));
        assert_eq!(phone.as_deref(), Some("+15550001111"));
        assert_eq!(whatsapp.as_deref(), Some("+15550002222"));
        clear_cred_env(&full);

        // Token without SID: no client, but the metadata still resolves.
        let partial = [
            ("TWILIO_AUTH_TOKEN", "tw-auth-3"),
            ("TWILIO_PHONE_NUMBER", "+15550001111"),
        ];
        let config = config_with_creds(&partial);
        let (client, auth, phone, _whatsapp) = init_twilio(&config);
        clear_cred_env(&partial);
        assert!(client.is_none(), "missing SID must not build a client");
        assert_eq!(auth.as_deref(), Some("tw-auth-3"));
        assert_eq!(phone.as_deref(), Some("+15550001111"));
    }

    /// Teams needs both the app ID and secret for a client; the secret is
    /// returned either way because webhook JWT validation uses it directly.
    #[test]
    fn init_teams_builds_client_only_with_both_credentials() {
        let _home = hermetic_borg_home();
        let full = [
            ("TEAMS_APP_ID", "teams-app-01"),
            ("TEAMS_APP_SECRET", "teams-sec-5"),
        ];
        let config = config_with_creds(&full);
        let (client, secret) = init_teams(&config).expect("init_teams must not error");
        assert!(client.is_some(), "app id + secret must build a client");
        assert_eq!(secret.as_deref(), Some("teams-sec-5"));
        clear_cred_env(&full);

        let partial = [("TEAMS_APP_ID", "teams-app-01")];
        let config = config_with_creds(&partial);
        let (client, secret) = init_teams(&config).expect("init_teams must not error");
        clear_cred_env(&partial);
        assert!(client.is_none(), "missing secret must not build a client");
        assert!(secret.is_none());
    }

    /// Google Chat builds its client from the service token; the separate
    /// webhook token passes through for inbound verification.
    #[test]
    fn init_google_chat_builds_client_and_returns_webhook_token() {
        let _home = hermetic_borg_home();
        let creds = [
            ("GOOGLE_CHAT_SERVICE_TOKEN", "gchat-svc-1"),
            ("GOOGLE_CHAT_WEBHOOK_TOKEN", "gchat-hook-9"),
        ];
        let config = config_with_creds(&creds);
        let (client, webhook_token) =
            init_google_chat(&config).expect("init_google_chat must not error");
        clear_cred_env(&creds);
        assert!(client.is_some(), "service token must build a client");
        assert_eq!(webhook_token.as_deref(), Some("gchat-hook-9"));
    }

    /// The module-local wrapper must actually land an info-level
    /// `"gateway"`-category activity row — it is the operator's only
    /// visibility into which channels came up.
    #[tokio::test]
    async fn log_gateway_activity_wrapper_records_info_gateway_row() {
        let _home = hermetic_borg_home();

        log_gateway_activity("channel wiring probe 4711").await;

        let db = cortex::db::Database::open_with_timeout(
            cortex::db::Database::GATEWAY_BUSY_TIMEOUT_MS,
        )
        .await
        .unwrap();
        let entries = db
            .query_activity(10, Some("info"), Some("gateway"))
            .await
            .unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.message == "channel wiring probe 4711"),
            "expected the wrapper to write an info/gateway activity row"
        );
    }

    /// Every credential key the gateway resolves must have a plugin-ID
    /// mapping in `Config::KEY_PLUGIN_MAP`, so the OS-keychain fallback can
    /// derive a service name. Drift here means a channel silently loses its
    /// keychain fallback when env + config-store both miss.
    #[test]
    fn every_gateway_key_maps_to_a_plugin() {
        for key in GATEWAY_CREDENTIAL_KEYS {
            assert!(
                Config::keychain_plugin_for_key(key).is_some(),
                "gateway credential {key} has no entry in Config::KEY_PLUGIN_MAP"
            );
        }
    }

    /// All keychain plugin IDs follow the `messaging/<channel>` namespace —
    /// this is the contract `resolve_keychain_fallback` and the catalog rely
    /// on for service-name derivation (`borg-messaging-<channel>`).
    #[test]
    fn keychain_plugin_ids_are_namespaced() {
        for (key, plugin_id) in Config::KEY_PLUGIN_MAP {
            assert!(
                plugin_id.starts_with("messaging/"),
                "key {key} maps to non-messaging plugin id {plugin_id}"
            );
        }
    }
}
