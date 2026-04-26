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
    admin_server::Admin, Empty, HealReport, MessageView, PairRequest, PairResponse, SessionDetail,
    SessionList, SessionRef, SessionSummary, SettingEntry, SettingKey, SettingMutation,
    SettingsSnapshot,
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
}
