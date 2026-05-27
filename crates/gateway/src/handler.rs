use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use cortex::agent::AgentEvent;
use cortex::config::Config;
use cortex::constants;
use cortex::db::{Database, NewDelivery};
use cortex::security::validation::{sanitize_filename, sanitize_thread_id};

use crate::constants::MAX_THREAD_ID_LEN;

use crate::channel::registry::RegisteredChannel;
use crate::constants::{DEFAULT_BINDING_ID, PEER_KIND_GROUP};
use crate::executor::ChannelExecutor;
use crate::health::ChannelHealthRegistry;
use crate::message_processing::chunker;
use crate::resilience::challenge_throttle::ChallengeThrottle;
use crate::resilience::retry::{self, RetryOutcome, RetryPolicy};

/// Global throttle for pairing challenge messages — suppresses repeated
/// challenges for the same sender within a 5-minute cooldown.
static CHALLENGE_THROTTLE: LazyLock<Mutex<ChallengeThrottle>> =
    LazyLock::new(|| Mutex::new(ChallengeThrottle::default()));

/// A media attachment on an inbound message.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InboundAttachment {
    /// MIME type of the attachment (e.g. "image/png").
    pub mime_type: String,
    /// Base64-encoded binary content.
    pub data: String,
    /// Original filename, if provided by the platform.
    #[serde(default)]
    pub filename: Option<String>,
}

/// Normalized inbound message parsed from the channel's inbound script.
#[derive(Debug, serde::Deserialize)]
pub struct InboundMessage {
    /// Unique identifier of the message sender.
    pub sender_id: String,
    /// Message text content.
    pub text: String,
    /// Platform channel/chat identifier, if applicable.
    #[serde(default)]
    pub channel_id: Option<String>,
    /// Thread identifier for threaded conversations.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Platform-specific message identifier.
    #[serde(default)]
    pub message_id: Option<String>,
    /// Slack-style thread timestamp.
    #[serde(default)]
    pub thread_ts: Option<String>,
    /// Media attachments (images, audio, etc.).
    #[serde(default)]
    pub attachments: Vec<InboundAttachment>,
    /// Emoji reaction event (e.g. from a reaction_added event).
    #[serde(default)]
    pub reaction: Option<String>,
    /// Platform-specific metadata escape hatch.
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// Whether this message is from a direct or group context.
    #[serde(default)]
    pub peer_kind: Option<String>,
}

impl InboundMessage {
    /// Build a session key for queue routing: `channel:sender:sub`.
    pub fn session_key(&self, channel: &str, sub: &str) -> String {
        format!("{}:{}:{}", channel, self.sender_id, sub)
    }
}

/// Process a webhook request for a channel end-to-end.
#[instrument(skip_all, fields(channel = %channel.manifest.name))]
pub async fn handle_webhook(
    channel: &RegisteredChannel,
    headers_json: serde_json::Value,
    body: String,
    config: &Config,
    health: Option<&Arc<RwLock<ChannelHealthRegistry>>>,
) -> Result<String> {
    let executor = ChannelExecutor::new(&channel.manifest, &channel.dir);
    let blocked_paths = &config.security.blocked_paths;

    if let Some(secret_env) = &channel.manifest.auth.secret_env {
        if channel.manifest.scripts.verify.is_none() {
            bail!(
                "Channel '{}' has secret_env configured but no verify script",
                channel.manifest.name
            );
        }

        let secret = config
            .resolve_credential_or_env(secret_env)
            .with_context(|| {
                format!(
                    "Verification credential '{secret_env}' not found for channel '{}'",
                    channel.manifest.name
                )
            })?;

        let verify_input = serde_json::json!({
            "headers": headers_json,
            "body": body,
            "secret": secret,
        });

        let verified = executor
            .verify(&verify_input.to_string(), blocked_paths)
            .await
            .context("Verification script failed")?;

        if !verified {
            bail!("Webhook verification failed");
        }
    }

    let inbound_input = serde_json::json!({
        "headers": headers_json,
        "body": body,
    });

    let inbound_output = executor
        .parse_inbound(&inbound_input.to_string(), blocked_paths)
        .await
        .context("Inbound parsing failed")?;

    let parsed: serde_json::Value =
        serde_json::from_str(&inbound_output).context("Invalid inbound script output JSON")?;
    if parsed
        .get("skip")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Ok("(skipped)".to_string());
    }
    let inbound: InboundMessage =
        serde_json::from_value(parsed).context("Invalid inbound message structure")?;

    process_message(channel, inbound, config, health).await
}

/// Process a polled message that is already normalized (no verify/parse needed).
#[instrument(skip_all, fields(channel = %channel.manifest.name))]
pub async fn handle_polled_message(
    channel: &RegisteredChannel,
    message: InboundMessage,
    config: &Config,
    health: Option<&Arc<RwLock<ChannelHealthRegistry>>>,
) -> Result<String> {
    process_message(channel, message, config, health).await
}

/// Build a RetryPolicy from channel settings, falling back to defaults.
fn build_retry_policy(channel: &RegisteredChannel) -> RetryPolicy {
    let mut policy = RetryPolicy::default();
    if let Some(max) = channel.manifest.settings.retry_max_attempts {
        policy.max_retries = max;
    }
    if let Some(delay) = channel.manifest.settings.retry_initial_delay_ms {
        policy.backoff.initial = std::time::Duration::from_millis(delay);
    }
    policy
}

/// Determine if the bot should respond to this message based on activation mode.
///
/// Returns `(should_respond, cleaned_text)` where `cleaned_text` has the @mention stripped
/// if the bot was mentioned in a group with mention-only activation.
pub fn check_activation(
    text: &str,
    peer_kind: Option<&str>,
    route: &crate::channel::routing::ResolvedRoute,
    config: &Config,
    bot_identifier: Option<&str>,
) -> (bool, String) {
    let is_group = peer_kind == Some(PEER_KIND_GROUP);

    // DMs always activate
    if !is_group {
        return (true, text.to_string());
    }

    // Determine activation mode: binding override > global default
    let activation = route
        .activation
        .as_ref()
        .unwrap_or(&config.gateway.group_activation);

    match activation {
        cortex::config::ActivationMode::Mention => {
            if let Some(bot_id) = bot_identifier {
                // Case-insensitive search for the bot mention
                let lower_text = text.to_lowercase();
                let lower_bot = bot_id.to_lowercase();
                if let Some(pos) = lower_text.find(&lower_bot) {
                    // Strip mention from original text at the found position
                    let cleaned = format!("{}{}", &text[..pos], &text[pos + bot_id.len()..])
                        .trim()
                        .to_string();
                    // If stripping left empty text, still activate with original
                    if cleaned.is_empty() {
                        return (true, text.to_string());
                    }
                    (true, cleaned)
                } else {
                    (false, text.to_string())
                }
            } else {
                // No bot identifier known, fall back to always
                (true, text.to_string())
            }
        }
        cortex::config::ActivationMode::Always => (true, text.to_string()),
    }
}

/// Invoke the agent with an inbound message and return the response text and session ID.
///
/// This is the shared core: session resolution, agent creation, message dispatch, response collection.
/// Used by both script-based channels and native integrations (e.g. Telegram).
///
/// Applies gateway routing to select per-channel/sender agent configuration overrides.
///
/// `bot_identifier` is the platform-specific mention string (e.g. `@botname` for Telegram,
/// `<@U123>` for Slack) used for group chat activation filtering.
#[instrument(skip_all, fields(channel = %channel_name))]
pub async fn invoke_agent(
    channel_name: &str,
    inbound: &InboundMessage,
    config: &Config,
    health: Option<&Arc<RwLock<ChannelHealthRegistry>>>,
    bot_identifier: Option<&str>,
) -> Result<(String, String)> {
    invoke_agent_with_auto_reply(
        channel_name,
        inbound,
        config,
        health,
        bot_identifier,
        None,
        None,
    )
    .await
}

