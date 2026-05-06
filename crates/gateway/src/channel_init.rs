//! Channel client initialization extracted from `GatewayServer::run`.
//!
//! Each function resolves credentials, creates a client, runs a probe/auth check,
//! and returns the client (if successful) plus any metadata fields.

use std::sync::Arc;

use cortex::config::Config;
use cortex::db::Database;
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
fn resolve_credential(config: &Config, _plugin_id: &str, key: &str) -> Option<String> {
    config.resolve_credential_or_env(key)
}

fn log_gateway_activity(message: &str) {
    if let Ok(adb) = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS) {
        cortex::activity_log::log_activity(&adb, "info", "gateway", message);
    }
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
                    log_gateway_activity("Telegram native integration active");
                    let bot_username = me.username.clone();

                    // Set webhook if public_url is configured
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

                    // Register bot command menu
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
                    log_gateway_activity("Slack native integration active");
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

                    // Register global slash commands
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
        // Set via env var (simulating config resolution)
        unsafe { std::env::set_var(key, "config-value") };
        config.credentials.insert(
            key.to_string(),
            cortex::config::CredentialValue::EnvVar(key.to_string()),
        );
        let result = resolve_credential(&config, "messaging/test", key);
        unsafe { std::env::remove_var(key) };
        assert_eq!(result.as_deref(), Some("config-value"));
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
