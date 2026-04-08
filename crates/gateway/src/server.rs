use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::extract::DefaultBodyLimit;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument, warn};

use borg_core::config::Config;
use borg_core::db::Database;
use borg_core::telemetry::BorgMetrics;

use crate::channel_trait::{self, NativeChannelRegistry, WebhookContext};
use crate::discord::channel::DiscordChannel;
use crate::google_chat::channel::GoogleChatChannel;
use crate::handler;
use crate::handler::InboundMessage;
use crate::health::ChannelHealthRegistry;
use crate::manifest::ChannelMode;
use crate::rate_limit::SlidingWindowLimiter;
use crate::registry::ChannelRegistry;
use crate::retry::RetryPolicy;
use crate::session_queue::SessionQueue;
use crate::slack::api::SlackClient;
use crate::slack::channel::SlackChannel;
use crate::slack::dedup::EventDeduplicator;
use crate::teams::channel::TeamsChannel;
use crate::telegram::api::TelegramClient;
use crate::telegram::channel::TelegramChannel;
use crate::telegram::dedup::UpdateDeduplicator;
use crate::twilio::channel::TwilioChannel;

use borg_core::constants;

const MAX_BODY_SIZE: usize = constants::GATEWAY_MAX_BODY_SIZE;

struct AppState {
    config: Config,
    registry: ChannelRegistry,
    native_channels: NativeChannelRegistry,
    session_queue: SessionQueue,
    request_timeout: Duration,
    health: Arc<RwLock<ChannelHealthRegistry>>,
    rate_limiter: Option<Arc<Mutex<SlidingWindowLimiter>>>,
    metrics: BorgMetrics,
    // Slack fields (still used by slack_command_handler)
    slack_client: Option<Arc<SlackClient>>,
    slack_signing_secret: Option<String>,
    /// Slack bot user ID for group mention activation (e.g. "U123ABC").
    slack_bot_user_id: Option<String>,
    poke_tx: Option<mpsc::Sender<()>>,
    /// Auto-reply state (shared across handlers).
    auto_reply_state: crate::auto_reply::SharedAutoReplyState,
}

/// HTTP webhook server for messaging channel integrations.
pub struct GatewayServer {
    config: Config,
    shutdown: CancellationToken,
    metrics: BorgMetrics,
    poke_tx: Option<mpsc::Sender<()>>,
}

impl GatewayServer {
    /// Create a new gateway server with the given config and shutdown token.
    pub fn new(
        config: Config,
        shutdown: CancellationToken,
        metrics: BorgMetrics,
        poke_tx: Option<mpsc::Sender<()>>,
    ) -> Result<Self> {
        Ok(Self {
            config,
            shutdown,
            metrics,
            poke_tx,
        })
    }

