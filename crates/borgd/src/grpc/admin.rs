//! Admin service — lifecycle + data-access RPCs.
//!
//! Backed by:
//! - `cortex::settings::SettingsResolver` for Get/Set/Unset.
//! - `cortex::db::Database` for ListSessions / GetSession.
//! - `cortex::maintenance::run_daily_maintenance` for Heal.
//! - The daemon's heartbeat-trigger channel for Poke / TriggerHeartbeat
//!   (set via `with_heartbeat_trigger_sender`).
//! - The daemon's `shutdown_tx` for Shutdown (set via `with_shutdown_sender`).

use crate::heartbeat::{HeartbeatBus, HeartbeatSource, HeartbeatTrigger};
use crate::session::{DaemonSessionExecutor, SessionRegistry};
use cortex::db::Database;
use cortex::settings::SettingsResolver;
use protocols::admin::{
    admin_server::Admin, ActivityLogQuery, ActivityLogText, AddProviderRequest, AmbientStatus,
    ApiKeyEntry, ApiKeyList, ApiKeyRef, ApiKeyValue, ApprovalGreetingRequest, ApprovedSender,
    ApprovedSenderList, ApprovedSenderRef, ClientTokenRef, CreateWebhookRequest,
    CreateWebhookResponse, DaemonStatus, Empty, ExportSessionRequest, ExportSessionResponse,
    GetScheduledTaskRunsRequest, HealReport, HeartbeatEvent as PbHeartbeatEvent,
    IssueClientTokenRequest, IssueClientTokenResponse, KeychainRef, LogActivityRequest,
    MessageView, MonthlyTokenTotal, MonthlyTotalCost, OnboardingState, PairingApproval,
    PairingFilter, PairingRequest, PairingRequestList, PluginInstallRequest, PluginInstallResponse,
    Project, ProjectCreate, ProjectList, ProjectRef, ProjectUpdate, RemoveProviderRequest,
    RevokedClientToken, ScheduledTask, ScheduledTaskCreate, ScheduledTaskDetail, ScheduledTaskList,
    ScheduledTaskRef, ScheduledTaskRun, ScheduledTaskRunList, ScheduledTaskUpdate, SessionDetail,
    SessionList, SessionRef, SessionSummary, SettingEntry, SettingKey, SettingMutation,
    SettingsSnapshot, TriggerHeartbeatRequest, WarningList, WebhookRef, WorkflowList,
    WorkflowSummary,
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
        }
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
}

