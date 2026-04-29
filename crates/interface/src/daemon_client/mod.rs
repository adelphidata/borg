//! gRPC client for `borgd`.
//!
//! Wraps a tonic UDS channel and exposes ergonomic methods that match the
//! shape of the in-process Agent API the TUI / REPL / CLI subcommands expect:
//! `connect()`, `open_session()`, `send_message()`, `stream_events()`,
//! `respond_to_prompt()`, `cancel()`, plus admin RPCs (`poke`, `heal`,
//! settings I/O, list_sessions, get_session).
//!
//! Auto-spawn convenience: `connect_or_spawn` launches a
//! fresh `borgd` if no socket is present at the expected path (dev
//! workflow). Production deployments should run `borgd` as a supervised
//! service and connect via `connect` which errors cleanly
//! when the socket is missing.

mod connect;
mod event_stream;

pub use connect::{connect, default_socket_path};
pub use event_stream::{forward_event_stream, spawn_event_adapter};

use anyhow::{anyhow, Context, Result};
use protocols::admin::admin_client::AdminClient;
use protocols::admin::{
    ActivityLogQuery, ActivityLogText, AddProviderRequest, AmbientStatus, ApiKeyList, ApiKeyRef,
    ApiKeyValue, ApprovalGreetingRequest, ApprovedSenderList, ApprovedSenderRef, ClientTokenRef,
    CreateWebhookRequest, CreateWebhookResponse, DaemonStatus, Empty as AdminEmpty,
    ExportSessionRequest, ExportSessionResponse, GetScheduledTaskRunsRequest, HealReport,
    HeartbeatEvent as PbHeartbeatEvent, IssueClientTokenRequest, IssueClientTokenResponse,
    LogActivityRequest, MonthlyTokenTotal, MonthlyTotalCost, OnboardingState, PairingApproval,
    PairingFilter, PairingRequestList, PluginInstallRequest, PluginInstallResponse, Project,
    ProjectCreate, ProjectList, ProjectRef, ProjectUpdate, RemoveProviderRequest,
    RevokedClientToken, ScheduledTask, ScheduledTaskCreate, ScheduledTaskDetail, ScheduledTaskList,
    ScheduledTaskRef, ScheduledTaskRunList, ScheduledTaskUpdate, SessionDetail, SessionList,
    SessionRef, SettingKey, SettingMutation, SettingsSnapshot, TriggerHeartbeatRequest,
    WarningList, WebhookRef, WorkflowList,
};
use protocols::pki::pki_client::PkiClient;
use protocols::pki::{
    CertLifecycleResponse, PairedDeviceList, PkiEmpty, RenewCertRequest, RenewCertResponse,
    RevokeDeviceRequest, StartPairingRequest, StartPairingResponse, VerifyAuditResponse,
};
use protocols::session::session_client::SessionClient;
use protocols::session::{
    AgentEvent, AutoSaveRequest, CancelRequest, ClearRequest, CloseRequest, CompactRequest,
    CompactResponse, ForkRequest, ForkResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, OpenRequest, PromptResponse, RewindRequest,
    RewindResponse, SendRequest, SessionState, SessionStateRequest, SteerRequest, StreamRequest,
    UndoRequest, UndoResponse,
};
use protocols::status::status_client::StatusClient;
use protocols::status::{
    BondEventList, BondStateProto, Empty as StatusEmpty, EvolutionEventList, EvolutionStateProto,
    Int64Cursor, StatusText, StatusTextRequest, Vitals, VitalsEventList, XpSnapshot,
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
    status: StatusClient<Channel>,
    pki: PkiClient<Channel>,
}

