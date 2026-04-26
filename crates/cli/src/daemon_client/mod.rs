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
mod event_stream;

pub use connect::{connect, default_socket_path};
pub use event_stream::spawn_event_adapter;

use anyhow::{anyhow, Context, Result};
use borg_proto::admin::admin_client::AdminClient;
use borg_proto::admin::{
    ApprovedSenderList, ApprovedSenderRef, DaemonStatus, Empty as AdminEmpty, HealReport,
    PairingApproval, PairingFilter, PairingRequestList, Project, ProjectCreate, ProjectList,
    ProjectRef, ProjectUpdate, ScheduledTask, ScheduledTaskCreate, ScheduledTaskList,
    ScheduledTaskRef, SessionDetail, SessionList, SessionRef, SettingKey, SettingMutation,
    SettingsSnapshot,
};
use borg_proto::session::session_client::SessionClient;
use borg_proto::session::{
    AgentEvent, AutoSaveRequest, CancelRequest, ClearRequest, CloseRequest, CompactRequest,
    CompactResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, OpenRequest, PromptResponse, RewindRequest, RewindResponse, SendRequest,
    SessionState, SessionStateRequest, SteerRequest, StreamRequest, UndoRequest, UndoResponse,
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
        self.send_message_with_images(session_id, text, Vec::new())
            .await
    }

    /// Send a multimodal message (text + inline images).
    pub async fn send_message_with_images(
        &mut self,
        session_id: &str,
        text: &str,
        images: Vec<borg_proto::session::InlineImage>,
    ) -> Result<Streaming<AgentEvent>> {
        let stream = self
            .session
            .send(SendRequest {
                session_id: session_id.to_string(),
                text: text.to_string(),
                images,
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

    // ── Session-state RPCs (replace direct agent calls in TUI) ────────────

    /// Snapshot of agent state for the given session_id.
    pub async fn get_state(&mut self, session_id: &str) -> Result<SessionState> {
        Ok(self
            .session
            .get_state(SessionStateRequest {
                session_id: session_id.to_string(),
            })
            .await
            .context("Session.GetState failed")?
            .into_inner())
    }

    /// Compact the agent's history.
    pub async fn compact(&mut self, session_id: &str) -> Result<CompactResponse> {
        Ok(self
            .session
            .compact(CompactRequest {
                session_id: session_id.to_string(),
            })
            .await
            .context("Session.Compact failed")?
            .into_inner())
    }

    /// Wipe history.
    pub async fn clear_history(&mut self, session_id: &str) -> Result<()> {
        self.session
            .clear_history(ClearRequest {
                session_id: session_id.to_string(),
            })
            .await
            .context("Session.ClearHistory failed")?;
        Ok(())
    }

    /// Drop the most recent assistant turn.
    pub async fn undo(&mut self, session_id: &str) -> Result<UndoResponse> {
        Ok(self
            .session
            .undo(UndoRequest {
                session_id: session_id.to_string(),
            })
            .await
            .context("Session.Undo failed")?
            .into_inner())
    }

    /// Rewind to the Nth user message.
    pub async fn rewind(&mut self, session_id: &str, n: u64) -> Result<RewindResponse> {
        Ok(self
            .session
            .rewind_to_nth_user_message(RewindRequest {
                session_id: session_id.to_string(),
                n,
            })
            .await
            .context("Session.RewindToNthUserMessage failed")?
            .into_inner())
    }

    /// Persist the agent's current state.
    pub async fn auto_save(&mut self, session_id: &str) -> Result<()> {
        self.session
            .auto_save(AutoSaveRequest {
                session_id: session_id.to_string(),
            })
            .await
            .context("Session.AutoSave failed")?;
        Ok(())
    }

    /// Start a fresh agent session inside the existing host.
    pub async fn new_agent_session(&mut self, session_id: &str) -> Result<NewSessionResponse> {
        Ok(self
            .session
            .new_session(NewSessionRequest {
                session_id: session_id.to_string(),
            })
            .await
            .context("Session.NewSession failed")?
            .into_inner())
    }

    /// Load an existing agent session by id inside the existing host.
    pub async fn load_agent_session(
        &mut self,
        session_id: &str,
        agent_session_id: &str,
    ) -> Result<LoadSessionResponse> {
        Ok(self
            .session
            .load_session(LoadSessionRequest {
                session_id: session_id.to_string(),
                agent_session_id: agent_session_id.to_string(),
            })
            .await
            .context("Session.LoadSession failed")?
            .into_inner())
    }

    /// Push a steer message into the in-flight turn.
    pub async fn steer(&mut self, session_id: &str, text: &str) -> Result<()> {
        self.session
            .steer(SteerRequest {
                session_id: session_id.to_string(),
                text: text.to_string(),
            })
            .await
            .context("Session.Steer failed")?;
        Ok(())
    }

    // ── Projects ──────────────────────────────────────────────────────────

    /// List all projects.
    pub async fn list_projects(&mut self) -> Result<ProjectList> {
        Ok(self
            .admin
            .list_projects(AdminEmpty {})
            .await
            .context("Admin.ListProjects failed")?
            .into_inner())
    }

    /// Create a project.
    pub async fn create_project(&mut self, name: &str, description: &str) -> Result<Project> {
        Ok(self
            .admin
            .create_project(ProjectCreate {
                name: name.to_string(),
                description: description.to_string(),
            })
            .await
            .context("Admin.CreateProject failed")?
            .into_inner())
    }

    /// Update a project (empty fields are skipped).
    pub async fn update_project(
        &mut self,
        id: &str,
        name: &str,
        description: &str,
        status: &str,
    ) -> Result<Project> {
        Ok(self
            .admin
            .update_project(ProjectUpdate {
                id: id.to_string(),
                name: name.to_string(),
                description: description.to_string(),
                status: status.to_string(),
            })
            .await
            .context("Admin.UpdateProject failed")?
            .into_inner())
    }

    /// Delete a project by id.
    pub async fn delete_project(&mut self, id: &str) -> Result<()> {
        self.admin
            .delete_project(ProjectRef { id: id.to_string() })
            .await
            .context("Admin.DeleteProject failed")?;
        Ok(())
    }

    // ── Scheduled tasks ───────────────────────────────────────────────────

    /// List all scheduled tasks.
    pub async fn list_scheduled_tasks(&mut self) -> Result<ScheduledTaskList> {
        Ok(self
            .admin
            .list_scheduled_tasks(AdminEmpty {})
            .await
            .context("Admin.ListScheduledTasks failed")?
            .into_inner())
    }

    /// Create a scheduled task.
    pub async fn create_scheduled_task(
        &mut self,
        spec: ScheduledTaskCreate,
    ) -> Result<ScheduledTask> {
        Ok(self
            .admin
            .create_scheduled_task(spec)
            .await
            .context("Admin.CreateScheduledTask failed")?
            .into_inner())
    }

    /// Delete a scheduled task by id.
    pub async fn delete_scheduled_task(&mut self, id: &str) -> Result<()> {
        self.admin
            .delete_scheduled_task(ScheduledTaskRef { id: id.to_string() })
            .await
            .context("Admin.DeleteScheduledTask failed")?;
        Ok(())
    }

    /// Pause a scheduled task.
    pub async fn pause_scheduled_task(&mut self, id: &str) -> Result<()> {
        self.admin
            .pause_scheduled_task(ScheduledTaskRef { id: id.to_string() })
            .await
            .context("Admin.PauseScheduledTask failed")?;
        Ok(())
    }

    /// Resume a paused scheduled task.
    pub async fn resume_scheduled_task(&mut self, id: &str) -> Result<()> {
        self.admin
            .resume_scheduled_task(ScheduledTaskRef { id: id.to_string() })
            .await
            .context("Admin.ResumeScheduledTask failed")?;
        Ok(())
    }

    // ── Pairing ───────────────────────────────────────────────────────────

    /// List pairing requests, optionally filtered by channel.
    pub async fn list_pairing_requests(
        &mut self,
        channel: Option<&str>,
    ) -> Result<PairingRequestList> {
        Ok(self
            .admin
            .list_pairing_requests(PairingFilter {
                channel_name: channel.unwrap_or("").to_string(),
            })
            .await
            .context("Admin.ListPairingRequests failed")?
            .into_inner())
    }

    /// Approve a pairing request by code.
    pub async fn approve_pairing(&mut self, channel: &str, code: &str) -> Result<()> {
        self.admin
            .approve_pairing(PairingApproval {
                channel_name: channel.to_string(),
                code: code.to_string(),
            })
            .await
            .context("Admin.ApprovePairing failed")?;
        Ok(())
    }

    /// List approved senders, optionally filtered by channel.
    pub async fn list_approved_senders(
        &mut self,
        channel: Option<&str>,
    ) -> Result<ApprovedSenderList> {
        Ok(self
            .admin
            .list_approved_senders(PairingFilter {
                channel_name: channel.unwrap_or("").to_string(),
            })
            .await
            .context("Admin.ListApprovedSenders failed")?
            .into_inner())
    }

    /// Revoke an approved sender.
    pub async fn revoke_approved_sender(&mut self, channel: &str, sender_id: &str) -> Result<()> {
        self.admin
            .revoke_approved_sender(ApprovedSenderRef {
                channel_name: channel.to_string(),
                sender_id: sender_id.to_string(),
            })
            .await
            .context("Admin.RevokeApprovedSender failed")?;
        Ok(())
    }

    /// Daemon-wide status snapshot.
    pub async fn get_daemon_status(&mut self) -> Result<DaemonStatus> {
        Ok(self
            .admin
            .get_daemon_status(AdminEmpty {})
            .await
            .context("Admin.GetDaemonStatus failed")?
            .into_inner())
    }
}
