//! Admin service — lifecycle + data-access RPCs.
//!
//! Backed by:
//! - `borg_core::settings::SettingsResolver` for Get/Set/Unset.
//! - `borg_core::db::Database` for ListSessions / GetSession.
//! - `borg_core::maintenance::run_daily_maintenance` for Heal.
//! - The daemon's `poke_tx` channel for Poke (set via `with_poke_sender`).
//! - The daemon's `shutdown_tx` for Shutdown (set via `with_shutdown_sender`).

use crate::session::SessionRegistry;
use borg_core::db::Database;
use borg_core::settings::SettingsResolver;
use borg_proto::admin::{
    admin_server::Admin, ApprovedSender, ApprovedSenderList, ApprovedSenderRef, DaemonStatus,
    Empty, HealReport, MessageView, PairRequest, PairResponse, PairingApproval, PairingFilter,
    PairingRequest, PairingRequestList, Project, ProjectCreate, ProjectList, ProjectRef,
    ProjectUpdate, ScheduledTask, ScheduledTaskCreate, ScheduledTaskList, ScheduledTaskRef,
    SessionDetail, SessionList, SessionRef, SessionSummary, SettingEntry, SettingKey,
    SettingMutation, SettingsSnapshot,
};
use std::sync::Mutex;
use tokio::sync::{broadcast, mpsc};
use tonic::{Request, Response, Status as TStatus};

/// Admin service implementation.
pub struct AdminSvc {
    registry: SessionRegistry,
    poke_tx: Mutex<Option<mpsc::Sender<()>>>,
    shutdown_tx: Mutex<Option<broadcast::Sender<()>>>,
}

impl AdminSvc {
    /// Construct a new Admin service tied to the live session registry.
    pub fn new(registry: SessionRegistry) -> Self {
        Self {
            registry,
            poke_tx: Mutex::new(None),
            shutdown_tx: Mutex::new(None),
        }
    }

    /// Wire a poke channel — `Admin.Poke` will send `()` on it. Optional;
    /// without this, Poke returns `failed_precondition`.
    pub fn with_poke_sender(self, tx: mpsc::Sender<()>) -> Self {
        *self
            .poke_tx
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
}

#[tonic::async_trait]
impl Admin for AdminSvc {
    async fn pair(&self, _req: Request<PairRequest>) -> Result<Response<PairResponse>, TStatus> {
        Err(TStatus::unimplemented(
            "pairing lands in Task 02 (mTLS + PAKE)",
        ))
    }

    async fn poke(&self, _req: Request<Empty>) -> Result<Response<Empty>, TStatus> {
        let tx = {
            let g = self
                .poke_tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.clone()
        };
        match tx {
            Some(tx) => {
                tx.send(())
                    .await
                    .map_err(|e| TStatus::internal(format!("poke channel closed: {e}")))?;
                Ok(Response::new(Empty {}))
            }
            None => Err(TStatus::failed_precondition(
                "no heartbeat scheduler is wired",
            )),
        }
    }

    async fn heal(&self, _req: Request<Empty>) -> Result<Response<HealReport>, TStatus> {
        let report = tokio::task::spawn_blocking(|| -> anyhow::Result<_> {
            let db = Database::open()?;
            let config = borg_core::config::Config::load_from_db()?;
            borg_core::maintenance::run_daily_maintenance(&db, &config)
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
        let db = Database::open().map_err(|e| TStatus::internal(format!("db open failed: {e}")))?;
        let rows = db
            .list_sessions(100)
            .map_err(|e| TStatus::internal(format!("list_sessions failed: {e}")))?;
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
        Ok(Response::new(SessionList { sessions }))
    }

    async fn get_session(
        &self,
        req: Request<SessionRef>,
    ) -> Result<Response<SessionDetail>, TStatus> {
        let id = req.into_inner().id;
        let db = Database::open().map_err(|e| TStatus::internal(format!("db open failed: {e}")))?;
        let row = db
            .session_by_id(&id)
            .map_err(|e| TStatus::internal(format!("session_by_id failed: {e}")))?
            .ok_or_else(|| TStatus::not_found(format!("session `{id}` not found")))?;
        let messages = db
            .load_session_messages(&id)
            .map_err(|e| TStatus::internal(format!("load_session_messages failed: {e}")))?;
        Ok(Response::new(SessionDetail {
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
            use borg_core::db::NewTask;
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
            let new_task = NewTask {
                id: &id,
                name: &req.name,
                prompt: &req.prompt,
                schedule_type: &req.schedule_type,
                schedule_expr: &req.schedule_expr,
                timezone: &req.timezone,
                next_run: None,
                max_retries: None,
                timeout_ms: None,
                delivery_channel,
                delivery_target,
                allowed_tools: None,
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
            let cfg = borg_core::config::Config::load_from_db()?;
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

fn project_row_to_proto(r: borg_core::db::ProjectRow) -> Project {
    Project {
        id: r.id,
        name: r.name,
        description: r.description,
        status: r.status,
        created_at: r.created_at as u64,
        updated_at: r.updated_at as u64,
    }
}

fn scheduled_task_row_to_proto(r: borg_core::db::ScheduledTaskRow) -> ScheduledTask {
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

fn pairing_row_to_proto(r: borg_core::db::PairingRequestRow) -> PairingRequest {
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

fn approved_sender_row_to_proto(r: borg_core::db::ApprovedSenderRow) -> ApprovedSender {
    ApprovedSender {
        id: r.id,
        channel_name: r.channel_name,
        sender_id: r.sender_id,
        display_name: r.display_name.unwrap_or_default(),
        approved_at: r.approved_at as u64,
    }
}