impl DaemonClient {
    /// Construct a client whose channel is lazily connected (and will fail
    /// every RPC). Test-only — used by TUI unit tests that build an `App`
    /// without spawning a real daemon.
    ///
    /// Internally enters a process-wide tokio runtime context so it works
    /// from sync `#[test]` fns; tonic's `connect_lazy` spawns reactor tasks
    /// at construction time.
    ///
    /// Gated to test builds so production binaries cannot accidentally
    /// instantiate a permanently-disconnected client.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn lazy_for_tests() -> Self {
        use std::sync::OnceLock;
        static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        let rt = RT.get_or_init(|| {
            match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => panic!("lazy_for_tests: failed to build tokio runtime: {e}"),
            }
        });
        let _guard = rt.enter();
        let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
        Self::from_channel(channel)
    }

    /// Wrap an already-connected channel.
    pub fn from_channel(channel: Channel) -> Self {
        Self {
            session: SessionClient::new(channel.clone()),
            admin: AdminClient::new(channel.clone()),
            status: StatusClient::new(channel.clone()),
            pki: PkiClient::new(channel),
        }
    }

    /// Mint a fresh pairing code on the daemon. Returns the plaintext code
    /// and its expiry. The code is shown to the operator and used by a
    /// separate `borg pair <code>` invocation that drives the SPAKE2 exchange
    /// over the bootstrap UDS.
    pub async fn start_pairing(&mut self, device_label: &str) -> Result<StartPairingResponse> {
        Ok(self
            .pki
            .start_pairing(StartPairingRequest {
                device_label: device_label.to_string(),
            })
            .await?
            .into_inner())
    }

    pub async fn revoke_device(&mut self, fingerprint: &str, reason: &str) -> Result<()> {
        self.pki
            .revoke_device(RevokeDeviceRequest {
                fingerprint: fingerprint.to_string(),
                reason: reason.to_string(),
            })
            .await?;
        Ok(())
    }

    pub async fn list_paired_devices(&mut self) -> Result<PairedDeviceList> {
        Ok(self
            .pki
            .list_paired_devices(PkiEmpty {})
            .await?
            .into_inner())
    }

    pub async fn verify_audit_chain(&mut self) -> Result<VerifyAuditResponse> {
        Ok(self.pki.verify_audit_chain(PkiEmpty {}).await?.into_inner())
    }

    /// Ask the daemon whether the calling client's mTLS cert is within the
    /// 14-day silent-renewal window. Cheap call (single indexed lookup).
    pub async fn get_cert_lifecycle(&mut self) -> Result<CertLifecycleResponse> {
        Ok(self.pki.get_cert_lifecycle(PkiEmpty {}).await?.into_inner())
    }

    /// Ask the daemon to mint a fresh client cert for `fingerprint` (must
    /// match the connection's principal — daemon enforces). Returned PEM
    /// is the caller's responsibility to persist into the OS keychain.
    pub async fn renew_cert(&mut self, fingerprint: &str) -> Result<RenewCertResponse> {
        Ok(self
            .pki
            .renew_cert(RenewCertRequest {
                fingerprint: fingerprint.to_string(),
            })
            .await?
            .into_inner())
    }

    /// If the daemon reports the calling client's mTLS cert is inside the
    /// 14-day silent-renewal window, mint a fresh cert and rotate the
    /// keychain-stored PEMs in place. Returns `Ok(true)` if a renewal
    /// happened, `Ok(false)` if no renewal was needed (no cert yet, daemon
    /// reports nothing to renew, or the channel is unauthenticated /
    /// unimplemented — e.g., bootstrap pairing UDS or in-process tests
    /// without mTLS).
    ///
    /// The daemon enforces that the request fingerprint matches the
    /// connection's `Principal` — so callers MUST invoke this on a
    /// channel established with the keychain-stored client cert; passing
    /// the keychain cert's fingerprint while connected with a different
    /// identity is rejected as `permission_denied` by the server.
    ///
    /// **Keychain rotation order** is key-then-cert: a crash between the
    /// two writes leaves the *old* cert paired with the *new* key, which
    /// fails cleanly at next handshake (the daemon will offer renewal
    /// again). The reverse order (cert-first) would brick the client,
    /// because the daemon revokes the old fingerprint as part of the
    /// successful `renew_cert` RPC.
    ///
    /// Errors when a renewal was attempted and failed mid-flight; callers
    /// in fire-and-forget contexts should `tracing::warn!` and continue
    /// rather than abort the user-visible operation.
    pub async fn ensure_cert_fresh<K: identity::KekStore>(&mut self, kek: &K) -> Result<bool> {
        // Pre-pairing connections (bootstrap UDS) have no cert yet — nothing
        // to renew. Skip BEFORE calling the daemon so we don't surface a
        // misleading "no client cert in keychain" error during pairing.
        // `KekStore::get` returns `NotFound` (not `Ok(None)`) for absent
        // accounts; treat that as the pre-pairing case.
        let cert_pem = match kek.get(crate::commands::device_pair::CLIENT_CERT_KEY_ACCOUNT) {
            Ok(pem) => pem,
            Err(identity::PkiError::NotFound(_)) => return Ok(false),
            Err(e) => return Err(anyhow!("read current client cert from keychain: {e}")),
        };

        // Call the underlying tonic stub so we can inspect the gRPC
        // `Status::code()` rather than the anyhow-wrapped form returned by
        // [`Self::get_cert_lifecycle`]. Distinguish "this channel can't
        // tell us" (unauthenticated / unimplemented / permission denied)
        // from real failures — the former is silently a no-op; the latter
        // must surface so the caller can `tracing::warn!`.
        let lifecycle = match self.pki.get_cert_lifecycle(PkiEmpty {}).await {
            Ok(resp) => resp.into_inner(),
            Err(status) => {
                use tonic::Code;
                return match status.code() {
                    Code::Unimplemented | Code::Unauthenticated | Code::PermissionDenied => {
                        Ok(false)
                    }
                    _ => Err(anyhow!("get_cert_lifecycle: {status}")),
                };
            }
        };
        if !lifecycle.renewal_due || lifecycle.not_after == 0 {
            return Ok(false);
        }
        let fp =
            identity::fingerprint_of_pem(&cert_pem).context("compute current cert fingerprint")?;
        let resp = self
            .renew_cert(&fp)
            .await
            .context("daemon renew_cert RPC")?;
        // Key first, then cert — see doc comment for the failure-mode
        // rationale. Anything that survives a key-only crash is recoverable.
        kek.put(
            crate::commands::device_pair::CLIENT_KEY_KEY_ACCOUNT,
            resp.new_key_pkcs8_pem.as_bytes(),
        )
        .context("rotate client key in keychain")?;
        kek.put(
            crate::commands::device_pair::CLIENT_CERT_KEY_ACCOUNT,
            resp.new_cert_pem.as_bytes(),
        )
        .context("rotate client cert in keychain")?;
        Ok(true)
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
        images: Vec<protocols::session::InlineImage>,
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

    /// Trigger a specific heartbeat source on the daemon.
    pub async fn trigger_heartbeat(
        &mut self,
        source: &str,
        bypass_quiet_hours: bool,
    ) -> Result<()> {
        self.admin
            .trigger_heartbeat(TriggerHeartbeatRequest {
                source: source.to_string(),
                bypass_quiet_hours,
            })
            .await
            .context("Admin.TriggerHeartbeat failed")?;
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

    /// Fork the active agent session at its last message. Daemon-side
    /// execution — the TUI must NOT open a SQLite writer to do this itself,
    /// since `borgd` is the single writer (CLAUDE.md "DB writer split-brain").
    pub async fn fork_session(&mut self, session_id: &str) -> Result<ForkResponse> {
        Ok(self
            .session
            .fork(ForkRequest {
                session_id: session_id.to_string(),
            })
            .await
            .context("Session.Fork failed")?
            .into_inner())
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

    /// Queue a scheduled task for immediate execution.
    pub async fn run_scheduled_task_now(&mut self, id: &str) -> Result<()> {
        self.admin
            .run_scheduled_task_now(ScheduledTaskRef { id: id.to_string() })
            .await
            .context("Admin.RunScheduledTaskNow failed")?;
        Ok(())
    }

    /// Fetch the most recent run history for a scheduled task.
    pub async fn get_scheduled_task_runs(
        &mut self,
        id: &str,
        limit: u32,
    ) -> Result<ScheduledTaskRunList> {
        Ok(self
            .admin
            .get_scheduled_task_runs(GetScheduledTaskRunsRequest {
                id: id.to_string(),
                limit,
            })
            .await
            .context("Admin.GetScheduledTaskRuns failed")?
            .into_inner())
    }

    /// Detailed view of a scheduled task: full row + last run summary.
    pub async fn get_scheduled_task_detail(&mut self, id: &str) -> Result<ScheduledTaskDetail> {
        Ok(self
            .admin
            .get_scheduled_task_detail(ScheduledTaskRef { id: id.to_string() })
            .await
            .context("Admin.GetScheduledTaskDetail failed")?
            .into_inner())
    }

    /// Render a session via the daemon's export pipeline.
    pub async fn export_session(
        &mut self,
        session_id: &str,
        format: &str,
    ) -> Result<ExportSessionResponse> {
        Ok(self
            .admin
            .export_session(ExportSessionRequest {
                session_id: session_id.to_string(),
                format: format.to_string(),
            })
            .await
            .context("Admin.ExportSession failed")?
            .into_inner())
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

    /// Update a scheduled task. Empty-string fields are left unchanged
    /// (mirrors `cortex::db::UpdateTask`'s `Option<&str>` semantics).
    pub async fn update_scheduled_task(
        &mut self,
        spec: ScheduledTaskUpdate,
    ) -> Result<ScheduledTask> {
        Ok(self
            .admin
            .update_scheduled_task(spec)
            .await
            .context("Admin.UpdateScheduledTask failed")?
            .into_inner())
    }

    /// Mark a project archived (convenience over UpdateProject).
    pub async fn archive_project(&mut self, id: &str) -> Result<()> {
        self.admin
            .archive_project(ProjectRef { id: id.to_string() })
            .await
            .context("Admin.ArchiveProject failed")?;
        Ok(())
    }

    /// Mark a project active (convenience over UpdateProject).
    pub async fn unarchive_project(&mut self, id: &str) -> Result<()> {
        self.admin
            .unarchive_project(ProjectRef { id: id.to_string() })
            .await
            .context("Admin.UnarchiveProject failed")?;
        Ok(())
    }

    /// List workflows for a project (used by the projects popup workflow_count).
    pub async fn list_workflows_by_project(&mut self, project_id: &str) -> Result<WorkflowList> {
        Ok(self
            .admin
            .list_workflows_by_project(ProjectRef {
                id: project_id.to_string(),
            })
            .await
            .context("Admin.ListWorkflowsByProject failed")?
            .into_inner())
    }

    /// Ambient status (the `class:` line in the TUI header).
    pub async fn get_ambient_status(&mut self) -> Result<AmbientStatus> {
        Ok(self
            .admin
            .get_ambient_status(AdminEmpty {})
            .await
            .context("Admin.GetAmbientStatus failed")?
            .into_inner())
    }

    /// Latest doctor-run persistent warnings.
    pub async fn get_persistent_warnings(&mut self) -> Result<WarningList> {
        Ok(self
            .admin
            .get_persistent_warnings(AdminEmpty {})
            .await
            .context("Admin.GetPersistentWarnings failed")?
            .into_inner())
    }

    // ── Status ────────────────────────────────────────────────────────────

    /// Vitals snapshot — already decay-applied, with optional drift_notice.
    pub async fn get_vitals(&mut self) -> Result<Vitals> {
        Ok(self
            .status
            .get_vitals(StatusEmpty {})
            .await
            .context("Status.GetVitals failed")?
            .into_inner())
    }

    /// Evolution state snapshot (replay-derived).
    pub async fn get_evolution(&mut self) -> Result<EvolutionStateProto> {
        Ok(self
            .status
            .get_evolution(StatusEmpty {})
            .await
            .context("Status.GetEvolution failed")?
            .into_inner())
    }

    /// Bond state snapshot.
    pub async fn get_bond_state(&mut self) -> Result<BondStateProto> {
        Ok(self
            .status
            .get_bond_state(StatusEmpty {})
            .await
            .context("Status.GetBondState failed")?
            .into_inner())
    }

    /// Bond events since a unix timestamp (DESC).
    pub async fn get_bond_events_since(&mut self, since_unix: i64) -> Result<BondEventList> {
        Ok(self
            .status
            .get_bond_events_since(Int64Cursor { since_unix })
            .await
            .context("Status.GetBondEventsSince failed")?
            .into_inner())
    }

    /// Evolution events since a unix timestamp (DESC).
    pub async fn get_evolution_events_since(
        &mut self,
        since_unix: i64,
    ) -> Result<EvolutionEventList> {
        Ok(self
            .status
            .get_evolution_events_since(Int64Cursor { since_unix })
            .await
            .context("Status.GetEvolutionEventsSince failed")?
            .into_inner())
    }

    /// Vitals events since a unix timestamp (DESC).
    pub async fn get_vitals_events_since(&mut self, since_unix: i64) -> Result<VitalsEventList> {
        Ok(self
            .status
            .get_vitals_events_since(Int64Cursor { since_unix })
            .await
            .context("Status.GetVitalsEventsSince failed")?
            .into_inner())
    }

    /// Pre-rendered text for the status popup's Overview tab. `card_width` is
    /// the popup's inner width (passed verbatim to the daemon's evolution-card
    /// formatter); pass 0 for a sane default.
    pub async fn get_status_overview_text(&mut self, card_width: u32) -> Result<StatusText> {
        Ok(self
            .status
            .get_status_overview_text(StatusTextRequest { card_width })
            .await
            .context("Status.GetStatusOverviewText failed")?
            .into_inner())
    }

    /// Pre-rendered `/evolution` text for the status popup's Evolution tab.
    pub async fn get_evolution_text(&mut self) -> Result<StatusText> {
        Ok(self
            .status
            .get_evolution_text(StatusEmpty {})
            .await
            .context("Status.GetEvolutionText failed")?
            .into_inner())
    }

    /// Pre-rendered `/xp` text for the status popup's XP tab.
    pub async fn get_xp_text(&mut self) -> Result<StatusText> {
        Ok(self
            .status
            .get_xp_text(StatusEmpty {})
            .await
            .context("Status.GetXpText failed")?
            .into_inner())
    }

    /// Pre-rendered archetype-scores text for the status popup.
    pub async fn get_archetype_scores_text(&mut self) -> Result<StatusText> {
        Ok(self
            .status
            .get_archetype_scores_text(StatusEmpty {})
            .await
            .context("Status.GetArchetypeScoresText failed")?
            .into_inner())
    }

    /// Pre-rendered evolution-history text for the status popup.
    pub async fn get_evolution_history_text(&mut self) -> Result<StatusText> {
        Ok(self
            .status
            .get_evolution_history_text(StatusEmpty {})
            .await
            .context("Status.GetEvolutionHistoryText failed")?
            .into_inner())
    }

    /// XP snapshot (`/xp` data).
    pub async fn get_xp(&mut self) -> Result<XpSnapshot> {
        Ok(self
            .status
            .get_xp(StatusEmpty {})
            .await
            .context("Status.GetXp failed")?
            .into_inner())
    }

    /// Ask the daemon to shut down gracefully. Returns once the daemon has
    /// acknowledged the request — the daemon's drain loop runs in the
    /// background. Subsequent `borg` invocations re-spawn the daemon.
    pub async fn shutdown(&mut self) -> Result<()> {
        self.admin
            .shutdown(AdminEmpty {})
            .await
            .context("Admin.Shutdown failed")?;
        Ok(())
    }

    /// Generate + deliver a personalized greeting for a newly approved sender.
    pub async fn send_approval_greeting(&mut self, channel: &str, sender_id: &str) -> Result<()> {
        self.admin
            .send_approval_greeting(ApprovalGreetingRequest {
                channel: channel.to_string(),
                sender_id: sender_id.to_string(),
            })
            .await
            .context("Admin.SendApprovalGreeting failed")?;
        Ok(())
    }

    /// Subscribe to heartbeat lifecycle events. Caller drains the stream;
    /// dropping it cleans up the daemon-side subscriber.
    pub async fn subscribe_heartbeat_events(&mut self) -> Result<Streaming<PbHeartbeatEvent>> {
        Ok(self
            .admin
            .subscribe_heartbeat_events(AdminEmpty {})
            .await
            .context("Admin.SubscribeHeartbeatEvents failed")?
            .into_inner())
    }

    /// Append a row to `activity_log` via the daemon (avoids opening the DB
    /// from the cli).
    pub async fn log_activity(&mut self, level: &str, source: &str, message: &str) -> Result<()> {
        self.admin
            .log_activity(LogActivityRequest {
                level: level.to_string(),
                source: source.to_string(),
                message: message.to_string(),
            })
            .await
            .context("Admin.LogActivity failed")?;
        Ok(())
    }

    /// Pre-rendered activity-log tail. `min_level` accepts the same level
    /// strings as `cortex::db::query_activity` (`"error"`, `"warn"`,
    /// `"info"`, `"debug"`, `"all"`); empty falls back to `"info"`.
    pub async fn get_activity_log_text(
        &mut self,
        count: u32,
        min_level: &str,
        category: &str,
    ) -> Result<ActivityLogText> {
        Ok(self
            .admin
            .get_activity_log_text(ActivityLogQuery {
                count,
                min_level: min_level.to_string(),
                category: category.to_string(),
            })
            .await
            .context("Admin.GetActivityLogText failed")?
            .into_inner())
    }

    /// Persist plugin install metadata + file hashes in a single round-trip.
    pub async fn plugin_install(
        &mut self,
        req: PluginInstallRequest,
    ) -> Result<PluginInstallResponse> {
        Ok(self
            .admin
            .plugin_install(req)
            .await
            .context("Admin.PluginInstall failed")?
            .into_inner())
    }

    // ── API key routing ──────────────────────────────────────────────────

    /// Fetch the routing metadata for a provider's API key. Returns an
    /// `ApiKeyValue` whose `body` is one of `literal`/`keychain`/`unset`.
    /// The cli does the keychain lookup locally — the daemon only stores
    /// the routing.
    pub async fn get_api_key(&mut self, provider: &str) -> Result<ApiKeyValue> {
        Ok(self
            .admin
            .get_api_key(ApiKeyRef {
                provider: provider.to_string(),
            })
            .await
            .context("Admin.GetApiKey failed")?
            .into_inner())
    }

    /// Persist API-key routing metadata for `provider`. The body must be a
    /// `literal` or `keychain` ref — `unset` is rejected here; use
    /// [`unset_api_key`](Self::unset_api_key) to clear.
    pub async fn set_api_key(&mut self, value: ApiKeyValue) -> Result<()> {
        self.admin
            .set_api_key(value)
            .await
            .context("Admin.SetApiKey failed")?;
        Ok(())
    }

    /// Clear the stored routing metadata for `provider`. The cli is
    /// responsible for deleting any companion keychain entry.
    pub async fn unset_api_key(&mut self, provider: &str) -> Result<()> {
        self.admin
            .unset_api_key(ApiKeyRef {
                provider: provider.to_string(),
            })
            .await
            .context("Admin.UnsetApiKey failed")?;
        Ok(())
    }

    /// List every provider with routing metadata stored, with the backend
    /// kind ("literal" or "keychain") so the cli can render which backend
    /// is in use without leaking the secret.
    pub async fn list_api_keys(&mut self) -> Result<ApiKeyList> {
        Ok(self
            .admin
            .list_api_keys(AdminEmpty {})
            .await
            .context("Admin.ListApiKeys failed")?
            .into_inner())
    }

    // ── Onboarding ───────────────────────────────────────────────────────

    /// Read the `onboarded` sentinel from the daemon's settings table.
    pub async fn get_onboarding_state(&mut self) -> Result<OnboardingState> {
        Ok(self
            .admin
            .get_onboarding_state(AdminEmpty {})
            .await
            .context("Admin.GetOnboardingState failed")?
            .into_inner())
    }

    /// Mark first-run onboarding complete (`onboarded=true` in settings).
    pub async fn set_onboarding_complete(&mut self) -> Result<()> {
        self.admin
            .set_onboarding_complete(AdminEmpty {})
            .await
            .context("Admin.SetOnboardingComplete failed")?;
        Ok(())
    }

    /// Monthly token total across `token_usage` rows in the current calendar
    /// month (UTC). Used by the TUI `/usage` view.
    pub async fn get_monthly_token_total(&mut self) -> Result<MonthlyTokenTotal> {
        Ok(self
            .admin
            .get_monthly_token_total(AdminEmpty {})
            .await
            .context("Admin.GetMonthlyTokenTotal failed")?
            .into_inner())
    }

    /// Monthly cost total. `present=false` means there are no rows this
    /// month — caller should hide the line rather than render `$0.00`.
    pub async fn get_monthly_total_cost(&mut self) -> Result<MonthlyTotalCost> {
        Ok(self
            .admin
            .get_monthly_total_cost(AdminEmpty {})
            .await
            .context("Admin.GetMonthlyTotalCost failed")?
            .into_inner())
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

    // ── Provider / webhook / client-token writes ──────────────────────────

    /// Insert a BYO provider row. Reads stay on `Database::open_readonly`.
    pub async fn add_provider(
        &mut self,
        id: &str,
        display_name: &str,
        base_url: &str,
        api_key_env: Option<&str>,
        category: &str,
    ) -> Result<()> {
        self.admin
            .add_provider(AddProviderRequest {
                id: id.to_string(),
                display_name: display_name.to_string(),
                base_url: base_url.to_string(),
                api_key_env: api_key_env.unwrap_or("").to_string(),
                category: category.to_string(),
            })
            .await
            .context("Admin.AddProvider failed")?;
        Ok(())
    }

    /// Remove a BYO provider row by id. Idempotent — succeeds even if the id
    /// is already absent.
    pub async fn remove_provider(&mut self, id: &str) -> Result<()> {
        self.admin
            .remove_provider(RemoveProviderRequest { id: id.to_string() })
            .await
            .context("Admin.RemoveProvider failed")?;
        Ok(())
    }

    /// Create a webhook definition; returns the raw HMAC secret bytes —
    /// the caller is responsible for printing the hex once and discarding.
    pub async fn create_webhook(
        &mut self,
        name: &str,
        sender_path: &str,
        thread_path: Option<&str>,
        body_path: &str,
    ) -> Result<CreateWebhookResponse> {
        Ok(self
            .admin
            .create_webhook(CreateWebhookRequest {
                name: name.to_string(),
                sender_path: sender_path.to_string(),
                thread_path: thread_path.unwrap_or("").to_string(),
                body_path: body_path.to_string(),
            })
            .await
            .context("Admin.CreateWebhook failed")?
            .into_inner())
    }

    /// Mark a webhook revoked. Idempotent.
    pub async fn revoke_webhook(&mut self, name: &str) -> Result<()> {
        self.admin
            .revoke_webhook(WebhookRef {
                name: name.to_string(),
            })
            .await
            .context("Admin.RevokeWebhook failed")?;
        Ok(())
    }

    /// Issue a bearer client token; the plaintext is returned exactly once.
    pub async fn issue_client_token(
        &mut self,
        label: &str,
        scopes: &str,
    ) -> Result<IssueClientTokenResponse> {
        Ok(self
            .admin
            .issue_client_token(IssueClientTokenRequest {
                label: label.to_string(),
                scopes: scopes.to_string(),
            })
            .await
            .context("Admin.IssueClientToken failed")?
            .into_inner())
    }

    /// Revoke a bearer token by label.
    pub async fn revoke_client_token(&mut self, label: &str) -> Result<RevokedClientToken> {
        Ok(self
            .admin
            .revoke_client_token(ClientTokenRef {
                label: label.to_string(),
            })
            .await
            .context("Admin.RevokeClientToken failed")?
            .into_inner())
    }
}