    #[instrument(skip_all)]
    /// Start the HTTP server and listen for webhook requests until shutdown.
    pub async fn run(self) -> Result<()> {
        let gateway_config = &self.config.gateway;
        let addr = format!("{}:{}", gateway_config.host, gateway_config.port);

        let registry = ChannelRegistry::new()?;

        // Initialize health registry
        let mut health_reg = ChannelHealthRegistry::new();
        for channel in registry.all_channels() {
            health_reg.register(&channel.manifest.name);
        }
        let health = Arc::new(RwLock::new(health_reg));

        // Initialize rate limiter (0 = disabled)
        let rate_limiter = if gateway_config.rate_limit_per_minute > 0 {
            Some(Arc::new(Mutex::new(SlidingWindowLimiter::new(
                gateway_config.rate_limit_per_minute,
                Duration::from_secs(60),
            ))))
        } else {
            None
        };

        // Initialize channel clients
        let (telegram_client, telegram_bot_username, telegram_secret) =
            crate::channel_init::init_telegram(&self.config).await;
        let telegram_dedup = Arc::new(Mutex::new(UpdateDeduplicator::new()));

        let (slack_client, slack_signing_secret, slack_bot_user_id) =
            crate::channel_init::init_slack(&self.config).await;

        let (twilio_client, twilio_auth_token, twilio_phone_number, twilio_whatsapp_number) =
            crate::channel_init::init_twilio(&self.config);

        let (discord_client, discord_public_key) =
            crate::channel_init::init_discord(&self.config).await;

        let (teams_client, teams_app_secret) = crate::channel_init::init_teams(&self.config)?;

        let (google_chat_client, google_chat_token) =
            crate::channel_init::init_google_chat(&self.config)?;

        let signal_client = crate::channel_init::init_signal(&self.config).await;

        // TTS synthesizer (shared between webhook and polling paths)
        let tts_synth: Option<Arc<borg_core::tts::TtsSynthesizer>> =
            if self.config.tts.enabled && self.config.tts.auto_mode {
                borg_core::tts::TtsSynthesizer::from_config(&self.config).map(Arc::new)
            } else {
                None
            };

        // Build native channel registry for unified webhook dispatch
        let native_channels = {
            let mut reg = NativeChannelRegistry::new();
            if let Some(ref client) = telegram_client {
                reg.register(Arc::new(TelegramChannel {
                    client: client.clone(),
                    dedup: telegram_dedup.clone(),
                    secret: telegram_secret.clone(),
                    bot_username: telegram_bot_username.clone(),
                    config: self.config.clone(),
                    tts_synthesizer: tts_synth.clone(),
                }));
            }
            if let Some(ref client) = slack_client {
                reg.register(Arc::new(SlackChannel {
                    client: client.clone(),
                    signing_secret: slack_signing_secret.clone(),
                    dedup: Arc::new(Mutex::new(EventDeduplicator::new())),
                    bot_user_id: slack_bot_user_id.clone(),
                }));
            }
            if let Some(ref client) = twilio_client {
                reg.register(Arc::new(TwilioChannel {
                    client: client.clone(),
                    auth_token: twilio_auth_token.clone(),
                    phone_number: twilio_phone_number.clone(),
                    whatsapp_number: twilio_whatsapp_number.clone(),
                    config: self.config.clone(),
                }));
            }
            if let Some(ref client) = discord_client {
                reg.register(Arc::new(DiscordChannel::new(
                    client.clone(),
                    discord_public_key.clone(),
                    self.config.gateway.discord_guild_allowlist.clone(),
                )));
            }
            if let Some(ref client) = teams_client {
                reg.register(Arc::new(TeamsChannel {
                    client: client.clone(),
                    app_secret: teams_app_secret.clone(),
                    dedup: Arc::new(std::sync::Mutex::new(
                        crate::teams::dedup::ActivityDeduplicator::new(),
                    )),
                }));
            }
            if let Some(ref client) = google_chat_client {
                reg.register(Arc::new(GoogleChatChannel {
                    client: client.clone(),
                    token: google_chat_token.clone(),
                }));
            }
            reg
        };

        // Log native channel init summary
        {
            let native_list = native_channels.list();
            if native_list.is_empty() {
                info!("Gateway: no native channel integrations active");
            } else {
                let names: Vec<&str> = native_list.iter().map(|c| c.names()[0]).collect();
                info!(
                    "Gateway: {} native channel(s) active: {}",
                    names.len(),
                    names.join(", ")
                );
            }
        }

        let channel_count = registry.list_channels().len() + native_channels.list().len();

        let state = Arc::new(AppState {
            config: self.config.clone(),
            registry,
            native_channels,
            session_queue: SessionQueue::new(gateway_config.max_concurrent),
            request_timeout: Duration::from_millis(gateway_config.request_timeout_ms),
            health: health.clone(),
            rate_limiter,
            metrics: self.metrics.clone(),
            slack_client,
            slack_signing_secret,
            slack_bot_user_id,
            poke_tx: self.poke_tx,
            auto_reply_state: std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::auto_reply::AutoReplyState::default(),
            )),
        });

        let app = Router::new()
            .route("/health", get(health_handler))
            .route("/healthz", get(health_handler))
            .route("/readyz", get(readyz_handler))
            .route("/health/channels", get(channel_health_handler))
            .route("/channels", get(list_channels_handler))
            .route("/internal/poke", post(poke_handler))
            .route("/internal/cancel", post(cancel_handler))
            .route("/internal/away", post(away_handler))
            .route("/internal/available", post(available_handler))
            .route("/webhook/slack/command", post(slack_command_handler))
            .route("/webhook/{name}", post(webhook_handler))
            .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
            .with_state(state.clone());

        // Check if a gateway is already running on this port before attempting to bind
        let probe_ok = async {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .ok()?;
            let resp = client
                .get(format!("http://{addr}/healthz"))
                .send()
                .await
                .ok()?;
            resp.status().is_success().then_some(())
        }
        .await
        .is_some();

        if probe_ok {
            info!("Gateway already running on {addr}, skipping bind");
            self.shutdown.cancelled().await;
            return Ok(());
        }

        let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                anyhow::anyhow!(
                    "Gateway port {addr} is already in use. Another borg instance may be running. \
                     Stop it or change [gateway] port in config.toml"
                )
            } else {
                anyhow::anyhow!("Failed to bind gateway to {addr}: {e}")
            }
        })?;
        info!("Gateway listening on {addr} with {channel_count} channel(s)");
        if let Ok(adb) = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS) {
            borg_core::activity_log::log_activity(
                &adb,
                "info",
                "gateway",
                &format!("Gateway listening on {addr} with {channel_count} channel(s)"),
            );
        }

        // Replay unfinished deliveries from previous run
        if let Ok(db) = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS) {
            match db.replay_unfinished() {
                Ok(0) => {}
                Ok(n) => info!("Reset {n} in-flight delivery(ies) to pending"),
                Err(e) => warn!("Failed to replay unfinished deliveries: {e}"),
            }
        }

        // Spawn delivery drain loop
        let drain_shutdown = self.shutdown.clone();
        let drain_state = state.clone();
        let drain_health = health.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                tokio::select! {
                    _ = drain_shutdown.cancelled() => break,
                    _ = interval.tick() => {
                        drain_pending_deliveries(&drain_state, &drain_health).await;
                    }
                }
            }
        });

        // Spawn Telegram polling if no public_url (polling mode)
        if self.config.gateway.public_url.is_none() {
            if let Some(ref tg_client) = telegram_client {
                info!("No public_url configured — starting Telegram long-polling mode");
                let poll_client = tg_client.clone();
                let poll_dedup = telegram_dedup.clone();
                let poll_shutdown = self.shutdown.clone();
                let poll_config = self.config.clone();
                let poll_health = health.clone();
                let poll_bot_username = telegram_bot_username.clone();
                let poll_tts = tts_synth.clone();
                let poll_request_timeout = Duration::from_millis(gateway_config.request_timeout_ms);

                let callback: crate::telegram::polling::PollCallback =
                    Arc::new(move |inbound, chat_id| {
                        let poll_tts = poll_tts.clone();
                        let config = poll_config.clone();
                        let health = poll_health.clone();
                        let tg = poll_client.clone();
                        let bot_mention = poll_bot_username.as_deref().map(|u| format!("@{u}"));
                        let request_timeout = poll_request_timeout;
                        Box::pin(async move {
                            let typing = crate::telegram::typing::TypingIndicator::start(
                                tg.clone(),
                                chat_id,
                            );

                            // Extract thread/reply IDs before passing inbound to invoke_agent
                            let thread_id: Option<i64> =
                                inbound.thread_id.as_deref().and_then(|id| id.parse().ok());
                            let reply_to: Option<i64> =
                                inbound.message_id.as_deref().and_then(|id| id.parse().ok());

                            let agent_result = tokio::time::timeout(
                                request_timeout,
                                handler::invoke_agent(
                                    "telegram",
                                    &inbound,
                                    &config,
                                    Some(&health),
                                    bot_mention.as_deref(),
                                ),
                            )
                            .await;

                            match agent_result {
                                Ok(Ok((response_text, _))) => {
                                    typing.stop().await;
                                    send_telegram_response(
                                        &tg,
                                        chat_id,
                                        &response_text,
                                        thread_id,
                                        reply_to,
                                        &health,
                                        poll_tts.as_deref(),
                                    )
                                    .await;
                                }
                                Ok(Err(e)) => {
                                    typing.stop().await;
                                    warn!("Agent error in Telegram poll mode: {e:#}");
                                    send_telegram_response(
                                        &tg,
                                        chat_id,
                                        "Something went wrong. Please try again.",
                                        thread_id,
                                        reply_to,
                                        &health,
                                        None,
                                    )
                                    .await;
                                }
                                Err(_) => {
                                    typing.stop().await;
                                    warn!("Agent timed out in Telegram poll mode");
                                    send_telegram_response(
                                        &tg,
                                        chat_id,
                                        "Request timed out. Please try again.",
                                        thread_id,
                                        reply_to,
                                        &health,
                                        None,
                                    )
                                    .await;
                                }
                            }
                        })
                    });

                let polling_client = tg_client.clone();
                tokio::spawn(async move {
                    crate::telegram::polling::run_polling(
                        polling_client,
                        poll_dedup,
                        callback,
                        poll_shutdown,
                    )
                    .await;
                });
            }
        }

        // Spawn Signal SSE inbound loop
        if let Some(ref sig_client) = signal_client {
            info!("Starting Signal SSE inbound loop");
            let sse_client = sig_client.clone();
            let sse_config = self.config.clone();
            let sse_health = health.clone();
            let sse_shutdown = self.shutdown.clone();

            let callback: crate::signal::sse::SseCallback =
                Arc::new(move |inbound, recipient, group_id| {
                    let config = sse_config.clone();
                    let health = sse_health.clone();
                    let client = sse_client.clone();
                    Box::pin(async move {
                        // Send typing indicator
                        let _ = client
                            .send_typing(Some(&recipient), group_id.as_deref())
                            .await;

                        match handler::invoke_agent(
                            "signal",
                            &inbound,
                            &config,
                            Some(&health),
                            None,
                        )
                        .await
                        {
                            Ok((response_text, _)) => {
                                let send_result = if let Some(ref gid) = group_id {
                                    client.send_group_message(gid, &response_text, None).await
                                } else {
                                    client.send_message(&recipient, &response_text, None).await
                                };
                                if let Err(e) = send_result {
                                    warn!("Failed to send Signal response: {e}");
                                }
                                // Send read receipt for the original message
                                if let Some(ref mid) = inbound.message_id {
                                    if let Ok(ts) = mid.parse::<i64>() {
                                        let _ = client.send_read_receipt(&recipient, &[ts]).await;
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("Agent error in Signal SSE mode: {e}");
                            }
                        }
                    })
                });

            let sig_sse_client = sig_client.clone();
            tokio::spawn(async move {
                crate::signal::sse::run_sse_loop(sig_sse_client, callback, sse_shutdown).await;
            });
        }

        // Spawn poll loops for poll-mode channels
        let mut poll_handles = Vec::new();
        for channel in state.registry.all_channels() {
            if channel.manifest.settings.mode != ChannelMode::Poll {
                continue;
            }

            // iMessage is handled natively by the daemon — skip its poll loop here.
            // When the imessage feature is off, a user-created "imessage" channel
            // falls through to the generic poll loop, which is the intended behavior.
            #[cfg(target_os = "macos")]
            if channel.manifest.name == "imessage" {
                info!(
                    "Skipping poll loop for '{}' (handled natively)",
                    channel.manifest.name
                );
                continue;
            }

            let poll_interval_ms = channel.manifest.settings.poll_interval_ms.unwrap_or(5000);
            let channel_name = channel.manifest.name.clone();
            let channel_dir = channel.dir.clone();
            let manifest = channel.manifest.clone();
            let config = state.config.clone();
            let shutdown = self.shutdown.clone();
            let poll_health = health.clone();

            info!(
                "Starting poll loop for channel '{}' (interval: {}ms)",
                channel_name, poll_interval_ms
            );

            let request_timeout = state.request_timeout;
            let handle = tokio::spawn(async move {
                let start = tokio::time::Instant::now() + Duration::from_millis(poll_interval_ms);
                let mut interval =
                    tokio::time::interval_at(start, Duration::from_millis(poll_interval_ms));

                let mut consecutive_errors: u32 = 0;
                let mut total_error_cycles: u32 = 0;
                let initial_backoff = Duration::from_secs(5);
                let max_backoff = Duration::from_secs(300);
                let max_consecutive_errors: u32 = 10;

                loop {
                    tokio::select! {
                        _ = shutdown.cancelled() => {
                            info!("Poll loop for '{}' shutting down", channel_name);
                            break;
                        }
                        _ = interval.tick() => {
                            // If in error state, apply backoff
                            if consecutive_errors > 0 {
                                let backoff_secs = initial_backoff.as_secs_f64()
                                    * 2.0_f64.powi((consecutive_errors - 1) as i32);
                                let backoff = Duration::from_secs_f64(
                                    backoff_secs.min(max_backoff.as_secs_f64())
                                );

                                if consecutive_errors >= max_consecutive_errors {
                                    total_error_cycles += 1;
                                    if total_error_cycles >= 3 {
                                        tracing::error!(
                                            "Poll loop for '{}' permanently failed after {} error cycles ({} total errors). Restart gateway to retry.",
                                            channel_name, total_error_cycles, consecutive_errors + (total_error_cycles - 1) * max_consecutive_errors
                                        );
                                        break;
                                    }
                                    warn!(
                                        "Poll loop for '{}' hit {} consecutive errors (cycle {}/3), pausing for {:?}",
                                        channel_name, consecutive_errors, total_error_cycles, max_backoff
                                    );
                                    tokio::time::sleep(max_backoff).await;
                                    consecutive_errors = 0;
                                    poll_health.write().await.record_reconnect(&channel_name);
                                    continue;
                                }

                                tokio::time::sleep(backoff).await;
                            }

                            let executor =
                                crate::executor::ChannelExecutor::new(&manifest, &channel_dir);
                            let blocked_paths = &config.security.blocked_paths;

                            let input = serde_json::json!({
                                "channel_dir": channel_dir.to_string_lossy(),
                            });

                            let poll_result = executor
                                .poll(&input.to_string(), blocked_paths)
                                .await;

                            let output = match poll_result {
                                Ok(o) => {
                                    if consecutive_errors > 0 {
                                        info!(
                                            "Poll loop for '{}' recovered after {} error(s)",
                                            channel_name, consecutive_errors
                                        );
                                        poll_health.write().await.record_reconnect(&channel_name);
                                        consecutive_errors = 0;
                                    }
                                    o
                                }
                                Err(e) => {
                                    consecutive_errors += 1;
                                    warn!(
                                        "Poll error for '{}' (consecutive: {}): {e}",
                                        channel_name, consecutive_errors
                                    );
                                    poll_health.write().await.record_error(&channel_name, &e.to_string());
                                    continue;
                                }
                            };

                            let trimmed = output.trim();
                            if trimmed.is_empty() {
                                continue;
                            }

                            let messages: Vec<InboundMessage> =
                                match serde_json::from_str(trimmed) {
                                    Ok(m) => m,
                                    Err(e) => {
                                        consecutive_errors += 1;
                                        warn!(
                                            "Failed to parse poll output for '{}': {e}",
                                            channel_name
                                        );
                                        poll_health.write().await.record_error(&channel_name, &e.to_string());
                                        continue;
                                    }
                                };

                            if messages.is_empty() {
                                continue;
                            }

                            info!(
                                "Poll for '{}' returned {} message(s)",
                                channel_name,
                                messages.len()
                            );

                            let reg_channel = crate::registry::RegisteredChannel {
                                manifest: manifest.clone(),
                                dir: channel_dir.clone(),
                            };

                            for msg in messages {
                                let result = tokio::time::timeout(
                                    request_timeout,
                                    handler::handle_polled_message(
                                        &reg_channel,
                                        msg,
                                        &config,
                                        Some(&poll_health),
                                    ),
                                )
                                .await;

                                match result {
                                    Ok(Err(e)) => warn!(
                                        "Failed to handle polled message for '{}': {e}",
                                        channel_name
                                    ),
                                    Err(_) => warn!(
                                        "Polled message for '{}' timed out",
                                        channel_name
                                    ),
                                    Ok(Ok(_)) => {}
                                }
                            }
                        }
                    }
                }
            });

            poll_handles.push(handle);
        }

        let shutdown = self.shutdown.clone();
        let shutdown_tg_client = telegram_client.clone();
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            shutdown.cancelled().await;
            info!("Gateway shutting down");

            // Clean up Telegram webhook on shutdown
            if let Some(client) = shutdown_tg_client {
                if let Err(e) = client.delete_webhook().await {
                    warn!("Failed to delete Telegram webhook on shutdown: {e}");
                } else {
                    info!("Telegram webhook removed");
                }
            }
        })
        .await?;

        // Wait for poll tasks to finish after server shutdown
        for handle in poll_handles {
            let _ = handle.await;
        }

        Ok(())
    }
}

