//! Admin service — lifecycle + data-access RPCs.
//!
//! Backed by:
//! - `cortex::settings::SettingsResolver` for Get/Set/Unset.
//! - `cortex::db::Database` for ListSessions / GetSession.
//! - `cortex::maintenance::run_daily_maintenance` for Heal.
//! - The daemon's heartbeat-trigger channel for Poke / TriggerHeartbeat
//!   (set via `with_heartbeat_trigger_sender`).
//! - The daemon's `shutdown_tx` for Shutdown (set via `with_shutdown_sender`).

use crate::goal_bus::{now_unix, GoalEventBus};
use crate::heartbeat::{HeartbeatBus, HeartbeatSource, HeartbeatTrigger};
use crate::session::{DaemonSessionExecutor, SessionRegistry};
use crate::tunnel_supervisor::TunnelSupervisor;
use cortex::db::Database;
use cortex::process_registry::{ProcessEntry, ProcessRegistry};
use cortex::settings::SettingsResolver;
use protocols::admin::{
    admin_server::Admin, goal_mutation::Op as GoalMutationOp, ActivityLogQuery, ActivityLogText,
    AddCredentialRequest, AddProviderRequest, AmbientStatus, ApiKeyEntry, ApiKeyList, ApiKeyRef,
    ApiKeyValue, ApprovalGreetingRequest, ApprovedSender, ApprovedSenderList, ApprovedSenderRef,
    ClientTokenRef, CreateWebhookRequest, CreateWebhookResponse, CredentialEntry, CredentialList,
    DaemonStatus, DoctorSquelchClearRequest, DoctorSquelchClearResponse, DoctorSquelchSetRequest,
    DreamReport, Empty, ExportSessionRequest, ExportSessionResponse, GetScheduledTaskRunsRequest,
    GoalDetail, GoalEvent as PbGoalEvent, GoalMutation, GoalRef, HandoffClaimRequest,
    HandoffClaimResponse, HandoffFailRequest, HandoffRequest, HandoffStatusReply, HealReport,
    HeartbeatEvent as PbHeartbeatEvent, IssueClientTokenRequest, IssueClientTokenResponse,
    KeychainRef, ListCredentialsRequest, LogActivityRequest, MessageView, MonthlyTokenTotal,
    MonthlyTotalCost, OnboardingState, PairCredentialRequest, PairCredentialResponse,
    PairingApproval, PairingFilter, PairingRequest, PairingRequestList, PluginInstallRequest,
    PluginInstallResponse, ProviderCredentials, RemoveCredentialRequest, RemoveProviderRequest,
    ResetCredentialsRequest, ResetCredentialsResponse, RevokedClientToken, ScheduledTask,
    ScheduledTaskCreate, ScheduledTaskDetail, ScheduledTaskList, ScheduledTaskRef,
    ScheduledTaskRun, ScheduledTaskRunList, ScheduledTaskUpdate, SessionDetail, SessionList,
    SessionRef, SessionSummary, SetCredentialStrategyRequest, SetSubprocessPinnedRequest,
    SettingEntry, SettingKey, SettingMutation, SettingsSnapshot, Subprocess as PbSubprocess,
    SubprocessSnapshot, TriggerHeartbeatRequest, TunnelStatusReply, WarningList, WebhookRef,
};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::{Request, Response, Status as TStatus};

/// Admin service implementation.
pub struct AdminSvc {
    registry: SessionRegistry,
    heartbeat_trigger_tx: Mutex<Option<mpsc::Sender<HeartbeatTrigger>>>,
    shutdown_tx: Mutex<Option<broadcast::Sender<()>>>,
    heartbeat_bus: Mutex<Option<HeartbeatBus>>,
    executor: Mutex<Option<Arc<DaemonSessionExecutor>>>,
    config_notify_tx: Mutex<Option<mpsc::Sender<cortex::config::Config>>>,
    process_registry: Mutex<Option<ProcessRegistry>>,
    goal_bus: Mutex<Option<GoalEventBus>>,
    tunnel_supervisor: Mutex<Option<TunnelSupervisor>>,
}

impl AdminSvc {
    /// Construct a new Admin service tied to the live session registry.
    pub fn new(registry: SessionRegistry) -> Self {
        Self {
            registry,
            heartbeat_trigger_tx: Mutex::new(None),
            shutdown_tx: Mutex::new(None),
            heartbeat_bus: Mutex::new(None),
            executor: Mutex::new(None),
            config_notify_tx: Mutex::new(None),
            process_registry: Mutex::new(None),
            goal_bus: Mutex::new(None),
            tunnel_supervisor: Mutex::new(None),
        }
    }