/// Like `invoke_agent` but accepts an optional auto-reply state for away-mode
/// checks and an optional progress sender. When `progress_tx` is provided,
/// the gateway will deliver "still working…" / inactivity-warning messages
/// to the user via that channel during long agent turns.
#[instrument(skip_all, fields(channel = %channel_name))]
pub async fn invoke_agent_with_auto_reply(
    channel_name: &str,
    inbound: &InboundMessage,
    config: &Config,
    health: Option<&Arc<RwLock<ChannelHealthRegistry>>>,
    bot_identifier: Option<&str>,
    auto_reply_state: Option<&crate::auto_reply::SharedAutoReplyState>,
    progress_tx: Option<mpsc::UnboundedSender<String>>,
) -> Result<(String, String)> {
    // Expose the originating (channel, sender, thread) to tool handlers via a
    // task-local so e.g. the `schedule` tool can resolve `delivery="origin"`.
    let origin = cortex::gateway_context::GatewayOriginContext {
        channel_name: channel_name.to_string(),
        sender_id: inbound.sender_id.clone(),
        thread_id: inbound.thread_id.clone(),
    };
    cortex::gateway_context::scope(
        origin,
        invoke_agent_inner(
            channel_name,
            inbound,
            config,
            health,
            bot_identifier,
            auto_reply_state,
            progress_tx,
        ),
    )
    .await
}

/// Check sender access control (pairing/approval status).
///
/// Returns the `AccessCheckResult` from the pairing subsystem.
async fn check_sender_access_control(
    config: &Config,
    channel_name: &str,
    sender_id: &str,
) -> Result<cortex::pairing::AccessCheckResult> {
    let db = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS)
        .await
        .context("Failed to open database for pairing check")?;
    cortex::pairing::check_sender_access(
        &db,
        config,
        channel_name,
        sender_id,
        config.user.agent_name.as_deref(),
    )
    .await
}

/// Result of session resolution and command handling.
struct SessionResolution {
    db: Database,
    session_id: String,
    /// If a slash command handled the request, contains the response text.
    /// When `Some`, no further agent processing is needed.
    command_response: Option<String>,
}

/// Resolve the session and handle slash commands (`/cancel`, `/poke`, other commands).
async fn resolve_session_and_commands(
    channel_name: &str,
    inbound: &InboundMessage,
    config: &Config,
    route: &crate::channel::routing::ResolvedRoute,
) -> Result<SessionResolution> {
    // Resolve session — include thread_id and binding_id in the key for isolation
    let base_key = match &inbound.thread_id {
        Some(tid) => format!(
            "{}:{}",
            inbound.sender_id,
            sanitize_thread_id(tid, MAX_THREAD_ID_LEN)
        ),
        None => inbound.sender_id.clone(),
    };
    let session_key = if route.binding_id != DEFAULT_BINDING_ID {
        format!("{}:{}", route.binding_id, base_key)
    } else {
        base_key
    };
    let db = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS)
        .await
        .context("Failed to open database")?;
    let session_id = db
        .resolve_channel_session(channel_name, &session_key)
        .await
        .context("Failed to resolve channel session")?;

    // Log inbound message
    if let Err(e) = db
        .log_channel_message(
            channel_name,
            &inbound.sender_id,
            "inbound",
            Some(&inbound.text),
            None,
            Some(&session_id),
        )
        .await
    {
        warn!("Failed to log inbound message for channel '{channel_name}': {e}");
    }

    // Handle `/cancel` before the sync dispatcher so the async registry call
    // doesn't hold `&db` across an await point (Database is !Send).
    if crate::commands::is_cancel_command(&inbound.text) {
        let cancelled = match crate::session_executor::global() {
            Some(exec) => exec.cancel(&session_id).await,
            None => {
                warn!("cancel command received but no SessionExecutor is installed");
                false
            }
        };
        let response = if cancelled {
            "Cancelled.".to_string()
        } else {
            "Nothing to cancel.".to_string()
        };
        if let Err(e) = db
            .log_channel_message(
                channel_name,
                &inbound.sender_id,
                "outbound",
                Some(&response),
                None,
                Some(&session_id),
            )
            .await
        {
            // Secondary logging failure (the response itself succeeded).
            // DEBUG keeps daemon.log clean on flaky DB locks.
            debug!("Failed to log /cancel response: {e}");
        }
        return Ok(SessionResolution {
            db,
            session_id,
            command_response: Some(response),
        });
    }

    // Handle `/poke` before the sync dispatcher (requires async HTTP call).
    if crate::commands::is_poke_command(&inbound.text) {
        let poke_url = format!(
            "http://{}:{}/internal/poke",
            config.gateway.host, config.gateway.port
        );
        let response = match reqwest::Client::new()
            .post(&poke_url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => "Heartbeat triggered.".to_string(),
            Ok(resp) => format!("Poke failed (HTTP {})", resp.status()),
            Err(e) => format!("Poke failed: {e}"),
        };
        if let Err(e) = db
            .log_channel_message(
                channel_name,
                &inbound.sender_id,
                "outbound",
                Some(&response),
                None,
                Some(&session_id),
            )
            .await
        {
            debug!("Failed to log /poke response: {e}");
        }
        return Ok(SessionResolution {
            db,
            session_id,
            command_response: Some(response),
        });
    }

    if crate::commands::is_dream_command(&inbound.text) {
        let config = config.clone();
        let response =
            match tokio::spawn(
                async move { cortex::memory::consolidation::run_dream(&config).await },
            )
            .await
            {
                Ok(Ok(report)) => format_dream_report(&report),
                Ok(Err(e)) => format!("Dream failed: {e}"),
                Err(e) => format!("Dream failed: {e}"),
            };
        if let Err(e) = db
            .log_channel_message(
                channel_name,
                &inbound.sender_id,
                "outbound",
                Some(&response),
                None,
                Some(&session_id),
            )
            .await
        {
            debug!("Failed to log /dream response: {e}");
        }
        return Ok(SessionResolution {
            db,
            session_id,
            command_response: Some(response),
        });
    }

    // Check for slash commands before creating agent (use original text for commands)
    if let Some(response) = crate::commands::try_handle_command(
        &inbound.text,
        &db,
        config,
        channel_name,
        inbound.channel_id.as_deref(),
        &session_key,
        &session_id,
        &inbound.sender_id,
    )
    .await
    {
        if let Err(e) = db
            .log_channel_message(
                channel_name,
                &inbound.sender_id,
                "outbound",
                Some(&response),
                None,
                Some(&session_id),
            )
            .await
        {
            debug!("Failed to log command response: {e}");
        }
        return Ok(SessionResolution {
            db,
            session_id,
            command_response: Some(response),
        });
    }

    Ok(SessionResolution {
        db,
        session_id,
        command_response: None,
    })
}

fn format_dream_report(report: &cortex::memory::consolidation::DreamReport) -> String {
    let mut text = format!(
        "Dream complete.\nEntries extracted: {}\nDupes merged: {}\nTokens used: {}",
        report.entries_extracted, report.dupes_merged, report.tokens_used
    );
    if !report.nightly_summary.is_empty() {
        text.push_str("\nNightly: ");
        text.push_str(&report.nightly_summary);
    }
    if !report.weekly_summary.is_empty() {
        text.push_str("\nWeekly: ");
        text.push_str(&report.weekly_summary);
    }
    if !report.warnings.is_empty() {
        text.push_str("\nWarnings:");
        for warning in &report.warnings {
            text.push_str("\n- ");
            text.push_str(warning);
        }
    }
    text
}