async fn health_handler() -> impl IntoResponse {
    axum::Json(serde_json::json!({ "status": "ok" }))
}

async fn readyz_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut checks = serde_json::Map::new();
    let mut ready = true;

    // Check database connectivity
    match Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS) {
        Ok(_) => {
            checks.insert("database".into(), serde_json::json!("ok"));
        }
        Err(e) => {
            ready = false;
            checks.insert(
                "database".into(),
                serde_json::json!({"error": e.to_string()}),
            );
        }
    }

    // Check that at least one channel is registered (script-based + native)
    let channel_count = state.registry.list_channels().len() + state.native_channels.list().len();
    checks.insert("channels".into(), serde_json::json!(channel_count));

    // Check LLM provider is configured
    let provider_ok = state.config.llm.provider.is_some()
        || !state.config.llm.api_key_env.is_empty()
        || std::env::var("OPENROUTER_API_KEY").is_ok()
        || std::env::var("OPENAI_API_KEY").is_ok()
        || std::env::var("ANTHROPIC_API_KEY").is_ok();
    checks.insert("provider".into(), serde_json::json!(provider_ok));
    if !provider_ok {
        ready = false;
    }

    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        axum::Json(serde_json::json!({ "ready": ready, "checks": checks })),
    )
}

async fn channel_health_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let snapshot = state.health.read().await.snapshot();
    axum::Json(serde_json::json!({ "channels": snapshot }))
}

