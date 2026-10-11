use std::sync::Arc;
use std::time::{Duration, Instant};

use opentelemetry::metrics::Histogram;

use anyhow::Result;
use axum::extract::DefaultBodyLimit;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use cortex::config::Config;
use cortex::db::Database;
use cortex::telemetry::BorgMetrics;

use crate::channel::channel_trait::{self, NativeChannelRegistry, WebhookContext};
use crate::channel::registry::{ChannelRegistry, RegisteredChannel};
use crate::constants::{POLL_INITIAL_BACKOFF, POLL_MAX_BACKOFF, POLL_MAX_CONSECUTIVE_ERRORS};
use crate::discord::channel::DiscordChannel;
use crate::google_chat::channel::GoogleChatChannel;
use crate::handler;
use crate::handler::InboundMessage;
use crate::health::ChannelHealthRegistry;
use crate::manifest::ChannelMode;
use crate::resilience::rate_limit::SlidingWindowLimiter;
use crate::resilience::retry::RetryPolicy;
use crate::session_queue::SessionQueue;
use crate::slack::api::SlackClient;
use crate::slack::channel::SlackChannel;
use crate::slack::dedup::EventDeduplicator;
use crate::teams::channel::TeamsChannel;
use crate::telegram::channel::TelegramChannel;
use crate::telegram::dedup::UpdateDeduplicator;
use crate::twilio::channel::TwilioChannel;

use cortex::constants;

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
    slack_client: Option<Arc<SlackClient>>,
    slack_signing_secret: Option<String>,
    /// Slack bot user ID for group mention activation (e.g. "U123ABC").
    slack_bot_user_id: Option<String>,
    poke_tx: Option<mpsc::Sender<()>>,
    /// Auto-reply state (shared across handlers).
    auto_reply_state: crate::auto_reply::SharedAutoReplyState,
    /// Shutdown token — used by /internal/restart to trigger graceful restart.
    shutdown: CancellationToken,
}

enum WebhookTarget<'a> {
    Native(&'a Arc<dyn channel_trait::NativeChannel>),
    Script(&'a RegisteredChannel),
}

fn resolve_webhook_target<'a>(
    native_channels: &'a NativeChannelRegistry,
    registry: &'a ChannelRegistry,
    name: &str,
) -> Option<WebhookTarget<'a>> {
    native_channels
        .get(name)
        .map(WebhookTarget::Native)
        .or_else(|| registry.get(name).map(WebhookTarget::Script))
}

/// RAII timer that records elapsed seconds into a histogram on drop.
/// Covers every early-return path through a webhook handler without per-branch
/// instrumentation.
struct GatewayTimer<'a> {
    start: Instant,
    hist: &'a Histogram<f64>,
}

impl<'a> GatewayTimer<'a> {
    fn new(hist: &'a Histogram<f64>) -> Self {
        Self {
            start: Instant::now(),
            hist,
        }
    }
}

impl Drop for GatewayTimer<'_> {
    fn drop(&mut self) {
        self.hist.record(self.start.elapsed().as_secs_f64(), &[]);
    }
}

/// HTTP webhook server for messaging channel integrations.
pub struct GatewayServer {
    config: Config,
    shutdown: CancellationToken,
    metrics: BorgMetrics,
    poke_tx: Option<mpsc::Sender<()>>,
    /// Filled in by borgd once its tonic services exist; `None` when the
    /// gateway runs standalone, which leaves the browser surface unmounted.
    grpc_web: Option<crate::webui::GrpcWebSlot>,
}

/// Native-channel clients and credentials produced once at startup by
/// [`GatewayServer::init_channels`], then threaded through channel
/// registration, `AppState`, and the polling/SSE spawn paths.
struct ChannelInits {
    telegram_client: Option<Arc<crate::telegram::api::TelegramClient>>,
    telegram_bot_username: Option<String>,
    telegram_secret: Option<String>,
    telegram_dedup: Arc<Mutex<UpdateDeduplicator>>,
    slack_client: Option<Arc<SlackClient>>,
    slack_signing_secret: Option<String>,
    slack_bot_user_id: Option<String>,
    twilio_client: Option<Arc<crate::twilio::api::TwilioClient>>,
    twilio_auth_token: Option<String>,
    twilio_phone_number: Option<String>,
    twilio_whatsapp_number: Option<String>,
    discord_client: Option<Arc<crate::discord::api::DiscordClient>>,
    discord_public_key: Option<String>,
    teams_client: Option<Arc<crate::teams::api::TeamsClient>>,
    teams_app_secret: Option<String>,
    google_chat_client: Option<Arc<crate::google_chat::api::GoogleChatClient>>,
    google_chat_token: Option<String>,
    signal_client: Option<Arc<crate::signal::api::SignalClient>>,
    /// TTS synthesizer (shared between webhook and polling paths).
    tts_synth: Option<Arc<cortex::media::tts::TtsSynthesizer>>,
}

impl GatewayServer {
    /// When `executor` is `Some`, it is installed as the process-wide handle so
    /// inbound channel messages route through `borgd::session::SessionRegistry`;
    /// when `None`, agent invocations short-circuit with an error.
    pub fn new(
        config: Config,
        shutdown: CancellationToken,
        metrics: BorgMetrics,
        poke_tx: Option<mpsc::Sender<()>>,
        executor: Option<crate::SessionExecutorHandle>,
        grpc_web: Option<crate::webui::GrpcWebSlot>,
    ) -> Result<Self> {
        if let Some(exec) = executor {
            crate::session_executor::set_global(exec);
        }
        Ok(Self {
            config,
            shutdown,
            metrics,
            poke_tx,
            grpc_web,
        })
    }