/// Prepare the final message text for the agent.
///
/// Handles link understanding, injection scanning, truncation, and wrapping.
/// Returns the final message text and whether the inbound has image attachments.
/// Collect the full agent response from the event channel.
///
/// Uses an **inactivity-based** timeout: the
/// timer resets every time the agent emits a meaningful progress event
/// (stream tokens, tool calls, tool results, etc.). A long-but-active
/// turn never times out. A separate hard wall-clock ceiling
/// (`gateway.request_timeout_ms`) guards against a runaway agent loop.
///
/// While the agent is busy, the function periodically emits "still working…"
/// progress messages to `progress_tx` (if provided) and a one-shot warning
/// before the final inactivity cancellation fires.
///
/// Returns the final response text.
async fn collect_agent_response(
    channel_name: &str,
    config: &Config,
    event_rx: &mut mpsc::Receiver<AgentEvent>,
    agent_cancel: &CancellationToken,
    progress_tx: Option<&mpsc::UnboundedSender<String>>,
) -> String {
    let inactivity_timeout = if config.gateway.inactivity_timeout_secs > 0 {
        Some(Duration::from_secs(config.gateway.inactivity_timeout_secs))
    } else {
        None
    };
    let inactivity_warning = if config.gateway.inactivity_warning_secs > 0 {
        Some(Duration::from_secs(config.gateway.inactivity_warning_secs))
    } else {
        None
    };
    let inactivity_notify = if config.gateway.inactivity_notify_secs > 0 {
        Some(Duration::from_secs(config.gateway.inactivity_notify_secs))
    } else {
        None
    };
    let wall_clock_ceiling = Duration::from_millis(config.gateway.request_timeout_ms);

    let mut response_text = String::new();
    let mut response_capped = false;
    let mut last_activity = tokio::time::Instant::now();
    let started_at = last_activity;
    let mut warning_fired = false;
    let mut timed_out = false;

    // Tick once a second to re-evaluate elapsed time. Cheap and avoids the
    // bookkeeping of recreating sleep futures on every event.
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;

            maybe_event = event_rx.recv() => {
                let Some(event) = maybe_event else { break };

                if event_is_activity(&event) {
                    last_activity = tokio::time::Instant::now();
                }

                match event {
                    AgentEvent::TextDelta(delta) => {
                        if !response_capped {
                            if response_text.len() + delta.len() > constants::MAX_RESPONSE_SIZE {
                                let remaining =
                                    constants::MAX_RESPONSE_SIZE.saturating_sub(response_text.len());
                                let safe_end = (0..=remaining)
                                    .rev()
                                    .find(|&i| delta.is_char_boundary(i))
                                    .unwrap_or(0);
                                response_text.push_str(&delta[..safe_end]);
                                response_text
                                    .push_str("\n\n[Response truncated: exceeded maximum size]");
                                response_capped = true;
                                warn!(
                                    "Agent response for channel '{}' exceeded {}KB cap, truncating",
                                    channel_name,
                                    constants::MAX_RESPONSE_SIZE / 1024
                                );
                            } else {
                                response_text.push_str(&delta);
                            }
                        }
                    }
                    AgentEvent::Error(e) => {
                        warn!("Agent error on channel '{}': {e}", channel_name);
                        let friendly = cortex::error_format::format_error_with_context(
                            &e,
                            cortex::error_format::ErrorContext::Gateway,
                        );
                        if response_text.is_empty() {
                            response_text = friendly;
                        } else {
                            response_text.push_str(&format!("\n\n[Error: {friendly}]"));
                        }
                    }
                    AgentEvent::ShellConfirmation { respond, command } => {
                        // Audit-trail event, not an error condition. INFO so it's
                        // visible without `RUST_LOG=warn` flagging it as a problem.
                        info!(channel = %channel_name, command = %command, "auto-denying shell confirmation in gateway mode");
                        response_text
                            .push_str("\n[Operation denied: shell command requires confirmation]");
                        if respond.send(false).is_err() {
                            warn!(
                                "Failed to send shell-confirmation denial on channel '{channel_name}': receiver dropped"
                            );
                        }
                    }
                    AgentEvent::UserInputRequest { respond, questions } => {
                        let topics: Vec<&str> = questions
                            .iter()
                            .map(|q| q.header.as_str())
                            .collect();
                        warn!(
                            "Auto-declining user input request in gateway mode (questions: {:?})",
                            topics
                        );
                        // Populate each question with an explicit unavailable
                        // marker so the LLM can distinguish "channel can't ask"
                        // from "user picked nothing" when reasoning about the
                        // returned answer map.
                        let decline: std::collections::HashMap<
                            String,
                            cortex::tool_handlers::user_input::Answer,
                        > = questions
                            .iter()
                            .map(|q| {
                                (
                                    q.id.clone(),
                                    cortex::tool_handlers::user_input::Answer {
                                        answers: vec![
                                            "[unavailable: gateway mode cannot prompt user]"
                                                .to_string(),
                                        ],
                                    },
                                )
                            })
                            .collect();
                        if respond.send(decline).is_err() {
                            warn!(
                                "Failed to send user-input decline on channel '{channel_name}': receiver dropped"
                            );
                        }
                    }
                    _ => {}
                }
            }

            _ = tick.tick() => {
                let now = tokio::time::Instant::now();
                let idle = now.saturating_duration_since(last_activity);
                let elapsed_total = now.saturating_duration_since(started_at);

                // Hard wall-clock ceiling — last-resort guard. Inactivity timer
                // is the primary control.
                if elapsed_total >= wall_clock_ceiling {
                    warn!(
                        "Agent hit wall-clock ceiling {wall_clock_ceiling:?} on channel '{channel_name}'"
                    );
                    timed_out = true;
                    break;
                }

                if let Some(timeout) = inactivity_timeout {
                    if idle >= timeout {
                        warn!(
                            "Agent inactivity timeout {timeout:?} on channel '{channel_name}'"
                        );
                        timed_out = true;
                        break;
                    }
                }

                if !warning_fired {
                    if let Some(warn_after) = inactivity_warning {
                        if idle >= warn_after {
                            warning_fired = true;
                            if let (Some(tx), Some(timeout)) = (progress_tx, inactivity_timeout) {
                                let elapsed_min = (idle.as_secs() / 60).max(1);
                                let remaining_min =
                                    (timeout.saturating_sub(idle).as_secs() / 60).max(1);
                                let msg = format!(
                                    "⚠️ No agent activity for {elapsed_min} min. \
                                     Will time out in {remaining_min} min if it does not respond."
                                );
                                if let Err(e) = tx.send(msg) {
                                    warn!(
                                        "Failed to enqueue inactivity warning on channel '{channel_name}': {e}"
                                    );
                                }
                            }
                        }
                    }
                }

                if let Some(notify) = inactivity_notify {
                    // Fire periodic notify pings while idle. Re-arms each interval
                    // by comparing idle against multiples of the notify period.
                    // Skip if we've already crossed into the warning window —
                    // the user has a more useful message there.
                    let already_warned = warning_fired
                        || inactivity_warning.is_some_and(|w| idle >= w);
                    if !already_warned && idle >= notify && idle.as_secs() % notify.as_secs() < 1 {
                        if let Some(tx) = progress_tx {
                            let elapsed_min = (idle.as_secs() / 60).max(1);
                            let msg = format!("⏳ Still working… ({elapsed_min} min idle)");
                            if let Err(e) = tx.send(msg) {
                                warn!(
                                    "Failed to enqueue progress notify on channel '{channel_name}': {e}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    if timed_out {
        agent_cancel.cancel();
        if response_text.is_empty() {
            response_text = "(request timed out — agent was idle too long)".to_string();
        } else {
            response_text.push_str("\n\n[Timed out — partial output above]");
        }
    }

    response_text
}

/// Returns true if an `AgentEvent` represents the agent making forward
/// progress. Used to reset the inactivity timer in `collect_agent_response`.
///
/// `UserInputRequest` and `ShellConfirmation` are intentionally excluded:
/// they mean the agent is *waiting on the user*, not working, so the
/// inactivity timer should keep counting.
fn event_is_activity(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::TextDelta(_)
            | AgentEvent::ThinkingDelta(_)
            | AgentEvent::ToolExecuting { .. }
            | AgentEvent::ToolResult { .. }
            | AgentEvent::ToolOutputDelta { .. }
            | AgentEvent::Usage(_)
            | AgentEvent::SubAgentUpdate { .. }
            | AgentEvent::SteerReceived { .. }
            | AgentEvent::PlanUpdated { .. }
            | AgentEvent::Preparing
            | AgentEvent::TurnComplete
            | AgentEvent::HistoryCompacted { .. }
    )
}

/// Check sender access and return an early response if denied or challenged.
/// Returns `Ok(None)` when the sender is allowed through.
async fn enforce_access_control(
    config: &Config,
    channel_name: &str,
    sender_id: &str,
) -> Result<Option<(String, String)>> {
    let access = check_sender_access_control(config, channel_name, sender_id).await?;
    match access {
        cortex::pairing::AccessCheckResult::Allowed => Ok(None),
        cortex::pairing::AccessCheckResult::Challenge { message, .. } => {
            let suppress = CHALLENGE_THROTTLE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .should_suppress(channel_name, sender_id);
            if suppress {
                return Ok(Some((String::new(), String::new())));
            }
            info!("Pairing challenge issued for sender '{sender_id}' on channel '{channel_name}'");
            if let Ok(adb) = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS).await {
                cortex::activity_log::log_activity(
                    &adb,
                    "info",
                    "gateway",
                    &format!("Pairing challenge issued for {sender_id}"),
                )
                .await;
            }
            Ok(Some((message, String::new())))
        }
        cortex::pairing::AccessCheckResult::Denied { reason } => {
            info!("Access denied for sender '{sender_id}' on channel '{channel_name}': {reason}");
            if let Ok(adb) = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS).await {
                cortex::activity_log::log_activity(
                    &adb,
                    "warn",
                    "gateway",
                    &format!("Access denied for {sender_id} on {channel_name}"),
                )
                .await;
            }
            Ok(Some((
                "Access denied. Contact the Borg owner for access.".to_string(),
                String::new(),
            )))
        }
    }
}

async fn invoke_agent_inner(
    channel_name: &str,
    inbound: &InboundMessage,
    config: &Config,
    health: Option<&Arc<RwLock<ChannelHealthRegistry>>>,
    bot_identifier: Option<&str>,
    auto_reply_state: Option<&crate::auto_reply::SharedAutoReplyState>,
    progress_tx: Option<mpsc::UnboundedSender<String>>,
) -> Result<(String, String)> {
    if let Ok(adb) = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS).await {
        cortex::activity_log::log_activity(
            &adb,
            "info",
            "gateway",
            &format!("Webhook from {channel_name}: {}", inbound.sender_id),
        )
        .await;
    }

    let route = crate::channel::routing::resolve_route(
        config,
        channel_name,
        &inbound.sender_id,
        inbound.peer_kind.as_deref(),
    );
    let config = &route.config;

    let (should_respond, cleaned_text) = check_activation(
        &inbound.text,
        inbound.peer_kind.as_deref(),
        &route,
        config,
        bot_identifier,
    );
    if !should_respond {
        return Ok((String::new(), String::new()));
    }

    if let Some(ar_state) = auto_reply_state {
        if let Some(reply) =
            crate::auto_reply::check_auto_reply(ar_state, &config.gateway.auto_reply).await
        {
            info!(
                "Auto-reply for '{}' on channel '{}': away",
                inbound.sender_id, channel_name
            );
            return Ok((reply, String::new()));
        }
    }

    info!(
        "Channel '{}' received message from '{}' (route: {})",
        channel_name, inbound.sender_id, route.matched_by
    );

    if let Some(early) = enforce_access_control(config, channel_name, &inbound.sender_id).await? {
        return Ok(early);
    }

    if let Some(h) = health {
        h.write().await.record_inbound(channel_name);
    }

    let resolution = resolve_session_and_commands(channel_name, inbound, config, &route).await?;
    let db = resolution.db;
    let session_id = resolution.session_id;

    if let Some(response) = resolution.command_response {
        return Ok((response, session_id));
    }

    // Strip `/plan ` prefix when it was a pass-through (mode was set in the
    // DB by try_handle_command, but the text still carries the prefix).
    let cleaned_text = {
        let lower = cleaned_text.trim().to_ascii_lowercase();
        if lower.starts_with("/plan ") {
            cleaned_text.trim()[6..].trim_start().to_string()
        } else {
            cleaned_text
        }
    };

    // Hand the turn to the daemon's SessionExecutor. Per-session config
    // overrides (gw mode, gateway action_limits, adaptive cache TTL) live
    // on the daemon side — `DaemonSessionExecutor` owns the AgentBackend's
    // Config.
    let _ = db;

    let executor = match crate::session_executor::global() {
        Some(exec) => exec.clone(),
        None => {
            warn!(
                "no SessionExecutor installed; cannot dispatch inbound message on '{channel_name}'"
            );
            anyhow::bail!("daemon SessionExecutor not configured");
        }
    };

    // Prepare the message text (link understanding, injection scan, truncation, wrapping)
    let (message_text, has_image_attachments) =
        crate::message_processing::inbound_prep::prepare_message_text(
            channel_name,
            inbound,
            config,
            cleaned_text,
        )
        .await;

    // Collect inline images for the multimodal turn payload.
    let images: Vec<crate::session_executor::InlineImage> = if has_image_attachments {
        inbound
            .attachments
            .iter()
            .filter(|a| a.mime_type.starts_with("image/"))
            .filter_map(|att| {
                use base64::Engine as _;
                let engine = base64::engine::general_purpose::STANDARD;
                match engine.decode(att.data.as_bytes()) {
                    Ok(bytes) => Some(crate::session_executor::InlineImage {
                        mime_type: att.mime_type.clone(),
                        data: bytes,
                        filename: sanitize_filename(&att.filename),
                    }),
                    Err(e) => {
                        warn!(
                            channel = %channel_name,
                            mime = %att.mime_type,
                            error = %e,
                            "failed to base64-decode inbound image attachment; dropping",
                        );
                        None
                    }
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    let (mut event_rx, agent_cancel) = match executor
        .start_turn(crate::session_executor::InboundTurn {
            session_key: session_id.clone(),
            text: message_text,
            images,
        })
        .await
    {
        Ok(pair) => pair,
        Err(e) => {
            warn!("SessionExecutor::start_turn failed on '{channel_name}': {e}");
            return Err(e.context("SessionExecutor::start_turn"));
        }
    };

    // Collect the full response text
    let mut response_text = collect_agent_response(
        channel_name,
        config,
        &mut event_rx,
        &agent_cancel,
        progress_tx.as_ref(),
    )
    .await;

    if response_text.is_empty() {
        response_text = "(no response)".to_string();
    }

    // Log outbound message — route through the same output-redaction policy
    // as live channel delivery so the toggle and audit trail apply uniformly.
    // `channel_messages` is a persisted DB surface; an operator inspecting it
    // must see the same redacted text the recipient saw.
    let redaction_context = format!(
        "{}:{}",
        inbound.sender_id,
        inbound.channel_id.as_deref().unwrap_or("-")
    );
    let redacted_response = cortex::output_redaction::current_policy()
        .apply(
            cortex::output_redaction::OutputSink::Channel(channel_name),
            Some(&redaction_context),
            &response_text,
        )
        .into_owned();
    if let Err(e) = db
        .log_channel_message(
            channel_name,
            &inbound.sender_id,
            "outbound",
            Some(&redacted_response),
            None,
            Some(&session_id),
        )
        .await
    {
        warn!("Failed to log outbound message for channel '{channel_name}': {e}");
    }

    Ok((response_text, session_id))
}

/// Shared message processing: session resolution, agent invocation, outbound with retry + chunking.
#[instrument(skip_all, fields(channel = %channel.manifest.name))]
async fn process_message(
    channel: &RegisteredChannel,
    inbound: InboundMessage,
    config: &Config,
    health: Option<&Arc<RwLock<ChannelHealthRegistry>>>,
) -> Result<String> {
    let channel_name = &channel.manifest.name;

    // Prepare auth tokens (resolve from credential store, falling back to env vars)
    let token = channel
        .manifest
        .auth
        .token_env
        .as_deref()
        .and_then(|env| config.resolve_credential_or_env(env))
        .unwrap_or_default();

    let secret = channel
        .manifest
        .auth
        .secret_env
        .as_deref()
        .and_then(|env| config.resolve_credential_or_env(env))
        .unwrap_or_default();

    let executor = ChannelExecutor::new(&channel.manifest, &channel.dir);
    let blocked_paths = config.security.blocked_paths.clone();
    let retry_policy = build_retry_policy(channel);

    // Drive the agent and inline-forward in-turn progress messages
    // ("still working…", inactivity warnings) on the same task. We can't
    // spawn a separate forwarder because `ChannelExecutor` borrows from
    // `channel`. `tokio::select!` interleaves draining progress sends with
    // waiting on the agent.
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<String>();
    let invoke_fut = invoke_agent_with_auto_reply(
        channel_name,
        &inbound,
        config,
        health,
        None,
        None,
        Some(progress_tx),
    );
    tokio::pin!(invoke_fut);
    let invoke_result = loop {
        tokio::select! {
            biased;

            result = &mut invoke_fut => break result,
            maybe_msg = progress_rx.recv() => {
                let Some(msg) = maybe_msg else { continue };
                let mut payload = serde_json::json!({
                    "text": msg,
                    "sender_id": inbound.sender_id,
                    "channel_id": inbound.channel_id,
                    "progress": true,
                });
                payload["token"] = serde_json::json!(token);
                payload["secret"] = serde_json::json!(secret);
                let outbound_str = payload.to_string();
                match retry::send_with_retry(
                    &executor,
                    &outbound_str,
                    &blocked_paths,
                    &retry_policy,
                )
                .await
                {
                    RetryOutcome::Success(_) => {
                        info!("Progress notify sent on channel '{channel_name}'");
                    }
                    RetryOutcome::PermanentFailure(e) | RetryOutcome::Exhausted(e) => {
                        warn!("Progress notify failed on channel '{channel_name}': {e}");
                    }
                }
            }
        }
    };
    // Drain any progress messages enqueued just before completion. They are
    // best-effort status pings; if delivery fails we just log and move on.
    while let Ok(msg) = progress_rx.try_recv() {
        let mut payload = serde_json::json!({
            "text": msg,
            "sender_id": inbound.sender_id,
            "channel_id": inbound.channel_id,
            "progress": true,
        });
        payload["token"] = serde_json::json!(token);
        payload["secret"] = serde_json::json!(secret);
        let outbound_str = payload.to_string();
        if let RetryOutcome::PermanentFailure(e) | RetryOutcome::Exhausted(e) =
            retry::send_with_retry(&executor, &outbound_str, &blocked_paths, &retry_policy).await
        {
            warn!("Progress notify (drain) failed on channel '{channel_name}': {e}");
        }
    }
    let (response_text, _session_id) = invoke_result?;

    // Chunk text if max_message_chars is configured
    let chunks = match channel.manifest.settings.max_message_chars {
        Some(max) if max > 0 => chunker::chunk_text(&response_text, max),
        _ => vec![response_text.clone()],
    };
    let total_chunks = chunks.len();

    let db = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS)
        .await
        .context("Failed to open database")?;

    for (i, chunk) in chunks.iter().enumerate() {
        // Build payload without secrets for persistence
        let mut outbound = serde_json::json!({
            "text": chunk,
            "sender_id": inbound.sender_id,
            "channel_id": inbound.channel_id,
        });

        if total_chunks > 1 {
            outbound["chunk_index"] = serde_json::json!(i);
            outbound["total_chunks"] = serde_json::json!(total_chunks);
        }

        let persist_str = outbound.to_string();

        // Add secrets for runtime sending only
        outbound["token"] = serde_json::json!(token);
        outbound["secret"] = serde_json::json!(secret);
        let outbound_str = outbound.to_string();

        // Enqueue to delivery queue for persistence (without secrets)
        let delivery_id = uuid::Uuid::new_v4().to_string();
        if let Err(e) = db
            .enqueue_delivery(&NewDelivery {
                id: &delivery_id,
                channel_name,
                sender_id: &inbound.sender_id,
                channel_id: inbound.channel_id.as_deref(),
                session_id: Some(&_session_id),
                payload_json: &persist_str,
                max_retries: retry_policy.max_retries as i32,
            })
            .await
        {
            warn!("Failed to enqueue delivery for channel '{channel_name}': {e}");
        }

        match retry::send_with_retry(&executor, &outbound_str, &blocked_paths, &retry_policy).await
        {
            RetryOutcome::Success(_) => {
                info!(
                    "Outbound sent for channel '{}' (chunk {}/{})",
                    channel_name,
                    i + 1,
                    total_chunks
                );
                if let Err(e) = db.mark_delivered(&delivery_id).await {
                    warn!("Failed to mark delivery '{delivery_id}' as delivered: {e}");
                }
                if let Some(h) = health {
                    h.write().await.record_outbound(channel_name);
                }
            }
            RetryOutcome::PermanentFailure(e) | RetryOutcome::Exhausted(e) => {
                warn!("Outbound failure for channel '{}': {e}", channel_name);
                record_delivery_failure(&db, &delivery_id, &e).await;
                if let Some(h) = health {
                    h.write().await.record_error(channel_name, &e);
                }
            }
        }
    }

    Ok(response_text)
}

async fn record_delivery_failure(db: &Database, delivery_id: &str, error: &str) {
    if let Err(db_err) = db.mark_failed(delivery_id, error, None).await {
        warn!("Failed to mark delivery '{delivery_id}' as failed: {db_err}");
    }
}

/// Format an error from `invoke_agent` into a user-friendly message suitable for
/// sending back to a messaging channel. Uses `ErrorContext::Gateway` to include
/// gateway-appropriate action hints (e.g. "try switching to a different model").
pub fn format_gateway_error(err: &anyhow::Error) -> String {
    cortex::error_format::format_error_with_context(
        &err.to_string(),
        cortex::error_format::ErrorContext::Gateway,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_dream_report_minimal() {
        let report = cortex::memory::consolidation::DreamReport::default();
        let out = format_dream_report(&report);
        assert!(out.starts_with("Dream complete."));
        assert!(out.contains("Entries extracted: 0"));
        assert!(out.contains("Dupes merged: 0"));
        assert!(out.contains("Tokens used: 0"));
        // Empty optional sections must not render their headers.
        assert!(!out.contains("Nightly:"), "got: {out}");
        assert!(!out.contains("Weekly:"), "got: {out}");
        assert!(!out.contains("Warnings:"), "got: {out}");
    }

    #[test]
    fn format_dream_report_with_summaries_and_warnings() {
        let report = cortex::memory::consolidation::DreamReport {
            entries_extracted: 7,
            dupes_merged: 2,
            tokens_used: 1234,
            nightly_completed: true,
            weekly_completed: true,
            warnings: vec!["disk near full".to_string(), "stale entry".to_string()],
            nightly_summary: "extracted 7 entries".to_string(),
            weekly_summary: "merged 2 dupes".to_string(),
        };
        let out = format_dream_report(&report);

        assert!(out.contains("Entries extracted: 7"));
        assert!(out.contains("Dupes merged: 2"));
        assert!(out.contains("Tokens used: 1234"));
        assert!(out.contains("Nightly: extracted 7 entries"));
        assert!(out.contains("Weekly: merged 2 dupes"));
        // Each warning must render as a discrete bullet, exactly once.
        let bullet_count = out.matches("\n- ").count();
        assert_eq!(bullet_count, 2, "expected 2 warning bullets, got: {out}");
        assert!(out.contains("\n- disk near full"));
        assert!(out.contains("\n- stale entry"));
    }

    #[test]
    fn format_dream_report_warnings_without_summaries() {
        // Real production case: consolidation aborts before summaries land.
        let report = cortex::memory::consolidation::DreamReport {
            entries_extracted: 0,
            dupes_merged: 0,
            tokens_used: 0,
            nightly_completed: false,
            weekly_completed: false,
            warnings: vec!["llm timeout".to_string()],
            nightly_summary: String::new(),
            weekly_summary: String::new(),
        };
        let out = format_dream_report(&report);
        assert!(out.contains("Warnings:"));
        assert!(out.contains("\n- llm timeout"));
        assert!(!out.contains("Nightly:"));
        assert!(!out.contains("Weekly:"));
        assert_eq!(out.matches("\n- ").count(), 1);
    }

    #[test]
    fn inbound_message_deserialize_minimal() {
        let json = r#"{"sender_id": "user123", "text": "hello"}"#;
        let msg: InboundMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.sender_id, "user123");
        assert_eq!(msg.text, "hello");
        assert!(msg.channel_id.is_none());
        assert!(msg.thread_id.is_none());
        assert!(msg.message_id.is_none());
        assert!(msg.thread_ts.is_none());
        assert!(msg.attachments.is_empty());
        assert!(msg.reaction.is_none());
    }

    #[test]
    fn inbound_message_deserialize_full() {
        let json = r#"{
            "sender_id": "u1",
            "text": "hi",
            "channel_id": "ch1",
            "thread_id": "t1",
            "message_id": "m1",
            "thread_ts": "123.456",
            "reaction": "thumbsup",
            "attachments": [
                {"mime_type": "image/png", "data": "abc123", "filename": "photo.png"}
            ],
            "metadata": {"platform": "slack"}
        }"#;
        let msg: InboundMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.sender_id, "u1");
        assert_eq!(msg.channel_id.as_deref(), Some("ch1"));
        assert_eq!(msg.thread_ts.as_deref(), Some("123.456"));
        assert_eq!(msg.reaction.as_deref(), Some("thumbsup"));
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].mime_type, "image/png");
        assert_eq!(msg.attachments[0].filename.as_deref(), Some("photo.png"));
    }

    #[test]
    fn inbound_attachment_deserialize() {
        let json = r#"{"mime_type": "audio/mp3", "data": "base64data"}"#;
        let att: InboundAttachment = serde_json::from_str(json).unwrap();
        assert_eq!(att.mime_type, "audio/mp3");
        assert_eq!(att.data, "base64data");
        assert!(att.filename.is_none());
    }

    #[test]
    fn inbound_attachment_serialize_roundtrip() {
        let att = InboundAttachment {
            mime_type: "image/jpeg".to_string(),
            data: "abc".to_string(),
            filename: Some("photo.jpg".to_string()),
        };
        let json = serde_json::to_string(&att).unwrap();
        let deserialized: InboundAttachment = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.mime_type, att.mime_type);
        assert_eq!(deserialized.filename, att.filename);
    }

    #[test]
    fn build_retry_policy_defaults() {
        let manifest: crate::manifest::ChannelManifest = toml::from_str(
            "name = \"test\"\ndescription = \"test\"\nruntime = \"bash\"\n\n[scripts]\ninbound = \"in.sh\"\noutbound = \"out.sh\"\n",
        )
        .unwrap();
        let channel = RegisteredChannel {
            manifest,
            dir: std::path::PathBuf::from("/tmp"),
        };
        let policy = build_retry_policy(&channel);
        assert_eq!(policy.max_retries, cortex::constants::RETRY_MAX_RETRIES);
        assert_eq!(
            policy.backoff.initial,
            std::time::Duration::from_millis(cortex::constants::RETRY_INITIAL_DELAY_MS)
        );
    }

    #[test]
    fn build_retry_policy_custom() {
        let manifest: crate::manifest::ChannelManifest = toml::from_str(
            "name = \"test\"\ndescription = \"test\"\nruntime = \"bash\"\n\n[scripts]\ninbound = \"in.sh\"\noutbound = \"out.sh\"\n\n[settings]\nretry_max_attempts = 10\nretry_initial_delay_ms = 500\n",
        )
        .unwrap();
        let channel = RegisteredChannel {
            manifest,
            dir: std::path::PathBuf::from("/tmp"),
        };
        let policy = build_retry_policy(&channel);
        assert_eq!(policy.max_retries, 10);
        assert_eq!(
            policy.backoff.initial,
            std::time::Duration::from_millis(500)
        );
    }

    #[test]
    fn skip_message_detected() {
        let json = r#"{"skip": true, "sender_id": "", "text": ""}"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(parsed
            .get("skip")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false));
    }

    fn default_route() -> crate::channel::routing::ResolvedRoute {
        let config = Config::default();
        crate::channel::routing::ResolvedRoute {
            config,
            binding_id: DEFAULT_BINDING_ID.to_string(),
            memory_scope: None,
            identity_path: None,
            matched_by: DEFAULT_BINDING_ID.to_string(),
            activation: None,
        }
    }

    #[test]
    fn dm_always_activates() {
        let route = default_route();
        let config = Config::default();
        let (activate, text) =
            check_activation("hello", Some("direct"), &route, &config, Some("@bot"));
        assert!(activate);
        assert_eq!(text, "hello");
    }

    #[test]
    fn dm_activates_even_with_mention_mode() {
        let mut route = default_route();
        route.activation = Some(cortex::config::ActivationMode::Mention);
        let config = Config::default();
        let (activate, _) =
            check_activation("hello", Some("direct"), &route, &config, Some("@bot"));
        assert!(activate);
    }

    #[test]
    fn group_mention_mode_activates_when_mentioned() {
        let route = default_route();
        let mut config = Config::default();
        config.gateway.group_activation = cortex::config::ActivationMode::Mention;
        let (activate, text) = check_activation(
            "@mybot help me",
            Some("group"),
            &route,
            &config,
            Some("@mybot"),
        );
        assert!(activate);
        assert_eq!(text, "help me");
    }

    #[test]
    fn group_mention_mode_skips_when_not_mentioned() {
        let route = default_route();
        let mut config = Config::default();
        config.gateway.group_activation = cortex::config::ActivationMode::Mention;
        let (activate, _) = check_activation(
            "hello everyone",
            Some("group"),
            &route,
            &config,
            Some("@mybot"),
        );
        assert!(!activate);
    }

    #[test]
    fn group_always_mode_activates() {
        let route = default_route();
        let mut config = Config::default();
        config.gateway.group_activation = cortex::config::ActivationMode::Always;
        let (activate, text) =
            check_activation("hello", Some("group"), &route, &config, Some("@bot"));
        assert!(activate);
        assert_eq!(text, "hello");
    }

    #[test]
    fn group_no_bot_identifier_falls_back_to_always() {
        let route = default_route();
        let mut config = Config::default();
        config.gateway.group_activation = cortex::config::ActivationMode::Mention;
        let (activate, _) = check_activation("hello", Some("group"), &route, &config, None);
        assert!(activate);
    }

    #[test]
    fn binding_activation_overrides_global() {
        let mut route = default_route();
        route.activation = Some(cortex::config::ActivationMode::Always);
        let mut config = Config::default();
        config.gateway.group_activation = cortex::config::ActivationMode::Mention;
        let (activate, _) = check_activation("hello", Some("group"), &route, &config, Some("@bot"));
        assert!(activate);
    }

    #[test]
    fn signal_phone_bot_id_required_in_groups_with_mention_mode() {
        // Regression: the Signal SSE callback used to pass `bot_identifier=None`,
        // which made `check_activation` fall through to "always" in groups and
        // the bot would answer every group message. Once the callback wires
        // `@<account>` through, Mention-mode groups must drop messages that
        // don't reference the bot's phone number.
        let route = default_route();
        let mut config = Config::default();
        config.gateway.group_activation = cortex::config::ActivationMode::Mention;
        let bot = "@+15555550101";

        // Group message without mention → dropped.
        let (activate, _) =
            check_activation("hey channel", Some("group"), &route, &config, Some(bot));
        assert!(!activate);

        // Group message with mention → activates, strips the @bot.
        let (activate, cleaned) = check_activation(
            "@+15555550101 ping",
            Some("group"),
            &route,
            &config,
            Some(bot),
        );
        assert!(activate);
        assert_eq!(cleaned, "ping");

        // DM still always activates regardless of mention.
        let (activate, _) = check_activation("ping", Some("direct"), &route, &config, Some(bot));
        assert!(activate);
    }

    #[test]
    fn mention_only_text_still_activates() {
        let route = default_route();
        let mut config = Config::default();
        config.gateway.group_activation = cortex::config::ActivationMode::Mention;
        let (activate, text) =
            check_activation("@mybot", Some("group"), &route, &config, Some("@mybot"));
        assert!(activate);
        // When only the mention remains, we return original text
        assert_eq!(text, "@mybot");
    }

    // -- InboundMessage::session_key --

    #[test]
    fn session_key_format() {
        let msg = InboundMessage {
            sender_id: "user42".to_string(),
            text: "hello".to_string(),
            channel_id: None,
            thread_id: None,
            message_id: None,
            thread_ts: None,
            attachments: Vec::new(),
            reaction: None,
            metadata: serde_json::Value::Null,
            peer_kind: None,
        };
        assert_eq!(msg.session_key("telegram", "main"), "telegram:user42:main");
    }

    #[test]
    fn session_key_different_channels() {
        let msg = InboundMessage {
            sender_id: "u1".to_string(),
            text: "hi".to_string(),
            channel_id: None,
            thread_id: None,
            message_id: None,
            thread_ts: None,
            attachments: Vec::new(),
            reaction: None,
            metadata: serde_json::Value::Null,
            peer_kind: None,
        };
        assert_ne!(
            msg.session_key("telegram", "main"),
            msg.session_key("slack", "main")
        );
    }

    #[test]
    fn session_key_different_subs() {
        let msg = InboundMessage {
            sender_id: "u1".to_string(),
            text: "hi".to_string(),
            channel_id: None,
            thread_id: None,
            message_id: None,
            thread_ts: None,
            attachments: Vec::new(),
            reaction: None,
            metadata: serde_json::Value::Null,
            peer_kind: None,
        };
        assert_ne!(
            msg.session_key("telegram", "main"),
            msg.session_key("telegram", "thread-123")
        );
    }

    #[test]
    fn inbound_message_peer_kind_deserialized() {
        let json = r#"{"sender_id": "u1", "text": "hi", "peer_kind": "group"}"#;
        let msg: InboundMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.peer_kind.as_deref(), Some("group"));
    }

    #[test]
    fn inbound_message_metadata_object() {
        let json = r#"{"sender_id": "u1", "text": "hi", "metadata": {"key": "val"}}"#;
        let msg: InboundMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.metadata["key"], "val");
    }

    /// Verify that the denial message constant is non-empty so users get feedback
    /// instead of silent drops.
    #[test]
    fn denied_sender_gets_denial_message() {
        let denial_msg = "Access denied. Contact the Borg owner for access.";
        assert!(!denial_msg.trim().is_empty());
        // Ensure the message is present in the source code (guards against removal)
        let source = include_str!("handler.rs");
        assert!(
            source.contains(denial_msg),
            "Denial message must be returned to users, not silently dropped"
        );
    }

    // -- Pairing integration tests --

    /// New sender with DmPolicy::Pairing receives a non-empty challenge message.
    #[tokio::test]
    async fn pairing_new_sender_gets_challenge() {
        let db = cortex::db::Database::test_db().await;
        let mut config = Config::default();
        config.gateway.dm_policy = cortex::pairing::DmPolicy::Pairing;

        let result =
            cortex::pairing::check_sender_access(&db, &config, "telegram", "new_user", None)
                .await
                .unwrap();
        match result {
            cortex::pairing::AccessCheckResult::Challenge { code, message } => {
                assert!(!code.is_empty(), "pairing code must not be empty");
                assert!(!message.is_empty(), "challenge message must not be empty");
                assert!(
                    message.contains(&code),
                    "challenge message must contain the pairing code"
                );
                assert!(
                    code.starts_with("tg/"),
                    "telegram pairing code must have tg/ prefix, got: {code}"
                );
                assert!(
                    message.contains("Borg's owner"),
                    "challenge message must reference the Borg owner"
                );
                assert!(
                    message.contains("new_user"),
                    "challenge message must contain the sender ID"
                );
            }
            other => panic!("expected Challenge, got {other:?}"),
        }
    }

    /// DM activation + pairing integration: a DM from a new sender should activate
    /// and then receive a pairing challenge (not be silently dropped).
    #[tokio::test]
    async fn dm_activation_then_pairing_produces_response() {
        let route = default_route();
        let config = Config::default(); // default dm_policy = Pairing

        let (should_respond, _cleaned) =
            check_activation("/start", Some("direct"), &route, &config, Some("@bot"));
        assert!(should_respond, "DMs must always activate");

        let db = cortex::db::Database::test_db().await;
        let result =
            cortex::pairing::check_sender_access(&db, &config, "telegram", "starter_user", None)
                .await
                .unwrap();
        match result {
            cortex::pairing::AccessCheckResult::Challenge { message, .. } => {
                assert!(
                    !message.trim().is_empty(),
                    "pairing challenge must not be empty (would be silently dropped)"
                );
            }
            other => panic!("expected Challenge for new sender, got {other:?}"),
        }
    }

    /// DmPolicy::Disabled returns a non-empty denial message (not silently dropped).
    #[tokio::test]
    async fn disabled_policy_returns_nonempty_denial() {
        let db = cortex::db::Database::test_db().await;
        let mut config = Config::default();
        config.gateway.dm_policy = cortex::pairing::DmPolicy::Disabled;

        let result = cortex::pairing::check_sender_access(&db, &config, "telegram", "anyone", None)
            .await
            .unwrap();
        match result {
            cortex::pairing::AccessCheckResult::Denied { reason } => {
                assert!(!reason.trim().is_empty(), "denial reason must not be empty");
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    /// DmPolicy::Open lets any sender through without challenge.
    #[tokio::test]
    async fn open_policy_allows_any_sender() {
        let db = cortex::db::Database::test_db().await;
        let mut config = Config::default();
        config.gateway.dm_policy = cortex::pairing::DmPolicy::Open;

        let result = cortex::pairing::check_sender_access(&db, &config, "telegram", "anyone", None)
            .await
            .unwrap();
        assert!(matches!(
            result,
            cortex::pairing::AccessCheckResult::Allowed
        ));
    }

    /// After approval, the same sender is allowed through.
    #[tokio::test]
    async fn approved_sender_passes_pairing() {
        let db = cortex::db::Database::test_db().await;
        let mut config = Config::default();
        config.gateway.dm_policy = cortex::pairing::DmPolicy::Pairing;

        // Get challenge
        let result = cortex::pairing::check_sender_access(&db, &config, "telegram", "user_x", None)
            .await
            .unwrap();
        let code = match result {
            cortex::pairing::AccessCheckResult::Challenge { code, .. } => code,
            other => panic!("expected Challenge, got {other:?}"),
        };

        // Approve
        db.approve_pairing("telegram", &code).await.unwrap();

        // Now should be allowed
        let result = cortex::pairing::check_sender_access(&db, &config, "telegram", "user_x", None)
            .await
            .unwrap();
        assert!(matches!(
            result,
            cortex::pairing::AccessCheckResult::Allowed
        ));
    }

    /// Verify the polling error path uses format_gateway_error (not a hard-coded string).
    #[test]
    fn polling_error_path_uses_formatted_errors() {
        let source = include_str!("server.rs");
        // The polling error arm must call format_gateway_error, not send a hard-coded message.
        assert!(
            source.contains("format_gateway_error"),
            "Telegram polling error path must use format_gateway_error"
        );
        // The old hard-coded messages must not appear.
        assert!(
            !source.contains("Something went wrong. Please try again."),
            "Hard-coded generic error message must not be used in polling path"
        );
    }

    /// Verify format_gateway_error produces Gateway-specific hints for rate-limit errors.
    #[test]
    fn format_gateway_error_rate_limit_includes_hint() {
        let err = anyhow::anyhow!("HTTP 429 rate limit exceeded");
        let msg = format_gateway_error(&err);
        assert!(
            msg.contains("rate-limited"),
            "Rate-limit error should mention rate-limiting: {msg}"
        );
        assert!(
            msg.contains("switching to a different model"),
            "Gateway context should suggest switching models: {msg}"
        );
    }

    /// Verify format_gateway_error produces a safe message for unknown errors.
    #[test]
    fn format_gateway_error_unknown_is_safe() {
        let err = anyhow::anyhow!("some internal panic trace xyz");
        let msg = format_gateway_error(&err);
        assert!(
            msg.contains("unexpected error"),
            "Unknown errors should be labeled as unexpected: {msg}"
        );
    }

    /// Verify webhook dispatch path formats errors instead of silently returning.
    #[test]
    fn webhook_dispatch_formats_errors() {
        let source = include_str!("channel/channel_trait.rs");
        // The webhook dispatch must call format_gateway_error on agent errors.
        assert!(
            source.contains("format_gateway_error"),
            "Webhook dispatch must use format_gateway_error for agent errors"
        );
        // The old silent-return pattern must not appear after an agent error log.
        // (We check that the error arm doesn't just `return;` with no response.)
        assert!(
            source.contains("format_error_with_context"),
            "Webhook dispatch must format timeout errors with context"
        );
    }

    // ── Inactivity-based timeout in collect_agent_response ──

    fn test_config(
        inactivity_secs: u64,
        warning_secs: u64,
        notify_secs: u64,
        wall_clock_ms: u64,
    ) -> Config {
        let mut cfg = Config::default();
        cfg.gateway.inactivity_timeout_secs = inactivity_secs;
        cfg.gateway.inactivity_warning_secs = warning_secs;
        cfg.gateway.inactivity_notify_secs = notify_secs;
        cfg.gateway.request_timeout_ms = wall_clock_ms;
        cfg
    }

    #[tokio::test(start_paused = true)]
    async fn collect_response_returns_text_on_turn_complete() {
        let config = test_config(60, 0, 0, 600_000);
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(8);
        let cancel = CancellationToken::new();

        tx.send(AgentEvent::TextDelta("hello ".to_string()))
            .await
            .unwrap();
        tx.send(AgentEvent::TextDelta("world".to_string()))
            .await
            .unwrap();
        tx.send(AgentEvent::TurnComplete).await.unwrap();
        drop(tx);

        let text = collect_agent_response("test", &config, &mut rx, &cancel, None).await;
        assert_eq!(text, "hello world");
        assert!(!cancel.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn collect_response_resets_inactivity_on_activity() {
        // Inactivity timeout = 5s. Send activity every 2s for 12s, then go
        // silent while keeping the channel open. The agent should NOT time
        // out during the active window — proving the timer resets on each
        // event — and only fire after silence begins.
        let config = test_config(5, 0, 0, 600_000);
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(32);
        let cancel = CancellationToken::new();

        // Keep `tx` alive for the duration of the test so channel-close
        // doesn't short-circuit the loop. The clone goes to the producer.
        let producer_tx = tx.clone();
        let producer = tokio::spawn(async move {
            for _ in 0..6 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                producer_tx
                    .send(AgentEvent::TextDelta(".".to_string()))
                    .await
                    .ok();
            }
            // Producer exits; outer `tx` keeps the channel open so the
            // inactivity timer (not channel close) ends the loop.
        });

        let text = collect_agent_response("test", &config, &mut rx, &cancel, None).await;
        producer.await.unwrap();
        drop(tx);
        // Six dots accumulated during the active window, then the timer
        // fires once silence begins. Total elapsed is well past the 5s
        // timeout, but it never tripped during the active period.
        assert!(text.starts_with("......"), "got: {text:?}");
        assert!(text.contains("Timed out"), "got: {text:?}");
        assert!(cancel.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn collect_response_fires_inactivity_timeout_when_silent() {
        let config = test_config(3, 0, 0, 600_000);
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(8);
        let cancel = CancellationToken::new();

        // Send one event then go silent; timer should fire ~3s later.
        tx.send(AgentEvent::TextDelta("partial".to_string()))
            .await
            .unwrap();
        // Hold the sender open so the channel doesn't close (which would
        // exit the loop early). Drop after the test asserts.
        let _hold = tx;

        let text = collect_agent_response("test", &config, &mut rx, &cancel, None).await;
        assert!(text.starts_with("partial"));
        assert!(text.contains("Timed out"));
        assert!(cancel.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn collect_response_emits_progress_notify() {
        // Inactivity timeout 30s, notify every 5s. After ~6s of silence we
        // should see one "Still working…" message land on the progress
        // channel; warning is disabled so it does not preempt notify.
        let config = test_config(30, 0, 5, 600_000);
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(8);
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<String>();
        let cancel = CancellationToken::new();

        // Send one delta to seed activity, then hold the channel open and
        // schedule a TurnComplete after ~7s so the loop exits cleanly.
        tx.send(AgentEvent::TextDelta("ok".to_string()))
            .await
            .unwrap();
        let producer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(7)).await;
            tx.send(AgentEvent::TurnComplete).await.ok();
        });

        let text =
            collect_agent_response("test", &config, &mut rx, &cancel, Some(&progress_tx)).await;
        producer.await.unwrap();
        drop(progress_tx);

        assert_eq!(text, "ok");
        let mut notifies = Vec::new();
        while let Ok(m) = progress_rx.try_recv() {
            notifies.push(m);
        }
        assert!(
            notifies.iter().any(|m| m.contains("Still working")),
            "expected at least one 'Still working' message, got {notifies:?}"
        );
        assert!(!cancel.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn collect_response_emits_inactivity_warning_once() {
        // Inactivity 60s, warning at 4s. After ~5s of silence we should
        // see exactly one warning message. Then the loop is allowed to exit
        // via TurnComplete before the final timeout fires.
        let config = test_config(60, 4, 0, 600_000);
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(8);
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<String>();
        let cancel = CancellationToken::new();

        tx.send(AgentEvent::TextDelta("seed".to_string()))
            .await
            .unwrap();
        let producer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(6)).await;
            tx.send(AgentEvent::TurnComplete).await.ok();
        });

        let _ = collect_agent_response("test", &config, &mut rx, &cancel, Some(&progress_tx)).await;
        producer.await.unwrap();
        drop(progress_tx);

        let mut warnings = Vec::new();
        while let Ok(m) = progress_rx.try_recv() {
            if m.contains("No agent activity") {
                warnings.push(m);
            }
        }
        assert_eq!(warnings.len(), 1, "expected one warning, got {warnings:?}");
    }

    #[test]
    fn event_is_activity_excludes_user_input_and_shell() {
        use cortex::tool_handlers::user_input::Answer;
        let (txb, _rxb) = tokio::sync::oneshot::channel::<bool>();
        let (txs, _rxs) =
            tokio::sync::oneshot::channel::<std::collections::HashMap<String, Answer>>();
        assert!(!event_is_activity(&AgentEvent::ShellConfirmation {
            command: "ls".into(),
            respond: txb,
        }));
        assert!(!event_is_activity(&AgentEvent::UserInputRequest {
            questions: Vec::new(),
            respond: txs,
        }));
        assert!(event_is_activity(&AgentEvent::TextDelta("x".into())));
        assert!(event_is_activity(&AgentEvent::TurnComplete));
        assert!(event_is_activity(&AgentEvent::Preparing));
        assert!(event_is_activity(&AgentEvent::ToolExecuting {
            tool_call_id: "call-1".into(),
            name: "n".into(),
            args: "{}".into(),
        }));
    }
}
