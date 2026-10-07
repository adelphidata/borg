//! Channel client initialization extracted from `GatewayServer::run`.
//!
//! Each function resolves credentials, creates a client, runs a probe/auth check,
//! and returns the client (if successful) plus any metadata fields.
//!
//! The probe is the only network call at startup, and the registry is built
//! once from its result — so a transient failure (DNS not up yet, a 5xx, a
//! rate limit) used to disable the channel until the daemon restarted.
//! [`probe_with_retry`] retries such failures with bounded exponential
//! backoff and gives up at once on a rejected token.
//!
//! The probes run before the gateway binds its TCP port, so the schedule is
//! also a startup budget: a stopped local daemon (connection refused on
//! every attempt) may hold every webhook back for the summed waits, and the
//! default keeps that under [`STARTUP_PROBE_BUDGET`].

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use cortex::config::Config;
use cortex::util::retry::{backoff_with_config, BackoffConfig};
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

/// The most the default probe schedule may delay the gateway bind when a
/// probe fails on every attempt (jitter excluded).
#[allow(dead_code)] // asserted in tests; documents the contract
pub(crate) const STARTUP_PROBE_BUDGET: Duration = Duration::from_secs(6);

/// Retry schedule for a channel's startup probe.
#[derive(Debug, Clone)]
pub(crate) struct ProbeRetry {
    /// Total attempts including the first; the default schedule (2s, 4s)
    /// waits six seconds in all, so a channel that is down delays the bind
    /// by at most that.
    pub(crate) max_attempts: u32,
    pub(crate) backoff: BackoffConfig,
}

impl Default for ProbeRetry {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff: BackoffConfig {
                initial: Duration::from_secs(2),
                factor: 2.0,
                max_delay: Duration::from_secs(4),
                jitter_fraction: 0.1,
            },
        }
    }
}

/// Error tokens that mean the credential itself was rejected: retrying
/// cannot help, and hammering the API with a bad token can get it revoked.
/// Matched on whole tokens so a port or trace id containing `401` does not
/// misfire.
const PERMANENT_PROBE_TOKENS: &[&str] = &[
    "401",
    "403",
    "unauthorized",
    "forbidden",
    "invalid_auth",
    "not_authed",
    "token_revoked",
    "account_inactive",
    "invalid_token",
];

/// Whether a probe error is a rejected credential rather than a transient
/// network, timeout, 5xx or rate-limit failure.
pub(crate) fn probe_error_is_permanent(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_ascii_lowercase();
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|token| PERMANENT_PROBE_TOKENS.contains(&token))
}