async fn list_channels_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut channels = state.registry.list_channels();
    for ch in state.native_channels.list() {
        let name = ch.names()[0];
        channels.push(format!(
            "{name}: native integration (webhook: /webhook/{name})"
        ));
    }
    axum::Json(serde_json::json!({ "channels": channels }))
}

/// Returns true if the address is a local (loopback) connection.
///
/// Handles both IPv4 loopback (`127.x.x.x`) and IPv6 loopback (`::1`), as
/// well as IPv4-mapped IPv6 addresses (`::ffff:127.0.0.1`) which some stacks
/// use when binding on `::` and receiving a connection from localhost.
fn is_local_request(addr: &std::net::SocketAddr) -> bool {
    match addr.ip() {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

async fn poke_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    // Only allow poke from localhost to prevent unauthorized LLM cost
    if !is_local_request(&addr) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "localhost only"})),
        );
    }
    match &state.poke_tx {
        Some(tx) => {
            let _ = tx.send(()).await;
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({"status": "ok"})),
            )
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "heartbeat poke channel not available"})),
        ),
    }
}

/// Cancel an in-progress agent turn. Only localhost.
///
/// Accepts an optional `?session=<id>` query parameter. If provided, only that
/// session's in-flight turn is cancelled. If omitted, **all** in-flight turns
/// are cancelled — this is the expected `borg cancel` (no args) behavior for a
/// user with a single active session.
async fn cancel_handler(
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if !is_local_request(&addr) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "localhost only"})),
        );
    }
    let registry = &*crate::in_flight::GLOBAL;
    if let Some(session_id) = params.get("session") {
        let cancelled = registry.cancel(session_id).await;
        let count = if cancelled { 1 } else { 0 };
        (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "status": "ok",
                "cancelled": count,
                "session": session_id,
            })),
        )
    } else {
        let count = registry.cancel_all().await;
        (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "status": "ok",
                "cancelled": count,
            })),
        )
    }
}