    #[instrument(skip_all)]
    /// Start the HTTP server and listen for webhook requests until shutdown.
    pub async fn run(self) -> Result<()> {
        let gateway_config = &self.config.gateway;
        let addr = format!("{}:{}", gateway_config.host, gateway_config.port);

        let registry = ChannelRegistry::new()?;

        let mut health_reg = ChannelHealthRegistry::new();
        for channel in registry.all_channels() {
            health_reg.register(&channel.manifest.name);
        }
        let health = Arc::new(RwLock::new(health_reg));

        // 0 disables the limiter entirely (operator opt-out, not a default).
        let rate_limiter = if gateway_config.rate_limit_per_minute > 0 {
            Some(Arc::new(Mutex::new(SlidingWindowLimiter::new(
                gateway_config.rate_limit_per_minute,
                Duration::from_secs(60),
            ))))
        } else {
            None
        };

        // Probes before bind: the registry below is immutable once built, so
        // a channel must pass its probe here or stay out. The probe schedule
        // bounds how long this can hold the bind (`STARTUP_PROBE_BUDGET`).
        let inits = self.init_channels().await?;
        let native_channels = self.register_native_channels(&inits);

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
            slack_client: inits.slack_client.clone(),
            slack_signing_secret: inits.slack_signing_secret.clone(),
            slack_bot_user_id: inits.slack_bot_user_id.clone(),
            poke_tx: self.poke_tx.clone(),
            auto_reply_state: std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::auto_reply::AutoReplyState::default(),
            )),
            shutdown: self.shutdown.clone(),
        });

        let app = build_router(state.clone(), &self.config, self.grpc_web.clone());

        // Probe before bind: another borgd already serving this port is an
        // idempotent no-op (parent will park on `shutdown`), avoiding a
        // confusing AddrInUse on supervisord-style double-starts.
        if probe_existing_instance(&addr).await {
            // Another instance owns the port. Park until shutdown instead of
            // erroring so the supervisor doesn't treat this as a crash and
            // respawn-loop against the live instance.
            info!("Gateway already running on {addr}, skipping bind");
            self.shutdown.cancelled().await;
            return Ok(());
        }

        // Fail closed on a blank host. `gateway.host` is a free-text setting the
        // agent itself can write, and this listener carries no mTLS — silently
        // reading "" as "every interface" would turn a cleared setting into a
        // public webhook endpoint. Make the operator say which interface.
        if gateway_config.host.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "gateway.host is empty. Set it to an interface (`borg settings set gateway.host \
                 127.0.0.1`), or to `0.0.0.0` to listen on every interface deliberately"
            ));
        }

        // A wildcard host expands to one target per address family; anything
        // else is a single target. Binding each family separately is what keeps
        // the gateway reachable over IPv6 on hosted deploys.
        let targets = bind_targets(&gateway_config.host, gateway_config.port);
        let mut listeners = Vec::with_capacity(targets.len());
        let mut bind_errors = Vec::new();
        for target in &targets {
            match tokio::net::TcpListener::bind(target).await {
                Ok(l) => {
                    info!("Gateway bound {target}");
                    listeners.push(l);
                }
                Err(e) => bind_errors.push((target.clone(), e)),
            }
        }

        if listeners.is_empty() {
            let (first_target, first_err) = bind_errors
                .into_iter()
                .next()
                .unwrap_or_else(|| unreachable!("bind_targets never returns an empty list"));
            return Err(if first_err.kind() == std::io::ErrorKind::AddrInUse {
                anyhow::anyhow!(
                    "Gateway port {first_target} is already in use. Another borg instance may be \
                     running. Stop it or change the gateway port (`borg settings set gateway.port`)"
                )
            } else {
                anyhow::anyhow!("Failed to bind gateway to {first_target}: {first_err}")
            });
        }

        // A partial failure is expected and benign in two cases: a dual-stack
        // host where the `[::]` listener already accepts IPv4 (the follow-up
        // `0.0.0.0` bind hits AddrInUse), and a host with no IPv6 stack at all
        // (AddrNotAvailable on `[::]`). Log it — never swallow it — but keep
        // serving on what did bind.
        for (target, e) in bind_errors {
            warn!(
                "Gateway could not bind {target}: {e} (continuing on {} other listener(s))",
                listeners.len()
            );
        }

        let bound: Vec<String> = listeners
            .iter()
            .map(|l| {
                l.local_addr()
                    .map_or_else(|_| "<unknown>".to_string(), |a| a.to_string())
            })
            .collect();
        let bound_desc = bound.join(", ");
        info!("Gateway listening on {bound_desc} with {channel_count} channel(s)");
        crate::activity::log_gateway_activity(
            "info",
            &format!("Gateway listening on {bound_desc} with {channel_count} channel(s)"),
        )
        .await;

        // Crash-recovery: rows left in `sending` from the prior process are
        // demoted to `pending` so the drain loop retries them.
        if let Ok(db) = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS).await {
            match db.replay_unfinished().await {
                Ok(0) => {}
                Ok(n) => info!("Reset {n} in-flight delivery(ies) to pending"),
                Err(e) => warn!("Failed to replay unfinished deliveries: {e}"),
            }
        }

        self.spawn_delivery_drain(state.clone(), health.clone());
        self.spawn_telegram_polling(&inits, &health);
        self.spawn_signal_sse(&inits, &health);
        let poll_handles = self.spawn_poll_loops(&state, &health);

        // One `axum::serve` per listener, all watching the same shutdown token.
        // The Telegram webhook teardown is one-shot, so it rides only the first
        // listener's shutdown future — running it once per address family would
        // race delete_webhook against itself.
        let mut serves = Vec::with_capacity(listeners.len());
        let mut shutdown_tg_client = inits.telegram_client.clone();
        for listener in listeners {
            let shutdown = self.shutdown.clone();
            let tg_client = shutdown_tg_client.take();
            let app = app.clone();
            serves.push(tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .with_graceful_shutdown(async move {
                    shutdown.cancelled().await;
                    info!("Gateway shutting down");

                    // Clean up Telegram webhook on shutdown
                    if let Some(client) = tg_client {
                        if let Err(e) = client.delete_webhook().await {
                            warn!("Failed to delete Telegram webhook on shutdown: {e}");
                        } else {
                            info!("Telegram webhook removed");
                        }
                    }
                })
                .await
            }));
        }

        // Await every listener even when one fails. Returning early would leave
        // the sibling `axum::serve` tasks detached and still holding the port,
        // so a supervisor respawn would hit AddrInUse against a gateway the
        // caller believes it stopped. Cancelling the shutdown token first is
        // what makes the remaining joins terminate.
        let mut first_error: Option<anyhow::Error> = None;
        for serve in serves {
            let outcome = match serve.await {
                Ok(Ok(())) => continue,
                Ok(Err(e)) => anyhow::anyhow!("Gateway listener failed: {e}"),
                Err(e) => anyhow::anyhow!("Gateway listener task panicked: {e}"),
            };
            warn!("{outcome}");
            self.shutdown.cancel();
            if first_error.is_none() {
                first_error = Some(outcome);
            }
        }
        if let Some(e) = first_error {
            return Err(e);
        }

        // Drain poll tasks after the server stops so long-poll HTTP requests
        // finish before the process exits. A JoinError just means the task
        // already died, which shutdown makes moot.
        for handle in poll_handles {
            let _ = handle.await;
        }

        Ok(())
    }

    /// Initialize every configured native-channel client (Telegram, Slack,
    /// Twilio, Discord, Teams, Google Chat, Signal) plus the shared TTS
    /// synthesizer.
    ///
    /// The four channels with a startup probe run concurrently: each probe
    /// retries transient failures with backoff (see `channel_init`), so a
    /// slow one must not hold the others — or the daemon — hostage. This
    /// runs before the TCP bind and the native-channel registry is built
    /// once from its result, so the retry schedule is capped at
    /// `channel_init::STARTUP_PROBE_BUDGET`: a channel that is down delays
    /// every webhook by at most that, then stays unregistered until restart.
    async fn init_channels(&self) -> Result<ChannelInits> {
        let (
            (telegram_client, telegram_bot_username, telegram_secret),
            (slack_client, slack_signing_secret, slack_bot_user_id),
            (discord_client, discord_public_key),
            signal_client,
        ) = tokio::join!(
            crate::channel::channel_init::init_telegram(&self.config),
            crate::channel::channel_init::init_slack(&self.config),
            crate::channel::channel_init::init_discord(&self.config),
            crate::channel::channel_init::init_signal(&self.config),
        );
        let telegram_dedup = Arc::new(Mutex::new(UpdateDeduplicator::new()));

        let (twilio_client, twilio_auth_token, twilio_phone_number, twilio_whatsapp_number) =
            crate::channel::channel_init::init_twilio(&self.config);

        // Discord voice-channel manager (feature-gated). When the
        // `discord-voice-channels` feature is on AND a bot token is
        // configured, spawn the songbird-backed manager so `/voice join`
        // can actually connect. Without this init, the command falls back
        // to its "not yet wired" stub.
        #[cfg(feature = "discord-voice-channels")]
        if discord_client.is_some() {
            if let Some(token) = crate::channel::channel_init::resolve_credential(
                &self.config,
                "messaging/discord",
                "DISCORD_BOT_TOKEN",
            ) {
                crate::discord::voice_channels::init_voice_manager(token, self.config.clone());
            }
        }

        let (teams_client, teams_app_secret) =
            crate::channel::channel_init::init_teams(&self.config)?;

        let (google_chat_client, google_chat_token) =
            crate::channel::channel_init::init_google_chat(&self.config)?;

        // TTS synthesizer (shared between webhook and polling paths)
        let tts_synth: Option<Arc<cortex::media::tts::TtsSynthesizer>> =
            crate::voice::configured_tts_synthesizer(&self.config).map(Arc::new);

        Ok(ChannelInits {
            telegram_client,
            telegram_bot_username,
            telegram_secret,
            telegram_dedup,
            slack_client,
            slack_signing_secret,
            slack_bot_user_id,
            twilio_client,
            twilio_auth_token,
            twilio_phone_number,
            twilio_whatsapp_number,
            discord_client,
            discord_public_key,
            teams_client,
            teams_app_secret,
            google_chat_client,
            google_chat_token,
            signal_client,
            tts_synth,
        })
    }

    /// Register each initialized client as a native channel and log the
    /// resulting active set.
    fn register_native_channels(&self, inits: &ChannelInits) -> NativeChannelRegistry {
        let mut reg = NativeChannelRegistry::new();
        if let Some(ref client) = inits.telegram_client {
            reg.register(Arc::new(TelegramChannel {
                client: client.clone(),
                dedup: inits.telegram_dedup.clone(),
                secret: inits.telegram_secret.clone(),
                bot_username: inits.telegram_bot_username.clone(),
                config: self.config.clone(),
                tts_synthesizer: inits.tts_synth.clone(),
            }));
        }
        if let Some(ref client) = inits.slack_client {
            reg.register(Arc::new(SlackChannel {
                client: client.clone(),
                signing_secret: inits.slack_signing_secret.clone(),
                dedup: Arc::new(Mutex::new(EventDeduplicator::new())),
                bot_user_id: inits.slack_bot_user_id.clone(),
                config: self.config.clone(),
            }));
        }
        if let Some(ref client) = inits.twilio_client {
            reg.register(Arc::new(TwilioChannel {
                client: client.clone(),
                auth_token: inits.twilio_auth_token.clone(),
                phone_number: inits.twilio_phone_number.clone(),
                whatsapp_number: inits.twilio_whatsapp_number.clone(),
                config: self.config.clone(),
            }));
        }
        if let Some(ref client) = inits.discord_client {
            reg.register(Arc::new(DiscordChannel::new(
                client.clone(),
                inits.discord_public_key.clone(),
                self.config.gateway.discord_guild_allowlist.clone(),
                self.config.clone(),
            )));
        }
        if let Some(ref client) = inits.teams_client {
            reg.register(Arc::new(TeamsChannel {
                client: client.clone(),
                app_secret: inits.teams_app_secret.clone(),
                dedup: Arc::new(std::sync::Mutex::new(
                    crate::teams::dedup::ActivityDeduplicator::new(),
                )),
                config: self.config.clone(),
            }));
        }
        if let Some(ref client) = inits.google_chat_client {
            reg.register(Arc::new(GoogleChatChannel {
                client: client.clone(),
                token: inits.google_chat_token.clone(),
                config: self.config.clone(),
            }));
        }

        {
            let native_list = reg.list();
            if native_list.is_empty() {
                debug!("Gateway: no native channel integrations active");
            } else {
                let names: Vec<&str> = native_list.iter().map(|c| c.names()[0]).collect();
                info!(
                    "Gateway: {} native channel(s) active: {}",
                    names.len(),
                    names.join(", ")
                );
            }
        }

        reg
    }

    /// Spawn the background loop that drains pending outbound deliveries
    /// every 10 seconds until shutdown.
    fn spawn_delivery_drain(
        &self,
        state: Arc<AppState>,
        health: Arc<RwLock<ChannelHealthRegistry>>,
    ) {
        let drain_shutdown = self.shutdown.clone();
        crate::supervised_loop::spawn_supervised_loop(
            "delivery-drain",
            drain_shutdown.clone(),
            move || {
                let state = state.clone();
                let health = health.clone();
                let drain_shutdown = drain_shutdown.clone();
                async move {
                    let mut interval = tokio::time::interval(Duration::from_secs(10));
                    loop {
                        tokio::select! {
                            _ = drain_shutdown.cancelled() => break,
                            _ = interval.tick() => {
                                drain_pending_deliveries(&state, &health).await;
                            }
                        }
                    }
                }
            },
        );
    }

    /// Spawn Telegram long-polling when no public_url is configured
    /// (polling mode). No-op when a public_url is set (webhook mode) or
    /// Telegram is not configured.
    fn spawn_telegram_polling(
        &self,
        inits: &ChannelInits,
        health: &Arc<RwLock<ChannelHealthRegistry>>,
    ) {
        if self.config.gateway.public_url.is_some() {
            return;
        }
        let Some(ref tg_client) = inits.telegram_client else {
            return;
        };
        info!("No public_url configured — starting Telegram long-polling mode");
        let poll_client = tg_client.clone();
        let poll_dedup = inits.telegram_dedup.clone();
        let poll_shutdown = self.shutdown.clone();
        let poll_config = self.config.clone();
        let poll_health = health.clone();
        let poll_bot_username = inits.telegram_bot_username.clone();
        let poll_tts = inits.tts_synth.clone();
        let poll_request_timeout = Duration::from_millis(self.config.gateway.request_timeout_ms);

        let callback: crate::telegram::polling::PollCallback = Arc::new(move |parsed, chat_id| {
            let poll_tts = poll_tts.clone();
            let config = poll_config.clone();
            let health = poll_health.clone();
            let tg = poll_client.clone();
            let bot_mention = poll_bot_username.as_deref().map(|u| format!("@{u}"));
            let request_timeout = poll_request_timeout;
            Box::pin(async move {
                let mut parsed = parsed;
                crate::telegram::enrich_parsed_message(&tg, &config, &mut parsed).await;
                let inbound = parsed.inbound;

                let typing = crate::telegram::typing::start_typing_keepalive(tg.clone(), chat_id);

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
                        let voice_reply_mode = match Database::open_with_timeout(
                            Database::GATEWAY_BUSY_TIMEOUT_MS,
                        )
                        .await
                        {
                            Ok(db) => match crate::voice::resolve_voice_reply_mode(
                                &db,
                                &config,
                                "telegram",
                                &chat_id.to_string(),
                            )
                            .await
                            {
                                Ok(mode) => Some(mode),
                                Err(e) => {
                                    warn!("Failed to resolve Telegram poll voice mode: {e}");
                                    None
                                }
                            },
                            Err(e) => {
                                warn!("Failed to open DB for Telegram poll voice mode lookup: {e}");
                                None
                            }
                        };
                        crate::telegram::delivery::send_response(
                            &tg,
                            chat_id,
                            &response_text,
                            thread_id,
                            reply_to,
                            matches!(
                                parsed.input_kind,
                                crate::telegram::parse::TelegramInputKind::Voice
                            ),
                            voice_reply_mode,
                            &health,
                            poll_tts.as_deref(),
                        )
                        .await
                        .unwrap_or_else(|e| warn!("Failed to send Telegram poll response: {e}"));
                    }
                    Ok(Err(e)) => {
                        typing.stop().await;
                        warn!("Agent error in Telegram poll mode: {e:#}");
                        let error_msg = handler::format_gateway_error(&e);
                        crate::telegram::delivery::send_response(
                            &tg, chat_id, &error_msg, thread_id, reply_to, false, None, &health,
                            None,
                        )
                        .await
                        .unwrap_or_else(|e| {
                            warn!("Failed to send Telegram poll error response: {e}")
                        });
                    }
                    Err(_) => {
                        typing.stop().await;
                        warn!("Agent timed out in Telegram poll mode");
                        let error_msg = cortex::error_format::format_error_with_context(
                            "request timed out",
                            cortex::error_format::ErrorContext::Gateway,
                        );
                        crate::telegram::delivery::send_response(
                            &tg, chat_id, &error_msg, thread_id, reply_to, false, None, &health,
                            None,
                        )
                        .await
                        .unwrap_or_else(|e| {
                            warn!("Failed to send Telegram poll timeout response: {e}")
                        });
                    }
                }
            })
        });

        let polling_client = tg_client.clone();
        crate::supervised_loop::spawn_supervised_loop(
            "telegram-polling",
            poll_shutdown.clone(),
            move || {
                let polling_client = polling_client.clone();
                let poll_dedup = poll_dedup.clone();
                let callback = callback.clone();
                let poll_shutdown = poll_shutdown.clone();
                async move {
                    crate::telegram::polling::run_polling(
                        polling_client,
                        poll_dedup,
                        callback,
                        poll_shutdown,
                    )
                    .await;
                }
            },
        );
    }

    /// Spawn the Signal SSE inbound loop. No-op when Signal is not configured.
    fn spawn_signal_sse(&self, inits: &ChannelInits, health: &Arc<RwLock<ChannelHealthRegistry>>) {
        let Some(ref sig_client) = inits.signal_client else {
            return;
        };
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
                    // Best-effort typing indicator; failure is a UX hint only,
                    // never blocks the agent response below.
                    let _ = client
                        .send_typing(Some(&recipient), group_id.as_deref())
                        .await;

                    // Bot mention identifier for group activation. Signal's
                    // resolve_mentions renders the bot's mention as
                    // `@<account>` (phone number) or `@<uuid>` — pass the
                    // phone form so `check_activation` can require an
                    // explicit @bot in group chats when
                    // `gateway.group_activation = Mention` (the default).
                    let bot_id = format!("@{}", client.account());
                    match handler::invoke_agent(
                        "signal",
                        &inbound,
                        &config,
                        Some(&health),
                        Some(&bot_id),
                    )
                    .await
                    {
                        Ok((response_text, _)) => {
                            // Empty = skip the send (silence marker or
                            // non-activated group message); still ack the
                            // inbound with a read receipt below.
                            if !response_text.trim().is_empty() {
                                let send_result = if let Some(ref gid) = group_id {
                                    client.send_group_message(gid, &response_text).await
                                } else {
                                    client.send_message(&recipient, &response_text).await
                                };
                                if let Err(e) = send_result {
                                    warn!("Failed to send Signal response: {e}");
                                }
                            }
                            if let Some(ref mid) = inbound.message_id {
                                if let Ok(ts) = mid.parse::<i64>() {
                                    if let Err(e) =
                                        client.send_read_receipt(&recipient, &[ts]).await
                                    {
                                        warn!("Failed to send Signal read receipt: {e}");
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Agent error in Signal SSE mode: {e}");
                            let error_msg = handler::format_gateway_error(&e);
                            let send_result = if let Some(ref gid) = group_id {
                                client.send_group_message(gid, &error_msg).await
                            } else {
                                client.send_message(&recipient, &error_msg).await
                            };
                            if let Err(send_err) = send_result {
                                warn!("Failed to send Signal error response: {send_err}");
                            }
                        }
                    }
                })
            });

        let sig_sse_client = sig_client.clone();
        let sse_config = self.config.clone();
        crate::supervised_loop::spawn_supervised_loop(
            "signal-sse",
            sse_shutdown.clone(),
            move || {
                let sig_sse_client = sig_sse_client.clone();
                let callback = callback.clone();
                let sse_shutdown = sse_shutdown.clone();
                let sse_config = sse_config.clone();
                async move {
                    crate::signal::sse::run_sse_loop(
                        sig_sse_client,
                        callback,
                        sse_shutdown,
                        sse_config,
                    )
                    .await;
                }
            },
        );
    }

    /// Spawn one generic poll loop per script channel with `mode = "poll"`,
    /// returning the join handles so `run()` can await them after shutdown.
    fn spawn_poll_loops(
        &self,
        state: &Arc<AppState>,
        health: &Arc<RwLock<ChannelHealthRegistry>>,
    ) -> Vec<tokio::task::JoinHandle<()>> {
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

            info!(
                "Starting poll loop for channel '{}' (interval: {}ms)",
                channel.manifest.name, poll_interval_ms
            );

            let channel = channel.clone();
            let poll_config = state.config.clone();
            let poll_shutdown = self.shutdown.clone();
            let poll_health = health.clone();
            let request_timeout = state.request_timeout;
            let handle = crate::supervised_loop::spawn_supervised_loop(
                "channel-poll",
                poll_shutdown.clone(),
                move || {
                    run_channel_poll_loop(
                        channel.clone(),
                        poll_interval_ms,
                        poll_config.clone(),
                        poll_shutdown.clone(),
                        poll_health.clone(),
                        request_timeout,
                    )
                },
            );

            poll_handles.push(handle);
        }
        poll_handles
    }
}