    /// Wire the daemon-side tunnel supervisor. `TunnelStart`/`TunnelStop`/
    /// `TunnelStatus` are no-ops with `FailedPrecondition` until this is
    /// set — the operator should see "tunnel not wired" rather than a
    /// silent success.
    pub fn with_tunnel_supervisor(self, sup: TunnelSupervisor) -> Self {
        *self
            .tunnel_supervisor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sup);
        self
    }

    fn tunnel_supervisor_handle(&self) -> Option<TunnelSupervisor> {
        self.tunnel_supervisor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Wire the goal-event broadcast bus. `Admin.SubscribeGoalEvents` reads
    /// from this; the four goal RPC handlers and the agent loop publish
    /// into it via [`GoalEventBus::publish`] / [`GoalEventBus::as_sink`].
    pub fn with_goal_bus(self, bus: GoalEventBus) -> Self {
        *self
            .goal_bus
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(bus);
        self
    }

    fn goal_bus_handle(&self) -> Option<GoalEventBus> {
        self.goal_bus
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Wire the daemon-side subprocess registry. `Admin.GetSubprocesses`
    /// and `Admin.SetSubprocessPinned` rely on this; Admin.Heal also
    /// folds the registry into the maintenance sweep when present.
    pub fn with_process_registry(self, reg: ProcessRegistry) -> Self {
        *self
            .process_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reg);
        self
    }

    fn process_registry_handle(&self) -> Option<ProcessRegistry> {
        self.process_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Wire a heartbeat trigger channel. `Admin.Poke` and
    /// `Admin.TriggerHeartbeat` enqueue requests on it.
    pub fn with_heartbeat_trigger_sender(self, tx: mpsc::Sender<HeartbeatTrigger>) -> Self {
        *self
            .heartbeat_trigger_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
        self
    }

    /// Wire the daemon's shutdown broadcaster — `Admin.Shutdown` sends `()`.
    pub fn with_shutdown_sender(self, tx: broadcast::Sender<()>) -> Self {
        *self
            .shutdown_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
        self
    }

    /// Wire the heartbeat broadcast bus —
    /// `Admin.SubscribeHeartbeatEvents` reads from it.
    pub fn with_heartbeat_bus(self, bus: HeartbeatBus) -> Self {
        *self
            .heartbeat_bus
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(bus);
        self
    }

    /// Wire the session executor used by `Admin.SendApprovalGreeting`.
    pub fn with_executor(self, executor: Arc<DaemonSessionExecutor>) -> Self {
        *self
            .executor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(executor);
        self
    }

    /// Wire the config watcher notifier used after setting changes.
    pub fn with_config_notify_sender(self, tx: mpsc::Sender<cortex::config::Config>) -> Self {
        *self
            .config_notify_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
        self
    }
}

#[tonic::async_trait]
impl Admin for AdminSvc {
    async fn poke(&self, _req: Request<Empty>) -> Result<Response<Empty>, TStatus> {
        // /poke routes through the in-process trigger channel so the heartbeat
        // turn runs on the existing `DaemonSessionExecutor` (with hooks, vitals,
        // evolution wiring). The unified dispatcher (`task_type="heartbeat"`)
        // owns *scheduled* firings only.
        let tx = {
            let g = self
                .heartbeat_trigger_tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.clone()
        };
        match tx {
            Some(tx) => {
                tx.send(HeartbeatTrigger {
                    source: HeartbeatSource::Scheduled,
                    bypass_quiet_hours: true,
                })
                .await
                .map_err(|e| TStatus::internal(format!("heartbeat trigger channel closed: {e}")))?;
                Ok(Response::new(Empty {}))
            }
            None => Err(TStatus::failed_precondition(
                "no heartbeat scheduler is wired",
            )),
        }
    }

    async fn trigger_heartbeat(
        &self,
        req: Request<TriggerHeartbeatRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        let source = HeartbeatSource::from_wire(&req.source).ok_or_else(|| {
            TStatus::invalid_argument(format!(
                "invalid heartbeat source '{}'; expected 'scheduled' or 'session_start'",
                req.source
            ))
        })?;
        let tx = {
            let g = self
                .heartbeat_trigger_tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.clone()
        };
        let tx =
            tx.ok_or_else(|| TStatus::failed_precondition("no heartbeat scheduler is wired"))?;
        // TriggerHeartbeat (both Scheduled and SessionStart) routes through
        // the in-process channel for executor-backed turns. The new
        // task_type="heartbeat" dispatcher fires scheduled cadence only.
        tx.send(HeartbeatTrigger {
            source,
            bypass_quiet_hours: req.bypass_quiet_hours,
        })
        .await
        .map_err(|e| TStatus::internal(format!("heartbeat trigger channel closed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn heal(&self, _req: Request<Empty>) -> Result<Response<HealReport>, TStatus> {
        let reg = self.process_registry_handle();
        let report = async {
            let db = Database::open().await?;
            let config = cortex::config::Config::load_from_db().await?;
            cortex::maintenance::run_daily_maintenance_with_registry(&db, &config, reg.as_ref())
                .await
        }
        .await
        .map_err(|e: anyhow::Error| TStatus::internal(format!("heal failed: {e}")))?;
        Ok(Response::new(HealReport {
            pruned_logs: report.log_files_deleted as u64,
            pruned_activity: report.activity_rows_deleted as u64,
            evicted_embeddings: report.embeddings_pruned as u64,
            healed_tasks: report.stalled_tasks_healed as u64,
            warnings: report.persistent_warnings,
            log_bytes_truncated: report.log_bytes_truncated,
            messages_purged: report.messages_purged as u64,
            audit_events_tombstoned: report.audit_events_tombstoned as u64,
            wedged_runs_recovered: report.wedged_runs_recovered as u64,
            backups_pruned: 0,
            auto_backup_path: String::new(),
            skipped: report.skipped,
        }))
    }

    async fn dream(&self, _req: Request<Empty>) -> Result<Response<DreamReport>, TStatus> {
        let config = cortex::config::Config::load_from_db()
            .await
            .map_err(|e| TStatus::internal(format!("dream config load failed: {e}")))?;
        let report = cortex::memory::consolidation::run_dream(&config)
            .await
            .map_err(|e| TStatus::internal(format!("dream failed: {e}")))?;
        Ok(Response::new(DreamReport {
            entries_extracted: report.entries_extracted,
            dupes_merged: report.dupes_merged,
            tokens_used: report.tokens_used,
            nightly_completed: report.nightly_completed,
            weekly_completed: report.weekly_completed,
            warnings: report.warnings,
            nightly_summary: report.nightly_summary,
            weekly_summary: report.weekly_summary,
        }))
    }

    async fn shutdown(&self, _req: Request<Empty>) -> Result<Response<Empty>, TStatus> {
        let tx = {
            let g = self
                .shutdown_tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.clone()
        };
        match tx {
            Some(tx) => {
                // broadcast::send returns Err only when no receivers are live
                // — reporting Ok in that case would lie about the requested
                // action having been applied.
                if tx.send(()).is_err() {
                    return Err(TStatus::failed_precondition(
                        "shutdown broadcaster has no live receivers; daemon may be mid-shutdown already",
                    ));
                }
                Ok(Response::new(Empty {}))
            }
            None => Err(TStatus::failed_precondition("no shutdown sender is wired")),
        }
    }

    async fn get_settings(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<SettingsSnapshot>, TStatus> {
        let resolver = SettingsResolver::load()
            .await
            .map_err(|e| TStatus::internal(format!("settings load failed: {e}")))?;
        let entries = resolver
            .list_all()
            .await
            .map_err(|e| TStatus::internal(format!("settings list failed: {e}")))?;
        Ok(Response::new(SettingsSnapshot {
            entries: entries
                .into_iter()
                .map(|e| SettingEntry {
                    key: e.key,
                    value: e.value,
                    category: e.source.to_string(),
                })
                .collect(),
        }))
    }

    async fn set_setting(&self, req: Request<SettingMutation>) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        let resolver = SettingsResolver::load()
            .await
            .map_err(|e| TStatus::internal(format!("settings load failed: {e}")))?;
        resolver
            .set(&req.key, &req.value)
            .await
            .map_err(|e| TStatus::invalid_argument(format!("set `{}` failed: {e}", req.key)))?;
        notify_runtime_config_reload(&self.config_notify_tx).await;
        Ok(Response::new(Empty {}))
    }

    async fn unset_setting(&self, req: Request<SettingKey>) -> Result<Response<Empty>, TStatus> {
        let key = req.into_inner().key;
        let resolver = SettingsResolver::load()
            .await
            .map_err(|e| TStatus::internal(format!("settings load failed: {e}")))?;
        resolver
            .unset(&key)
            .await
            .map_err(|e| TStatus::invalid_argument(format!("unset `{key}` failed: {e}")))?;
        notify_runtime_config_reload(&self.config_notify_tx).await;
        Ok(Response::new(Empty {}))
    }

    async fn list_sessions(&self, _req: Request<Empty>) -> Result<Response<SessionList>, TStatus> {
        // Combine in-memory live sessions with the most recent persisted ones.
        let live: std::collections::HashSet<String> =
            self.registry.live_ids().into_iter().collect();
        let sessions = async {
            let db = Database::open().await?;
            let rows = db.list_sessions(100).await?;
            let mut sessions = Vec::with_capacity(rows.len());
            for r in rows {
                let count = db.count_session_messages(&r.id).await.unwrap_or(0);
                let mut title = r.title.clone();
                if live.contains(&r.id) && !title.contains("(live)") {
                    title = format!("{title} (live)");
                }
                sessions.push(SessionSummary {
                    id: r.id,
                    title,
                    message_count: count as u64,
                    updated_unix_ts: r.updated_at as u64,
                });
            }
            anyhow::Ok(sessions)
        }
        .await
        .map_err(|e| TStatus::internal(format!("list_sessions failed: {e}")))?;
        Ok(Response::new(SessionList { sessions }))
    }

    async fn get_session(
        &self,
        req: Request<SessionRef>,
    ) -> Result<Response<SessionDetail>, TStatus> {
        let id = req.into_inner().id;
        let detail = async {
            let db = Database::open().await?;
            let Some(row) = db.session_by_id(&id).await? else {
                return anyhow::Ok(None);
            };
            let messages = db.load_session_messages(&row.id).await?;
            Ok(Some(SessionDetail {
                id: row.id,
                title: row.title,
                messages: messages
                    .into_iter()
                    .map(|m| MessageView {
                        role: m.role,
                        text: m.content.unwrap_or_default(),
                        unix_ts: m.created_at as u64,
                    })
                    .collect(),
            }))
        }
        .await
        .map_err(|e| TStatus::internal(format!("get_session failed: {e}")))?
        .ok_or_else(|| TStatus::not_found("session not found".to_string()))?;
        Ok(Response::new(detail))
    }

    // ── Scheduled tasks ───────────────────────────────────────────────────

    async fn list_scheduled_tasks(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<ScheduledTaskList>, TStatus> {
        let rows = async {
            let db = Database::open().await?;
            db.list_tasks().await
        }
        .await
        .map_err(|e: anyhow::Error| TStatus::internal(format!("list_tasks failed: {e}")))?;
        Ok(Response::new(ScheduledTaskList {
            tasks: rows.into_iter().map(scheduled_task_row_to_proto).collect(),
        }))
    }

    async fn create_scheduled_task(
        &self,
        req: Request<ScheduledTaskCreate>,
    ) -> Result<Response<ScheduledTask>, TStatus> {
        let req = req.into_inner();
        let id = uuid::Uuid::new_v4().to_string();
        let id_for_lookup = id.clone();
        let inserted = async {
            use cortex::db::NewTask;
            let db = Database::open().await?;
            let delivery_channel =
                (!req.delivery_channel.is_empty()).then_some(req.delivery_channel.as_str());
            let delivery_target =
                (!req.delivery_target.is_empty()).then_some(req.delivery_target.as_str());
            let task_type = if req.task_type.is_empty() {
                "prompt"
            } else {
                req.task_type.as_str()
            };
            let allowed_tools =
                (!req.allowed_tools.is_empty()).then_some(req.allowed_tools.as_str());
            let new_task = NewTask {
                id: &id,
                name: &req.name,
                prompt: &req.prompt,
                schedule_type: &req.schedule_type,
                schedule_expr: &req.schedule_expr,
                timezone: &req.timezone,
                // 0 = unset; positive = explicit unix-ts.
                next_run: if req.next_run_unix > 0 {
                    Some(req.next_run_unix)
                } else {
                    None
                },
                // <0 sentinel = leave to DB default; >=0 = explicit.
                max_retries: if req.max_retries >= 0 {
                    Some(req.max_retries)
                } else {
                    None
                },
                timeout_ms: if req.timeout_ms > 0 {
                    Some(req.timeout_ms)
                } else {
                    None
                },
                delivery_channel,
                delivery_target,
                allowed_tools,
                task_type,
                pre_run_command: None,
                pre_run_cwd: None,
                pre_run_timeout_ms: None,
                min_interval_secs: None,
            };
            db.create_task(&new_task).await?;
            anyhow::Ok(db.list_tasks().await?.into_iter().find(|t| t.id == id))
        }
        .await
        .map_err(|e| TStatus::invalid_argument(format!("create_task failed: {e}")))?
        .ok_or_else(|| {
            TStatus::internal(format!(
                "create_task: row `{id_for_lookup}` not found after insert"
            ))
        })?;
        Ok(Response::new(scheduled_task_row_to_proto(inserted)))
    }

    async fn delete_scheduled_task(
        &self,
        req: Request<ScheduledTaskRef>,
    ) -> Result<Response<Empty>, TStatus> {
        let id = req.into_inner().id;
        let removed = async {
            let db = Database::open().await?;
            db.delete_task(&id).await
        }
        .await
        .map_err(|e: anyhow::Error| TStatus::internal(format!("delete_task failed: {e}")))?;
        if !removed {
            return Err(TStatus::not_found("scheduled task not found"));
        }
        Ok(Response::new(Empty {}))
    }

    async fn update_scheduled_task(
        &self,
        req: Request<ScheduledTaskUpdate>,
    ) -> Result<Response<ScheduledTask>, TStatus> {
        let req = req.into_inner();
        let id_for_lookup = req.id.clone();
        let updated = async {
            use cortex::db::UpdateTask;
            let db = Database::open().await?;
            // Empty string sentinel = "leave unchanged", matching the
            // ScheduledTaskUpdate proto semantics + Option<&str> on UpdateTask.
            let update = UpdateTask {
                name: (!req.name.is_empty()).then_some(req.name.as_str()),
                prompt: (!req.prompt.is_empty()).then_some(req.prompt.as_str()),
                schedule_type: (!req.schedule_type.is_empty())
                    .then_some(req.schedule_type.as_str()),
                schedule_expr: (!req.schedule_expr.is_empty())
                    .then_some(req.schedule_expr.as_str()),
                timezone: (!req.timezone.is_empty()).then_some(req.timezone.as_str()),
            };
            let found = db.update_task(&req.id, &update).await?;
            if !found {
                return anyhow::Ok(None);
            }
            // Re-read the row so the caller observes the persisted state, not
            // a locally-merged copy that could drift if a column gets added.
            Ok(db.list_tasks().await?.into_iter().find(|t| t.id == req.id))
        }
        .await
        .map_err(|e| TStatus::invalid_argument(format!("update_task failed: {e}")))?
        .ok_or_else(|| TStatus::not_found(format!("scheduled task `{id_for_lookup}` not found")))?;
        Ok(Response::new(scheduled_task_row_to_proto(updated)))
    }

    async fn pause_scheduled_task(
        &self,
        req: Request<ScheduledTaskRef>,
    ) -> Result<Response<Empty>, TStatus> {
        update_task_status(req.into_inner().id, "paused").await
    }

    async fn resume_scheduled_task(
        &self,
        req: Request<ScheduledTaskRef>,
    ) -> Result<Response<Empty>, TStatus> {
        update_task_status(req.into_inner().id, "active").await
    }

    async fn run_scheduled_task_now(
        &self,
        req: Request<ScheduledTaskRef>,
    ) -> Result<Response<Empty>, TStatus> {
        let id = req.into_inner().id;
        let id_for_err = id.clone();
        let found = async {
            let db = Database::open().await?;
            if db.get_task_by_id(&id).await?.is_none() {
                return anyhow::Ok(false);
            }
            let now = chrono::Utc::now().timestamp();
            db.update_task_next_run(&id, Some(now)).await?;
            db.clear_task_retry(&id).await?;
            Ok(true)
        }
        .await
        .map_err(|e| TStatus::internal(format!("run_scheduled_task_now failed: {e}")))?;
        if !found {
            return Err(TStatus::not_found(format!(
                "scheduled task `{id_for_err}` not found"
            )));
        }
        Ok(Response::new(Empty {}))
    }

    async fn get_scheduled_task_runs(
        &self,
        req: Request<GetScheduledTaskRunsRequest>,
    ) -> Result<Response<ScheduledTaskRunList>, TStatus> {
        let req = req.into_inner();
        let id = req.id;
        // 0 = caller didn't pin a limit; pick a sane default.
        let limit = if req.limit == 0 {
            10
        } else {
            req.limit as usize
        };
        let runs = async {
            let db = Database::open().await?;
            db.task_run_history(&id, limit).await
        }
        .await
        .map_err(|e: anyhow::Error| {
            TStatus::internal(format!("get_scheduled_task_runs failed: {e}"))
        })?;
        Ok(Response::new(ScheduledTaskRunList {
            runs: runs.into_iter().map(task_run_row_to_proto).collect(),
        }))
    }

    async fn get_scheduled_task_detail(
        &self,
        req: Request<ScheduledTaskRef>,
    ) -> Result<Response<ScheduledTaskDetail>, TStatus> {
        let id = req.into_inner().id;
        let id_for_err = id.clone();
        let detail = async {
            let db = Database::open().await?;
            let Some(row) = db.get_task_by_id(&id).await? else {
                return anyhow::Ok(None);
            };
            let last = db.last_task_run(&id).await?;
            let proto_task = scheduled_task_row_to_proto(row.clone());
            Ok(Some(ScheduledTaskDetail {
                task: Some(proto_task),
                max_retries: row.max_retries,
                retry_count: row.retry_count,
                retry_after_unix: row.retry_after.unwrap_or(0),
                last_error: row.last_error.unwrap_or_default(),
                timeout_ms: row.timeout_ms,
                has_last_run: last.is_some(),
                last_run: last.map(task_run_row_to_proto),
            }))
        }
        .await
        .map_err(|e| TStatus::internal(format!("get_scheduled_task_detail failed: {e}")))?;
        let detail = detail.ok_or_else(|| {
            TStatus::not_found(format!("scheduled task `{id_for_err}` not found"))
        })?;
        Ok(Response::new(detail))
    }

    async fn export_session(
        &self,
        req: Request<ExportSessionRequest>,
    ) -> Result<Response<ExportSessionResponse>, TStatus> {
        let req = req.into_inner();
        if req.session_id.is_empty() {
            return Err(TStatus::invalid_argument("session_id required"));
        }
        let format_str = if req.format.is_empty() {
            "json".to_string()
        } else {
            req.format
        };
        let session_id = req.session_id;
        let (rendered, suggested) = async {
            use cortex::export::{export_session, ExportFormat, ExportOptions};
            use std::str::FromStr;
            let format = ExportFormat::from_str(&format_str)?;
            let db = Database::open().await?;
            export_session(&db, &session_id, ExportOptions { format }).await
        }
        .await
        .map_err(|e: anyhow::Error| {
            TStatus::invalid_argument(format!("export_session failed: {e}"))
        })?;
        Ok(Response::new(ExportSessionResponse {
            content: rendered.into_bytes(),
            suggested_filename: suggested,
        }))
    }

    // ── Pairing ───────────────────────────────────────────────────────────

    async fn list_pairing_requests(
        &self,
        req: Request<PairingFilter>,
    ) -> Result<Response<PairingRequestList>, TStatus> {
        let filter = req.into_inner().channel_name;
        let rows = async {
            let db = Database::open().await?;
            let f = (!filter.is_empty()).then_some(filter.as_str());
            db.list_pairings(f).await
        }
        .await
        .map_err(|e: anyhow::Error| TStatus::internal(format!("list_pairings failed: {e}")))?;
        Ok(Response::new(PairingRequestList {
            requests: rows.into_iter().map(pairing_row_to_proto).collect(),
        }))
    }

    async fn approve_pairing(
        &self,
        req: Request<PairingApproval>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        async {
            let db = Database::open().await?;
            db.approve_pairing(&req.channel_name, &req.code)
                .await
                .map(|_| ())
        }
        .await
        .map_err(|e: anyhow::Error| {
            TStatus::failed_precondition(format!("approve_pairing failed: {e}"))
        })?;
        Ok(Response::new(Empty {}))
    }

    async fn list_approved_senders(
        &self,
        req: Request<PairingFilter>,
    ) -> Result<Response<ApprovedSenderList>, TStatus> {
        let filter = req.into_inner().channel_name;
        let rows = async {
            let db = Database::open().await?;
            let f = (!filter.is_empty()).then_some(filter.as_str());
            db.list_approved_senders(f).await
        }
        .await
        .map_err(|e: anyhow::Error| {
            TStatus::internal(format!("list_approved_senders failed: {e}"))
        })?;
        Ok(Response::new(ApprovedSenderList {
            senders: rows.into_iter().map(approved_sender_row_to_proto).collect(),
        }))
    }

    async fn revoke_approved_sender(
        &self,
        req: Request<ApprovedSenderRef>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        let removed = async {
            let db = Database::open().await?;
            db.revoke_sender(&req.channel_name, &req.sender_id).await
        }
        .await
        .map_err(|e: anyhow::Error| TStatus::internal(format!("revoke_sender failed: {e}")))?;
        if !removed {
            return Err(TStatus::not_found("approved sender not found"));
        }
        Ok(Response::new(Empty {}))
    }

    // ── Daemon status ─────────────────────────────────────────────────────

    async fn get_daemon_status(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<DaemonStatus>, TStatus> {
        let live_count = self.registry.live_ids().len() as u64;
        let snapshot = async {
            let db = Database::open().await?;
            let cfg = cortex::config::Config::load_from_db().await?;
            let tasks = db.list_tasks().await?;
            let pairings = db.list_pairings(None).await?;
            let active = tasks.iter().filter(|t| t.status == "active").count() as u64;
            let paused = tasks.iter().filter(|t| t.status == "paused").count() as u64;
            let pending = pairings.iter().filter(|p| p.status == "pending").count() as u64;
            anyhow::Ok((cfg, active, paused, pending))
        }
        .await
        .map_err(|e| TStatus::internal(format!("daemon_status failed: {e}")))?;
        let (cfg, active, paused, pending) = snapshot;
        Ok(Response::new(DaemonStatus {
            version: env!("CARGO_PKG_VERSION").into(),
            uptime_secs: 0,
            live_session_count: live_count,
            llm_provider: cfg.llm.provider.unwrap_or_default(),
            llm_model: cfg.llm.model,
            active_tasks: active,
            paused_tasks: paused,
            pending_pairings: pending,
        }))
    }

    async fn get_ambient_status(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<AmbientStatus>, TStatus> {
        let snap = load_ambient_status()
            .await
            .map_err(|e| TStatus::internal(format!("ambient_status failed: {e}")))?;
        Ok(Response::new(snap))
    }

    async fn get_persistent_warnings(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<WarningList>, TStatus> {
        let warnings = async {
            let db = Database::open().await?;
            anyhow::Ok(
                db.latest_doctor_run()
                    .await?
                    .map(|r| r.persistent_warnings)
                    .unwrap_or_default(),
            )
        }
        .await
        .map_err(|e| TStatus::internal(format!("persistent_warnings failed: {e}")))?;
        Ok(Response::new(WarningList { warnings }))
    }

    async fn send_approval_greeting(
        &self,
        req: Request<ApprovalGreetingRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        if req.channel.is_empty() || req.sender_id.is_empty() {
            return Err(TStatus::invalid_argument(
                "channel and sender_id are required",
            ));
        }
        let executor = {
            let g = self
                .executor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.clone()
        };
        let executor = executor
            .ok_or_else(|| TStatus::failed_precondition("approval greeting executor not wired"))?;
        // Fire-and-forget so the caller doesn't block on an LLM turn.
        let channel = req.channel;
        let sender_id = req.sender_id;
        tokio::spawn(async move {
            let config = match cortex::config::Config::load_from_db().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("approval greeting: load_from_db failed: {e}");
                    return;
                }
            };
            crate::approval_greeting::send(&executor, &config, &channel, &sender_id).await;
        });
        Ok(Response::new(Empty {}))
    }

    type SubscribeHeartbeatEventsStream =
        Pin<Box<dyn Stream<Item = Result<PbHeartbeatEvent, TStatus>> + Send + 'static>>;

    async fn subscribe_heartbeat_events(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<Self::SubscribeHeartbeatEventsStream>, TStatus> {
        let bus = {
            let g = self
                .heartbeat_bus
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.clone()
        };
        let bus = bus.ok_or_else(|| TStatus::failed_precondition("heartbeat bus not wired"))?;
        let mut sub = bus.subscribe();
        let (tx, rx) = mpsc::channel::<Result<PbHeartbeatEvent, TStatus>>(16);
        tokio::spawn(async move {
            loop {
                match sub.recv().await {
                    Ok(evt) => {
                        let pb = PbHeartbeatEvent {
                            source: evt.source.as_wire().to_string(),
                            started_at_unix: evt.started_at_unix,
                            finished_at_unix: evt.finished_at_unix,
                            status: evt.status.to_string(),
                            message: evt.message,
                            duration_ms: evt.duration_ms,
                            skip_reason: evt.skip_reason.unwrap_or_default().to_string(),
                        };
                        if tx.send(Ok(pb)).await.is_err() {
                            return; // client gone
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("heartbeat stream subscriber lagged by {n}; continuing");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        let stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn log_activity(
        &self,
        req: Request<LogActivityRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        let level = req.level;
        let source = req.source;
        let message = req.message;
        async {
            let db = Database::open().await?;
            cortex::activity_log::log_activity(&db, &level, &source, &message).await;
            anyhow::Ok(())
        }
        .await
        .map_err(|e| TStatus::internal(format!("log_activity failed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn get_activity_log_text(
        &self,
        req: Request<ActivityLogQuery>,
    ) -> Result<Response<ActivityLogText>, TStatus> {
        let req = req.into_inner();
        // Server-side cap so a buggy client can't pull the entire log into RAM.
        let count = req.count.min(200) as usize;
        let count = if count == 0 { 50 } else { count };
        let min_level = if req.min_level.is_empty() {
            None
        } else {
            Some(req.min_level)
        };
        let category = if req.category.is_empty() {
            None
        } else {
            Some(req.category)
        };
        let text = async {
            let db = Database::open().await?;
            let entries = db
                .query_activity(count, min_level.as_deref(), category.as_deref())
                .await?;
            if entries.is_empty() {
                return anyhow::Ok("No activity log entries.".to_string());
            }
            Ok(entries
                .iter()
                .rev()
                .map(cortex::activity_log::format_activity_entry)
                .collect::<Vec<_>>()
                .join("\n"))
        }
        .await
        .map_err(|e| TStatus::internal(format!("get_activity_log_text failed: {e}")))?;
        Ok(Response::new(ActivityLogText { text }))
    }

    async fn plugin_install(
        &self,
        req: Request<PluginInstallRequest>,
    ) -> Result<Response<PluginInstallResponse>, TStatus> {
        let req = req.into_inner();
        if req.plugin_id.is_empty() {
            return Err(TStatus::invalid_argument("plugin_id required"));
        }
        let recorded = async {
            let db = Database::open().await?;
            db.insert_plugin(&req.plugin_id, &req.plugin_name, &req.kind, &req.category)
                .await?;
            let mut count = 0u32;
            for fh in &req.file_hashes {
                if let Err(e) = db
                    .insert_file_hash(&req.plugin_id, &fh.path, &fh.hash)
                    .await
                {
                    tracing::warn!("plugin_install: insert_file_hash {}: {e}", fh.path);
                } else {
                    count += 1;
                }
            }
            if let Some(t) = &req.installed_tool {
                if !t.name.is_empty() {
                    db.insert_installed_tool(&t.name, &t.description, &t.runtime, &t.plugin_id)
                        .await?;
                }
            }
            if let Some(c) = &req.installed_channel {
                if !c.name.is_empty() {
                    db.insert_installed_channel(
                        &c.name,
                        &c.description,
                        &c.runtime,
                        &c.plugin_id,
                        &c.webhook,
                    )
                    .await?;
                }
            }
            if !req.credentials_json.is_empty() {
                db.set_setting("credentials", &req.credentials_json).await?;
            }
            anyhow::Ok(count)
        }
        .await
        .map_err(|e| TStatus::internal(format!("plugin_install failed: {e}")))?;
        Ok(Response::new(PluginInstallResponse {
            file_hashes_recorded: recorded,
        }))
    }

    // ── Onboarding ────────────────────────────────────────────────────────
    //
    // Single sentinel: settings-table key `onboarded` set to "true". The cli
    // used to write this directly via `Database::open()`; routing through
    // the daemon ensures one writer (matches CLAUDE.md's no-multi-writer rule)
    // and lets the daemon's settings cache stay in sync after onboarding.

    async fn get_onboarding_state(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<OnboardingState>, TStatus> {
        let onboarded = async {
            let db = Database::open().await?;
            anyhow::Ok(matches!(
                db.get_setting("onboarded").await?.as_deref(),
                Some("true")
            ))
        }
        .await
        .map_err(|e| TStatus::internal(format!("get_onboarding_state failed: {e}")))?;
        Ok(Response::new(OnboardingState { onboarded }))
    }

    async fn set_onboarding_complete(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<Empty>, TStatus> {
        async {
            let db = Database::open().await?;
            db.set_setting("onboarded", "true").await?;
            anyhow::Ok(())
        }
        .await
        .map_err(|e| TStatus::internal(format!("set_onboarding_complete failed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    // ── API key routing ───────────────────────────────────────────────────
    //
    // Stored in `settings` under `credentials.{provider}` as a JSON blob
    // matching one of:
    //   {"type":"literal","value":"<key>"}
    //   {"type":"keychain","service":"<svc>","account":"<acct>"}
    //
    // The cli does keychain I/O; the daemon only persists/returns the
    // routing metadata.

    async fn get_api_key(&self, req: Request<ApiKeyRef>) -> Result<Response<ApiKeyValue>, TStatus> {
        let provider = req.into_inner().provider;
        if provider.is_empty() {
            return Err(TStatus::invalid_argument("provider required"));
        }
        let provider_for_lookup = provider.clone();
        let body = async {
            let db = Database::open().await?;
            let key = format!("credentials.{provider_for_lookup}");
            let raw = db.get_setting(&key).await?;
            match raw {
                Some(s) if !s.is_empty() => anyhow::Ok(Some(serde_json::from_str(&s)?)),
                _ => Ok(None),
            }
        }
        .await
        .map_err(|e| TStatus::internal(format!("get_api_key failed: {e}")))?;

        let value = match body {
            None => ApiKeyValue {
                provider,
                body: Some(protocols::admin::api_key_value::Body::Unset(Empty {})),
            },
            Some(ApiKeyRouting::Literal { value }) => ApiKeyValue {
                provider,
                body: Some(protocols::admin::api_key_value::Body::Literal(value)),
            },
            Some(ApiKeyRouting::Keychain { service, account }) => ApiKeyValue {
                provider,
                body: Some(protocols::admin::api_key_value::Body::Keychain(
                    KeychainRef { service, account },
                )),
            },
        };
        Ok(Response::new(value))
    }

    async fn set_api_key(&self, req: Request<ApiKeyValue>) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        if req.provider.is_empty() {
            return Err(TStatus::invalid_argument("provider required"));
        }
        let routing = match req.body {
            Some(protocols::admin::api_key_value::Body::Literal(v)) => {
                if v.is_empty() {
                    return Err(TStatus::invalid_argument("literal value must be non-empty"));
                }
                ApiKeyRouting::Literal { value: v }
            }
            Some(protocols::admin::api_key_value::Body::Keychain(kr)) => {
                if kr.service.is_empty() || kr.account.is_empty() {
                    return Err(TStatus::invalid_argument(
                        "keychain ref requires service and account",
                    ));
                }
                ApiKeyRouting::Keychain {
                    service: kr.service,
                    account: kr.account,
                }
            }
            Some(protocols::admin::api_key_value::Body::Unset(_)) | None => {
                return Err(TStatus::invalid_argument(
                    "set_api_key requires literal or keychain body; use unset_api_key to clear",
                ));
            }
        };
        let provider = req.provider;
        async {
            let db = Database::open().await?;
            let key = format!("credentials.{provider}");
            let json = serde_json::to_string(&routing)?;
            db.set_setting(&key, &json).await?;
            anyhow::Ok(())
        }
        .await
        .map_err(|e| TStatus::internal(format!("set_api_key failed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn unset_api_key(&self, req: Request<ApiKeyRef>) -> Result<Response<Empty>, TStatus> {
        let provider = req.into_inner().provider;
        if provider.is_empty() {
            return Err(TStatus::invalid_argument("provider required"));
        }
        async {
            let db = Database::open().await?;
            let key = format!("credentials.{provider}");
            db.delete_setting(&key).await?;
            anyhow::Ok(())
        }
        .await
        .map_err(|e| TStatus::internal(format!("unset_api_key failed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn list_api_keys(&self, _req: Request<Empty>) -> Result<Response<ApiKeyList>, TStatus> {
        let entries = async {
            let db = Database::open().await?;
            let rows = db.list_settings().await?;
            let mut out = Vec::new();
            for (key, value, _) in rows {
                if !key.starts_with("credentials.") {
                    continue;
                }
                let provider = match key.strip_prefix("credentials.") {
                    Some(p) if !p.is_empty() => p.to_string(),
                    _ => continue,
                };
                let backend = match serde_json::from_str::<ApiKeyRouting>(&value) {
                    Ok(ApiKeyRouting::Literal { .. }) => "literal",
                    Ok(ApiKeyRouting::Keychain { .. }) => "keychain",
                    Err(e) => {
                        tracing::warn!(
                            "list_api_keys: unparseable credentials.{provider} JSON: {e}"
                        );
                        "unset"
                    }
                };
                out.push(ApiKeyEntry {
                    provider,
                    backend: backend.to_string(),
                });
            }
            anyhow::Ok(out)
        }
        .await
        .map_err(|e| TStatus::internal(format!("list_api_keys failed: {e}")))?;
        Ok(Response::new(ApiKeyList { entries }))
    }

    async fn get_monthly_token_total(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<MonthlyTokenTotal>, TStatus> {
        let total = async {
            let db = Database::open().await?;
            db.monthly_token_total().await
        }
        .await
        .map_err(|e: anyhow::Error| {
            TStatus::internal(format!("monthly_token_total failed: {e}"))
        })?;
        Ok(Response::new(MonthlyTokenTotal { total }))
    }

    async fn get_monthly_total_cost(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<MonthlyTotalCost>, TStatus> {
        let cost = async {
            let db = Database::open().await?;
            db.monthly_total_cost().await
        }
        .await
        .map_err(|e: anyhow::Error| TStatus::internal(format!("monthly_total_cost failed: {e}")))?;
        let (cost_val, present) = match cost {
            Some(c) => (c, true),
            None => (0.0, false),
        };
        Ok(Response::new(MonthlyTotalCost {
            cost: cost_val,
            present,
        }))
    }

    // ── Provider CRUD (writer-only) ──────────────────────────────────────

    async fn add_provider(
        &self,
        req: Request<AddProviderRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        if req.id.is_empty() || req.id.contains(char::is_whitespace) {
            return Err(TStatus::invalid_argument(
                "id must be non-empty and contain no whitespace",
            ));
        }
        let category = match req.category.to_ascii_lowercase().as_str() {
            "byo" | "byo_endpoint" => cortex::providers::ProviderCategory::BYOEndpoint,
            "local" => cortex::providers::ProviderCategory::Local,
            "public" | "public_cloud" => cortex::providers::ProviderCategory::PublicCloud,
            other => {
                return Err(TStatus::invalid_argument(format!(
                    "unknown category '{other}'; expected byo, local, or public_cloud"
                )));
            }
        };
        let id_for_err = req.id.clone();
        async {
            let db = Database::open().await?;
            let display_name = if req.display_name.is_empty() {
                req.id.clone()
            } else {
                req.display_name
            };
            let api_key_env = if req.api_key_env.is_empty() {
                None
            } else {
                Some(req.api_key_env)
            };
            let row = cortex::providers::ProviderRow {
                id: req.id,
                category,
                display_name,
                endpoint_url: req.base_url,
                api_key_env,
                enabled: true,
            };
            row.insert(&db).await?;
            anyhow::Ok(())
        }
        .await
        .map_err(|e| TStatus::internal(format!("add_provider '{id_for_err}' failed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn remove_provider(
        &self,
        req: Request<RemoveProviderRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        let id = req.into_inner().id;
        if id.is_empty() {
            return Err(TStatus::invalid_argument("id must be non-empty"));
        }
        let id_for_err = id.clone();
        async {
            let db = Database::open().await?;
            // Idempotent — `false` (already absent) is a successful no-op.
            cortex::providers::ProviderRow::delete(&db, &id).await?;
            anyhow::Ok(())
        }
        .await
        .map_err(|e| TStatus::internal(format!("remove_provider '{id_for_err}' failed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    // ── Webhook CRUD (writer-only) ───────────────────────────────────────

    async fn create_webhook(
        &self,
        req: Request<CreateWebhookRequest>,
    ) -> Result<Response<CreateWebhookResponse>, TStatus> {
        let req = req.into_inner();
        let name = req.name.clone();
        let created = async {
            let db = Database::open().await?;
            let thread_path = if req.thread_path.is_empty() {
                None
            } else {
                Some(req.thread_path.as_str())
            };
            gateway::generic_webhook::definitions::create(
                &db,
                &req.name,
                &req.sender_path,
                thread_path,
                &req.body_path,
            )
            .await
        }
        .await
        .map_err(|e: anyhow::Error| {
            TStatus::internal(format!("create_webhook '{name}' failed: {e}"))
        })?;
        Ok(Response::new(CreateWebhookResponse {
            name: created.row.name,
            hmac_secret: created.row.secret_hash,
        }))
    }

    async fn revoke_webhook(&self, req: Request<WebhookRef>) -> Result<Response<Empty>, TStatus> {
        let name = req.into_inner().name;
        async {
            let db = Database::open().await?;
            gateway::generic_webhook::definitions::revoke(&db, &name).await
        }
        .await
        .map_err(|e: anyhow::Error| TStatus::internal(format!("revoke_webhook failed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    // ── Client tokens (writer-only) ──────────────────────────────────────

    async fn issue_client_token(
        &self,
        req: Request<IssueClientTokenRequest>,
    ) -> Result<Response<IssueClientTokenResponse>, TStatus> {
        let req = req.into_inner();
        let label = req.label.clone();
        let issued = async {
            let db = Database::open().await?;
            cortex::auth::tokens::issue(&db, &req.label, &req.scopes).await
        }
        .await
        .map_err(|e: anyhow::Error| {
            TStatus::internal(format!("issue_client_token '{label}' failed: {e}"))
        })?;
        Ok(Response::new(IssueClientTokenResponse {
            id: issued.row.id,
            label: issued.row.label,
            plaintext: issued.plaintext,
        }))
    }

    async fn revoke_client_token(
        &self,
        req: Request<ClientTokenRef>,
    ) -> Result<Response<RevokedClientToken>, TStatus> {
        let label = req.into_inner().label;
        let label_for_err = label.clone();
        let row = async {
            let db = Database::open().await?;
            cortex::auth::tokens::revoke_by_label(&db, &label).await
        }
        .await
        .map_err(|e: anyhow::Error| {
            TStatus::internal(format!("revoke_client_token '{label_for_err}' failed: {e}"))
        })?;
        Ok(Response::new(RevokedClientToken {
            id: row.id,
            label: row.label,
            revoked_at: row.revoked_at.unwrap_or(0) as u64,
        }))
    }

    async fn doctor_squelch_set(
        &self,
        req: Request<DoctorSquelchSetRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        if req.category.is_empty() || req.name.is_empty() {
            return Err(TStatus::invalid_argument(
                "category and name must be non-empty",
            ));
        }
        let now = chrono::Utc::now().timestamp();
        if req.expires_at_unix <= now {
            return Err(TStatus::invalid_argument(
                "expires_at_unix must be in the future",
            ));
        }
        async {
            let db = Database::open().await?;
            let reason = (!req.reason.is_empty()).then_some(req.reason.as_str());
            db.doctor_squelch_set(&req.category, &req.name, now, req.expires_at_unix, reason)
                .await
        }
        .await
        .map_err(|e: anyhow::Error| TStatus::internal(format!("doctor_squelch_set failed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn doctor_squelch_clear(
        &self,
        req: Request<DoctorSquelchClearRequest>,
    ) -> Result<Response<DoctorSquelchClearResponse>, TStatus> {
        let req = req.into_inner();
        if req.category.is_empty() || req.name.is_empty() {
            return Err(TStatus::invalid_argument(
                "category and name must be non-empty",
            ));
        }
        let removed = async {
            let db = Database::open().await?;
            db.doctor_squelch_clear(&req.category, &req.name).await
        }
        .await
        .map_err(|e: anyhow::Error| {
            TStatus::internal(format!("doctor_squelch_clear failed: {e}"))
        })?;
        Ok(Response::new(DoctorSquelchClearResponse { removed }))
    }

    // ── Credential pool ─────────────────────────────────────────────────
    //
    // The pool's persisted state is the source of truth: each mutation
    // writes the SQLite row; running `LlmClient`s see the change on their
    // next pool reload (config-notify path) or restart.
    //
    // Today these RPCs operate on the **primary** provider only — the
    // configured `llm.provider` plus its `llm.api_keys`. Fallback-provider
    // admin will land alongside per-slot strategy/cooldown settings.
    async fn list_credentials(
        &self,
        req: Request<ListCredentialsRequest>,
    ) -> Result<Response<CredentialList>, TStatus> {
        let req = req.into_inner();
        let providers = async {
            let db = Database::open().await?;
            let config = cortex::config::Config::load_from_db().await?;
            let pool = build_primary_pool(&config, &db).await?;
            if !req.provider.is_empty() && !pool_provider_matches(&pool, &req.provider) {
                return anyhow::Ok(vec![]);
            }
            Ok(vec![credential_pool_to_proto(&config, &pool)])
        }
        .await
        .map_err(|e| TStatus::internal(format!("list_credentials failed: {e}")))?;
        Ok(Response::new(CredentialList { providers }))
    }

    async fn add_credential(
        &self,
        req: Request<AddCredentialRequest>,
    ) -> Result<Response<CredentialEntry>, TStatus> {
        let req = req.into_inner();
        let key = req.key.trim().to_string();
        if key.is_empty() {
            return Err(TStatus::invalid_argument("key must not be empty"));
        }
        let provider = req.provider;
        let label_in = req.label;
        let entry = async {
            let db = Database::open().await?;
            let config = cortex::config::Config::load_from_db().await?;
            let pool = build_primary_pool(&config, &db).await?;
            require_provider_match(&pool, &provider)?;
            let label = if label_in.trim().is_empty() {
                // Count live entries only — tombstones shouldn't bump the
                // default label counter ("manual-7" after one add post-remove
                // looks like a bug to operators).
                let live = pool
                    .entries()
                    .iter()
                    .filter(|c| c.removed_at_ms.is_none())
                    .count();
                format!("manual-{}", live + 1)
            } else {
                label_in.trim().to_string()
            };
            let cred = pool.add_entry(
                key,
                label,
                cortex::llm::credential_pool::CredentialSource::Manual,
            );
            anyhow::Ok(pooled_to_proto(pool.provider_str(), &cred))
        }
        .await
        .map_err(|e| TStatus::invalid_argument(format!("add_credential: {e}")))?;
        notify_runtime_config_reload(&self.config_notify_tx).await;
        Ok(Response::new(entry))
    }

    async fn remove_credential(
        &self,
        req: Request<RemoveCredentialRequest>,
    ) -> Result<Response<CredentialEntry>, TStatus> {
        let req = req.into_inner();
        if req.target.trim().is_empty() {
            return Err(TStatus::invalid_argument("target must not be empty"));
        }
        let entry = async {
            let db = Database::open().await?;
            let config = cortex::config::Config::load_from_db().await?;
            let pool = build_primary_pool(&config, &db).await?;
            require_provider_match(&pool, &req.provider)?;
            let (idx_zero, _existing) = pool
                .resolve_target(&req.target)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let removed = pool
                .remove_index(idx_zero + 1)
                .ok_or_else(|| anyhow::anyhow!("index out of range after resolve"))?;
            anyhow::Ok(pooled_to_proto(pool.provider_str(), &removed))
        }
        .await
        .map_err(|e| TStatus::invalid_argument(format!("remove_credential: {e}")))?;
        notify_runtime_config_reload(&self.config_notify_tx).await;
        Ok(Response::new(entry))
    }

    async fn reset_credentials(
        &self,
        req: Request<ResetCredentialsRequest>,
    ) -> Result<Response<ResetCredentialsResponse>, TStatus> {
        let req = req.into_inner();
        let cleared = async {
            let db = Database::open().await?;
            let config = cortex::config::Config::load_from_db().await?;
            let pool = build_primary_pool(&config, &db).await?;
            require_provider_match(&pool, &req.provider)?;
            anyhow::Ok(pool.reset_cooldowns() as u64)
        }
        .await
        .map_err(|e| TStatus::invalid_argument(format!("reset_credentials: {e}")))?;
        notify_runtime_config_reload(&self.config_notify_tx).await;
        Ok(Response::new(ResetCredentialsResponse { cleared }))
    }

    async fn pair_credential(
        &self,
        req: Request<PairCredentialRequest>,
    ) -> Result<Response<PairCredentialResponse>, TStatus> {
        let raw_kind = req.into_inner().kind.trim().to_ascii_lowercase();
        if raw_kind.is_empty() {
            return Err(TStatus::invalid_argument("kind must not be empty"));
        }
        // Normalize hyphenated forms (`claude-code`) to the discoverer's
        // canonical `name()` (`claude_code`) — the registry keys on underscores.
        let kind = raw_kind.replace('-', "_");
        let entries = async {
            use anyhow::Context;
            use cortex::llm::credential_pool::{
                is_safe_base_url, pool_load_config_for_provider, AuthType, CredentialPool,
                PoolLoadConfig,
            };
            use cortex::llm::credential_sources::discoverer_by_name;
            use cortex::llm::provider::Provider;

            let discoverer = discoverer_by_name(&kind).ok_or_else(|| {
                anyhow::anyhow!("unknown OAuth kind `{kind}` — try claude_code | codex | qwen_cli")
            })?;
            let provider = discoverer.provider();

            let db = Database::open().await?;
            let db_path = cortex::config::Config::db_path()?;
            let config = cortex::config::Config::load_from_db().await?;

            // Build a load config pointing at this OAuth provider. Pairing
            // seeds the pool regardless of whether the operator has switched
            // their primary to that flavor yet, so we bypass
            // `config.llm.provider`. Env-var seeding is disabled — pairing
            // is an explicit operator action, not an env scan.
            let mut load_cfg: PoolLoadConfig = pool_load_config_for_provider(
                provider,
                config.llm.credential_strategy,
                config.llm.credential_cooldown_secs,
                config.llm.max_concurrent_per_credential,
                vec![],
            );
            load_cfg.proactive_rate_limit_threshold_pct =
                config.llm.proactive_rate_limit_threshold_pct;
            load_cfg.proactive_rate_limit_min_window_secs =
                config.llm.proactive_rate_limit_min_window_secs;
            load_cfg.env_vars.clear();
            let pool = CredentialPool::load(&db, &db_path, provider, load_cfg).await?;

            let home = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("cannot resolve $HOME for OAuth discovery"))?;
            let discovered = discoverer
                .discover(&home)
                .with_context(|| format!("running discoverer `{}`", discoverer.name()))?;

            let mut out = Vec::new();
            for disc in discovered {
                // Custom-endpoint sources carry a per-credential base_url
                // that needs SSRF-style validation before it lands in the
                // pool — operator-writable files can otherwise redirect
                // traffic on next request.
                if let Some(url) = disc.base_url.as_deref() {
                    if !is_safe_base_url(url) {
                        anyhow::bail!(
                            "discoverer `{}` returned unsafe base_url `{}` — must be https or http+loopback",
                            discoverer.name(),
                            url
                        );
                    }
                }
                let source = match provider {
                    Provider::Custom => cortex::llm::credential_pool::CredentialSource::Custom {
                        name: discoverer.name().to_string(),
                        base_url: disc.base_url.clone().unwrap_or_default(),
                    },
                    _ => cortex::llm::credential_pool::CredentialSource::OAuth {
                        name: discoverer.name().to_string(),
                    },
                };
                let auth_type = if disc.refresh_token.is_some() {
                    AuthType::OAuth
                } else {
                    AuthType::ApiKey
                };
                let cred = pool.add_entry_full(
                    disc.access_token,
                    disc.label,
                    source,
                    auth_type,
                    disc.refresh_token,
                    disc.expires_at_ms,
                    disc.base_url,
                    disc.extra_json,
                );
                out.push(pooled_to_proto(pool.provider_str(), &cred));
            }
            anyhow::Ok(out)
        }
        .await
        .map_err(|e| TStatus::invalid_argument(format!("pair_credential: {e}")))?;
        notify_runtime_config_reload(&self.config_notify_tx).await;
        Ok(Response::new(PairCredentialResponse { entries }))
    }

    async fn set_credential_strategy(
        &self,
        req: Request<SetCredentialStrategyRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        // Validate input early so we don't write garbage to settings.
        use std::str::FromStr;
        cortex::llm::credential_pool::CredentialStrategy::from_str(&req.strategy).map_err(
            |_| {
                TStatus::invalid_argument(format!(
                    "unknown strategy `{}` — try fill_first, round_robin, least_used, random",
                    req.strategy
                ))
            },
        )?;
        let provider = req.provider;
        let strategy_value = req.strategy;
        async {
            let db = Database::open().await?;
            let config = cortex::config::Config::load_from_db().await?;
            let pool = build_primary_pool(&config, &db).await?;
            require_provider_match(&pool, &provider)?;
            // Persist via the settings registry so it's restart-stable, and
            // notify the runtime config reload so the live LlmClient picks
            // up the new strategy on its next rebuild.
            let resolver = cortex::settings::SettingsResolver::load().await?;
            // The `json_quoted` setter wraps the value itself before
            // deserializing — we pass the raw variant string.
            resolver
                .set("llm.credential_strategy", &strategy_value)
                .await
                .map_err(|e| anyhow::anyhow!("set llm.credential_strategy: {e}"))?;
            anyhow::Ok(())
        }
        .await
        .map_err(|e| TStatus::invalid_argument(format!("set_credential_strategy: {e}")))?;
        notify_runtime_config_reload(&self.config_notify_tx).await;
        Ok(Response::new(Empty {}))
    }

    async fn get_subprocesses(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<SubprocessSnapshot>, TStatus> {
        let reg = self.process_registry_handle();
        let Some(reg) = reg else {
            return Ok(Response::new(SubprocessSnapshot::default()));
        };
        let snap = reg.snapshot();
        Ok(Response::new(SubprocessSnapshot {
            running: snap
                .running
                .into_iter()
                .map(process_entry_to_proto)
                .collect(),
            finished: snap
                .finished
                .into_iter()
                .map(process_entry_to_proto)
                .collect(),
        }))
    }

    async fn set_subprocess_pinned(
        &self,
        req: Request<SetSubprocessPinnedRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        let reg = self
            .process_registry_handle()
            .ok_or_else(|| TStatus::failed_precondition("process registry not wired"))?;
        let result = if req.pinned {
            reg.pin(req.id)
        } else {
            reg.unpin(req.id)
        };
        result.map_err(|e| TStatus::not_found(e.to_string()))?;
        Ok(Response::new(Empty {}))
    }

    async fn request_handoff(
        &self,
        req: Request<HandoffRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        async {
            let db = Database::open().await?;
            // Empty session_id means "the most recently active session" —
            // same convention `/goal` uses so operators don't have to look
            // up an id to operate on their current session.
            let session_id = match resolve_goal_session_id(&db, &req.session_id).await? {
                Some(id) => id,
                None => anyhow::bail!("no session to hand off — start one first"),
            };
            db.request_handoff(&session_id, &req.target_platform).await
        }
        .await
        .map_err(|e: anyhow::Error| {
            TStatus::failed_precondition(format!("request_handoff failed: {e}"))
        })?;
        Ok(Response::new(Empty {}))
    }

    async fn claim_handoff(
        &self,
        req: Request<HandoffClaimRequest>,
    ) -> Result<Response<HandoffClaimResponse>, TStatus> {
        let req = req.into_inner();
        let claimed = async {
            let db = Database::open().await?;
            db.claim_handoff_for_platform(&req.platform).await
        }
        .await
        .map_err(|e: anyhow::Error| TStatus::internal(format!("claim_handoff failed: {e}")))?;
        Ok(Response::new(HandoffClaimResponse {
            session_id: claimed.clone().unwrap_or_default(),
            claimed: claimed.is_some(),
        }))
    }

    async fn complete_handoff(&self, req: Request<SessionRef>) -> Result<Response<Empty>, TStatus> {
        let id = req.into_inner().id;
        async {
            let db = Database::open().await?;
            db.complete_handoff(&id).await
        }
        .await
        .map_err(|e: anyhow::Error| {
            TStatus::failed_precondition(format!("complete_handoff failed: {e}"))
        })?;
        Ok(Response::new(Empty {}))
    }

    async fn fail_handoff(
        &self,
        req: Request<HandoffFailRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        async {
            let db = Database::open().await?;
            db.fail_handoff(&req.session_id, &req.reason).await
        }
        .await
        .map_err(|e: anyhow::Error| {
            TStatus::failed_precondition(format!("fail_handoff failed: {e}"))
        })?;
        Ok(Response::new(Empty {}))
    }

    async fn get_handoff_status(
        &self,
        req: Request<SessionRef>,
    ) -> Result<Response<HandoffStatusReply>, TStatus> {
        let id = req.into_inner().id;
        let status = async {
            let db = Database::open().await?;
            let session_id = resolve_goal_session_id(&db, &id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no session to inspect"))?;
            db.handoff_status(&session_id).await
        }
        .await
        .map_err(|e: anyhow::Error| TStatus::internal(format!("get_handoff_status failed: {e}")))?;
        Ok(Response::new(HandoffStatusReply {
            state: status
                .state
                .map(|s| s.as_str().to_string())
                .unwrap_or_default(),
            target_platform: status.target.unwrap_or_default(),
            error: status.error.unwrap_or_default(),
            requested_at_unix: status.requested_at.unwrap_or(0),
        }))
    }

    async fn get_goal(&self, req: Request<GoalRef>) -> Result<Response<GoalDetail>, TStatus> {
        let session_id_req = req.into_inner().session_id;
        let detail = async {
            let db = Database::open().await?;
            let Some(session_id) = resolve_goal_session_id(&db, &session_id_req).await? else {
                return anyhow::Ok(GoalDetail::default());
            };
            Ok(db
                .get_goal(&session_id)
                .await?
                .map(|g| goal_to_detail(&g))
                .unwrap_or_default())
        }
        .await
        .map_err(|e| TStatus::internal(format!("get_goal failed: {e}")))?;
        Ok(Response::new(detail))
    }

    async fn set_goal(&self, req: Request<GoalMutation>) -> Result<Response<GoalDetail>, TStatus> {
        let req = req.into_inner();
        let op = GoalMutationOp::try_from(req.op).map_err(|_| {
            TStatus::invalid_argument(format!("unknown op discriminator: {}", req.op))
        })?;
        if op == GoalMutationOp::Unspecified {
            return Err(TStatus::invalid_argument("op is required (UNSPECIFIED)"));
        }
        let session_id_req = req.session_id.clone();
        let objective = req.objective.clone();
        let status_raw = req.status.clone();
        let token_budget = if req.token_budget_present {
            Some(req.token_budget)
        } else {
            None
        };
        let bus = self.goal_bus_handle();
        let outcome = async {
            let db = Database::open().await.map_err(SetGoalError::Internal)?;
            let session_id = resolve_goal_session_id(&db, &session_id_req)
                .await
                .map_err(SetGoalError::Internal)?
                .ok_or_else(|| {
                    SetGoalError::Precondition("no active session — start a turn first".into())
                })?;
            match op {
                GoalMutationOp::Unspecified => unreachable!(),
                GoalMutationOp::Create => {
                    let obj = cortex::goals::validate_objective(&objective)
                        .map_err(SetGoalError::Invalid)?;
                    let budget = cortex::goals::validate_budget(token_budget)
                        .map_err(SetGoalError::Invalid)?;
                    let inserted = db
                        .insert_goal(&session_id, &obj, cortex::goals::GoalStatus::Active, budget)
                        .await
                        .map_err(SetGoalError::Internal)?;
                    let goal = inserted.ok_or_else(|| {
                        SetGoalError::AlreadyExists("goal already exists for this session".into())
                    })?;
                    Ok(SetGoalOutcome {
                        goal: Some(goal),
                        kind: cortex::goals::event_kind::CREATED,
                    })
                }
                GoalMutationOp::Replace => {
                    let obj = cortex::goals::validate_objective(&objective)
                        .map_err(SetGoalError::Invalid)?;
                    let budget = cortex::goals::validate_budget(token_budget)
                        .map_err(SetGoalError::Invalid)?;
                    let existed = db
                        .get_goal(&session_id)
                        .await
                        .map_err(SetGoalError::Internal)?
                        .is_some();
                    let g = db
                        .replace_goal(&session_id, &obj, cortex::goals::GoalStatus::Active, budget)
                        .await
                        .map_err(SetGoalError::Internal)?;
                    Ok(SetGoalOutcome {
                        goal: Some(g),
                        kind: if existed {
                            cortex::goals::event_kind::REPLACED
                        } else {
                            cortex::goals::event_kind::CREATED
                        },
                    })
                }
                GoalMutationOp::SetStatus => {
                    let new_status = cortex::goals::GoalStatus::from_str(status_raw.trim())
                        .ok_or_else(|| {
                            SetGoalError::Invalid(format!(
                                "status must be one of active|paused|complete (got {status_raw:?}); blocked and budget_limited are system-only"
                            ))
                        })?;
                    if matches!(new_status, cortex::goals::GoalStatus::BudgetLimited) {
                        return Err(SetGoalError::Invalid(
                            "budget_limited is system-only; cannot be set via RPC".into(),
                        ));
                    }
                    if matches!(new_status, cortex::goals::GoalStatus::Blocked) {
                        return Err(SetGoalError::Invalid(
                            "blocked is agent-only; cannot be set via RPC".into(),
                        ));
                    }
                    let current = db
                        .get_goal(&session_id)
                        .await
                        .map_err(SetGoalError::Internal)?
                        .ok_or_else(|| {
                            SetGoalError::Precondition("no goal set for this session".into())
                        })?;
                    use cortex::goals::GoalStatus as S;
                    let allowed = matches!(
                        (current.status, new_status),
                        (S::Active, S::Paused)
                            | (S::Paused, S::Active)
                            | (S::Active, S::Complete)
                            | (S::Paused, S::Complete)
                            | (S::BudgetLimited, S::Complete)
                            // Blocked is set by the agent; the user resumes
                            // it back to Active or marks it Complete.
                            | (S::Blocked, S::Active)
                            | (S::Blocked, S::Complete)
                    );
                    if !allowed {
                        return Err(SetGoalError::Invalid(format!(
                            "illegal transition {} -> {}",
                            current.status.as_str(),
                            new_status.as_str()
                        )));
                    }
                    let updated = db
                        .update_goal(
                            &session_id,
                            cortex::db::GoalUpdate {
                                status: Some(new_status),
                                expected_goal_id: Some(current.goal_id),
                                ..Default::default()
                            },
                        )
                        .await
                        .map_err(SetGoalError::Internal)?;
                    let goal = updated.ok_or_else(|| {
                        SetGoalError::Precondition(
                            "goal changed concurrently during status update".into(),
                        )
                    })?;
                    let kind = if goal.status == cortex::goals::GoalStatus::Complete {
                        cortex::goals::event_kind::COMPLETED
                    } else {
                        cortex::goals::event_kind::STATUS_CHANGED
                    };
                    Ok(SetGoalOutcome {
                        goal: Some(goal),
                        kind,
                    })
                }
                GoalMutationOp::SetBudget => {
                    let budget = cortex::goals::validate_budget(token_budget)
                        .map_err(SetGoalError::Invalid)?;
                    let current = db
                        .get_goal(&session_id)
                        .await
                        .map_err(SetGoalError::Internal)?
                        .ok_or_else(|| {
                            SetGoalError::Precondition("no goal set for this session".into())
                        })?;
                    let updated = db
                        .update_goal(
                            &session_id,
                            cortex::db::GoalUpdate {
                                token_budget: Some(budget),
                                expected_goal_id: Some(current.goal_id),
                                ..Default::default()
                            },
                        )
                        .await
                        .map_err(SetGoalError::Internal)?;
                    let goal = updated.ok_or_else(|| {
                        SetGoalError::Precondition(
                            "goal changed concurrently during budget update".into(),
                        )
                    })?;
                    Ok(SetGoalOutcome {
                        goal: Some(goal),
                        kind: cortex::goals::event_kind::BUDGET_CHANGED,
                    })
                }
            }
        }
        .await
        .map_err(|e| match e {
            SetGoalError::Invalid(msg) => TStatus::invalid_argument(format!("set_goal: {msg}")),
            SetGoalError::Precondition(msg) => {
                TStatus::failed_precondition(format!("set_goal: {msg}"))
            }
            SetGoalError::AlreadyExists(msg) => TStatus::already_exists(format!("set_goal: {msg}")),
            SetGoalError::Internal(err) => TStatus::internal(format!("set_goal: {err}")),
        })?;

        let detail = outcome
            .goal
            .as_ref()
            .map(goal_to_detail)
            .unwrap_or_default();
        if let (Some(bus), Some(goal)) = (bus, outcome.goal.as_ref()) {
            bus.publish(crate::goal_bus::DaemonGoalEvent {
                session_id: goal.session_id.clone(),
                kind: outcome.kind.to_string(),
                goal: Some(goal.clone()),
                emitted_at_unix: now_unix(),
            });
        }
        Ok(Response::new(detail))
    }

    async fn clear_goal(&self, req: Request<GoalRef>) -> Result<Response<Empty>, TStatus> {
        let session_id_req = req.into_inner().session_id;
        let bus = self.goal_bus_handle();
        let removed = async {
            let db = Database::open().await?;
            let Some(session_id) = resolve_goal_session_id(&db, &session_id_req).await? else {
                return anyhow::Ok(None);
            };
            let did_remove = db.delete_goal(&session_id).await?;
            Ok(if did_remove { Some(session_id) } else { None })
        }
        .await
        .map_err(|e| TStatus::internal(format!("clear_goal failed: {e}")))?;
        if let (Some(bus), Some(session_id)) = (bus, removed) {
            bus.publish(crate::goal_bus::DaemonGoalEvent {
                session_id,
                kind: cortex::goals::event_kind::CLEARED.to_string(),
                goal: None,
                emitted_at_unix: now_unix(),
            });
        }
        Ok(Response::new(Empty {}))
    }

    async fn pause_goal_on_interrupt(
        &self,
        req: Request<GoalRef>,
    ) -> Result<Response<GoalDetail>, TStatus> {
        let session_id_req = req.into_inner().session_id;
        let bus = self.goal_bus_handle();
        let paused = async {
            let db = Database::open().await?;
            let Some(session_id) = resolve_goal_session_id(&db, &session_id_req).await? else {
                return anyhow::Ok(None);
            };
            db.pause_active_goal(&session_id).await
        }
        .await
        .map_err(|e: anyhow::Error| TStatus::internal(format!("pause_goal failed: {e}")))?;
        let detail = paused.as_ref().map(goal_to_detail).unwrap_or_default();
        if let (Some(bus), Some(goal)) = (bus, paused.as_ref()) {
            bus.publish(crate::goal_bus::DaemonGoalEvent {
                session_id: goal.session_id.clone(),
                kind: cortex::goals::event_kind::STATUS_CHANGED.to_string(),
                goal: Some(goal.clone()),
                emitted_at_unix: now_unix(),
            });
        }
        Ok(Response::new(detail))
    }

    type SubscribeGoalEventsStream =
        Pin<Box<dyn Stream<Item = Result<PbGoalEvent, TStatus>> + Send + 'static>>;

    async fn subscribe_goal_events(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<Self::SubscribeGoalEventsStream>, TStatus> {
        let bus = self
            .goal_bus_handle()
            .ok_or_else(|| TStatus::failed_precondition("goal event bus not wired"))?;
        let mut sub = bus.subscribe();
        let (tx, rx) = mpsc::channel::<Result<PbGoalEvent, TStatus>>(16);
        tokio::spawn(async move {
            loop {
                match sub.recv().await {
                    Ok(evt) => {
                        let pb = PbGoalEvent {
                            session_id: evt.session_id,
                            kind: evt.kind,
                            goal: Some(evt.goal.as_ref().map(goal_to_detail).unwrap_or_default()),
                            emitted_at_unix: evt.emitted_at_unix,
                        };
                        if tx.send(Ok(pb)).await.is_err() {
                            return; // client gone
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("goal stream subscriber lagged by {n}; continuing");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn tunnel_start(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<TunnelStatusReply>, TStatus> {
        let sup = self
            .tunnel_supervisor_handle()
            .ok_or_else(|| TStatus::failed_precondition("tunnel supervisor not wired"))?;
        let config = cortex::config::Config::load_from_db()
            .await
            .map_err(|e| TStatus::internal(format!("config load failed: {e}")))?;
        // apply() records the error into the snapshot before returning Err,
        // so we explicitly read status() on the failure path to surface the
        // operator-actionable hint (settings key, install URL, etc.).
        match sup.apply(&config.tunnel).await {
            Ok(snap) => Ok(Response::new(snapshot_to_reply(snap))),
            Err(_) => Ok(Response::new(snapshot_to_reply(sup.status().await))),
        }
    }

    async fn tunnel_stop(&self, _req: Request<Empty>) -> Result<Response<Empty>, TStatus> {
        let sup = self
            .tunnel_supervisor_handle()
            .ok_or_else(|| TStatus::failed_precondition("tunnel supervisor not wired"))?;
        sup.stop()
            .await
            .map_err(|e| TStatus::internal(format!("tunnel stop failed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn tunnel_status(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<TunnelStatusReply>, TStatus> {
        let sup = self
            .tunnel_supervisor_handle()
            .ok_or_else(|| TStatus::failed_precondition("tunnel supervisor not wired"))?;
        Ok(Response::new(snapshot_to_reply(sup.status().await)))
    }
}

fn snapshot_to_reply(snap: crate::tunnel_supervisor::TunnelSnapshot) -> TunnelStatusReply {
    TunnelStatusReply {
        provider: snap.provider,
        running: snap.running,
        public_url: snap.public_url,
        error: snap.last_error,
    }
}

struct SetGoalOutcome {
    goal: Option<cortex::goals::Goal>,
    kind: &'static str,
}

/// Classified failures from `set_goal` so the boundary picks the right
/// `tonic::Status` code. Without this every error collapses to
/// `InvalidArgument`, hiding real DB / infra problems behind a client-error
/// shaped response and breaking retry/alerting heuristics.
enum SetGoalError {
    /// Caller-supplied input or domain rule violated (validation, illegal
    /// transition, system-only status). Surfaces as
    /// `tonic::Code::InvalidArgument`.
    Invalid(String),
    /// Server state precludes the request: no session yet, no goal row,
    /// or a concurrent writer changed the goal mid-CAS. Surfaces as
    /// `tonic::Code::FailedPrecondition` so clients can retry safely.
    Precondition(String),
    /// `Create` requested when a row already exists. Surfaces as
    /// `tonic::Code::AlreadyExists` so the client can branch into a
    /// `Replace` flow without parsing message text.
    AlreadyExists(String),
    /// Storage or unexpected runtime failure. Surfaces as
    /// `tonic::Code::Internal` to trigger backoff / paging.
    Internal(anyhow::Error),
}

/// Resolve the session id the goal RPC operates on. Empty session_id means
/// "the most recently active session"; matches the TUI's `latest_session_id`
/// behaviour so users issuing `/goal …` from a freshly-opened terminal hit
/// the session they expect. Returns `None` when no session exists yet.
async fn resolve_goal_session_id(
    db: &cortex::db::Database,
    requested: &str,
) -> anyhow::Result<Option<String>> {
    let trimmed = requested.trim();
    if !trimmed.is_empty() {
        return Ok(Some(trimmed.to_string()));
    }
    Ok(db.list_sessions(1).await?.into_iter().next().map(|r| r.id))
}

/// Convert the core goal struct to its wire projection. `Option<i64>` budget
/// is encoded as `(present, value)` so the proto can distinguish "no
/// budget" from "budget == 0" (which `validate_budget` rejects anyway, but
/// the wire is self-describing).
fn goal_to_detail(g: &cortex::goals::Goal) -> GoalDetail {
    GoalDetail {
        present: true,
        session_id: g.session_id.clone(),
        goal_id: g.goal_id.clone(),
        objective: g.objective.clone(),
        status: g.status.as_str().to_string(),
        token_budget_present: g.token_budget.is_some(),
        token_budget: g.token_budget.unwrap_or(0),
        tokens_used: g.tokens_used,
        time_used_seconds: g.time_used_seconds,
        created_at: g.created_at,
        updated_at: g.updated_at,
    }
}

fn process_entry_to_proto(entry: ProcessEntry) -> PbSubprocess {
    PbSubprocess {
        id: entry.id,
        pid: entry.pid,
        kind: format!("{:?}", entry.kind),
        command: entry.command,
        session_id: entry.session_id.unwrap_or_default(),
        started_at_unix: entry.started_at,
        finished_at_unix: entry.finished_at.unwrap_or(0),
        finished: entry.finished_at.is_some(),
        exit_code: entry.exit_code.unwrap_or(0),
        exit_code_present: entry.exit_code.is_some(),
        status: format!("{:?}", entry.status),
        pinned: entry.pinned,
    }
}

async fn notify_runtime_config_reload(
    config_notify_tx: &Mutex<Option<mpsc::Sender<cortex::config::Config>>>,
) {
    let tx = {
        let guard = config_notify_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.clone()
    };
    let Some(tx) = tx else {
        return;
    };
    match cortex::config::Config::load_from_db().await {
        Ok(config) => {
            if tx.try_send(config).is_err() {
                tracing::debug!("config watcher notify queue full; poll loop will pick it up");
            }
        }
        Err(e) => tracing::warn!("settings changed but config reload failed: {e}"),
    }
}

/// Routing JSON persisted at `credentials.{provider}`. Tagged enum mirrors
/// the discriminator the cli expects when parsing the same blob back.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ApiKeyRouting {
    Literal { value: String },
    Keychain { service: String, account: String },
}

async fn load_ambient_status() -> anyhow::Result<AmbientStatus> {
    let db = Database::open().await?;
    let evo = db.get_evolution_state().await?;
    let vitals = db.get_vitals_state().await?;
    let bond_events = db.get_all_bond_events().await?;
    let bond_key = db.derive_hmac_key(cortex::bond::BOND_HMAC_DOMAIN);
    let bond = cortex::bond::replay_events_with_key(&bond_key, &bond_events);
    let mood = cortex::evolution::compute_mood(&evo, &vitals, &bond);
    Ok(AmbientStatus {
        evolution_name: evo.evolution_name.unwrap_or_default(),
        level: evo.level as u32,
        mood: mood.to_string(),
        archetype: evo
            .dominant_archetype
            .map(|a| a.to_string().to_lowercase())
            .unwrap_or_default(),
    })
}

async fn update_task_status(id: String, status: &'static str) -> Result<Response<Empty>, TStatus> {
    let updated = async {
        let db = Database::open().await?;
        db.update_task_status(&id, status).await
    }
    .await
    .map_err(|e: anyhow::Error| TStatus::internal(format!("update_task_status failed: {e}")))?;
    if !updated {
        return Err(TStatus::not_found("scheduled task not found"));
    }
    Ok(Response::new(Empty {}))
}

fn scheduled_task_row_to_proto(r: cortex::db::ScheduledTaskRow) -> ScheduledTask {
    ScheduledTask {
        id: r.id,
        name: r.name,
        prompt: r.prompt,
        schedule_type: r.schedule_type,
        schedule_expr: r.schedule_expr,
        timezone: r.timezone,
        status: r.status,
        next_run_unix: r.next_run.unwrap_or(0),
        created_at: r.created_at as u64,
        task_type: r.task_type,
        delivery_channel: r.delivery_channel.unwrap_or_default(),
        delivery_target: r.delivery_target.unwrap_or_default(),
    }
}

fn task_run_row_to_proto(r: cortex::db::TaskRunRow) -> ScheduledTaskRun {
    ScheduledTaskRun {
        id: r.id,
        task_id: r.task_id,
        started_at_unix: r.started_at,
        duration_ms: r.duration_ms,
        status: r.status,
        result: r.result.unwrap_or_default(),
        error: r.error.unwrap_or_default(),
    }
}

fn pairing_row_to_proto(r: cortex::db::PairingRequestRow) -> PairingRequest {
    PairingRequest {
        id: r.id,
        channel_name: r.channel_name,
        sender_id: r.sender_id,
        code: r.code,
        status: r.status,
        display_name: r.display_name.unwrap_or_default(),
        created_at: r.created_at as u64,
        expires_at: r.expires_at as u64,
    }
}

fn approved_sender_row_to_proto(r: cortex::db::ApprovedSenderRow) -> ApprovedSender {
    ApprovedSender {
        id: r.id,
        channel_name: r.channel_name,
        sender_id: r.sender_id,
        display_name: r.display_name.unwrap_or_default(),
        approved_at: r.approved_at as u64,
    }
}

// ── Credential pool helpers ─────────────────────────────────────────────

async fn build_primary_pool(
    config: &cortex::config::Config,
    _db: &cortex::db::Database,
) -> anyhow::Result<std::sync::Arc<cortex::llm::credential_pool::CredentialPool>> {
    let (provider, keys) = config.resolve_api_keys()?;
    let pool = cortex::llm::credential_pool::try_build_from_keys(
        provider,
        keys,
        config.llm.credential_strategy,
        config.llm.credential_cooldown_secs,
        config.llm.max_concurrent_per_credential,
        config.llm.proactive_rate_limit_threshold_pct,
        config.llm.proactive_rate_limit_min_window_secs,
    )
    .await
    .ok_or_else(|| {
        anyhow::anyhow!(
            "could not build credential pool for primary provider — check DB path and llm.api_keys config"
        )
    })?;
    Ok(pool)
}

fn pool_provider_matches(
    pool: &cortex::llm::credential_pool::CredentialPool,
    requested: &str,
) -> bool {
    let req = requested.trim().to_ascii_lowercase();
    req.is_empty() || pool.provider_str().eq_ignore_ascii_case(&req)
}

fn require_provider_match(
    pool: &cortex::llm::credential_pool::CredentialPool,
    requested: &str,
) -> anyhow::Result<()> {
    if pool_provider_matches(pool, requested) {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "provider `{}` is not the configured primary (`{}`) — fallback-provider admin is not yet wired",
            requested,
            pool.provider_str()
        ))
    }
}

fn credential_pool_to_proto(
    config: &cortex::config::Config,
    pool: &cortex::llm::credential_pool::CredentialPool,
) -> ProviderCredentials {
    let entries = pool
        .entries()
        .iter()
        .map(|c| pooled_to_proto(pool.provider_str(), c))
        .collect();
    ProviderCredentials {
        provider: pool.provider_str().to_string(),
        strategy: config.llm.credential_strategy.to_string(),
        cooldown_secs: pool.cooldown_secs(),
        max_concurrent_per_credential: pool.max_concurrent_per_credential(),
        entries,
    }
}

fn pooled_to_proto(
    provider: &str,
    c: &cortex::llm::credential_pool::PooledCredential,
) -> CredentialEntry {
    let (source_kind, source_value) = match &c.source {
        cortex::llm::credential_pool::CredentialSource::Manual => ("manual", String::new()),
        cortex::llm::credential_pool::CredentialSource::Env { var } => ("env", var.clone()),
        cortex::llm::credential_pool::CredentialSource::Config { name } => ("config", name.clone()),
        cortex::llm::credential_pool::CredentialSource::OAuth { name } => ("oauth", name.clone()),
        cortex::llm::credential_pool::CredentialSource::Custom { name, .. } => {
            ("custom", name.clone())
        }
    };
    let status = if c.is_removed() {
        "removed"
    } else {
        match c.last_status {
            cortex::llm::credential_pool::CredentialStatus::Ok => "ok",
            cortex::llm::credential_pool::CredentialStatus::Exhausted => "exhausted",
        }
    };
    CredentialEntry {
        id: c.id.clone(),
        label: c.label.clone(),
        provider: provider.to_string(),
        priority: c.priority,
        source_kind: source_kind.to_string(),
        source_value,
        fingerprint: c.fingerprint.clone(),
        use_count: c.use_count,
        status: status.to_string(),
        last_status_at_ms: c.last_status_at_ms.unwrap_or(0),
        last_error_code: c.last_error_code.map(u32::from).unwrap_or(0),
        last_error_reason: c.last_error_reason.clone().unwrap_or_default(),
        last_error_message: c.last_error_message.clone().unwrap_or_default(),
        last_error_reset_at_ms: c.last_error_reset_at_ms.unwrap_or(0),
        removed_at_ms: c.removed_at_ms.unwrap_or(0),
    }
}