/// Run a channel's startup probe, retrying transient failures on the
/// [`ProbeRetry`] schedule and stopping at once on a permanent one.
/// `None` means the channel stays unregistered.
pub(crate) async fn probe_with_retry<T, F, Fut>(
    label: &str,
    retry: &ProbeRetry,
    mut probe: F,
) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let attempts = retry.max_attempts.max(1);
    for attempt in 0..attempts {
        let err = match probe().await {
            Ok(value) => return Some(value),
            Err(err) => err,
        };
        if probe_error_is_permanent(&err) {
            warn!("{label} failed: {err:#} — credential rejected, not retrying");
            return None;
        }
        if attempt + 1 >= attempts {
            warn!("{label} failed after {attempts} attempts, giving up: {err:#}");
            return None;
        }
        let delay = backoff_with_config(attempt, &retry.backoff);
        warn!(
            "{label} failed (attempt {}/{attempts}), retrying in {delay:?}: {err:#}",
            attempt + 1
        );
        tokio::time::sleep(delay).await;
    }
    None
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
            let probed =
                probe_with_retry("Telegram getMe", &ProbeRetry::default(), || client.get_me())
                    .await;
            match probed {
                Some(me) => {
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
                None => {
                    warn!("TELEGRAM_BOT_TOKEN set but getMe failed; Telegram not registered");
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
        Some(token) => {
            // Surface a local construction failure once, before the probe.
            if let Err(e) = SlackClient::new(&token) {
                warn!("Failed to create Slack HTTP client: {e}");
                return (None, slack_signing_secret, None);
            }
            // `auth_test` needs `&mut self` to record the bot user id, so
            // each attempt builds its own client and the passing one wins.
            let probed = probe_with_retry("Slack auth.test", &ProbeRetry::default(), || {
                let token = token.clone();
                async move {
                    let mut client = SlackClient::new(&token)?;
                    let resp = client.auth_test().await?;
                    Ok((client, resp))
                }
            })
            .await;
            match probed {
                Some((client, resp)) => {
                    info!(
                        "Slack native integration active (bot: {}, team: {})",
                        resp.user.as_deref().unwrap_or("unknown"),
                        resp.team.as_deref().unwrap_or("unknown"),
                    );
                    log_gateway_activity("Slack native integration active").await;
                    let bot_uid = client.bot_user_id().map(String::from);
                    (Some(Arc::new(client)), bot_uid)
                }
                None => {
                    warn!("SLACK_BOT_TOKEN set but auth.test failed; Slack not registered");
                    (None, None)
                }
            }
        }
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
            let probed = probe_with_retry("Discord /users/@me", &ProbeRetry::default(), || {
                client.get_current_user()
            })
            .await;
            match probed {
                Some(user) => {
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
                None => {
                    warn!("DISCORD_BOT_TOKEN set but /users/@me failed; Discord not registered");
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
                Ok(client) => {
                    let probed =
                        probe_with_retry("Signal daemon probe", &ProbeRetry::default(), || {
                            client.probe()
                        })
                        .await;
                    match probed {
                        Some(version) => {
                            info!(
                                "Signal native integration active (account: {}, daemon: v{})",
                                account, version
                            );
                            Some(Arc::new(client))
                        }
                        None => {
                            warn!(
                                "SIGNAL_ACCOUNT set but signal-cli daemon not reachable; Signal not registered"
                            );
                            None
                        }
                    }
                }
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
        _guard: crate::test_env::BorgHomeGuard,
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

        let db =
            cortex::db::Database::open_with_timeout(cortex::db::Database::GATEWAY_BUSY_TIMEOUT_MS)
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

    fn fast_retry(max_attempts: u32) -> ProbeRetry {
        ProbeRetry {
            max_attempts,
            backoff: BackoffConfig {
                initial: Duration::from_millis(1),
                factor: 2.0,
                max_delay: Duration::from_millis(4),
                jitter_fraction: 0.0,
            },
        }
    }

    /// Spec: a probe that fails with a transient error (network, 5xx,
    /// 429) and then succeeds still yields the channel, with one attempt
    /// per failure plus the success.
    #[tokio::test]
    async fn probe_with_retry_recovers_from_transient_failures() {
        let calls = std::cell::Cell::new(0u32);
        let out = probe_with_retry("probe", &fast_retry(8), || {
            let n = calls.get() + 1;
            calls.set(n);
            async move {
                match n {
                    1 => Err(anyhow::anyhow!("Failed to call getMe: connection refused")),
                    2 => Err(anyhow::anyhow!(
                        "GET /users/@me failed (503 Service Unavailable): down"
                    )),
                    _ => Ok(format!("bot-{n}")),
                }
            }
        })
        .await;
        assert_eq!(out.as_deref(), Some("bot-3"));
        assert_eq!(calls.get(), 3, "two failures then one success");
    }

    /// Spec: a rejected credential is permanent — one attempt, no retry,
    /// no channel. Covers each channel's real error shape.
    #[tokio::test]
    async fn probe_with_retry_gives_up_at_once_on_rejected_credential() {
        let permanent = [
            "GET /users/@me failed (401 Unauthorized): {\"message\": \"401: Unauthorized\"}",
            "getMe failed: Unauthorized",
            "auth.test failed: invalid_auth — check the bot token",
            "auth.test failed: token_revoked",
            "GET /users/@me failed (403 Forbidden): missing access",
        ];
        for msg in permanent {
            let calls = std::cell::Cell::new(0u32);
            let out: Option<()> = probe_with_retry("probe", &fast_retry(8), || {
                calls.set(calls.get() + 1);
                let msg = msg.to_string();
                async move { Err(anyhow::anyhow!("{msg}")) }
            })
            .await;
            assert!(out.is_none(), "{msg}: must not yield a channel");
            assert_eq!(calls.get(), 1, "{msg}: must not be retried");
        }
    }

    /// Spec: transient failures are bounded — after `max_attempts` the
    /// probe gives up so the daemon still finishes startup.
    #[tokio::test]
    async fn probe_with_retry_is_bounded_by_max_attempts() {
        let calls = std::cell::Cell::new(0u32);
        let out: Option<()> = probe_with_retry("probe", &fast_retry(3), || {
            calls.set(calls.get() + 1);
            async { Err(anyhow::anyhow!("Signal daemon probe failed: timed out")) }
        })
        .await;
        assert!(out.is_none());
        assert_eq!(calls.get(), 3);
    }

    /// Token matching must not misfire on a port or id that merely
    /// contains the digits of an auth status.
    #[test]
    fn probe_error_classification_matches_whole_tokens() {
        assert!(!probe_error_is_permanent(&anyhow::anyhow!(
            "Failed to call getMe: connect to 127.0.0.1:4010 refused"
        )));
        assert!(!probe_error_is_permanent(&anyhow::anyhow!(
            "auth.test failed: ratelimited (429)"
        )));
        assert!(probe_error_is_permanent(&anyhow::anyhow!(
            "auth.test failed: account_inactive"
        )));
        // The classifier reads the whole context chain, not only the
        // outermost message.
        let chained = anyhow::anyhow!("401 Unauthorized").context("Signal daemon probe failed");
        assert!(probe_error_is_permanent(&chained));
    }

    /// Spec: the probes run before the TCP bind, so the default schedule is
    /// a startup budget. A channel whose daemon is stopped (connection
    /// refused on every attempt) may delay every webhook by the summed
    /// waits, which must stay within [`STARTUP_PROBE_BUDGET`] — while still
    /// retrying at least once, with a real pause, so a DNS blip at boot
    /// does not lose the channel.
    #[test]
    fn default_probe_schedule_fits_startup_budget() {
        let retry = ProbeRetry::default();
        assert!(
            (2..=3).contains(&retry.max_attempts),
            "default must retry, but not more than twice: {}",
            retry.max_attempts
        );
        let mut no_jitter = retry.backoff.clone();
        no_jitter.jitter_fraction = 0.0;
        let total: Duration = (0..retry.max_attempts.saturating_sub(1))
            .map(|a| backoff_with_config(a, &no_jitter))
            .sum();
        assert!(
            total <= STARTUP_PROBE_BUDGET,
            "summed backoff {total:?} exceeds the startup budget {STARTUP_PROBE_BUDGET:?}"
        );
        assert!(
            backoff_with_config(0, &no_jitter) >= Duration::from_secs(1),
            "first retry must wait, not spin"
        );
    }

    /// Spec: a probe that is refused on every attempt (a stopped local
    /// daemon) finishes within the startup budget in wall-clock time, not
    /// only on paper — the schedule's delays are what the bind waits on.
    #[tokio::test(start_paused = true)]
    async fn refused_probe_gives_up_within_startup_budget() {
        let calls = std::cell::Cell::new(0u32);
        let started = tokio::time::Instant::now();
        let out: Option<()> = probe_with_retry("probe", &ProbeRetry::default(), || {
            calls.set(calls.get() + 1);
            async {
                Err(anyhow::anyhow!(
                    "Signal daemon probe failed: connection refused"
                ))
            }
        })
        .await;
        let elapsed = started.elapsed();
        assert!(out.is_none());
        assert_eq!(calls.get(), ProbeRetry::default().max_attempts);
        // Jitter may stretch each wait by at most 10%.
        let ceiling = STARTUP_PROBE_BUDGET + STARTUP_PROBE_BUDGET / 10;
        assert!(
            elapsed <= ceiling,
            "refused probe held startup for {elapsed:?}, budget {ceiling:?}"
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
