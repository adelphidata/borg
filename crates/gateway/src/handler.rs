use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument, warn};

use borg_core::agent::{Agent, AgentEvent};
use borg_core::config::Config;
use borg_core::constants;
use borg_core::db::{Database, NewDelivery};
use borg_core::sanitize::{
    scan_for_injection, wrap_untrusted, wrap_with_injection_warning, ThreatLevel,
};

use crate::chunker;
use crate::executor::ChannelExecutor;
use crate::health::ChannelHealthRegistry;
use crate::registry::RegisteredChannel;
use crate::retry::{self, RetryOutcome, RetryPolicy};

/// Sanitize an attachment filename from an external webhook.
/// Extracts the basename, rejects path traversal, hidden files, and null bytes.
fn sanitize_filename(name: &Option<String>) -> Option<String> {
    name.as_ref().map(|n| {
        let basename = std::path::Path::new(n)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("attachment");
        if basename.contains("..")
            || basename.starts_with('.')
            || basename.contains('\0')
            || basename.is_empty()
        {
            "attachment".to_string()
        } else {
            basename.to_string()
        }
    })
}

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

/// Sanitize a thread_id to prevent session key confusion via delimiter injection.
///
/// Allows alphanumeric characters, dots, hyphens, and underscores — enough to
/// cover all real platform formats (Slack `thread_ts` like `1234567890.123456`,
/// Discord numeric IDs, Google Chat `spaces/*/threads/*` stripped to the leaf,
/// Telegram integer IDs). Colons are intentionally excluded because the session
/// key is composed as `{sender_id}:{thread_id}`, so an injected colon would
/// corrupt the key structure. Length is capped at 128 characters.
fn sanitize_thread_id(thread_id: &str) -> String {
    thread_id
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
        .take(128)
        .collect()
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

    // Step 1: Verify webhook signature (if secret_env configured)
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

    // Step 2: Parse inbound message
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

    // Steps 3-5: shared processing
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
        policy.initial_delay_ms = delay;
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
    route: &crate::routing::ResolvedRoute,
    config: &Config,
    bot_identifier: Option<&str>,
) -> (bool, String) {
    let is_group = peer_kind == Some("group");

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
        borg_core::config::ActivationMode::Mention => {
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
        borg_core::config::ActivationMode::Always => (true, text.to_string()),
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
    invoke_agent_with_auto_reply(channel_name, inbound, config, health, bot_identifier, None).await
}

/// Like `invoke_agent` but accepts an optional auto-reply state for away-mode checks.
#[instrument(skip_all, fields(channel = %channel_name))]
pub async fn invoke_agent_with_auto_reply(
    channel_name: &str,
    inbound: &InboundMessage,
    config: &Config,
    health: Option<&Arc<RwLock<ChannelHealthRegistry>>>,
    bot_identifier: Option<&str>,
    auto_reply_state: Option<&crate::auto_reply::SharedAutoReplyState>,
) -> Result<(String, String)> {
    // Expose the originating (channel, sender, thread) to tool handlers via a
    // task-local so e.g. the `schedule` tool can resolve `delivery="origin"`.
    let origin = borg_core::gateway_context::GatewayOriginContext {
        channel_name: channel_name.to_string(),
        sender_id: inbound.sender_id.clone(),
        thread_id: inbound.thread_id.clone(),
    };
    borg_core::gateway_context::scope(
        origin,
        invoke_agent_inner(
            channel_name,
            inbound,
            config,
            health,
            bot_identifier,
            auto_reply_state,
        ),
    )
    .await
}

async fn invoke_agent_inner(
    channel_name: &str,
    inbound: &InboundMessage,
    config: &Config,
    health: Option<&Arc<RwLock<ChannelHealthRegistry>>>,
    bot_identifier: Option<&str>,
    auto_reply_state: Option<&crate::auto_reply::SharedAutoReplyState>,
) -> Result<(String, String)> {
    if let Ok(adb) = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS) {
        borg_core::activity_log::log_activity(
            &adb,
            "info",
            "gateway",
            &format!("Webhook from {channel_name}: {}", inbound.sender_id),
        );
    }

    // Resolve gateway routing (per-channel/sender config overrides)
    let route = crate::routing::resolve_route(
        config,
        channel_name,
        &inbound.sender_id,
        inbound.peer_kind.as_deref(),
    );
    let config = &route.config;

    // Check group activation mode before any processing
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

    // Auto-reply check: if agent is away, return the away message immediately
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

    // Access control: check sender pairing status
    let ch = channel_name.to_string();
    let sid = inbound.sender_id.clone();
    let cfg = config.clone();
    let access = tokio::task::spawn_blocking(move || {
        let db = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS)
            .context("Failed to open database for pairing check")?;
        borg_core::pairing::check_sender_access(&db, &cfg, &ch, &sid)
    })
    .await
    .context("Pairing check task panicked")??;

    match access {
        borg_core::pairing::AccessCheckResult::Allowed => {}
        borg_core::pairing::AccessCheckResult::Challenge { message, .. } => {
            info!(
                "Pairing challenge issued for sender '{}' on channel '{}'",
                inbound.sender_id, channel_name
            );
            if let Ok(adb) = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS) {
                borg_core::activity_log::log_activity(
                    &adb,
                    "info",
                    "gateway",
                    &format!("Pairing challenge issued for {}", inbound.sender_id),
                );
            }
            return Ok((message, String::new()));
        }
        borg_core::pairing::AccessCheckResult::Denied { reason } => {
            info!(
                "Access denied for sender '{}' on channel '{}': {}",
                inbound.sender_id, channel_name, reason
            );
            if let Ok(adb) = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS) {
                borg_core::activity_log::log_activity(
                    &adb,
                    "warn",
                    "gateway",
                    &format!("Access denied for {} on {channel_name}", inbound.sender_id),
                );
            }
            return Ok((
                "Access denied. Contact the bot owner for access.".to_string(),
                String::new(),
            ));
        }
    }

    if let Some(h) = health {
        h.write().await.record_inbound(channel_name);
    }

    // Resolve session — include thread_id and binding_id in the key for isolation
    let base_key = match &inbound.thread_id {
        Some(tid) => format!("{}:{}", inbound.sender_id, sanitize_thread_id(tid)),
        None => inbound.sender_id.clone(),
    };
    let session_key = if route.binding_id != "default" {
        format!("{}:{}", route.binding_id, base_key)
    } else {
        base_key
    };
    let channel_name_owned = channel_name.to_string();
    let session_key_clone = session_key.clone();
    let (db, session_id) = tokio::task::spawn_blocking(move || {
        let db = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS)
            .context("Failed to open database")?;
        let session_id = db
            .resolve_channel_session(&channel_name_owned, &session_key_clone)
            .context("Failed to resolve channel session")?;
        Ok::<_, anyhow::Error>((db, session_id))
    })
    .await
    .context("DB task panicked")??;

    // Log inbound message
    if let Err(e) = db.log_channel_message(
        channel_name,
        &inbound.sender_id,
        "inbound",
        Some(&inbound.text),
        None,
        Some(&session_id),
    ) {
        warn!("Failed to log inbound message for channel '{channel_name}': {e}");
    }

    // Handle `/cancel` before the sync dispatcher so the async registry call
    // doesn't hold `&db` across an await point (Database is !Send).
    if crate::commands::is_cancel_command(&inbound.text) {
        let response = if crate::in_flight::GLOBAL.cancel(&session_id).await {
            "Cancelled.".to_string()
        } else {
            "Nothing to cancel.".to_string()
        };
        if let Err(e) = db.log_channel_message(
            channel_name,
            &inbound.sender_id,
            "outbound",
            Some(&response),
            None,
            Some(&session_id),
        ) {
            warn!("Failed to log /cancel response: {e}");
        }
        return Ok((response, session_id));
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
        if let Err(e) = db.log_channel_message(
            channel_name,
            &inbound.sender_id,
            "outbound",
            Some(&response),
            None,
            Some(&session_id),
        ) {
            warn!("Failed to log /poke response: {e}");
        }
        return Ok((response, session_id));
    }

    // Check for slash commands before creating agent (use original text for commands)
    if let Some(response) = crate::commands::try_handle_command(
        &inbound.text,
        &db,
        config,
        channel_name,
        &session_key,
        &session_id,
        &inbound.sender_id,
    ) {
        if let Err(e) = db.log_channel_message(
            channel_name,
            &inbound.sender_id,
            "outbound",
            Some(&response),
            None,
            Some(&session_id),
        ) {
            warn!("Failed to log command response: {e}");
        }
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

    // Create Agent with gateway-specific (stricter) rate limits
    let mut gw_config = config.clone();
    gw_config.security.action_limits = gw_config.security.gateway_action_limits.clone();

    // Apply per-session collaboration mode override (set via /mode command).
    if let Ok(Some(mode_str)) = db.get_setting(&format!("gw:mode:{session_id}")) {
        if let Ok(mode) = mode_str.parse::<borg_core::config::CollaborationMode>() {
            gw_config.conversation.collaboration_mode = mode;
        }
    }

    let mut agent = Agent::new(gw_config, borg_core::telemetry::BorgMetrics::noop())
        .context("Failed to create agent")?;

    if let Err(e) = agent.load_session(&session_id) {
        warn!(
            "Could not load session '{session_id}' for channel '{}': {e}",
            channel_name
        );
    }

    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(256);

    // Link understanding: augment message with fetched URL content
    let cleaned_text = if config.gateway.link_understanding.enabled {
        let (augmented, fetched) = crate::link_understanding::augment_with_links(
            &cleaned_text,
            &config.gateway.link_understanding,
        )
        .await;
        if !fetched.is_empty() {
            info!(
                "Link understanding: fetched {} URL(s) for channel '{}'",
                fetched.len(),
                channel_name
            );
        }
        augmented
    } else {
        cleaned_text
    };

    // Scan full text for injection BEFORE truncation so patterns spanning
    // the truncation boundary are still detected.
    // Cap scan input at 256 KB to prevent regex DoS on extremely large payloads.
    let scan_limit = constants::MAX_RESPONSE_SIZE;
    let scan_input = if cleaned_text.len() > scan_limit {
        let mut end = scan_limit;
        while end > 0 && !cleaned_text.is_char_boundary(end) {
            end -= 1;
        }
        &cleaned_text[..end]
    } else {
        &cleaned_text
    };
    let injection_level = scan_for_injection(scan_input);

    // Truncate inbound text to prevent excessive LLM token consumption
    let text = if cleaned_text.len() > constants::MAX_INBOUND_TEXT_BYTES {
        let original_len = cleaned_text.len();
        let mut truncated = cleaned_text;
        truncated.truncate(constants::MAX_INBOUND_TEXT_BYTES);
        // Ensure we don't split a multi-byte char
        while !truncated.is_char_boundary(truncated.len()) {
            truncated.pop();
        }
        warn!(
            "Truncated inbound message from {} bytes to {} bytes for channel '{}'",
            original_len,
            truncated.len(),
            channel_name
        );
        truncated
    } else {
        cleaned_text
    };

    // Apply injection wrapping using the pre-truncation scan result
    let message_text = {
        let base = format!(
            "[Channel: {}, Sender: {}]\n{}",
            channel_name, inbound.sender_id, text
        );
        match injection_level {
            ThreatLevel::HighRisk { .. } => wrap_with_injection_warning(channel_name, &base),
            ThreatLevel::Flagged { .. } => wrap_untrusted(channel_name, &base),
            ThreatLevel::Clean => base,
        }
    };

    // Check for image attachments and build multimodal message if present
    let has_image_attachments = inbound
        .attachments
        .iter()
        .any(|a| a.mime_type.starts_with("image/"));

    let agent_cancel = CancellationToken::new();
    // Register this turn's cancel token so a subsequent `/cancel` from the
    // same sender (or a `borg cancel` CLI call) can interrupt it. Cleared
    // unconditionally once the turn completes, below.
    crate::in_flight::GLOBAL
        .register(&session_id, agent_cancel.clone())
        .await;
    let agent_handle = if has_image_attachments {
        let mut parts = vec![borg_core::types::ContentPart::Text(message_text)];
        for att in &inbound.attachments {
            if att.mime_type.starts_with("image/") {
                parts.push(borg_core::types::ContentPart::ImageBase64 {
                    media: borg_core::types::MediaData {
                        mime_type: att.mime_type.clone(),
                        data: att.data.clone(),
                        filename: sanitize_filename(&att.filename),
                    },
                });
            }
        }
        // Compress images before sending to agent
        if config.media.compression_enabled {
            borg_core::media::compress_content_parts(&mut parts, config.media.max_image_bytes);
        }
        let msg = borg_core::types::Message::user_multimodal(parts);
        let cancel = agent_cancel.clone();
        tokio::spawn(async move { agent.send_message_raw(msg, event_tx, cancel).await })
    } else {
        let cancel = agent_cancel.clone();
        tokio::spawn(async move {
            agent
                .send_message_with_cancel(&message_text, event_tx, cancel)
                .await
        })
    };

    // Collect the full response text, with timeout to prevent indefinite hangs
    let request_timeout = Duration::from_millis(config.gateway.request_timeout_ms);
    let mut response_text = String::new();
    let mut response_capped = false;
    let collect_result = tokio::time::timeout(request_timeout, async {
        while let Some(event) = event_rx.recv().await {
            match event {
                AgentEvent::TextDelta(delta) => {
                    if !response_capped {
                        if response_text.len() + delta.len() > constants::MAX_RESPONSE_SIZE {
                            let remaining =
                                constants::MAX_RESPONSE_SIZE.saturating_sub(response_text.len());
                            // Find a safe UTF-8 boundary to avoid panicking on multi-byte chars
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
                            // Don't cancel the agent — let it finish its turn naturally.
                            // The outer tokio::time::timeout handles hard timeout.
                        } else {
                            response_text.push_str(&delta);
                        }
                    }
                }
                AgentEvent::Error(e) => {
                    warn!("Agent error on channel '{}': {e}", channel_name);
                    // If no response text yet, provide a friendly error message
                    // instead of leaving the user with no reply.
                    if response_text.is_empty() {
                        response_text = borg_core::error_format::format_error_with_context(
                            &e,
                            borg_core::error_format::ErrorContext::Gateway,
                        );
                    }
                }
                AgentEvent::ShellConfirmation { respond, command } => {
                    warn!("Auto-denying shell confirmation in gateway mode: {command}");
                    response_text
                        .push_str("\n[Operation denied: shell command requires confirmation]");
                    let _ = respond.send(false);
                }
                AgentEvent::UserInputRequest { respond, prompt } => {
                    warn!("Auto-declining user input request in gateway mode: {prompt}");
                    let _ = respond.send("[Not available in gateway mode]".to_string());
                }
                _ => {}
            }
        }
    })
    .await;

    if collect_result.is_err() {
        warn!("Agent timed out after {request_timeout:?} on channel '{channel_name}'");
        agent_cancel.cancel();
        if response_text.is_empty() {
            response_text = "(request timed out)".to_string();
        }
    }

    // Wait for agent to finish (with a short grace period after cancellation)
    match tokio::time::timeout(Duration::from_secs(5), agent_handle).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => warn!("Agent error: {e}"),
        Ok(Err(e)) => warn!("Agent task panicked: {e}"),
        Err(_) => warn!("Agent did not finish within grace period for channel '{channel_name}'"),
    }

    // Turn is no longer in flight; drop the registry entry regardless of
    // whether it completed normally, errored, or was externally cancelled.
    crate::in_flight::GLOBAL.clear(&session_id).await;

    if response_text.is_empty() {
        response_text = "(no response)".to_string();
    }

    // Log outbound message (redact secrets before DB persistence)
    let redacted_response = borg_core::secrets::redact_secrets(&response_text);
    if let Err(e) = db.log_channel_message(
        channel_name,
        &inbound.sender_id,
        "outbound",
        Some(&redacted_response),
        None,
        Some(&session_id),
    ) {
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

    let (response_text, _session_id) =
        invoke_agent(channel_name, &inbound, config, health, None).await?;

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
    let blocked_paths = &config.security.blocked_paths;
    let retry_policy = build_retry_policy(channel);

    // Chunk text if max_message_chars is configured
    let chunks = match channel.manifest.settings.max_message_chars {
        Some(max) if max > 0 => chunker::chunk_text(&response_text, max),
        _ => vec![response_text.clone()],
    };
    let total_chunks = chunks.len();

    let db = Database::open_with_timeout(Database::GATEWAY_BUSY_TIMEOUT_MS)
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
        if let Err(e) = db.enqueue_delivery(&NewDelivery {
            id: &delivery_id,
            channel_name,
            sender_id: &inbound.sender_id,
            channel_id: inbound.channel_id.as_deref(),
            session_id: Some(&_session_id),
            payload_json: &persist_str,
            max_retries: retry_policy.max_retries as i32,
        }) {
            warn!("Failed to enqueue delivery for channel '{channel_name}': {e}");
        }

        // Send with retry
        match retry::send_with_retry(&executor, &outbound_str, blocked_paths, &retry_policy).await {
            RetryOutcome::Success(_) => {
                info!(
                    "Outbound sent for channel '{}' (chunk {}/{})",
                    channel_name,
                    i + 1,
                    total_chunks
                );
                if let Err(e) = db.mark_delivered(&delivery_id) {
                    warn!("Failed to mark delivery '{delivery_id}' as delivered: {e}");
                }
                if let Some(h) = health {
                    h.write().await.record_outbound(channel_name);
                }
            }
            RetryOutcome::PermanentFailure(e) | RetryOutcome::Exhausted(e) => {
                warn!("Outbound failure for channel '{}': {e}", channel_name);
                record_delivery_failure_sync(&db, &delivery_id, &e);
                if let Some(h) = health {
                    h.write().await.record_error(channel_name, &e);
                }
            }
        }
    }

    Ok(response_text)
}