/// Set the agent to "away" mode. Only localhost.
async fn away_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    body: String,
) -> impl IntoResponse {
    if !is_local_request(&addr) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "localhost only"})),
        );
    }
    let message = if body.trim().is_empty() {
        state.config.gateway.auto_reply.away_message.clone()
    } else {
        // Try to parse JSON body for {"message": "..."}, fall back to raw text
        serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("message")?.as_str().map(String::from))
            .unwrap_or_else(|| body.trim().to_string())
    };
    *state.auto_reply_state.write().await =
        crate::auto_reply::AutoReplyState::Away(message.clone());
    info!("Auto-reply set to away: {message}");
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"status": "away", "message": message})),
    )
}

/// Set the agent back to "available" mode. Only localhost.
async fn available_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    if !is_local_request(&addr) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "localhost only"})),
        );
    }
    *state.auto_reply_state.write().await = crate::auto_reply::AutoReplyState::Available;
    info!("Auto-reply set to available");
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"status": "available"})),
    )
}

fn extract_client_ip(
    headers: &HeaderMap,
    connect_info: &ConnectInfo<std::net::SocketAddr>,
) -> String {
    let peer_ip = connect_info.0.ip();

    // Only trust proxy headers when the peer is a loopback address (i.e. behind a
    // local reverse proxy). This prevents arbitrary clients from spoofing their IP
    // via X-Forwarded-For to bypass rate limiting.
    if peer_ip.is_loopback() {
        // Check X-Forwarded-For first
        if let Some(xff) = headers.get("x-forwarded-for") {
            if let Ok(val) = xff.to_str() {
                if let Some(first) = val.split(',').next() {
                    let trimmed = first.trim();
                    if !trimmed.is_empty() {
                        return trimmed.to_string();
                    }
                }
            }
        }

        // Check X-Real-IP
        if let Some(xri) = headers.get("x-real-ip") {
            if let Ok(val) = xri.to_str() {
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }

    // Fall back to peer address
    peer_ip.to_string()
}

#[instrument(skip_all, fields(channel = %name))]
async fn webhook_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: String,
) -> WebhookResponse {
    state.metrics.gateway_requests.add(1, &[]);

    let channel = match state.registry.get(&name) {
        Some(c) => c,
        None => {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({ "error": "Unknown channel" })),
            );
        }
    };

    // Rate limiting
    if let Some(ref limiter) = state.rate_limiter {
        let client_ip_str = extract_client_ip(&headers, &ConnectInfo(addr));
        let is_loopback = client_ip_str
            .parse::<IpAddr>()
            .map(|ip| SlidingWindowLimiter::is_exempt(&ip))
            .unwrap_or(false);

        if !is_loopback {
            let allowed = limiter.lock().await.check(&client_ip_str);
            if !allowed {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(serde_json::json!({ "error": "Rate limit exceeded" })),
                );
            }
        }
    }

    // Native channel handling via unified dispatch — messages are enqueued to the
    // SessionQueue for per-session sequential processing with global concurrency control.
    if let Some(channel) = state.native_channels.get(&name) {
        let ctx = WebhookContext {
            config: &state.config,
            health: &state.health,
        };
        return channel_trait::dispatch_webhook(
            channel,
            &headers,
            &body,
            &ctx,
            &state.session_queue,
            state.request_timeout,
        )
        .await;
    }

    // Convert headers to JSON
    let mut headers_map = serde_json::Map::new();
    for (key, value) in headers.iter() {
        if let Ok(v) = value.to_str() {
            headers_map.insert(
                key.as_str().to_string(),
                serde_json::Value::String(v.to_string()),
            );
        }
    }
    let headers_json = serde_json::Value::Object(headers_map);

    // Script-based channels need the response in the HTTP body, so we use a
    // oneshot to wait for the result while still routing through the session queue.
    let session_key = format!("script:{}:{}", name, "default");
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<String, String>>();

    let channel = channel.clone();
    let config = state.config.clone();
    let health = state.health.clone();
    let request_timeout = state.request_timeout;

    let enqueued = state
        .session_queue
        .enqueue(
            session_key,
            Box::pin(async move {
                let result = tokio::time::timeout(
                    request_timeout,
                    handler::handle_webhook(&channel, headers_json, body, &config, Some(&health)),
                )
                .await;

                let response = match result {
                    Ok(Ok(r)) => Ok(r),
                    Ok(Err(e)) => Err(format!("{e:#}")),
                    Err(_) => Err("Request timed out".to_string()),
                };
                let _ = tx.send(response);
            }),
        )
        .await;

    if !enqueued {
        return service_unavailable_response();
    }

    match rx.await {
        Ok(Ok(response)) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({ "ok": true, "response": response })),
        ),
        Ok(Err(e)) => {
            warn!("Webhook handler error for '{name}': {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({ "error": "Internal server error" })),
            )
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({ "error": "Internal server error" })),
        ),
    }
}

