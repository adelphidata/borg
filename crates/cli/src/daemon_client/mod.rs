//! gRPC client for `borgd`.
//!
//! Wraps a tonic UDS channel and exposes ergonomic methods that match the
//! shape of the in-process Agent API the TUI / REPL / CLI subcommands expect:
//! `connect()`, `open_session()`, `send_message()`, `stream_events()`,
//! `respond_to_prompt()`, `cancel()`, plus admin RPCs (`poke`, `heal`,
//! settings I/O, list_sessions, get_session).
//!
//! Auto-spawn convenience: [`DaemonClient::connect_or_spawn`] launches a
//! fresh `borgd --insecure-uds` if no socket is present at the expected
//! path (dev workflow). Production deployments should run `borgd` as a
//! supervised service and connect via [`DaemonClient::connect`] which
//! errors cleanly when the socket is missing.

mod connect;

pub use connect::{connect, default_socket_path};

use anyhow::{anyhow, Context, Result};
use borg_proto::admin::admin_client::AdminClient;
use borg_proto::admin::{
    Empty as AdminEmpty, HealReport, SessionDetail, SessionList, SessionRef, SettingKey,
    SettingMutation, SettingsSnapshot,
};
use borg_proto::session::session_client::SessionClient;
use borg_proto::session::{
    AgentEvent, CancelRequest, CloseRequest, OpenRequest, PromptResponse, SendRequest,
    StreamRequest,
};
use tonic::transport::Channel;
use tonic::Streaming;

#[allow(unused_imports)]
pub use connect::connect_or_spawn;

/// Lightweight handle wrapping a tonic channel + the four primary clients.
#[derive(Clone, Debug)]
pub struct DaemonClient {
    session: SessionClient<Channel>,
    admin: AdminClient<Channel>,
}

impl DaemonClient {
    /// Wrap an already-connected channel.
    pub fn from_channel(channel: Channel) -> Self {
        Self {
            session: SessionClient::new(channel.clone()),
            admin: AdminClient::new(channel),
        }
    }

    /// Open a fresh session (or resume the named one if `resume_id` is set).
    /// Returns `(session_id, last_event_seq)`.
    pub async fn open_session(&mut self, resume_id: Option<&str>) -> Result<(String, u64)> {
        let resp = self
            .session
            .open(OpenRequest {
                resume_id: resume_id.unwrap_or_default().to_string(),
            })
            .await
            .context("Session.Open failed")?
            .into_inner();
        Ok((resp.session_id, resp.last_event_seq))
    }

    /// Send a user message and stream the resulting `AgentEvent`s for this
    /// turn. The stream closes after `TurnComplete` or `Error`.
    pub async fn send_message(
        &mut self,
        session_id: &str,
        text: &str,
    ) -> Result<Streaming<AgentEvent>> {
        let stream = self
            .session
            .send(SendRequest {
                session_id: session_id.to_string(),
                text: text.to_string(),
            })
            .await
            .context("Session.Send failed")?
            .into_inner();
        Ok(stream)
    }

    /// Subscribe to a session's event stream from `since_event_seq` (use 0 to
    /// replay everything still in the daemon's ring buffer). Long-lived
    /// stream — terminates only on disconnect or session close.
    pub async fn stream_events(
        &mut self,
        session_id: &str,
        since_event_seq: u64,
    ) -> Result<Streaming<AgentEvent>> {
        let stream = self
            .session
            .stream(StreamRequest {
                session_id: session_id.to_string(),
                since_event_seq,
            })
            .await
            .context("Session.Stream failed")?
            .into_inner();
        Ok(stream)
    }

    /// Reply to a `ShellConfirmation` or `UserInputRequest`. The `value` is
    /// `"true"`/`"false"` (or `"yes"`/`"no"`) for confirmations, free-form
    /// text for user-input requests.
    pub async fn respond_to_prompt(
        &mut self,
        session_id: &str,
        prompt_id: &str,
        value: &str,
    ) -> Result<()> {
        self.session
            .respond_to_prompt(PromptResponse {
                session_id: session_id.to_string(),
                prompt_id: prompt_id.to_string(),
                value: value.to_string(),
            })
            .await
            .map_err(|e| anyhow!("Session.RespondToPrompt failed: {e}"))?;
        Ok(())
    }

    /// Cancel the in-flight turn for `session_id`. No-op if the session is
    /// idle.
    pub async fn cancel(&mut self, session_id: &str) -> Result<()> {
        self.session
            .cancel(CancelRequest {
                session_id: session_id.to_string(),
            })
            .await
            .context("Session.Cancel failed")?;
        Ok(())
    }

    /// Close a session. Returns `Err` if the id is unknown.
    pub async fn close_session(&mut self, session_id: &str) -> Result<()> {
        self.session
            .close(CloseRequest {
                session_id: session_id.to_string(),
            })
            .await
            .context("Session.Close failed")?;
        Ok(())
    }

    // ── Admin ──────────────────────────────────────────────────────────────

    /// Trigger an immediate heartbeat. Bypasses quiet hours per the in-process
    /// `/poke` semantics.
    pub async fn poke(&mut self) -> Result<()> {
        self.admin
            .poke(AdminEmpty {})
            .await
            .context("Admin.Poke failed")?;
        Ok(())
    }

    /// Run a daily-maintenance sweep on the daemon. Same code path as the
    /// nightly scheduled task and the in-process `/heal`.
    pub async fn heal(&mut self) -> Result<HealReport> {
        let report = self
            .admin
            .heal(AdminEmpty {})
            .await
            .context("Admin.Heal failed")?
            .into_inner();
        Ok(report)
    }

    /// Get every setting (db + default).
    pub async fn get_settings(&mut self) -> Result<SettingsSnapshot> {
        Ok(self
            .admin
            .get_settings(AdminEmpty {})
            .await
            .context("Admin.GetSettings failed")?
            .into_inner())
    }

    /// Set a setting value (validated and persisted to the daemon's DB).
    pub async fn set_setting(&mut self, key: &str, value: &str) -> Result<()> {
        self.admin
            .set_setting(SettingMutation {
                key: key.to_string(),
                value: value.to_string(),
            })
            .await
            .context("Admin.SetSetting failed")?;
        Ok(())
    }

    /// Remove a setting override (revert to default).
    pub async fn unset_setting(&mut self, key: &str) -> Result<()> {
        self.admin
            .unset_setting(SettingKey {
                key: key.to_string(),
            })
            .await
            .context("Admin.UnsetSetting failed")?;
        Ok(())
    }

    /// List recent sessions. Live sessions are tagged `(live)` in the title.
    pub async fn list_sessions(&mut self) -> Result<SessionList> {
        Ok(self
            .admin
            .list_sessions(AdminEmpty {})
            .await
            .context("Admin.ListSessions failed")?
            .into_inner())
    }

    /// Fetch a single session's transcript.
    pub async fn get_session(&mut self, id: &str) -> Result<SessionDetail> {
        Ok(self
            .admin
            .get_session(SessionRef { id: id.to_string() })
            .await
            .context("Admin.GetSession failed")?
            .into_inner())
    }
}