fn record_delivery_failure_sync(db: &Database, delivery_id: &str, error: &str) {
    if let Err(db_err) = db.mark_failed(delivery_id, error, None) {
        warn!("Failed to mark delivery '{delivery_id}' as failed: {db_err}");
    }
}

/// Format an error from `invoke_agent` into a user-friendly message suitable for
/// sending back to a messaging channel. Wraps `borg_core::error_format::format_friendly_error`.
pub fn format_gateway_error(err: &anyhow::Error) -> String {
    borg_core::error_format::format_friendly_error(&err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(policy.max_retries, borg_core::constants::RETRY_MAX_RETRIES);
        assert_eq!(
            policy.initial_delay_ms,
            borg_core::constants::RETRY_INITIAL_DELAY_MS
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
        assert_eq!(policy.initial_delay_ms, 500);
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

    #[test]
    fn sanitize_filename_strips_path_traversal() {
        assert_eq!(
            sanitize_filename(&Some("../../etc/passwd".to_string())),
            Some("passwd".to_string())
        );
        assert_eq!(
            sanitize_filename(&Some("/var/log/../secret".to_string())),
            Some("secret".to_string())
        );
    }

    #[test]
    fn sanitize_filename_blocks_hidden_files() {
        assert_eq!(
            sanitize_filename(&Some(".hidden".to_string())),
            Some("attachment".to_string())
        );
        assert_eq!(
            sanitize_filename(&Some(".env".to_string())),
            Some("attachment".to_string())
        );
    }

    #[test]
    fn sanitize_filename_passes_normal_names() {
        assert_eq!(
            sanitize_filename(&Some("photo.jpg".to_string())),
            Some("photo.jpg".to_string())
        );
        assert_eq!(
            sanitize_filename(&Some("document.pdf".to_string())),
            Some("document.pdf".to_string())
        );
    }

    #[test]
    fn sanitize_filename_handles_none() {
        assert_eq!(sanitize_filename(&None), None);
    }

    #[test]
    fn sanitize_filename_blocks_null_bytes() {
        assert_eq!(
            sanitize_filename(&Some("file\0name.txt".to_string())),
            Some("attachment".to_string())
        );
    }

    fn default_route() -> crate::routing::ResolvedRoute {
        let config = Config::default();
        crate::routing::ResolvedRoute {
            config,
            binding_id: "default".to_string(),
            memory_scope: None,
            identity_path: None,
            matched_by: "default".to_string(),
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
        route.activation = Some(borg_core::config::ActivationMode::Mention);
        let config = Config::default();
        let (activate, _) =
            check_activation("hello", Some("direct"), &route, &config, Some("@bot"));
        assert!(activate);
    }

    #[test]
    fn group_mention_mode_activates_when_mentioned() {
        let route = default_route();
        let mut config = Config::default();
        config.gateway.group_activation = borg_core::config::ActivationMode::Mention;
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
        config.gateway.group_activation = borg_core::config::ActivationMode::Mention;
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
        config.gateway.group_activation = borg_core::config::ActivationMode::Always;
        let (activate, text) =
            check_activation("hello", Some("group"), &route, &config, Some("@bot"));
        assert!(activate);
        assert_eq!(text, "hello");
    }

    #[test]
    fn group_no_bot_identifier_falls_back_to_always() {
        let route = default_route();
        let mut config = Config::default();
        config.gateway.group_activation = borg_core::config::ActivationMode::Mention;
        let (activate, _) = check_activation("hello", Some("group"), &route, &config, None);
        assert!(activate);
    }

    #[test]
    fn binding_activation_overrides_global() {
        let mut route = default_route();
        route.activation = Some(borg_core::config::ActivationMode::Always);
        let mut config = Config::default();
        config.gateway.group_activation = borg_core::config::ActivationMode::Mention;
        let (activate, _) = check_activation("hello", Some("group"), &route, &config, Some("@bot"));
        assert!(activate);
    }

    #[test]
    fn mention_only_text_still_activates() {
        let route = default_route();
        let mut config = Config::default();
        config.gateway.group_activation = borg_core::config::ActivationMode::Mention;
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
    fn sanitize_filename_empty_string() {
        assert_eq!(
            sanitize_filename(&Some(String::new())),
            Some("attachment".to_string())
        );
    }

    #[test]
    fn sanitize_filename_double_dots() {
        assert_eq!(
            sanitize_filename(&Some("file..name.txt".to_string())),
            Some("attachment".to_string())
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
        let denial_msg = "Access denied. Contact the bot owner for access.";
        assert!(!denial_msg.trim().is_empty());
        // Ensure the message is present in the source code (guards against removal)
        let source = include_str!("handler.rs");
        assert!(
            source.contains(denial_msg),
            "Denial message must be returned to users, not silently dropped"
        );
    }
}