type WebhookResponse = (StatusCode, axum::Json<serde_json::Value>);

fn ok_response() -> WebhookResponse {
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "ok": true })),
    )
}

fn service_unavailable_response() -> WebhookResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({ "error": "Server at capacity, try again later" })),
    )
}

// Old per-channel webhook handler functions removed — native channels now use
// NativeChannelRegistry + dispatch_webhook from channel_trait.rs.

/// Slack slash-command handler (separate route from webhook_handler).
async fn slack_command_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> WebhookResponse {
    state.metrics.gateway_requests.add(1, &[]);

    // Verify signature
    if let Some(ref secret) = state.slack_signing_secret {
        if let Err(e) = crate::slack::verify::verify_slack_signature(&headers, &body, secret) {
            warn!("Slack command signature verification failed: {e}");
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({ "error": "Unauthorized" })),
            );
        }
    }

    // Parse form-urlencoded payload
    let payload: crate::slack::types::SlashCommandPayload = match serde_urlencoded::from_str(&body)
    {
        Ok(p) => p,
        Err(e) => {
            warn!("Failed to parse Slack slash command: {e}");
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({ "error": "Invalid payload" })),
            );
        }
    };

    let Some(ref slack_client) = state.slack_client else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({ "error": "Slack not configured" })),
        );
    };

    let command_text = payload.text.unwrap_or_default();
    let display_text = if command_text.is_empty() {
        payload.command.clone()
    } else {
        format!("{} {}", payload.command, command_text)
    };

    let inbound = handler::InboundMessage {
        sender_id: payload.user_id.clone(),
        text: display_text,
        channel_id: Some(payload.channel_id.clone()),
        thread_id: None,
        message_id: None,
        thread_ts: None,
        attachments: Vec::new(),
        reaction: None,
        metadata: serde_json::Value::Null,
        peer_kind: None,
    };

    let session_key = inbound.session_key("slack", "command");

    let slack_client = slack_client.clone();
    let work_state = state.clone();
    let enqueued = state
        .session_queue
        .enqueue(
            session_key,
            Box::pin(async move {
                let state = work_state;
                let result = tokio::time::timeout(state.request_timeout, async {
                    let channel_id = payload.channel_id.clone();

                    let slack_mention = state
                        .slack_bot_user_id
                        .as_deref()
                        .map(|id| format!("<@{id}>"));
                    let (response_text, _session_id) = handler::invoke_agent(
                        "slack",
                        &inbound,
                        &state.config,
                        Some(&state.health),
                        slack_mention.as_deref(),
                    )
                    .await?;

                    let formatted = crate::slack::format::markdown_to_mrkdwn(&response_text);

                    // Post response to channel (slash command responses via chat.postMessage)
                    if let Err(e) = slack_client
                        .post_message(&channel_id, &formatted, None)
                        .await
                    {
                        warn!("Failed to send Slack command response: {e}");
                        state
                            .health
                            .write()
                            .await
                            .record_error("slack", &e.to_string());
                    } else {
                        state.health.write().await.record_outbound("slack");
                    }

                    Ok::<_, anyhow::Error>(())
                })
                .await;

                if let Err(e) = result {
                    warn!("Slack command work error: {e:?}");
                }
            }),
        )
        .await;

    if !enqueued {
        return service_unavailable_response();
    }
    ok_response()
}