#[tonic::async_trait]
impl Admin for AdminSvc {
    async fn poke(&self, _req: Request<Empty>) -> Result<Response<Empty>, TStatus> {
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
        tx.send(HeartbeatTrigger {
            source,
            bypass_quiet_hours: req.bypass_quiet_hours,
        })
        .await
        .map_err(|e| TStatus::internal(format!("heartbeat trigger channel closed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn heal(&self, _req: Request<Empty>) -> Result<Response<HealReport>, TStatus> {
        let report = tokio::task::spawn_blocking(|| -> anyhow::Result<_> {
            let db = Database::open()?;
            let config = cortex::config::Config::load_from_db()?;
            cortex::maintenance::run_daily_maintenance(&db, &config)
        })
        .await
        .map_err(|e| TStatus::internal(format!("heal task panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("heal failed: {e}")))?;
        Ok(Response::new(HealReport {
            pruned_logs: report.log_files_deleted as u64,
            pruned_activity: report.activity_rows_deleted as u64,
            evicted_embeddings: report.embeddings_pruned as u64,
            healed_tasks: report.stalled_tasks_healed as u64,
            warnings: report.persistent_warnings,
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
            .map_err(|e| TStatus::internal(format!("settings load failed: {e}")))?;
        let entries = resolver
            .list_all()
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
            .map_err(|e| TStatus::internal(format!("settings load failed: {e}")))?;
        resolver
            .set(&req.key, &req.value)
            .map_err(|e| TStatus::invalid_argument(format!("set `{}` failed: {e}", req.key)))?;
        Ok(Response::new(Empty {}))
    }

    async fn unset_setting(&self, req: Request<SettingKey>) -> Result<Response<Empty>, TStatus> {
        let key = req.into_inner().key;
        let resolver = SettingsResolver::load()
            .map_err(|e| TStatus::internal(format!("settings load failed: {e}")))?;
        resolver
            .unset(&key)
            .map_err(|e| TStatus::invalid_argument(format!("unset `{key}` failed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn list_sessions(&self, _req: Request<Empty>) -> Result<Response<SessionList>, TStatus> {
        // Combine in-memory live sessions with the most recent persisted ones.
        let live: std::collections::HashSet<String> =
            self.registry.live_ids().into_iter().collect();
        let sessions =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<SessionSummary>> {
                let db = Database::open()?;
                let rows = db.list_sessions(100)?;
                let mut sessions = Vec::with_capacity(rows.len());
                for r in rows {
                    let count = db.count_session_messages(&r.id).unwrap_or(0);
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
                Ok(sessions)
            })
            .await
            .map_err(|e| TStatus::internal(format!("list_sessions task panicked: {e}")))?
            .map_err(|e| TStatus::internal(format!("list_sessions failed: {e}")))?;
        Ok(Response::new(SessionList { sessions }))
    }

    async fn get_session(
        &self,
        req: Request<SessionRef>,
    ) -> Result<Response<SessionDetail>, TStatus> {
        let id = req.into_inner().id;
        let detail =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Option<SessionDetail>> {
                let db = Database::open()?;
                let Some(row) = db.session_by_id(&id)? else {
                    return Ok(None);
                };
                let messages = db.load_session_messages(&row.id)?;
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
            })
            .await
            .map_err(|e| TStatus::internal(format!("get_session task panicked: {e}")))?
            .map_err(|e| TStatus::internal(format!("get_session failed: {e}")))?
            .ok_or_else(|| TStatus::not_found("session not found".to_string()))?;
        Ok(Response::new(detail))
    }

    // ── Projects ──────────────────────────────────────────────────────────

    async fn list_projects(&self, _req: Request<Empty>) -> Result<Response<ProjectList>, TStatus> {
        let rows = tokio::task::spawn_blocking(|| -> anyhow::Result<_> {
            let db = Database::open()?;
            db.list_projects(None)
        })
        .await
        .map_err(|e| TStatus::internal(format!("projects task panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("list_projects failed: {e}")))?;
        Ok(Response::new(ProjectList {
            projects: rows.into_iter().map(project_row_to_proto).collect(),
        }))
    }

    async fn create_project(
        &self,
        req: Request<ProjectCreate>,
    ) -> Result<Response<Project>, TStatus> {
        let req = req.into_inner();
        let id = uuid::Uuid::new_v4().to_string();
        let id_for_lookup = id.clone();
        let inserted = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let db = Database::open()?;
            db.create_project(&id, &req.name, &req.description)?;
            Ok(db.list_projects(None)?.into_iter().find(|p| p.id == id))
        })
        .await
        .map_err(|e| TStatus::internal(format!("create_project task panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("create_project failed: {e}")))?
        .ok_or_else(|| {
            TStatus::internal(format!(
                "create_project: row `{id_for_lookup}` not found after insert"
            ))
        })?;
        Ok(Response::new(project_row_to_proto(inserted)))
    }

    async fn update_project(
        &self,
        req: Request<ProjectUpdate>,
    ) -> Result<Response<Project>, TStatus> {
        let req = req.into_inner();
        let id_for_lookup = req.id.clone();
        let updated = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let db = Database::open()?;
            let name = (!req.name.is_empty()).then_some(req.name.as_str());
            let desc = (!req.description.is_empty()).then_some(req.description.as_str());
            let status = (!req.status.is_empty()).then_some(req.status.as_str());
            db.update_project(&req.id, name, desc, status)?;
            Ok(db.list_projects(None)?.into_iter().find(|p| p.id == req.id))
        })
        .await
        .map_err(|e| TStatus::internal(format!("update_project task panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("update_project failed: {e}")))?
        .ok_or_else(|| TStatus::not_found(format!("project `{id_for_lookup}` not found")))?;
        Ok(Response::new(project_row_to_proto(updated)))
    }

    async fn delete_project(&self, req: Request<ProjectRef>) -> Result<Response<Empty>, TStatus> {
        let id = req.into_inner().id;
        let removed = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let db = Database::open()?;
            db.delete_project(&id)
        })
        .await
        .map_err(|e| TStatus::internal(format!("delete_project task panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("delete_project failed: {e}")))?;
        if !removed {
            return Err(TStatus::not_found("project not found"));
        }
        Ok(Response::new(Empty {}))
    }

    // ── Scheduled tasks ───────────────────────────────────────────────────

    async fn list_scheduled_tasks(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<ScheduledTaskList>, TStatus> {
        let rows = tokio::task::spawn_blocking(|| -> anyhow::Result<_> {
            let db = Database::open()?;
            db.list_tasks()
        })
        .await
        .map_err(|e| TStatus::internal(format!("list_tasks task panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("list_tasks failed: {e}")))?;
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
        let inserted = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            use cortex::db::NewTask;
            let db = Database::open()?;
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
            };
            db.create_task(&new_task)?;
            Ok(db.list_tasks()?.into_iter().find(|t| t.id == id))
        })
        .await
        .map_err(|e| TStatus::internal(format!("create_task task panicked: {e}")))?
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
        let removed = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let db = Database::open()?;
            db.delete_task(&id)
        })
        .await
        .map_err(|e| TStatus::internal(format!("delete_task task panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("delete_task failed: {e}")))?;
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
        let updated = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            use cortex::db::UpdateTask;
            let db = Database::open()?;
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
            let found = db.update_task(&req.id, &update)?;
            if !found {
                return Ok(None);
            }
            // Re-read the row so the caller observes the persisted state, not
            // a locally-merged copy that could drift if a column gets added.
            Ok(db.list_tasks()?.into_iter().find(|t| t.id == req.id))
        })
        .await
        .map_err(|e| TStatus::internal(format!("update_task task panicked: {e}")))?
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
        let found = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            let db = Database::open()?;
            if db.get_task_by_id(&id)?.is_none() {
                return Ok(false);
            }
            let now = chrono::Utc::now().timestamp();
            db.update_task_next_run(&id, Some(now))?;
            db.clear_task_retry(&id)?;
            Ok(true)
        })
        .await
        .map_err(|e| TStatus::internal(format!("run_scheduled_task_now panicked: {e}")))?
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
        let runs = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let db = Database::open()?;
            db.task_run_history(&id, limit)
        })
        .await
        .map_err(|e| TStatus::internal(format!("get_scheduled_task_runs panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("get_scheduled_task_runs failed: {e}")))?;
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
        let detail =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Option<ScheduledTaskDetail>> {
                let db = Database::open()?;
                let Some(row) = db.get_task_by_id(&id)? else {
                    return Ok(None);
                };
                let last = db.last_task_run(&id)?;
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
            })
            .await
            .map_err(|e| TStatus::internal(format!("get_scheduled_task_detail panicked: {e}")))?
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
        let (rendered, suggested) =
            tokio::task::spawn_blocking(move || -> anyhow::Result<(String, String)> {
                use cortex::export::{export_session, ExportFormat, ExportOptions};
                use std::str::FromStr;
                let format = ExportFormat::from_str(&format_str)?;
                let db = Database::open()?;
                export_session(&db, &session_id, ExportOptions { format })
            })
            .await
            .map_err(|e| TStatus::internal(format!("export_session panicked: {e}")))?
            .map_err(|e| TStatus::invalid_argument(format!("export_session failed: {e}")))?;
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
        let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let db = Database::open()?;
            let f = (!filter.is_empty()).then_some(filter.as_str());
            db.list_pairings(f)
        })
        .await
        .map_err(|e| TStatus::internal(format!("list_pairings task panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("list_pairings failed: {e}")))?;
        Ok(Response::new(PairingRequestList {
            requests: rows.into_iter().map(pairing_row_to_proto).collect(),
        }))
    }

    async fn approve_pairing(
        &self,
        req: Request<PairingApproval>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let db = Database::open()?;
            db.approve_pairing(&req.channel_name, &req.code).map(|_| ())
        })
        .await
        .map_err(|e| TStatus::internal(format!("approve_pairing task panicked: {e}")))?
        .map_err(|e| TStatus::failed_precondition(format!("approve_pairing failed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn list_approved_senders(
        &self,
        req: Request<PairingFilter>,
    ) -> Result<Response<ApprovedSenderList>, TStatus> {
        let filter = req.into_inner().channel_name;
        let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let db = Database::open()?;
            let f = (!filter.is_empty()).then_some(filter.as_str());
            db.list_approved_senders(f)
        })
        .await
        .map_err(|e| TStatus::internal(format!("list_approved_senders task panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("list_approved_senders failed: {e}")))?;
        Ok(Response::new(ApprovedSenderList {
            senders: rows.into_iter().map(approved_sender_row_to_proto).collect(),
        }))
    }

    async fn revoke_approved_sender(
        &self,
        req: Request<ApprovedSenderRef>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        let removed = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let db = Database::open()?;
            db.revoke_sender(&req.channel_name, &req.sender_id)
        })
        .await
        .map_err(|e| TStatus::internal(format!("revoke_sender task panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("revoke_sender failed: {e}")))?;
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
        let snapshot = tokio::task::spawn_blocking(|| -> anyhow::Result<_> {
            let db = Database::open()?;
            let cfg = cortex::config::Config::load_from_db()?;
            let tasks = db.list_tasks()?;
            let pairings = db.list_pairings(None)?;
            let active = tasks.iter().filter(|t| t.status == "active").count() as u64;
            let paused = tasks.iter().filter(|t| t.status == "paused").count() as u64;
            let pending = pairings.iter().filter(|p| p.status == "pending").count() as u64;
            Ok((cfg, active, paused, pending))
        })
        .await
        .map_err(|e| TStatus::internal(format!("daemon_status task panicked: {e}")))?
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

    async fn archive_project(&self, req: Request<ProjectRef>) -> Result<Response<Empty>, TStatus> {
        set_project_status(req.into_inner().id, "archived").await
    }

    async fn unarchive_project(
        &self,
        req: Request<ProjectRef>,
    ) -> Result<Response<Empty>, TStatus> {
        set_project_status(req.into_inner().id, "active").await
    }

    async fn list_workflows_by_project(
        &self,
        req: Request<ProjectRef>,
    ) -> Result<Response<WorkflowList>, TStatus> {
        let id = req.into_inner().id;
        if id.is_empty() {
            return Err(TStatus::invalid_argument("project id required"));
        }
        let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let db = Database::open()?;
            db.list_workflows_by_project(&id)
        })
        .await
        .map_err(|e| TStatus::internal(format!("list_workflows_by_project panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("list_workflows_by_project failed: {e}")))?;
        Ok(Response::new(WorkflowList {
            workflows: rows
                .into_iter()
                .map(|w| WorkflowSummary {
                    id: w.id,
                    title: w.title,
                    status: w.status,
                })
                .collect(),
        }))
    }

    async fn get_ambient_status(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<AmbientStatus>, TStatus> {
        let snap = tokio::task::spawn_blocking(load_ambient_status)
            .await
            .map_err(|e| TStatus::internal(format!("ambient_status panicked: {e}")))?
            .map_err(|e| TStatus::internal(format!("ambient_status failed: {e}")))?;
        Ok(Response::new(snap))
    }

    async fn get_persistent_warnings(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<WarningList>, TStatus> {
        let warnings = tokio::task::spawn_blocking(|| -> anyhow::Result<Vec<String>> {
            let db = Database::open()?;
            Ok(db
                .latest_doctor_run()?
                .map(|r| r.persistent_warnings)
                .unwrap_or_default())
        })
        .await
        .map_err(|e| TStatus::internal(format!("persistent_warnings panicked: {e}")))?
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
            let config = match cortex::config::Config::load_from_db() {
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
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let db = Database::open()?;
            cortex::activity_log::log_activity(&db, &level, &source, &message);
            Ok(())
        })
        .await
        .map_err(|e| TStatus::internal(format!("log_activity panicked: {e}")))?
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
        let text = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let db = Database::open()?;
            let entries = db.query_activity(count, min_level.as_deref(), category.as_deref())?;
            if entries.is_empty() {
                return Ok("No activity log entries.".to_string());
            }
            Ok(entries
                .iter()
                .rev()
                .map(cortex::activity_log::format_activity_entry)
                .collect::<Vec<_>>()
                .join("\n"))
        })
        .await
        .map_err(|e| TStatus::internal(format!("get_activity_log_text panicked: {e}")))?
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
        let recorded = tokio::task::spawn_blocking(move || -> anyhow::Result<u32> {
            let db = Database::open()?;
            db.insert_plugin(&req.plugin_id, &req.plugin_name, &req.kind, &req.category)?;
            let mut count = 0u32;
            for fh in &req.file_hashes {
                if let Err(e) = db.insert_file_hash(&req.plugin_id, &fh.path, &fh.hash) {
                    tracing::warn!("plugin_install: insert_file_hash {}: {e}", fh.path);
                } else {
                    count += 1;
                }
            }
            if let Some(t) = &req.installed_tool {
                if !t.name.is_empty() {
                    db.insert_installed_tool(&t.name, &t.description, &t.runtime, &t.plugin_id)?;
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
                    )?;
                }
            }
            if !req.credentials_json.is_empty() {
                db.set_setting("credentials", &req.credentials_json)?;
            }
            Ok(count)
        })
        .await
        .map_err(|e| TStatus::internal(format!("plugin_install panicked: {e}")))?
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
        let onboarded = tokio::task::spawn_blocking(|| -> anyhow::Result<bool> {
            let db = Database::open()?;
            Ok(matches!(
                db.get_setting("onboarded")?.as_deref(),
                Some("true")
            ))
        })
        .await
        .map_err(|e| TStatus::internal(format!("get_onboarding_state panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("get_onboarding_state failed: {e}")))?;
        Ok(Response::new(OnboardingState { onboarded }))
    }

    async fn set_onboarding_complete(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<Empty>, TStatus> {
        tokio::task::spawn_blocking(|| -> anyhow::Result<()> {
            let db = Database::open()?;
            db.set_setting("onboarded", "true")?;
            Ok(())
        })
        .await
        .map_err(|e| TStatus::internal(format!("set_onboarding_complete panicked: {e}")))?
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
        let body = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<ApiKeyRouting>> {
            let db = Database::open()?;
            let key = format!("credentials.{provider_for_lookup}");
            let raw = db.get_setting(&key)?;
            match raw {
                Some(s) if !s.is_empty() => Ok(Some(serde_json::from_str(&s)?)),
                _ => Ok(None),
            }
        })
        .await
        .map_err(|e| TStatus::internal(format!("get_api_key panicked: {e}")))?
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
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let db = Database::open()?;
            let key = format!("credentials.{provider}");
            let json = serde_json::to_string(&routing)?;
            db.set_setting(&key, &json)?;
            Ok(())
        })
        .await
        .map_err(|e| TStatus::internal(format!("set_api_key panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("set_api_key failed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn unset_api_key(&self, req: Request<ApiKeyRef>) -> Result<Response<Empty>, TStatus> {
        let provider = req.into_inner().provider;
        if provider.is_empty() {
            return Err(TStatus::invalid_argument("provider required"));
        }
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let db = Database::open()?;
            let key = format!("credentials.{provider}");
            db.delete_setting(&key)?;
            Ok(())
        })
        .await
        .map_err(|e| TStatus::internal(format!("unset_api_key panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("unset_api_key failed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn list_api_keys(&self, _req: Request<Empty>) -> Result<Response<ApiKeyList>, TStatus> {
        let entries = tokio::task::spawn_blocking(|| -> anyhow::Result<Vec<ApiKeyEntry>> {
            let db = Database::open()?;
            let rows = db.list_settings()?;
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
            Ok(out)
        })
        .await
        .map_err(|e| TStatus::internal(format!("list_api_keys panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("list_api_keys failed: {e}")))?;
        Ok(Response::new(ApiKeyList { entries }))
    }

    async fn get_monthly_token_total(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<MonthlyTokenTotal>, TStatus> {
        let total = tokio::task::spawn_blocking(|| -> anyhow::Result<u64> {
            let db = Database::open()?;
            db.monthly_token_total()
        })
        .await
        .map_err(|e| TStatus::internal(format!("monthly_token_total panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("monthly_token_total failed: {e}")))?;
        Ok(Response::new(MonthlyTokenTotal { total }))
    }

    async fn get_monthly_total_cost(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<MonthlyTotalCost>, TStatus> {
        let cost = tokio::task::spawn_blocking(|| -> anyhow::Result<Option<f64>> {
            let db = Database::open()?;
            db.monthly_total_cost()
        })
        .await
        .map_err(|e| TStatus::internal(format!("monthly_total_cost panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("monthly_total_cost failed: {e}")))?;
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
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let db = Database::open()?;
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
            row.insert(db.conn())?;
            Ok(())
        })
        .await
        .map_err(|e| TStatus::internal(format!("add_provider task panicked: {e}")))?
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
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let db = Database::open()?;
            // Idempotent — `false` (already absent) is a successful no-op.
            cortex::providers::ProviderRow::delete(db.conn(), &id)?;
            Ok(())
        })
        .await
        .map_err(|e| TStatus::internal(format!("remove_provider task panicked: {e}")))?
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
        let created = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let db = Database::open()?;
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
        })
        .await
        .map_err(|e| TStatus::internal(format!("create_webhook task panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("create_webhook '{name}' failed: {e}")))?;
        Ok(Response::new(CreateWebhookResponse {
            name: created.row.name,
            hmac_secret: created.row.secret_hash,
        }))
    }

    async fn revoke_webhook(&self, req: Request<WebhookRef>) -> Result<Response<Empty>, TStatus> {
        let name = req.into_inner().name;
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let db = Database::open()?;
            gateway::generic_webhook::definitions::revoke(&db, &name)
        })
        .await
        .map_err(|e| TStatus::internal(format!("revoke_webhook task panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("revoke_webhook failed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    // ── Client tokens (writer-only) ──────────────────────────────────────

    async fn issue_client_token(
        &self,
        req: Request<IssueClientTokenRequest>,
    ) -> Result<Response<IssueClientTokenResponse>, TStatus> {
        let req = req.into_inner();
        let label = req.label.clone();
        let issued = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let db = Database::open()?;
            cortex::auth::tokens::issue(&db, &req.label, &req.scopes)
        })
        .await
        .map_err(|e| TStatus::internal(format!("issue_client_token task panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("issue_client_token '{label}' failed: {e}")))?;
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
        let row = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let db = Database::open()?;
            cortex::auth::tokens::revoke_by_label(&db, &label)
        })
        .await
        .map_err(|e| TStatus::internal(format!("revoke_client_token task panicked: {e}")))?
        .map_err(|e| {
            TStatus::internal(format!("revoke_client_token '{label_for_err}' failed: {e}"))
        })?;
        Ok(Response::new(RevokedClientToken {
            id: row.id,
            label: row.label,
            revoked_at: row.revoked_at.unwrap_or(0) as u64,
        }))
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

async fn set_project_status(id: String, status: &'static str) -> Result<Response<Empty>, TStatus> {
    let id_for_err = id.clone();
    let updated = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let db = Database::open()?;
        db.update_project(&id, None, None, Some(status))
    })
    .await
    .map_err(|e| TStatus::internal(format!("update_project panicked: {e}")))?
    .map_err(|e| TStatus::internal(format!("update_project failed: {e}")))?;
    if !updated {
        return Err(TStatus::not_found(format!(
            "project `{id_for_err}` not found"
        )));
    }
    Ok(Response::new(Empty {}))
}

fn load_ambient_status() -> anyhow::Result<AmbientStatus> {
    let db = Database::open()?;
    let evo = db.get_evolution_state()?;
    let vitals = db.get_vitals_state()?;
    let bond_events = db.get_all_bond_events()?;
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
    let updated = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let db = Database::open()?;
        db.update_task_status(&id, status)
    })
    .await
    .map_err(|e| TStatus::internal(format!("update_task_status panicked: {e}")))?
    .map_err(|e| TStatus::internal(format!("update_task_status failed: {e}")))?;
    if !updated {
        return Err(TStatus::not_found("scheduled task not found"));
    }
    Ok(Response::new(Empty {}))
}

fn project_row_to_proto(r: cortex::db::ProjectRow) -> Project {
    Project {
        id: r.id,
        name: r.name,
        description: r.description,
        status: r.status,
        created_at: r.created_at as u64,
        updated_at: r.updated_at as u64,
    }
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