/// Generic poll loop for one script-based channel (`mode = "poll"`): invokes
/// the channel's poll script on its configured interval, applies exponential
/// backoff on consecutive errors (giving up permanently after three exhausted
/// error cycles), and routes returned messages through the agent.
async fn run_channel_poll_loop(
    channel: RegisteredChannel,
    poll_interval_ms: u64,
    config: Config,
    shutdown: CancellationToken,
    poll_health: Arc<RwLock<ChannelHealthRegistry>>,
    request_timeout: Duration,
) {
    let channel_name = channel.manifest.name.clone();
    let start = tokio::time::Instant::now() + Duration::from_millis(poll_interval_ms);
    let mut interval = tokio::time::interval_at(start, Duration::from_millis(poll_interval_ms));

    let mut consecutive_errors: u32 = 0;
    let mut total_error_cycles: u32 = 0;
    let initial_backoff = POLL_INITIAL_BACKOFF;
    let max_backoff = POLL_MAX_BACKOFF;
    let max_consecutive_errors: u32 = POLL_MAX_CONSECUTIVE_ERRORS;

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
                            // Bail out of this run and let the supervisor
                            // restart the loop after its backoff. Previously
                            // this was terminal — the channel stayed dead
                            // until someone restarted the gateway, even after
                            // a transient upstream outage had recovered.
                            tracing::error!(
                                "Poll loop for '{}' failed {} error cycles ({} total errors); restarting after backoff",
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

                let executor = crate::executor::ChannelExecutor::new(
                    &channel.manifest,
                    &channel.dir,
                );
                let blocked_paths = &config.security.blocked_paths;

                let input = serde_json::json!({
                    "channel_dir": channel.dir.to_string_lossy(),
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

                for msg in messages {
                    let result = tokio::time::timeout(
                        request_timeout,
                        handler::handle_polled_message(
                            &channel,
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
}

/// Build the gateway's axum router: health/introspection routes, internal
/// control routes, webhook routes, plus the merged OpenAI-compat, generic
/// webhook, MCP HTTP, and browser (SPA + gRPC-Web) sub-routers.
fn build_router(
    state: Arc<AppState>,
    config: &Config,
    grpc_web: Option<crate::webui::GrpcWebSlot>,
) -> Router {
    let web_surface = grpc_web
        .map(|slot| crate::webui::router(slot, db_open_factory()))
        .unwrap_or_default();

    Router::new()
        .route("/health", get(health_handler))
        .route("/healthz", get(health_handler))
        .route("/readyz", get(readyz_handler))
        .route("/health/channels", get(channel_health_handler))
        .route("/channels", get(list_channels_handler))
        .route("/internal/poke", post(poke_handler))
        .route("/internal/restart", post(restart_handler))
        .route("/internal/cancel", post(cancel_handler))
        .route("/internal/away", post(away_handler))
        .route("/internal/available", post(available_handler))
        .route("/webhook/slack/command", post(slack_command_handler))
        .route("/webhook/{name}", post(webhook_handler))
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .with_state(state)
        .merge(build_openai_compat_router(config))
        .merge(build_generic_webhook_router(config))
        .merge(build_mcp_http_router(config))
        .merge(web_surface)
}

/// Probe `http://{addr}/healthz` to detect an already-running gateway
/// instance before binding the port.
async fn probe_existing_instance(addr: &str) -> bool {
    async {
        // Two seconds, not the shared default: this runs before binding the
        // port, so startup stalls for exactly as long as this probe takes.
        let resp = cortex::http::shared()
            .get(format!("http://{addr}/healthz"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .ok()?;
        resp.status().is_success().then_some(())
    }
    .await
    .is_some()
}

/// Per-request DB factory used by the three v1 HTTP surfaces.
///
/// libsql connections are `Send + Sync + Clone`, but each handler still opens
/// its own connection so the busy-timeout profile (`GATEWAY_BUSY_TIMEOUT_MS`)
/// is applied per request. The factory returns a boxed Future so the closure
/// stays a `Fn` (the inner `async move` block is what awaits the open).
fn db_open_factory() -> crate::DbOpener {
    Arc::new(|| {
        Box::pin(async {
            Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS)
                .await
                .map_err(|e| anyhow::anyhow!("open db: {e}"))
        })
    })
}

/// Build the shared `IngressGate` used by OpenAI-compat + generic webhook.
fn build_ingress_gate(config: &Config) -> Arc<cortex::ingress::IngressGate> {
    let max_per_min = config.gateway.rate_limit_per_minute.max(1);
    Arc::new(cortex::ingress::IngressGate::with_noop_audit(
        max_per_min,
        Duration::from_secs(60),
    ))
}

/// Mount the OpenAI-compat router. Bearer auth, per-token sessions,
/// streaming + non-stream `chat/completions`, `GET /v1/models`.
///
/// `/v1/models` advertises a small fixed alias set rather than every
/// configured `gateway.bindings` row — bindings are routing rules
/// (channel + sender → provider/model), not user-facing model names.
/// `borg-default` and `borg-fast` are what ChatBox / Open WebUI / the
/// official OpenAI SDK starter configs autocomplete to; the agent
/// resolves the actual provider/model per its own config regardless
/// of which alias the client picks.
fn build_openai_compat_router(config: &Config) -> Router {
    use crate::openai_compat::{router, OpenAiState};
    router(OpenAiState {
        db_open: db_open_factory(),
        gate: build_ingress_gate(config),
        dispatcher: crate::openai_compat::arc_dispatcher(),
        embedding_dispatcher: crate::openai_compat::arc_embedding_dispatcher(config),
        model_aliases: Arc::new(vec!["borg-default".into(), "borg-fast".into()]),
    })
}

/// Mount the generic-webhook router (`POST /v1/webhook/{name}`).
fn build_generic_webhook_router(config: &Config) -> Router {
    use crate::generic_webhook::{router, WebhookState};
    router(WebhookState {
        db_open: db_open_factory(),
        gate: build_ingress_gate(config),
        sink: crate::generic_webhook::arc_sink(),
    })
}

/// Mount the MCP HTTP router (`POST /mcp`). Bearer-authenticated;
/// exposes the read-only tool set built by
/// [`crate::mcp::build_read_only_tools`].
fn build_mcp_http_router(config: &Config) -> Router {
    use crate::mcp::{build_read_only_tools, http_router, McpHttpState, McpServer};
    let server = Arc::new(McpServer::new(
        build_read_only_tools(config.clone()),
        env!("CARGO_PKG_VERSION"),
    ));
    http_router(McpHttpState {
        db_open: db_open_factory(),
        server,
    })
}

async fn health_handler() -> impl IntoResponse {
    axum::Json(serde_json::json!({ "status": "ok" }))
}

async fn readyz_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut checks = serde_json::Map::new();
    let mut ready = true;

    match Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS).await {
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

    let channel_count = state.registry.list_channels().len() + state.native_channels.list().len();
    checks.insert("channels".into(), serde_json::json!(channel_count));

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
            // Fire-and-forget: a poke is a hint, and a closed receiver means the
            // heartbeat runtime is already gone — still a 200 for the caller.
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

/// How long a restart waits for in-flight turns to finish before stopping
/// anyway. A ceiling, not a promise: a wedged turn must not block a restart
/// forever, but the common case (a turn a few seconds from done) should survive.
const RESTART_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll interval while draining. Short enough that an idle gateway restarts
/// with no perceptible delay.
const RESTART_DRAIN_POLL: Duration = Duration::from_millis(250);

/// Expand a configured `host` into the concrete socket addresses to bind.
///
/// A wildcard host means "every interface", but `0.0.0.0` is the IPv4 wildcard
/// *only* — binding it leaves the gateway unreachable on an IPv6-only network
/// (a common hosted-deploy setup). `::` is not a safe substitute either: on a
/// host with `bindv6only=1` it covers IPv6 alone, mirroring the bug. So a
/// wildcard expands to one address per family and the caller binds each.
///
/// Any non-wildcard host is returned verbatim as a single target — an operator
/// who names an interface gets exactly that interface.
fn bind_targets(host: &str, port: u16) -> Vec<String> {
    let trimmed = host.trim();
    // Empty is deliberately NOT a wildcard — see `blank_host_is_not_a_wildcard`.
    let is_wildcard = matches!(trimmed, "0.0.0.0" | "::" | "[::]" | "*");
    if is_wildcard {
        // IPv6 first, deliberately. On a dual-stack host (`bindv6only=0`, the
        // Linux default) a `[::]` listener already accepts IPv4, so binding
        // `0.0.0.0` first makes the subsequent `[::]` bind fail with
        // `AddrInUse` and leaves the gateway IPv4-only — reinstating the exact
        // bug this expansion exists to fix. In the reverse order the redundant
        // bind is the IPv4 one, and losing it costs nothing.
        vec![format!("[::]:{port}"), format!("0.0.0.0:{port}")]
    } else if trimmed.contains(':') && !trimmed.starts_with('[') {
        // Bare IPv6 literal — bracket it so `addr:port` parses.
        vec![format!("[{trimmed}]:{port}")]
    } else {
        vec![format!("{trimmed}:{port}")]
    }
}

/// Poll `exec` until no turns are in flight, or `timeout` elapses. Returns the
/// number still running at exit — 0 means a clean drain, anything else means
/// the ceiling was hit and the caller should stop anyway.
///
/// The timeout is deliberately a ceiling rather than a promise: a wedged turn
/// must never make the gateway unrestartable.
async fn wait_for_turns_to_drain(
    exec: &dyn crate::session_executor::SessionExecutor,
    timeout: Duration,
    poll: Duration,
) -> usize {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = exec.active_turns().await;
        if remaining == 0 {
            return 0;
        }
        if tokio::time::Instant::now() >= deadline {
            return remaining;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Restart the gateway: cancels the shutdown token so the owning process (daemon/TUI)
/// can detect the clean exit and respawn with fresh config. Only localhost.
///
/// The cancel is **deferred until in-flight turns finish**. A restart is
/// normally requested from inside a turn (`borg restart`, or the agent itself),
/// and cancelling immediately tore down the gateway while that very turn was
/// still running — it landed in the shutdown drain wait-set and was killed at
/// the drain timeout, so the request that asked for the restart never got its
/// reply. Draining first means the stop happens against an idle gateway.
///
/// The HTTP response returns immediately; the wait happens on a spawned task so
/// the caller (often the in-flight turn itself) is never blocked on its own
/// restart.
async fn restart_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    if !is_local_request(&addr) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "localhost only"})),
        );
    }
    info!("Gateway restart requested via /internal/restart");
    crate::activity::log_gateway_activity("info", "Gateway restart requested").await;

    // No executor installed (standalone gateway, tests) means no turns to
    // drain — restart immediately rather than waiting out the full timeout.
    let active = match crate::session_executor::global() {
        Some(exec) => exec.active_turns().await,
        None => 0,
    };

    if active == 0 {
        state.shutdown.cancel();
        return (
            StatusCode::OK,
            axum::Json(serde_json::json!({"status": "restarting", "deferred": false})),
        );
    }

    info!(
        active,
        "Deferring gateway restart until in-flight turns finish"
    );
    let shutdown = state.shutdown.clone();
    let exec = crate::session_executor::global().cloned();
    tokio::spawn(async move {
        if let Some(exec) = exec {
            let remaining =
                wait_for_turns_to_drain(exec.as_ref(), RESTART_DRAIN_TIMEOUT, RESTART_DRAIN_POLL)
                    .await;
            if remaining == 0 {
                info!("In-flight turns drained; restarting gateway");
            } else {
                warn!(
                    remaining,
                    timeout_secs = RESTART_DRAIN_TIMEOUT.as_secs(),
                    "Restart drain timed out with turns still in flight; stopping anyway"
                );
            }
        }
        shutdown.cancel();
    });

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "status": "restarting",
            "deferred": true,
            "active_turns": active,
        })),
    )
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
    let executor = match crate::session_executor::global() {
        Some(exec) => exec,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({"error": "no session executor installed"})),
            );
        }
    };
    if let Some(session_id) = params.get("session") {
        let cancelled = executor.cancel(session_id).await;
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
        let count = executor.cancel_all().await;
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

/// Resolve the client address a request is attributed to.
///
/// Proxy headers are only honored when the direct peer is loopback (a local
/// reverse proxy or tunnel). Those proxies *append* to `X-Forwarded-For`, so
/// the entry they wrote is the RIGHTMOST one; everything to its left is
/// whatever the remote client sent and is attacker-controlled. Taking the
/// leftmost entry let a client pick its own rate-limit key.
fn extract_client_ip(
    headers: &HeaderMap,
    connect_info: &ConnectInfo<std::net::SocketAddr>,
) -> String {
    let peer_ip = connect_info.0.ip();

    if peer_ip.is_loopback() {
        if let Some(xff) = headers.get("x-forwarded-for") {
            if let Ok(val) = xff.to_str() {
                if let Some(last) = val.rsplit(',').map(str::trim).find(|s| !s.is_empty()) {
                    return last.to_string();
                }
            }
        }

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

/// Whether a request may skip the webhook rate limiter: only a direct
/// loopback peer that carries NO proxy headers. A loopback peer *with*
/// `X-Forwarded-For`/`X-Real-IP` is a tunnelled remote request, and the
/// header value is the client's own claim — a spoofed `127.0.0.1` there must
/// still be limited, under whatever key `extract_client_ip` resolves.
fn rate_limit_exempt(
    headers: &HeaderMap,
    connect_info: &ConnectInfo<std::net::SocketAddr>,
) -> bool {
    SlidingWindowLimiter::is_exempt(&connect_info.0.ip())
        && !headers.contains_key("x-forwarded-for")
        && !headers.contains_key("x-real-ip")
}

async fn webhook_handler(
    state: State<Arc<AppState>>,
    addr: ConnectInfo<std::net::SocketAddr>,
    name: Path<String>,
    headers: HeaderMap,
    body: String,
) -> WebhookResponse {
    use tracing::Instrument;
    // Chain the inbound webhook to its upstream trace (W3C `traceparent`), so
    // collectors stitch the gateway span under the caller's distributed trace.
    let span = tracing::info_span!("webhook.process", channel = %name.0);
    cortex::telemetry::propagation::set_span_parent_from_headers(&span, &headers);
    webhook_handler_inner(state, addr, name, headers, body)
        .instrument(span)
        .await
}

async fn webhook_handler_inner(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: String,
) -> WebhookResponse {
    let _timer = GatewayTimer::new(&state.metrics.gateway_duration);
    state.metrics.gateway_requests.add(1, &[]);

    let target = match resolve_webhook_target(&state.native_channels, &state.registry, &name) {
        Some(target) => target,
        None => {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({ "error": "Unknown channel" })),
            );
        }
    };

    // Rate-limit before signature verification: HMAC over an attacker-sized
    // body is the expensive part we're protecting.
    if let Some(ref limiter) = state.rate_limiter {
        let connect_info = ConnectInfo(addr);
        if !rate_limit_exempt(&headers, &connect_info) {
            let client_ip_str = extract_client_ip(&headers, &connect_info);
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
    let channel = match target {
        WebhookTarget::Native(channel) => {
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
        WebhookTarget::Script(channel) => channel,
    };

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
                // Best-effort: the HTTP caller may have disconnected and dropped
                // the receiver, which is not an error worth surfacing here.
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

/// Ephemeral reply to a slash command from a channel off
/// `gateway.slack_channel_allowlist`: says why nothing happened and what to do.
const SLACK_CHANNEL_NOT_ALLOWED_TEXT: &str = "Borg is not turned on in this channel. \
     Message Borg directly, or ask your Borg admin to add this channel to \
     gateway.slack_channel_allowlist.";

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

/// Slack slash-command handler (separate route from webhook_handler).
async fn slack_command_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> WebhookResponse {
    let _timer = GatewayTimer::new(&state.metrics.gateway_duration);
    state.metrics.gateway_requests.add(1, &[]);

    // Verify signature — fail closed. A slash command is an arbitrary agent
    // prompt, so an unauthenticated request here is equivalent to full command
    // injection. If no signing secret is configured we refuse rather than trust
    // the request (matching the inbound webhook fail-closed posture).
    let secret = state.slack_signing_secret.as_deref();
    if let Err(e) =
        crate::verify_common::require_verification_secret("Slack", secret, "SLACK_SIGNING_SECRET")
    {
        warn!("Slack slash command refused: {e}");
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({ "error": "Unauthorized" })),
        );
    }
    let secret = secret.unwrap_or_default();
    if let Err(e) = crate::slack::verify::verify_slack_signature(&headers, &body, secret) {
        warn!("Slack command signature verification failed: {e}");
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({ "error": "Unauthorized" })),
        );
    }

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

    if !crate::slack::channel_allowed(
        state.config.gateway.slack_channel_allowlist.as_deref(),
        Some(&payload.channel_id),
    ) {
        info!(
            "Slack slash command from channel {} dropped: not in gateway.slack_channel_allowlist",
            payload.channel_id
        );
        // Slack shows an ephemeral reply only to the person who ran the
        // command; an empty ack would leave them with no answer at all.
        return (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "response_type": "ephemeral",
                "text": SLACK_CHANNEL_NOT_ALLOWED_TEXT,
            })),
        );
    }

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

                    // Empty = skip delivery (silence marker or non-activated
                    // group message); chat.postMessage rejects empty text.
                    if response_text.trim().is_empty() {
                        return Ok::<_, anyhow::Error>(());
                    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use cortex::telemetry::names;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    #[test]
    fn gateway_timer_records_one_histogram_observation_on_drop() {
        // Spec: GatewayTimer must record exactly one duration observation on
        // drop, regardless of where in a handler an early return happens.
        // This is the regression guard for the `gateway_duration` histogram
        // that was declared but never recorded for the entire pre-fix
        // lifetime of the instrument.
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone())
            .with_interval(std::time::Duration::from_secs(3600))
            .build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let meter = provider.meter("borg-test");
        let metrics = BorgMetrics::from_meter(&meter);

        {
            let _timer = GatewayTimer::new(&metrics.gateway_duration);
            // simulated handler body, including an early return
        }

        provider.force_flush().expect("flush");
        let resource_metrics = exporter.get_finished_metrics().expect("metrics present");
        let mut count = 0u64;
        for rm in resource_metrics {
            for sm in rm.scope_metrics() {
                for m in sm.metrics() {
                    if m.name() == names::GATEWAY_DURATION {
                        if let AggregatedMetrics::F64(MetricData::Histogram(h)) = m.data() {
                            count += h.data_points().map(|dp| dp.count()).sum::<u64>();
                        }
                    }
                }
            }
        }
        assert_eq!(
            count, 1,
            "GatewayTimer must record exactly one observation on drop"
        );
    }

    #[test]
    fn webhook_handlers_install_gateway_timer() {
        // Source-grep guard: both webhook entry points must instantiate a
        // GatewayTimer at the top so all early-return paths are timed. If a
        // future edit removes one, the gateway_duration histogram silently
        // becomes a partial sample of webhook traffic. The invariant ("every
        // webhook entry point starts a timer") is not type-expressible, hence
        // the source-grep guard.
        let src = include_str!("server.rs");
        // `webhook_handler` is a thin tracing wrapper that always delegates to
        // `webhook_handler_inner`; the inner fn holds every early-return path,
        // so that is where the timer must sit to time them all.
        let webhook_idx = src
            .find("async fn webhook_handler_inner")
            .expect("webhook_handler_inner exists");
        let slack_idx = src
            .find("async fn slack_command_handler")
            .expect("slack_command_handler exists");
        for (name, idx) in [
            ("webhook_handler_inner", webhook_idx),
            ("slack_command_handler", slack_idx),
        ] {
            // Search the next 400 chars for the timer construction.
            let window = &src[idx..idx + 400.min(src.len() - idx)];
            assert!(
                window.contains("GatewayTimer::new(&state.metrics.gateway_duration)"),
                "{name} must install GatewayTimer at entry — gateway_duration won't record otherwise"
            );
        }
    }

    fn loopback_connect_info() -> ConnectInfo<std::net::SocketAddr> {
        ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 12345)))
    }

    fn non_loopback_connect_info() -> ConnectInfo<std::net::SocketAddr> {
        ConnectInfo(std::net::SocketAddr::from(([192, 168, 1, 100], 12345)))
    }

    #[test]
    fn extract_client_ip_xff_from_loopback() {
        // The trusted local proxy appends its observed peer LAST; the
        // leftmost entry is whatever the remote client chose to send.
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.2.3.4, 5.6.7.8"),
        );
        let info = loopback_connect_info();
        assert_eq!(extract_client_ip(&headers, &info), "5.6.7.8");
    }

    #[test]
    fn extract_client_ip_xff_skips_trailing_empty_entry() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.2.3.4, 5.6.7.8, "),
        );
        let info = loopback_connect_info();
        assert_eq!(extract_client_ip(&headers, &info), "5.6.7.8");
    }

    #[test]
    fn spoofed_loopback_xff_is_still_rate_limited() {
        // A tunnelled request reaches the gateway from a loopback peer, so a
        // client-supplied `X-Forwarded-For: 127.0.0.1` used to look like a
        // local caller and skip the limiter protecting the HMAC check.
        let cases: &[(&str, &str)] = &[
            ("x-forwarded-for", "127.0.0.1"),
            ("x-forwarded-for", "9.9.9.9, 127.0.0.1"),
            ("x-real-ip", "127.0.0.1"),
            ("x-forwarded-for", "1.2.3.4"),
        ];
        for (name, value) in cases {
            let mut headers = HeaderMap::new();
            headers.insert(*name, HeaderValue::from_static(value));
            assert!(
                !rate_limit_exempt(&headers, &loopback_connect_info()),
                "{name}: {value} must not be exempt"
            );
        }
        // Only a bare loopback peer with no proxy header is exempt.
        assert!(rate_limit_exempt(
            &HeaderMap::new(),
            &loopback_connect_info()
        ));
        assert!(!rate_limit_exempt(
            &HeaderMap::new(),
            &non_loopback_connect_info()
        ));
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
        use crate::channel::channel_trait::{NativeChannel, WebhookContext, WebhookOutcome};

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
        use crate::channel::channel_trait::{NativeChannel, WebhookContext, WebhookOutcome};

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
    fn native_webhook_resolves_without_script_manifest() {
        use crate::channel::channel_trait::{NativeChannel, WebhookContext, WebhookOutcome};

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
        let dir = tempfile::tempdir().unwrap();
        let scripts = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();

        match resolve_webhook_target(&native, &scripts, "telegram") {
            Some(WebhookTarget::Native(channel)) => assert_eq!(channel.names()[0], "telegram"),
            _ => panic!("native webhook must resolve without a matching script channel"),
        }
    }

    #[test]
    fn native_webhook_takes_precedence_over_same_name_script_channel() {
        use crate::channel::channel_trait::{NativeChannel, WebhookContext, WebhookOutcome};

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

        let dir = tempfile::tempdir().unwrap();
        let channel_dir = dir.path().join("telegram-script");
        std::fs::create_dir_all(&channel_dir).unwrap();
        std::fs::write(
            channel_dir.join("channel.toml"),
            r#"
name = "telegram"
description = "Script fallback"
"#,
        )
        .unwrap();

        let mut native = NativeChannelRegistry::new();
        native.register(std::sync::Arc::new(TestChannel("telegram")));
        let scripts = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();

        match resolve_webhook_target(&native, &scripts, "telegram") {
            Some(WebhookTarget::Native(channel)) => assert_eq!(channel.names()[0], "telegram"),
            _ => panic!("native channel must win when script and native names collide"),
        }
    }

    #[test]
    fn readyz_provider_detection_logic() {
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

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Executor whose active-turn count follows a scripted sequence, so the
    /// drain loop can be exercised without a live daemon.
    struct DrainingExecutor {
        counts: std::sync::Mutex<std::collections::VecDeque<usize>>,
        /// Value returned once the script is exhausted.
        tail: usize,
        polls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::session_executor::SessionExecutor for DrainingExecutor {
        async fn start_turn(
            &self,
            _turn: crate::session_executor::InboundTurn,
        ) -> anyhow::Result<(
            tokio::sync::mpsc::Receiver<cortex::agent::AgentEvent>,
            CancellationToken,
        )> {
            anyhow::bail!("not used")
        }
        async fn cancel(&self, _key: &str) -> bool {
            false
        }
        async fn cancel_all(&self) -> usize {
            0
        }
        async fn active_turns(&self) -> usize {
            self.polls.fetch_add(1, Ordering::SeqCst);
            self.counts
                .lock()
                .expect("script lock")
                .pop_front()
                .unwrap_or(self.tail)
        }
    }

    fn draining_executor(script: &[usize], tail: usize) -> (DrainingExecutor, Arc<AtomicUsize>) {
        let polls = Arc::new(AtomicUsize::new(0));
        (
            DrainingExecutor {
                counts: std::sync::Mutex::new(script.iter().copied().collect()),
                tail,
                polls: polls.clone(),
            },
            polls,
        )
    }

    /// REGRESSION: `/internal/restart` cancelled the shutdown token outright,
    /// so the turn that requested the restart was still running when the
    /// gateway tore down — it landed in the drain wait-set and was killed at
    /// the drain timeout, and the request never got its reply. The restart
    /// must wait for in-flight turns to reach zero first.
    #[tokio::test(start_paused = true)]
    async fn restart_drain_waits_for_in_flight_turns() {
        let (exec, polls) = draining_executor(&[2, 2, 1], 0);

        let remaining = wait_for_turns_to_drain(
            &exec,
            RESTART_DRAIN_TIMEOUT,
            std::time::Duration::from_millis(250),
        )
        .await;

        assert_eq!(
            remaining, 0,
            "the drain must report a clean exit once the last turn finishes"
        );
        assert_eq!(
            polls.load(Ordering::SeqCst),
            4,
            "must keep polling while turns are live, then stop at the first zero"
        );
    }

    /// The wait is a ceiling, not a promise: a wedged turn must never make the
    /// gateway unrestartable. On timeout the drain reports what is still
    /// running so the caller can stop anyway and say why.
    #[tokio::test(start_paused = true)]
    async fn restart_drain_gives_up_at_the_timeout() {
        // Never drains.
        let (exec, _polls) = draining_executor(&[], 3);

        let remaining = wait_for_turns_to_drain(
            &exec,
            std::time::Duration::from_secs(2),
            std::time::Duration::from_millis(250),
        )
        .await;

        assert_eq!(
            remaining, 3,
            "a timed-out drain must surface the still-running count, not silently report success"
        );
    }

    /// REGRESSION: the gateway formatted one `host:port` into a single
    /// `TcpListener::bind`, so an operator setting `0.0.0.0` for a hosted
    /// deploy got an IPv4-only listener and the service was silently
    /// unreachable over IPv6.
    #[test]
    fn wildcard_host_expands_to_both_address_families() {
        let targets = bind_targets("0.0.0.0", 7842);
        assert!(
            targets.contains(&"0.0.0.0:7842".to_string()),
            "IPv4 wildcard must still be bound: {targets:?}"
        );
        assert!(
            targets.contains(&"[::]:7842".to_string()),
            "IPv6 wildcard must also be bound, or IPv6-only networks cannot reach the gateway: \
             {targets:?}"
        );

        // `::` is not a safe stand-in for both: on a bindv6only=1 host it
        // covers IPv6 alone, which is the same bug mirrored. It must expand
        // identically.
        assert_eq!(
            bind_targets("::", 7842),
            bind_targets("0.0.0.0", 7842),
            "every wildcard spelling must expand to the same per-family set"
        );
    }

    /// A blank `gateway.host` must never read as "every interface". It is a
    /// free-text setting the agent can clear, and this listener has no mTLS, so
    /// treating empty as a wildcard turns a cleared setting into a public
    /// webhook endpoint. Blank has to fail, not expand.
    #[test]
    fn blank_host_is_not_a_wildcard() {
        assert_ne!(
            bind_targets("", 7842),
            bind_targets("0.0.0.0", 7842),
            "an empty host must not expand to the wildcard set"
        );
        assert!(
            !bind_targets("", 7842).contains(&"0.0.0.0:7842".to_string()),
            "an empty host must never produce an all-interfaces bind"
        );
    }

    /// An operator who names a specific interface must get exactly that one —
    /// expanding it would bind addresses they did not ask for, which for the
    /// default `127.0.0.1` would mean silently exposing the gateway.
    #[test]
    fn explicit_host_binds_only_that_interface() {
        assert_eq!(bind_targets("127.0.0.1", 7842), vec!["127.0.0.1:7842"]);
        assert_eq!(bind_targets("192.168.1.5", 80), vec!["192.168.1.5:80"]);
        assert_eq!(
            bind_targets("example.internal", 443),
            vec!["example.internal:443"]
        );
    }

    /// A bare IPv6 literal has to be bracketed or `addr:port` parsing splits it
    /// at the wrong colon and the bind fails with a confusing parse error.
    #[test]
    fn bare_ipv6_literal_is_bracketed() {
        assert_eq!(bind_targets("::1", 7842), vec!["[::1]:7842"]);
        assert_eq!(
            bind_targets("fd00::1", 7842),
            vec!["[fd00::1]:7842"],
            "a ULA address must bind as written, not be re-expanded"
        );
        // Already-bracketed input must not be double-bracketed.
        assert_eq!(bind_targets("[::1]", 7842), vec!["[::1]:7842"]);
    }

    /// An idle gateway must restart immediately — no drain delay at all.
    #[tokio::test(start_paused = true)]
    async fn restart_drain_returns_immediately_when_idle() {
        let (exec, polls) = draining_executor(&[0], 0);

        let remaining = wait_for_turns_to_drain(
            &exec,
            RESTART_DRAIN_TIMEOUT,
            std::time::Duration::from_millis(250),
        )
        .await;

        assert_eq!(remaining, 0);
        assert_eq!(
            polls.load(Ordering::SeqCst),
            1,
            "an idle gateway must not sleep even one poll interval before restarting"
        );
    }

    // ── shared fixtures for handler-level tests ─────────────────────────

    use crate::channel::channel_trait::{NativeChannel, WebhookContext as WCtx, WebhookOutcome};

    /// Minimal native channel whose webhook handling always skips — enough to
    /// exercise routing, rate limiting, and dispatch without a real backend.
    struct StubNative(&'static str);

    #[async_trait::async_trait]
    impl NativeChannel for StubNative {
        fn names(&self) -> Vec<&str> {
            vec![self.0]
        }
        async fn handle_webhook(
            &self,
            _h: &HeaderMap,
            _b: &str,
            _c: &WCtx<'_>,
        ) -> anyhow::Result<WebhookOutcome> {
            Ok(WebhookOutcome::Skip)
        }
        async fn send_response(
            &self,
            _t: &str,
            _c: &serde_json::Value,
            _h: &Arc<RwLock<ChannelHealthRegistry>>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn test_metrics() -> BorgMetrics {
        let provider = SdkMeterProvider::builder().build();
        BorgMetrics::from_meter(&provider.meter("gateway-server-test"))
    }

    #[allow(clippy::too_many_arguments)]
    fn test_state(
        config: Config,
        registry: ChannelRegistry,
        native_names: &[&'static str],
        rate_limit: u32,
        poke_tx: Option<mpsc::Sender<()>>,
        slack: Option<(Arc<SlackClient>, String)>,
    ) -> Arc<AppState> {
        let mut native = NativeChannelRegistry::new();
        for n in native_names {
            native.register(Arc::new(StubNative(n)));
        }
        let rate_limiter = (rate_limit > 0).then(|| {
            Arc::new(Mutex::new(SlidingWindowLimiter::new(
                rate_limit,
                Duration::from_secs(60),
            )))
        });
        let (slack_client, slack_signing_secret) = match slack {
            Some((c, s)) => (Some(c), Some(s)),
            None => (None, None),
        };
        Arc::new(AppState {
            config,
            registry,
            native_channels: native,
            session_queue: SessionQueue::new(4),
            request_timeout: Duration::from_secs(10),
            health: Arc::new(RwLock::new(ChannelHealthRegistry::new())),
            rate_limiter,
            metrics: test_metrics(),
            slack_client,
            slack_signing_secret,
            slack_bot_user_id: None,
            poke_tx,
            auto_reply_state: Arc::new(tokio::sync::RwLock::new(
                crate::auto_reply::AutoReplyState::default(),
            )),
            shutdown: CancellationToken::new(),
        })
    }

    fn empty_state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
        (
            test_state(Config::default(), registry, &[], 0, None, None),
            dir,
        )
    }

    async fn response_parts(resp: axum::response::Response) -> (StatusCode, serde_json::Value) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    /// Save, clear, and restore a set of env vars on drop — keeps env-driven
    /// assertions deterministic regardless of the host shell.
    struct EnvVarsCleared {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvVarsCleared {
        fn new(keys: &[&'static str]) -> Self {
            let saved = keys.iter().map(|k| (*k, std::env::var_os(k))).collect();
            for k in keys {
                std::env::remove_var(k);
            }
            Self { saved }
        }
    }

    impl Drop for EnvVarsCleared {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    /// Point `BORG_HOME` at a tempdir for the guard's lifetime.
    struct BorgHomeTemp {
        _dir: tempfile::TempDir,
        saved: Option<std::ffi::OsString>,
    }

    impl BorgHomeTemp {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let saved = std::env::var_os("BORG_HOME");
            std::env::set_var("BORG_HOME", dir.path());
            Self { _dir: dir, saved }
        }
    }

    impl Drop for BorgHomeTemp {
        fn drop(&mut self) {
            match &self.saved {
                Some(v) => std::env::set_var("BORG_HOME", v),
                None => std::env::remove_var("BORG_HOME"),
            }
        }
    }

    // ── router construction ─────────────────────────────────────────────

    /// The composed router and each merged sub-router must actually mount
    /// their routes — an accidentally-empty router would 404 every surface
    /// while the gateway still reports healthy startup.
    #[tokio::test]
    async fn router_mounts_core_and_merged_surfaces() {
        use tower::ServiceExt;
        let _guard = crate::test_env::borg_home_guard();
        let _home = BorgHomeTemp::new();
        let config = Config::default();

        let (state, _dir) = empty_state();
        let app = build_router(state, &config, None);
        let resp = app
            .oneshot(
                axum::http::Request::get("/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = response_parts(resp).await;
        assert_eq!(status, StatusCode::OK, "/health must be routed");
        assert_eq!(body["status"], "ok");

        // OpenAI-compat surface: an unauthenticated /v1/models must be
        // answered (401), not unrouted (404).
        let resp = build_openai_compat_router(&config)
            .oneshot(
                axum::http::Request::get("/v1/models")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "/v1/models must be mounted and enforce bearer auth"
        );

        // Generic webhook surface: an unknown name is answered with the
        // anti-enumeration 404 *body*, which an unrouted 404 lacks.
        let resp = build_generic_webhook_router(&config)
            .oneshot(
                axum::http::Request::post("/v1/webhook/no-such-hook")
                    .body(axum::body::Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = response_parts(resp).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            body.to_string().contains("no such webhook"),
            "generic webhook route must answer with its own error body, got: {body}"
        );

        // MCP surface: unauthenticated POST /mcp must be answered (401).
        let resp = build_mcp_http_router(&config)
            .oneshot(
                axum::http::Request::post("/mcp")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "/mcp must be mounted and enforce bearer auth"
        );
    }

    // ── startup probe ───────────────────────────────────────────────────

    #[tokio::test]
    async fn probe_detects_live_instance_and_absent_port() {
        // A live gateway on the port must be detected (so a double-start
        // parks instead of crashing), and a free port must NOT be detected
        // (or the gateway would never bind at all).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/healthz", get(|| async { "ok" }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        assert!(
            probe_existing_instance(&addr.to_string()).await,
            "a live /healthz responder must be detected"
        );

        let free = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let free_addr = free.local_addr().unwrap();
        drop(free);
        assert!(
            !probe_existing_instance(&free_addr.to_string()).await,
            "an unbound port must not read as an existing instance"
        );
    }

    // ── readiness ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn readyz_counts_channels_and_detects_each_provider_source() {
        let _guard = crate::test_env::borg_home_guard();
        let _home = BorgHomeTemp::new();
        let _env =
            EnvVarsCleared::new(&["OPENROUTER_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY"]);

        // (api_key_env, env var to set, expected readiness)
        let cases: &[(&str, Option<&'static str>, StatusCode)] = &[
            // Default config carries a non-empty api_key_env — that alone
            // must count as a provider source.
            ("OPENROUTER_API_KEY", None, StatusCode::OK),
            // Each recognized env var must independently satisfy the check.
            ("", Some("OPENROUTER_API_KEY"), StatusCode::OK),
            ("", Some("OPENAI_API_KEY"), StatusCode::OK),
            ("", Some("ANTHROPIC_API_KEY"), StatusCode::OK),
            // No provider anywhere → not ready.
            ("", None, StatusCode::SERVICE_UNAVAILABLE),
        ];

        for (api_key_env, env_key, expected) in cases {
            let mut config = Config::default();
            config.llm.provider = None;
            config.llm.api_key_env = (*api_key_env).to_string();
            if let Some(k) = env_key {
                std::env::set_var(k, "test-key");
            }

            let dir = tempfile::tempdir().unwrap();
            let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
            let state = test_state(config, registry, &["alpha", "beta"], 0, None, None);

            let resp = readyz_handler(State(state)).await.into_response();
            let (status, body) = response_parts(resp).await;

            if let Some(k) = env_key {
                std::env::remove_var(k);
            }

            assert_eq!(
                status, *expected,
                "api_key_env={api_key_env:?} env={env_key:?} body={body}"
            );
            assert_eq!(
                body["ready"],
                serde_json::json!(*expected == StatusCode::OK),
                "ready flag must match the status code"
            );
            assert_eq!(
                body["checks"]["channels"],
                serde_json::json!(2),
                "channel count must be the script+native sum"
            );
        }
    }

    // ── loopback gating ─────────────────────────────────────────────────

    #[test]
    fn is_local_request_accepts_only_loopback_forms() {
        let cases: &[(&str, bool)] = &[
            ("127.0.0.1:1", true),
            ("127.8.9.10:1", true),
            ("192.168.1.100:1", false),
            ("8.8.8.8:1", false),
            ("[::1]:1", true),
            ("[::ffff:127.0.0.1]:1", true),
            ("[::ffff:8.8.8.8]:1", false),
            ("[2001:db8::1]:1", false),
        ];
        for (addr, expected) in cases {
            let addr: std::net::SocketAddr = addr.parse().unwrap();
            assert_eq!(
                is_local_request(&addr),
                *expected,
                "is_local_request({addr})"
            );
        }
    }

    #[tokio::test]
    async fn poke_requires_loopback_and_signals_heartbeat() {
        let (tx, mut rx) = mpsc::channel(1);
        let dir = tempfile::tempdir().unwrap();
        let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
        let state = test_state(Config::default(), registry, &[], 0, Some(tx), None);

        let resp = poke_handler(State(state.clone()), loopback_connect_info())
            .await
            .into_response();
        let (status, body) = response_parts(resp).await;
        assert_eq!(status, StatusCode::OK, "loopback poke must succeed: {body}");
        assert!(
            rx.try_recv().is_ok(),
            "a successful poke must actually signal the heartbeat channel"
        );

        let resp = poke_handler(State(state), non_loopback_connect_info())
            .await
            .into_response();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "non-loopback poke must be refused (it triggers LLM cost)"
        );
        assert!(
            rx.try_recv().is_err(),
            "a refused poke must not signal the heartbeat channel"
        );
    }

    #[tokio::test]
    async fn restart_from_loopback_cancels_immediately_when_idle() {
        let _guard = crate::test_env::borg_home_guard();
        let _home = BorgHomeTemp::new();
        let (state, _dir) = empty_state();

        // No executor installed → zero active turns → the restart must be
        // immediate (deferred=false) and cancel the shutdown token in-band.
        let resp = restart_handler(State(state.clone()), loopback_connect_info())
            .await
            .into_response();
        let (status, body) = response_parts(resp).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(
            body["deferred"],
            serde_json::json!(false),
            "an idle gateway must not defer its restart"
        );
        assert!(
            state.shutdown.is_cancelled(),
            "an immediate restart must cancel the shutdown token before responding"
        );

        // Non-loopback callers must never be able to restart the gateway.
        let (other, _dir2) = empty_state();
        let resp = restart_handler(State(other.clone()), non_loopback_connect_info())
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(
            !other.shutdown.is_cancelled(),
            "a refused restart must leave the gateway running"
        );
    }

    #[tokio::test]
    async fn cancel_requires_loopback() {
        // Loopback without an installed executor → 503, not a silent OK and
        // never FORBIDDEN.
        let resp = cancel_handler(
            loopback_connect_info(),
            axum::extract::Query(std::collections::HashMap::new()),
        )
        .await
        .into_response();
        let (status, body) = response_parts(resp).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
        assert!(body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no session executor"));

        let resp = cancel_handler(
            non_loopback_connect_info(),
            axum::extract::Query(std::collections::HashMap::new()),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn away_and_available_toggle_auto_reply_for_loopback_only() {
        let (state, _dir) = empty_state();

        // Away with an explicit JSON message.
        let resp = away_handler(
            State(state.clone()),
            loopback_connect_info(),
            r#"{"message":"brb"}"#.to_string(),
        )
        .await
        .into_response();
        let (status, body) = response_parts(resp).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(body["message"], "brb");
        match &*state.auto_reply_state.read().await {
            crate::auto_reply::AutoReplyState::Away(msg) => assert_eq!(msg, "brb"),
            other => panic!("state must be Away after /internal/away, got {other:?}"),
        }

        // Non-loopback must not flip the state.
        let resp = available_handler(State(state.clone()), non_loopback_connect_info())
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(
            matches!(
                &*state.auto_reply_state.read().await,
                crate::auto_reply::AutoReplyState::Away(_)
            ),
            "a refused request must leave auto-reply untouched"
        );

        // Loopback available restores the state.
        let resp = available_handler(State(state.clone()), loopback_connect_info())
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(matches!(
            &*state.auto_reply_state.read().await,
            crate::auto_reply::AutoReplyState::Available
        ));

        // Non-loopback away is refused too.
        let resp = away_handler(
            State(state.clone()),
            non_loopback_connect_info(),
            String::new(),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(matches!(
            &*state.auto_reply_state.read().await,
            crate::auto_reply::AutoReplyState::Available
        ));
    }

    // ── webhook dispatch ────────────────────────────────────────────────

    #[tokio::test]
    async fn webhook_unknown_channel_is_a_404_error() {
        let (state, _dir) = empty_state();
        let (status, body) = webhook_handler(
            State(state),
            loopback_connect_info(),
            Path("no-such-channel".to_string()),
            HeaderMap::new(),
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body.0["error"], "Unknown channel");
    }

    #[tokio::test]
    async fn webhook_rate_limit_exempts_loopback_and_throttles_remote() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
        // Limit of 1/min so the second remote request must trip.
        let state = test_state(Config::default(), registry, &["nat"], 1, None, None);

        // Remote caller: first request passes, second is throttled.
        let (status, _) = webhook_handler(
            State(state.clone()),
            non_loopback_connect_info(),
            Path("nat".to_string()),
            HeaderMap::new(),
            "{}".to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "first remote request must pass");
        let (status, body) = webhook_handler(
            State(state.clone()),
            non_loopback_connect_info(),
            Path("nat".to_string()),
            HeaderMap::new(),
            "{}".to_string(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "second remote request must be rate-limited: {}",
            body.0
        );

        // Loopback caller is exempt even though the window is exhausted.
        for _ in 0..2 {
            let (status, body) = webhook_handler(
                State(state.clone()),
                loopback_connect_info(),
                Path("nat".to_string()),
                HeaderMap::new(),
                "{}".to_string(),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "loopback must be exempt from rate limiting: {}",
                body.0
            );
        }
    }

    #[tokio::test]
    async fn script_webhook_surfaces_handler_failure_as_500() {
        let _guard = crate::test_env::borg_home_guard();
        let _home = BorgHomeTemp::new();
        let dir = tempfile::tempdir().unwrap();
        let chan_dir = dir.path().join("scriptchan");
        std::fs::create_dir_all(&chan_dir).unwrap();
        // Manifest with default scripts that do not exist on disk — the
        // enqueued work must fail and the caller must see a 500, not the
        // 503 reserved for a full queue.
        std::fs::write(
            chan_dir.join("channel.toml"),
            "name = \"scriptchan\"\ndescription = \"t\"\n",
        )
        .unwrap();
        let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
        let state = test_state(Config::default(), registry, &[], 0, None, None);

        let (status, body) = webhook_handler(
            State(state),
            loopback_connect_info(),
            Path("scriptchan".to_string()),
            HeaderMap::new(),
            "{}".to_string(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "an enqueued-but-failed script webhook must be a 500: {}",
            body.0
        );
        assert_eq!(body.0["error"], "Internal server error");
    }

    #[test]
    fn webhook_ack_and_capacity_responses_have_contract_shape() {
        // These bodies are the wire contract channel platforms retry on:
        // 200 {"ok": true} acknowledges, 503 + error asks for a retry.
        let (status, body) = ok_response();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0, serde_json::json!({ "ok": true }));

        let (status, body) = service_unavailable_response();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            body.0["error"]
                .as_str()
                .unwrap_or_default()
                .contains("capacity"),
            "503 body must explain the capacity condition: {}",
            body.0
        );
    }

    #[tokio::test]
    async fn slack_command_acks_ok_after_enqueue() {
        let _guard = crate::test_env::borg_home_guard();
        let _home = BorgHomeTemp::new();

        let secret = "test-signing-secret";
        let body = "command=%2Fborg&text=hi&user_id=U1&channel_id=C1";
        let ts = chrono::Utc::now().timestamp().to_string();
        let sig = {
            use hmac::{Hmac, Mac};
            use sha2::Sha256;
            let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
            mac.update(format!("v0:{ts}:{body}").as_bytes());
            format!("v0={}", hex::encode(mac.finalize().into_bytes()))
        };
        let mut headers = HeaderMap::new();
        headers.insert("x-slack-request-timestamp", ts.parse().unwrap());
        headers.insert("x-slack-signature", sig.parse().unwrap());

        let dir = tempfile::tempdir().unwrap();
        let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
        let slack_client = Arc::new(SlackClient::new("xoxb-test-token").unwrap());
        let state = test_state(
            Config::default(),
            registry,
            &[],
            0,
            None,
            Some((slack_client, secret.to_string())),
        );

        let (status, resp_body) =
            slack_command_handler(State(state), headers, body.to_string()).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a verified, enqueued slash command must be acked: {}",
            resp_body.0
        );
        assert_eq!(resp_body.0, serde_json::json!({ "ok": true }));

        // Let the spawned work finish against the temp BORG_HOME before the
        // guard restores the real one.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    /// A blank signing secret is unconfigured: the slash-command route must
    /// refuse a request even when it was signed with that same blank secret,
    /// just as the webhook route does through `require_verification_secret`.
    #[tokio::test]
    async fn slack_command_rejects_blank_signing_secret_even_when_signature_matches() {
        let _guard = crate::test_env::borg_home_guard();
        let _home = BorgHomeTemp::new();

        for secret in ["", "  ", "\t\n"] {
            let body = "command=%2Fborg&text=hi&user_id=U1&channel_id=C1";
            let ts = chrono::Utc::now().timestamp().to_string();
            let sig = {
                use hmac::{Hmac, Mac};
                use sha2::Sha256;
                let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
                mac.update(format!("v0:{ts}:{body}").as_bytes());
                format!("v0={}", hex::encode(mac.finalize().into_bytes()))
            };
            let mut headers = HeaderMap::new();
            headers.insert("x-slack-request-timestamp", ts.parse().unwrap());
            headers.insert("x-slack-signature", sig.parse().unwrap());

            let dir = tempfile::tempdir().unwrap();
            let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
            let slack_client = Arc::new(SlackClient::new("xoxb-test-token").unwrap());
            let state = test_state(
                Config::default(),
                registry,
                &[],
                0,
                None,
                Some((slack_client, secret.to_string())),
            );

            let (status, resp_body) =
                slack_command_handler(State(state.clone()), headers, body.to_string()).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "secret {secret:?}: blank signing secret must fail closed: {}",
                resp_body.0
            );
            assert_eq!(
                state.session_queue.active_sessions().await,
                0,
                "secret {secret:?}: no turn may start"
            );
        }
    }

    /// `gateway.slack_channel_allowlist` gates slash commands: a channel off
    /// the list gets an ephemeral reply that names the fix and starts no
    /// turn; a listed channel and a DM (`D…`) both get the bare ack and start
    /// one.
    #[tokio::test]
    async fn slack_command_respects_channel_allowlist() {
        let _guard = crate::test_env::borg_home_guard();
        let _home = BorgHomeTemp::new();
        let secret = "test-signing-secret";

        for (channel, expect_turn) in [("C2", false), ("C1", true), ("D9", true)] {
            let body = format!("command=%2Fborg&text=hi&user_id=U1&channel_id={channel}");
            let ts = chrono::Utc::now().timestamp().to_string();
            let sig = {
                use hmac::{Hmac, Mac};
                use sha2::Sha256;
                let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
                mac.update(format!("v0:{ts}:{body}").as_bytes());
                format!("v0={}", hex::encode(mac.finalize().into_bytes()))
            };
            let mut headers = HeaderMap::new();
            headers.insert("x-slack-request-timestamp", ts.parse().unwrap());
            headers.insert("x-slack-signature", sig.parse().unwrap());

            let mut config = Config::default();
            config.gateway.slack_channel_allowlist = Some(vec!["C1".to_string()]);
            let dir = tempfile::tempdir().unwrap();
            let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
            let slack_client = Arc::new(SlackClient::new("xoxb-test-token").unwrap());
            let state = test_state(
                config,
                registry,
                &[],
                0,
                None,
                Some((slack_client, secret.to_string())),
            );

            let (status, resp_body) =
                slack_command_handler(State(state.clone()), headers, body).await;
            assert_eq!(status, StatusCode::OK, "channel {channel}");
            if expect_turn {
                assert_eq!(resp_body.0, serde_json::json!({ "ok": true }));
            } else {
                assert_eq!(resp_body.0["response_type"], "ephemeral");
                let text = resp_body.0["text"].as_str().unwrap_or_default();
                assert!(
                    text.contains("gateway.slack_channel_allowlist")
                        && text.contains("Message Borg directly"),
                    "reply must name the next action: {text}"
                );
            }
            assert_eq!(
                state.session_queue.active_sessions().await > 0,
                expect_turn,
                "channel {channel}: turn started must be {expect_turn}"
            );
        }

        // Let the spawned work finish against the temp BORG_HOME before the
        // guard restores the real one.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // ── background loops ────────────────────────────────────────────────

    #[tokio::test]
    async fn drain_marks_missing_channel_failed_and_schedules_retry_in_a_minute() {
        let _guard = crate::test_env::borg_home_guard();
        let _home = BorgHomeTemp::new();

        // One channel exists but its outbound script does not (delivery
        // exhausts retries → +60s reschedule); one channel does not exist at
        // all (immediate failure).
        let dir = tempfile::tempdir().unwrap();
        let chan_dir = dir.path().join("scriptchan");
        std::fs::create_dir_all(&chan_dir).unwrap();
        std::fs::write(
            chan_dir.join("channel.toml"),
            "name = \"scriptchan\"\ndescription = \"t\"\n\n[settings]\nretry_max_attempts = 1\nretry_initial_delay_ms = 1\n",
        )
        .unwrap();
        let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
        let state = test_state(Config::default(), registry, &[], 0, None, None);
        let health = state.health.clone();

        let db = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS)
            .await
            .unwrap();
        for (id, channel) in [("d-ghost", "ghost"), ("d-script", "scriptchan")] {
            db.enqueue_delivery(&cortex::db::NewDelivery {
                id,
                channel_name: channel,
                sender_id: "s1",
                channel_id: None,
                session_id: None,
                payload_json: "{}",
                max_retries: 5,
            })
            .await
            .unwrap();
        }

        let before = chrono::Utc::now().timestamp();
        drain_pending_deliveries(&state, &health).await;
        let after = chrono::Utc::now().timestamp();

        let stmt = db
            .conn()
            .prepare("SELECT id, error, next_retry_at FROM delivery_queue ORDER BY id ASC")
            .await
            .unwrap();
        let mut rows_iter = stmt.query(()).await.unwrap();
        let mut rows: Vec<(String, Option<String>, Option<i64>)> = Vec::new();
        while let Some(row) = rows_iter.next().await.unwrap() {
            rows.push((
                row.get::<String>(0).unwrap(),
                row.get::<Option<String>>(1).unwrap(),
                row.get::<Option<i64>>(2).unwrap(),
            ));
        }
        assert_eq!(rows.len(), 2, "both deliveries must be processed");

        let (_, ghost_err, ghost_retry) = &rows[0];
        assert_eq!(
            ghost_err.as_deref(),
            Some("channel not found"),
            "a delivery for an unregistered channel must be marked failed"
        );
        assert_eq!(*ghost_retry, None);

        let (_, script_err, script_retry) = &rows[1];
        assert!(
            script_err.is_some(),
            "an exhausted delivery must record its error"
        );
        let retry_at = script_retry.expect("an exhausted delivery must be rescheduled");
        assert!(
            (before + 60..=after + 60).contains(&retry_at),
            "reschedule must be one minute out (got {retry_at}, now ~{before})"
        );
    }

    fn write_channel_manifest(base: &std::path::Path, name: &str, mode: &str) {
        let d = base.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("channel.toml"),
            format!(
                "name = \"{name}\"\ndescription = \"t\"\n\n[settings]\nmode = \"{mode}\"\npoll_interval_ms = 3600000\n"
            ),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn spawn_poll_loops_targets_only_generic_poll_channels() {
        // (channel name, mode, expected loop count)
        let imessage_expected = if cfg!(target_os = "macos") {
            0 // handled natively by the daemon on macOS
        } else {
            1
        };
        let cases: &[(&str, &str, usize)] = &[
            ("pollchan", "poll", 1),
            ("hookchan", "webhook", 0),
            ("imessage", "poll", imessage_expected),
        ];

        for (name, mode, expected) in cases {
            let dir = tempfile::tempdir().unwrap();
            write_channel_manifest(dir.path(), name, mode);
            let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
            let state = test_state(Config::default(), registry, &[], 0, None, None);
            let health = state.health.clone();

            let shutdown = CancellationToken::new();
            let server = GatewayServer::new(
                Config::default(),
                shutdown.clone(),
                test_metrics(),
                None,
                None,
                None,
            )
            .unwrap();

            let handles = server.spawn_poll_loops(&state, &health);
            assert_eq!(
                handles.len(),
                *expected,
                "channel {name} (mode {mode}) spawned the wrong number of poll loops"
            );

            shutdown.cancel();
            for h in handles {
                let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
            }
        }
    }

    #[tokio::test]
    async fn poll_loop_waits_one_interval_then_polls_and_records_errors() {
        let dir = tempfile::tempdir().unwrap();
        let manifest: crate::manifest::ChannelManifest = toml::from_str(
            "name = \"pollo\"\ndescription = \"t\"\n\n[settings]\nmode = \"poll\"\n",
        )
        .unwrap();
        let channel = RegisteredChannel {
            manifest,
            dir: dir.path().to_path_buf(),
        };
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));
        let shutdown = CancellationToken::new();

        let task = tokio::spawn(run_channel_poll_loop(
            channel,
            1000,
            Config::default(),
            shutdown.clone(),
            health.clone(),
            Duration::from_secs(5),
        ));

        // The first tick is one full interval out — polling immediately would
        // hammer the channel script on every gateway (re)start.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let early: u64 = health
            .read()
            .await
            .snapshot()
            .iter()
            .map(|c| c.error_count)
            .sum();
        assert_eq!(early, 0, "no poll may fire before the first interval");

        // After the interval the loop must actually poll; this channel has no
        // poll script, so the attempt surfaces as a recorded health error.
        tokio::time::sleep(Duration::from_millis(1300)).await;
        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;

        let snapshot = health.read().await.snapshot();
        let entry = snapshot
            .iter()
            .find(|c| c.name == "pollo")
            .expect("the poll loop must have recorded an error for its channel");
        assert!(entry.error_count >= 1);
        assert!(
            entry
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("no poll script"),
            "error must come from the poll attempt: {:?}",
            entry.last_error
        );
    }

    #[tokio::test]
    async fn spawn_signal_sse_connects_to_the_event_stream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (req_tx, req_rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let _ = req_tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n")
                .await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        });

        let shutdown = CancellationToken::new();
        let server = GatewayServer::new(
            Config::default(),
            shutdown.clone(),
            test_metrics(),
            None,
            None,
            None,
        )
        .unwrap();
        let inits = ChannelInits {
            telegram_client: None,
            telegram_bot_username: None,
            telegram_secret: None,
            telegram_dedup: Arc::new(Mutex::new(UpdateDeduplicator::new())),
            slack_client: None,
            slack_signing_secret: None,
            slack_bot_user_id: None,
            twilio_client: None,
            twilio_auth_token: None,
            twilio_phone_number: None,
            twilio_whatsapp_number: None,
            discord_client: None,
            discord_public_key: None,
            teams_client: None,
            teams_app_secret: None,
            google_chat_client: None,
            google_chat_token: None,
            signal_client: Some(Arc::new(
                crate::signal::api::SignalClient::new(&format!("http://{addr}"), "+15550001111")
                    .unwrap(),
            )),
            tts_synth: None,
        };
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));

        server.spawn_signal_sse(&inits, &health);

        let request = tokio::time::timeout(Duration::from_secs(5), req_rx)
            .await
            .expect("a configured Signal client must open the SSE stream at spawn")
            .unwrap();
        assert!(
            request.contains("GET /api/v1/events"),
            "the SSE loop must subscribe to the daemon event stream, got: {request}"
        );
        shutdown.cancel();
    }
}

/// Background task: claim pending deliveries and attempt to send them.
#[instrument(skip_all)]
async fn drain_pending_deliveries(
    state: &Arc<AppState>,
    health: &Arc<RwLock<ChannelHealthRegistry>>,
) {
    let db = match Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS).await {
        Ok(db) => db,
        Err(e) => {
            warn!("Drain loop: failed to open database: {e}");
            return;
        }
    };

    let deliveries = match db.claim_pending_deliveries(20).await {
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
                if let Err(e) = db
                    .mark_failed(&delivery.id, "channel not found", None)
                    .await
                {
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
            policy.backoff.initial = std::time::Duration::from_millis(delay);
        }
        // For drain loop, use fewer retries to avoid blocking
        policy.max_retries = policy.max_retries.min(2);

        match crate::resilience::retry::send_with_retry(
            &executor,
            &delivery.payload_json,
            blocked_paths,
            &policy,
        )
        .await
        {
            crate::resilience::retry::RetryOutcome::Success(_) => {
                if let Err(e) = db.mark_delivered(&delivery.id).await {
                    warn!(delivery_id = %delivery.id, "Failed to persist delivery success status: {e}");
                }
                health.write().await.record_outbound(&delivery.channel_name);
            }
            crate::resilience::retry::RetryOutcome::PermanentFailure(e) => {
                if let Err(db_err) = db.mark_failed(&delivery.id, &e, None).await {
                    warn!(delivery_id = %delivery.id, "Failed to persist delivery failure status: {db_err}");
                }
                health
                    .write()
                    .await
                    .record_error(&delivery.channel_name, &e);
            }
            crate::resilience::retry::RetryOutcome::Exhausted(e) => {
                let next_retry = chrono::Utc::now().timestamp() + 60;
                if let Err(db_err) = db.mark_failed(&delivery.id, &e, Some(next_retry)).await {
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