/// Send a Telegram response with HTML formatting, TTS, and fallback.
/// Used by the Telegram long-polling code path (not webhooks).
async fn send_telegram_response(
    client: &TelegramClient,
    chat_id: i64,
    response_text: &str,
    thread_id: Option<i64>,
    reply_to: Option<i64>,
    health: &Arc<RwLock<ChannelHealthRegistry>>,
    tts_synthesizer: Option<&borg_core::tts::TtsSynthesizer>,
) {
    if response_text.trim().is_empty() {
        tracing::debug!("Telegram response empty after trim, skipping send for chat {chat_id}");
        return;
    }

    let html = crate::telegram::format::markdown_to_telegram_html(response_text);

    if let Err(e) = client
        .send_message(chat_id, &html, Some("HTML"), thread_id, reply_to)
        .await
    {
        warn!("HTML send failed, retrying as plain text: {e}");
        if let Err(e2) = client
            .send_message(chat_id, response_text, None, thread_id, reply_to)
            .await
        {
            warn!("Failed to send Telegram response: {e2}");
            health
                .write()
                .await
                .record_error("telegram", &e2.to_string());
        } else {
            health.write().await.record_outbound("telegram");
        }
    } else {
        health.write().await.record_outbound("telegram");
    }

    // Auto-TTS: synthesize and send voice message after text
    if let Some(synth) = tts_synthesizer {
        let tts_text = borg_core::tts::truncate_for_tts(response_text, 4096);
        match synth
            .synthesize(&tts_text, None, Some(borg_core::tts::AudioFormat::Opus))
            .await
        {
            Ok((audio_bytes, _, _)) => {
                if let Err(e) = client
                    .send_voice(chat_id, &audio_bytes, None, thread_id, None)
                    .await
                {
                    warn!("Failed to send TTS voice message: {e}");
                }
            }
            Err(e) => warn!("TTS synthesis failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn loopback_connect_info() -> ConnectInfo<std::net::SocketAddr> {
        ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 12345)))
    }

    fn non_loopback_connect_info() -> ConnectInfo<std::net::SocketAddr> {
        ConnectInfo(std::net::SocketAddr::from(([192, 168, 1, 100], 12345)))
    }

    #[test]
    fn extract_client_ip_xff_from_loopback() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.2.3.4, 5.6.7.8"),
        );
        let info = loopback_connect_info();
        assert_eq!(extract_client_ip(&headers, &info), "1.2.3.4");
    }

    #[test]
    fn extract_client_ip_xri_from_loopback() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", HeaderValue::from_static("10.0.0.1"));
        let info = loopback_connect_info();
        assert_eq!(extract_client_ip(&headers, &info), "10.0.0.1");
    }

    #[test]
    fn extract_client_ip_xff_takes_precedence_over_xri() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        headers.insert("x-real-ip", HeaderValue::from_static("10.0.0.1"));
        let info = loopback_connect_info();
        assert_eq!(extract_client_ip(&headers, &info), "1.2.3.4");
    }

    #[test]
    fn extract_client_ip_ignores_headers_from_non_loopback() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        let info = non_loopback_connect_info();
        assert_eq!(extract_client_ip(&headers, &info), "192.168.1.100");
    }

    #[test]
    fn extract_client_ip_no_proxy_headers_returns_peer() {
        let headers = HeaderMap::new();
        let info = loopback_connect_info();
        assert_eq!(extract_client_ip(&headers, &info), "127.0.0.1");
    }

    #[test]
    fn extract_client_ip_empty_xff_falls_to_xri() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static(""));
        headers.insert("x-real-ip", HeaderValue::from_static("10.0.0.1"));
        let info = loopback_connect_info();
        assert_eq!(extract_client_ip(&headers, &info), "10.0.0.1");
    }

    #[test]
    fn combined_channel_count_includes_native() {
        use crate::channel_trait::{NativeChannel, WebhookContext, WebhookOutcome};

        struct TestChannel(&'static str);

        #[async_trait::async_trait]
        impl NativeChannel for TestChannel {
            fn names(&self) -> Vec<&str> {
                vec![self.0]
            }
            async fn handle_webhook(
                &self,
                _h: &HeaderMap,
                _b: &str,
                _c: &WebhookContext<'_>,
            ) -> anyhow::Result<WebhookOutcome> {
                Ok(WebhookOutcome::Skip)
            }
            async fn send_response(
                &self,
                _t: &str,
                _c: &serde_json::Value,
                _h: &std::sync::Arc<tokio::sync::RwLock<crate::health::ChannelHealthRegistry>>,
            ) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let mut native = NativeChannelRegistry::new();
        native.register(std::sync::Arc::new(TestChannel("telegram")));
        native.register(std::sync::Arc::new(TestChannel("slack")));

        // Script-based registry will have 0 channels in test env
        let script_count = 0;
        let total = script_count + native.list().len();
        assert_eq!(total, 2);
    }

    #[test]
    fn list_channels_includes_native_names() {
        use crate::channel_trait::{NativeChannel, WebhookContext, WebhookOutcome};

        struct TestChannel(&'static str);

        #[async_trait::async_trait]
        impl NativeChannel for TestChannel {
            fn names(&self) -> Vec<&str> {
                vec![self.0]
            }
            async fn handle_webhook(
                &self,
                _h: &HeaderMap,
                _b: &str,
                _c: &WebhookContext<'_>,
            ) -> anyhow::Result<WebhookOutcome> {
                Ok(WebhookOutcome::Skip)
            }
            async fn send_response(
                &self,
                _t: &str,
                _c: &serde_json::Value,
                _h: &std::sync::Arc<tokio::sync::RwLock<crate::health::ChannelHealthRegistry>>,
            ) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let mut native = NativeChannelRegistry::new();
        native.register(std::sync::Arc::new(TestChannel("telegram")));

        let mut channels: Vec<String> = Vec::new();
        for ch in native.list() {
            let name = ch.names()[0];
            channels.push(format!(
                "{name}: native integration (webhook: /webhook/{name})"
            ));
        }

        assert_eq!(channels.len(), 1);
        assert!(channels[0].contains("telegram"));
        assert!(channels[0].contains("native integration"));
    }

    #[test]
    fn readyz_provider_detection_logic() {
        // Verify the provider check logic used in readyz_handler
        let config = Config::default();
        let has_provider = config.llm.provider.is_some()
            || !config.llm.api_key_env.is_empty()
            || std::env::var("OPENROUTER_API_KEY").is_ok()
            || std::env::var("OPENAI_API_KEY").is_ok()
            || std::env::var("ANTHROPIC_API_KEY").is_ok();
        // Default config has no provider set — result depends on env vars
        // This test validates the detection logic compiles and runs without panic
        let _ = has_provider;
    }
}

/// Background task: claim pending deliveries and attempt to send them.
#[instrument(skip_all)]
async fn drain_pending_deliveries(
    state: &Arc<AppState>,
    health: &Arc<RwLock<ChannelHealthRegistry>>,
) {
    let mut db = match Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS) {
        Ok(db) => db,
        Err(e) => {
            warn!("Drain loop: failed to open database: {e}");
            return;
        }
    };

    let deliveries = match db.claim_pending_deliveries(20) {
        Ok(d) => d,
        Err(e) => {
            warn!("Drain loop: failed to claim deliveries: {e}");
            return;
        }
    };

    if deliveries.is_empty() {
        return;
    }

    info!(
        "Drain loop: processing {} pending delivery(ies)",
        deliveries.len()
    );

    for delivery in deliveries {
        let channel = match state.registry.get(&delivery.channel_name) {
            Some(c) => c,
            None => {
                warn!(
                    "Drain loop: channel '{}' not found, marking failed",
                    delivery.channel_name
                );
                if let Err(e) = db.mark_failed(&delivery.id, "channel not found", None) {
                    warn!(delivery_id = %delivery.id, "Failed to persist delivery failure status: {e}");
                }
                continue;
            }
        };

        let executor = crate::executor::ChannelExecutor::new(&channel.manifest, &channel.dir);
        let blocked_paths = &state.config.security.blocked_paths;

        let mut policy = RetryPolicy::default();
        if let Some(max) = channel.manifest.settings.retry_max_attempts {
            policy.max_retries = max;
        }
        if let Some(delay) = channel.manifest.settings.retry_initial_delay_ms {
            policy.initial_delay_ms = delay;
        }
        // For drain loop, use fewer retries to avoid blocking
        policy.max_retries = policy.max_retries.min(2);

        match crate::retry::send_with_retry(
            &executor,
            &delivery.payload_json,
            blocked_paths,
            &policy,
        )
        .await
        {
            crate::retry::RetryOutcome::Success(_) => {
                if let Err(e) = db.mark_delivered(&delivery.id) {
                    warn!(delivery_id = %delivery.id, "Failed to persist delivery success status: {e}");
                }
                health.write().await.record_outbound(&delivery.channel_name);
            }
            crate::retry::RetryOutcome::PermanentFailure(e) => {
                if let Err(db_err) = db.mark_failed(&delivery.id, &e, None) {
                    warn!(delivery_id = %delivery.id, "Failed to persist delivery failure status: {db_err}");
                }
                health
                    .write()
                    .await
                    .record_error(&delivery.channel_name, &e);
            }
            crate::retry::RetryOutcome::Exhausted(e) => {
                let next_retry = chrono::Utc::now().timestamp() + 60;
                if let Err(db_err) = db.mark_failed(&delivery.id, &e, Some(next_retry)) {
                    warn!(delivery_id = %delivery.id, "Failed to persist delivery failure status: {db_err}");
                }
                health
                    .write()
                    .await
                    .record_error(&delivery.channel_name, &e);
            }
        }
    }
}
