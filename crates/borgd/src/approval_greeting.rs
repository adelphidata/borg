//! `Admin.SendApprovalGreeting` — generate and deliver a one-shot personalized
//! greeting after a manual sender approval.
//!
//! Mirrors `crates/cli/src/service/mod.rs::send_approval_greeting` but lives
//! in the daemon so the cli no longer needs to construct an `Agent` for this.
//!
//! On any failure (LLM down, channel API error, empty response) we fall back
//! to the stock static notification via `cortex::pairing::send_approval_notification`.

use crate::session::DaemonSessionExecutor;
use cortex::agent::AgentEvent;
use cortex::config::{CollaborationMode, Config};
use gateway::{InboundTurn, SessionExecutor};

/// Run a quick agent turn to draft a greeting and deliver it. Fire-and-forget;
/// errors are logged + the static fallback is sent.
pub async fn send(
    executor: &DaemonSessionExecutor,
    config: &Config,
    channel_name: &str,
    sender_id: &str,
) {
    let agent_name = config
        .user
        .agent_name
        .as_deref()
        .unwrap_or("Borg")
        .to_string();

    match generate_and_deliver(executor, config, channel_name, sender_id, &agent_name).await {
        Ok(()) => {
            tracing::info!(
                channel = channel_name,
                sender = sender_id,
                "Approval greeting delivered via LLM"
            );
            if let Ok(adb) = cortex::db::Database::open().await {
                cortex::activity_log::log_activity(
                    &adb,
                    "info",
                    "pairing",
                    &format!("Approval greeting delivered to {channel_name}:{sender_id}"),
                )
                .await;
            }
        }
        Err(e) => {
            tracing::warn!(
                channel = channel_name,
                sender = sender_id,
                "LLM greeting failed ({e}), falling back to static message"
            );
            cortex::pairing::send_approval_notification(
                config,
                channel_name,
                sender_id,
                &agent_name,
            )
            .await;
        }
    }
}

async fn generate_and_deliver(
    executor: &DaemonSessionExecutor,
    config: &Config,
    channel_name: &str,
    sender_id: &str,
    agent_name: &str,
) -> anyhow::Result<()> {
    let mut config = config.clone();
    // Plan mode prevents any mutating tool from firing inside this brief turn.
    config.conversation.collaboration_mode = CollaborationMode::Plan;

    let channel_display = cortex::pairing::channel_display_name(channel_name);
    let prompt = format!(
        "A new user has just been approved to message you on {channel_display}. \
         Generate a brief, warm greeting to introduce yourself as {agent_name}. \
         Keep it to 2-3 short sentences. Do NOT use any tools — just reply with the greeting text."
    );

    // Per-sender session key so the greeting attaches to the same thread
    // subsequent messages will land in.
    let session_key = format!("greet:{channel_name}:{sender_id}");

    let turn = InboundTurn {
        session_key: session_key.clone(),
        text: prompt,
        images: Vec::new(),
    };

    let (mut rx, _cancel) = executor.start_turn(turn).await?;
    let mut response = String::new();
    while let Some(event) = rx.recv().await {
        if let AgentEvent::TextDelta(delta) = event {
            response.push_str(&delta);
        }
    }

    let greeting = response.trim().to_string();
    if greeting.is_empty() {
        anyhow::bail!("LLM returned empty greeting");
    }
    let greeting = if greeting.chars().count() > 500 {
        let truncated: String = greeting.chars().take(500).collect();
        format!("{truncated}…")
    } else {
        greeting
    };

    crate::channel_delivery::send(&config, channel_name, sender_id, &greeting).await
}
