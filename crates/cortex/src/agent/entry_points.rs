//! Public turn-entry methods plus shutdown for the headless browser and DB.
//!
//! Pulled out of `agent/mod.rs`. Each `send_message*` variant is the same
//! one-shot prelude (persist user input, bump turn count, dispatch into
//! `run_agent_loop`); colocating them makes the prelude differences obvious
//! at a glance. The two `close_*` shutdown helpers live alongside since they
//! are the matching teardown for the same operator-facing surface.

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::instrument;

use crate::types::Message;

use super::{Agent, AgentEvent};

impl Agent {
    #[instrument(skip_all, fields(session_id = %self.session.meta.id))]
    /// Send a user message and run the agent loop, emitting events to the channel.
    pub async fn send_message(
        &mut self,
        user_input: &str,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> Result<()> {
        self.send_message_with_cancel(user_input, event_tx, CancellationToken::new())
            .await
    }

    #[instrument(skip_all, fields(session_id = %self.session.meta.id, turn_count = self.turn_count))]
    /// Send a user message with cancellation support.
    pub async fn send_message_with_cancel(
        &mut self,
        user_input: &str,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        if user_input.trim().is_empty() {
            tracing::debug!("Ignoring empty user input");
            if let Err(e) = event_tx.send(AgentEvent::TurnComplete).await {
                tracing::warn!("Failed to send TurnComplete for empty input: {e}");
            }
            return Ok(());
        }
        let msg = Message::user(user_input);
        self.log_and_persist(msg);
        self.turn_count += 1;
        // Reset recent tool names each user turn so conditional loading
        // only considers tools used in the current turn's agent loop.
        self.recent_tool_names.clear();
        self.run_agent_loop(event_tx, cancel).await
    }

    /// Send a pre-constructed Message (e.g. multimodal) through the agent loop.
    #[instrument(skip_all, fields(session_id = %self.session.meta.id, turn_count = self.turn_count))]
    pub async fn send_message_raw(
        &mut self,
        msg: Message,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        self.log_and_persist(msg);
        self.turn_count += 1;
        self.run_agent_loop(event_tx, cancel).await
    }

    /// Close the headless browser session if one is open.
    #[instrument(skip_all)]
    pub async fn close_browser(&mut self) {
        if let Some(session) = self.browser_session.take() {
            let _ = session.close().await;
        }
    }

    /// Close the database connection, releasing all file locks.
    /// Call before deleting the data directory (e.g. during uninstall).
    pub fn close_db(&self) {
        let mut guard = self.db_guard();
        if guard.take().is_some() {
            tracing::info!("Database connection closed for uninstall");
        }
    }
}
