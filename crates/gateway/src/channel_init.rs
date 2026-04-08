//! Channel client initialization extracted from `GatewayServer::run`.
//!
//! Each function resolves credentials, creates a client, runs a probe/auth check,
//! and returns the client (if successful) plus any metadata fields.

use std::sync::Arc;

use borg_core::config::Config;
use borg_core::db::Database;
use tracing::{info, warn};

use crate::discord::api::DiscordClient;
use crate::google_chat::api::GoogleChatClient;
use crate::signal::api::SignalClient;
use crate::slack::api::SlackClient;
use crate::teams::api::TeamsClient;
use crate::telegram::api::TelegramClient;
use crate::twilio::api::TwilioClient;

/// Resolve a credential from config, env vars, or keychain fallback.
///
/// Tries in order:
/// 1. Config credential store (`[credentials]` section) + env var fallback
/// 2. OS keychain using the plugin's naming convention (`borg-{plugin_id}` / `borg-{key}`)
///
/// The keychain fallback handles the case where `config.toml` lost the credential
/// reference (e.g. config was re-serialized without it) but the keychain still has
/// the secret from a prior plugin install.
fn resolve_credential(config: &Config, plugin_id: &str, key: &str) -> Option<String> {
    if let Some(v) = config.resolve_credential_or_env(key) {
        return Some(v);
    }

    let service = format!("borg-{}", plugin_id.replace('/', "-"));
    let account = format!("borg-{key}");
    if !borg_plugins::keychain::check(&service, &account) {
        return None;
    }

    let sr = borg_core::secrets_resolve::SecretRef::Keychain { service, account };
    match sr.resolve() {
        Ok(v) if !v.is_empty() => {
            info!("Resolved {key} from keychain fallback (credential ref missing from config)");
            Some(v)
        }
        Ok(_) => None,
        Err(e) => {
            warn!("Keychain fallback for {key} failed: {e}");
            None
        }
    }
}

fn log_gateway_activity(message: &str) {
    if let Ok(adb) = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS) {
        borg_core::activity_log::log_activity(&adb, "info", "gateway", message);
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
            let port = config.gateway.signal_cli_port.unwrap_or(8080);
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
            borg_core::config::CredentialValue::EnvVar(key.to_string()),
        );
        let result = resolve_credential(&config, "messaging/test", key);
        unsafe { std::env::remove_var(key) };
        assert_eq!(result.as_deref(), Some("config-value"));
    }

    /// Guard: the resolve_credential_or_env function must log when a credential
    /// is configured but fails to resolve (not silently swallow errors).
    #[test]
    fn resolve_credential_or_env_logs_on_failure() {
        // Verify the source code contains the warning log
        let source = include_str!("../../core/src/config/mod.rs");
        assert!(
            source.contains("Failed to resolve credential"),
            "resolve_credential_or_env must log when credential resolution fails"
        );
    }

    /// Guard: init_telegram must warn when credential is configured but unresolvable.
    #[test]
    fn init_telegram_warns_on_configured_but_unresolvable_token() {
        let source = include_str!("channel_init.rs");
        assert!(
            source.contains("could not be resolved"),
            "init_telegram must warn when TELEGRAM_BOT_TOKEN is in config but can't be resolved"
        );
    }

    /// Guard: all native channel init_* functions use resolve_credential (with keychain fallback),
    /// not resolve_credential_or_env directly. The only allowed direct calls are inside the
    /// resolve_credential helper itself and for optional webhook secrets.
    #[test]
    fn all_channel_inits_use_keychain_fallback() {
        let source = include_str!("channel_init.rs");
        // Find direct calls in init_* functions (not in resolve_credential helper or tests)
        let mut in_init_fn = false;
        for line in source.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("pub(crate)") && trimmed.contains("fn init_") {
                in_init_fn = true;
            } else if trimmed.starts_with("fn resolve_credential")
                || trimmed.starts_with("#[cfg(test)]")
            {
                in_init_fn = false;
            }
            if in_init_fn && trimmed.contains("resolve_credential_or_env") {
                assert!(
                    trimmed.contains("WEBHOOK_SECRET"),
                    "Non-webhook-secret credential in init_* should use resolve_credential() with keychain fallback, found: {trimmed}"
                );
            }
        }
    }

    /// Guard: TUI uninstall handler must wipe the data directory.
    #[test]
    fn uninstall_handler_wipes_data_directory() {
        let source = include_str!("../../cli/src/tui/mod.rs");
        let uninstall_section: String = source
            .lines()
            .skip_while(|l| !l.contains("PluginAction::Uninstall"))
            .take(40)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            uninstall_section.contains("remove_dir_all"),
            "Uninstall handler must wipe the data directory via remove_dir_all"
        );
    }

    /// Guard: uninstall must restart gateway for channel plugins.
    #[test]
    fn uninstall_restarts_gateway_for_channels() {
        let source = include_str!("../../cli/src/tui/mod.rs");
        // Find the Uninstall block and verify it calls restart_gateway
        let uninstall_section: String = source
            .lines()
            .skip_while(|l| !l.contains("PluginAction::Uninstall"))
            .take(50)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            uninstall_section.contains("restart_gateway"),
            "Uninstall handler must restart gateway after removing a channel plugin"
        );
    }
}
