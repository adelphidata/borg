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
use cortex::process_registry::ProcessRegistry;
use cortex::settings::SettingsResolver;
use protocols::admin::{
    admin_server::Admin, goal_mutation::Op as GoalMutationOp, ActivityLogQuery, ActivityLogText,
    AddCredentialRequest, AddProviderRequest, AgentEntry as PbAgentEntry, AgentList as PbAgentList,
    AgentListRequest, AgentRef, AgentSnapshotList, AgentSnapshotRef as PbAgentSnapshotRef,
    AgentSnapshotRefRequest, AgentTreeEvent as PbAgentTreeEvent, AmbientStatus, ApiKeyEntry,
    ApiKeyList, ApiKeyRef, ApiKeyValue, ApprovalGreetingRequest, ApprovedSenderList,
    ApprovedSenderRef, BrowserManageRequest, BrowserManageResponse, ClientTokenRef,
    CreateWebhookRequest, CreateWebhookResponse, CredentialEntry, CredentialList, DaemonStatus,
    DoctorSquelchClearRequest, DoctorSquelchClearResponse, DoctorSquelchSetRequest, DreamReport,
    Empty, ExportSessionRequest, ExportSessionResponse, GetScheduledTaskRunsRequest, GoalDetail,
    GoalEvent as PbGoalEvent, GoalMutation, GoalRef, HandoffClaimRequest, HandoffClaimResponse,
    HandoffFailRequest, HandoffRequest, HandoffStatusReply, HealReport,
    HeartbeatEvent as PbHeartbeatEvent, IssueClientTokenRequest, IssueClientTokenResponse,
    KeychainRef, ListCredentialsRequest, ListSessionsRequest, LogActivityRequest, McpReloadFailure,
    MessageView, MonthlyTokenTotal, MonthlyTotalCost, OnboardingState, PairCredentialRequest,
    PairCredentialResponse, PairingApproval, PairingFilter, PairingRequestList,
    PluginInstallRequest, PluginInstallResponse, ReloadRequest, ReloadResponse,
    RemoveCredentialRequest, RemoveProviderRequest, ResetCredentialsRequest,
    ResetCredentialsResponse, RevokedClientToken, ScheduledTask, ScheduledTaskCreate,
    ScheduledTaskDetail, ScheduledTaskList, ScheduledTaskRef, ScheduledTaskRunList,
    ScheduledTaskUpdate, SessionActivityEvent as PbSessionActivityEvent, SessionArchiveMutation,
    SessionDetail, SessionList, SessionRef, SessionSummary, SessionTitleMutation,
    SetCredentialPriorityRequest, SetCredentialPriorityResponse, SetCredentialStrategyRequest,
    SetSubprocessPinnedRequest, SettingEntry, SettingKey, SettingMutation, SettingsSnapshot,
    SkillEntry as PbSkillEntry, SkillList as PbSkillList, SubprocessSnapshot,
    TriggerHeartbeatRequest, TunnelStatusReply, WarningList, WebhookRef,
};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::{Request, Response, Status as TStatus};

/// A stored message as the transcript should show it.
///
/// Only a user row is summarized. An attached file is composed into the user's
/// turn as a wrapped text part — the model must keep reading it, but replaying
/// the whole file into the bubble means a reopened conversation looks nothing
/// like the sentence the person actually typed. Every other role is returned
/// verbatim: a tool row's `<untrusted_content>` *is* the tool's output, and
/// hiding it would hide what the agent was working from.
fn display_text(role: &str, content: String) -> String {
    if role == "user" {
        cortex::security::sanitize::summarize_untrusted_blocks(&content)
    } else {
        content
    }
}

/// Longest preview a session row can carry. Long enough to recognise the
/// conversation, short enough that the daemon is not shipping transcripts to
/// anything that only asked for a list.
const PREVIEW_MAX_CHARS: usize = 140;

/// One line of a message, safe to render in a list.
///
/// Normalized here rather than in each client so the desktop, the browser and
/// the TUI cannot disagree about what a session's last line was. Redacted for
/// the same reason an export is: a pasted key rendering in a conversation list
/// is the one place nobody thinks to look for it.
fn preview_line(content: &str) -> String {
    let redacted = cortex::security::secrets::redact_secrets(content);
    let flattened = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    // Leading markdown is scaffolding, not what was said.
    let trimmed = flattened
        .trim_start_matches("```")
        .trim_start_matches('#')
        .trim_start_matches('>')
        .trim();
    if trimmed.chars().count() <= PREVIEW_MAX_CHARS {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(PREVIEW_MAX_CHARS).collect();
    format!("{}…", cut.trim_end())
}

/// Boxed session-activity stream; the `Admin::SubscribeSessionActivityStream`
/// associated type. No `WiredSlot` guard here — the bus lives on the session
/// registry every `AdminSvc` is built with, so the feed can never be unwired.
pub type SessionActivityStream =
    Pin<Box<dyn Stream<Item = Result<PbSessionActivityEvent, TStatus>> + Send + 'static>>;

/// Admin service implementation.
pub struct AdminSvc {
    registry: SessionRegistry,
    heartbeat_trigger_tx: WiredSlot<mpsc::Sender<HeartbeatTrigger>>,
    shutdown_tx: WiredSlot<broadcast::Sender<()>>,
    heartbeat_bus: WiredSlot<HeartbeatBus>,
    executor: WiredSlot<Arc<DaemonSessionExecutor>>,
    config_notify_tx: WiredSlot<mpsc::Sender<cortex::config::Config>>,
    process_registry: WiredSlot<ProcessRegistry>,
    goal_bus: WiredSlot<GoalEventBus>,
    tunnel_supervisor: WiredSlot<TunnelSupervisor>,
    /// Construction time, so `GetDaemonStatus` can report a real uptime.
    started_at: std::time::Instant,
}

impl AdminSvc {
    /// Construct a new Admin service tied to the live session registry.
    pub fn new(registry: SessionRegistry) -> Self {
        Self {
            registry,
            heartbeat_trigger_tx: WiredSlot::empty(),
            shutdown_tx: WiredSlot::empty(),
            heartbeat_bus: WiredSlot::empty(),
            executor: WiredSlot::empty(),
            config_notify_tx: WiredSlot::empty(),
            process_registry: WiredSlot::empty(),
            goal_bus: WiredSlot::empty(),
            tunnel_supervisor: WiredSlot::empty(),
            started_at: std::time::Instant::now(),
        }
    }

    /// Wire the daemon-side tunnel supervisor. `TunnelStart`/`TunnelStop`/
    /// `TunnelStatus` are no-ops with `FailedPrecondition` until this is
    /// set — the operator should see "tunnel not wired" rather than a
    /// silent success.
    pub fn with_tunnel_supervisor(self, sup: TunnelSupervisor) -> Self {
        self.tunnel_supervisor.set(sup);
        self
    }

    fn tunnel_supervisor_handle(&self) -> Option<TunnelSupervisor> {
        self.tunnel_supervisor.get()
    }

    /// Wire the goal-event broadcast bus. `Admin.SubscribeGoalEvents` reads
    /// from this; the four goal RPC handlers and the agent loop publish
    /// into it via [`GoalEventBus::publish`] / [`GoalEventBus::as_sink`].
    pub fn with_goal_bus(self, bus: GoalEventBus) -> Self {
        self.goal_bus.set(bus);
        self
    }

    fn goal_bus_handle(&self) -> Option<GoalEventBus> {
        self.goal_bus.get()
    }

    /// Wire the daemon-side subprocess registry. `Admin.GetSubprocesses`
    /// and `Admin.SetSubprocessPinned` rely on this; Admin.Heal also
    /// folds the registry into the maintenance sweep when present.
    pub fn with_process_registry(self, reg: ProcessRegistry) -> Self {
        self.process_registry.set(reg);
        self
    }

    fn process_registry_handle(&self) -> Option<ProcessRegistry> {
        self.process_registry.get()
    }

    /// Wire a heartbeat trigger channel. `Admin.Poke` and
    /// `Admin.TriggerHeartbeat` enqueue requests on it.
    pub fn with_heartbeat_trigger_sender(self, tx: mpsc::Sender<HeartbeatTrigger>) -> Self {
        self.heartbeat_trigger_tx.set(tx);
        self
    }

    /// Wire the daemon's shutdown broadcaster — `Admin.Shutdown` sends `()`.
    pub fn with_shutdown_sender(self, tx: broadcast::Sender<()>) -> Self {
        self.shutdown_tx.set(tx);
        self
    }

    /// Wire the heartbeat broadcast bus —
    /// `Admin.SubscribeHeartbeatEvents` reads from it.
    pub fn with_heartbeat_bus(self, bus: HeartbeatBus) -> Self {
        self.heartbeat_bus.set(bus);
        self
    }

    /// Wire the session executor used by `Admin.SendApprovalGreeting`.
    pub fn with_executor(self, executor: Arc<DaemonSessionExecutor>) -> Self {
        self.executor.set(executor);
        self
    }

    /// Wire the config watcher notifier used after setting changes.
    pub fn with_config_notify_sender(self, tx: mpsc::Sender<cortex::config::Config>) -> Self {
        self.config_notify_tx.set(tx);
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
        match self.heartbeat_trigger_tx.get() {
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
        let tx = self
            .heartbeat_trigger_tx
            .get()
            .ok_or_else(|| TStatus::failed_precondition("no heartbeat scheduler is wired"))?;
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
        match self.shutdown_tx.get() {
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
        let resolver = open_settings_resolver().await?;
        let entries = resolver
            .list_all()
            .await
            .map_err(map_internal("settings list"))?;
        Ok(Response::new(SettingsSnapshot {
            entries: entries
                .into_iter()
                .map(|e| {
                    // Curated display metadata, when the settings table names
                    // this key. Clients that get an empty label fall back to
                    // the key itself, so an uncurated setting stays reachable
                    // without being promoted as if it were user-facing.
                    let decl = cortex::settings::tui_setting(&e.key);
                    SettingEntry {
                        key: e.key,
                        value: e.value,
                        category: e.source.to_string(),
                        label: decl.map(|d| d.label.to_string()).unwrap_or_default(),
                        kind: decl.map(|d| d.kind.to_string()).unwrap_or_default(),
                        group: decl.map(|d| d.category.to_string()).unwrap_or_default(),
                    }
                })
                .collect(),
        }))
    }

    /// Write order: a write of `provider` that moves to a different provider
    /// resets `llm.base_url`. A client that sets both must send `provider`
    /// first and `llm.base_url` second, or the endpoint is lost.
    async fn set_setting(&self, req: Request<SettingMutation>) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        let resolver = open_settings_resolver().await?;
        resolver
            .set(&req.key, &req.value)
            .await
            .map_err(|e| TStatus::invalid_argument(format!("set `{}` failed: {e}", req.key)))?;
        notify_runtime_config_reload(&self.config_notify_tx).await;
        Ok(Response::new(Empty {}))
    }

    async fn unset_setting(&self, req: Request<SettingKey>) -> Result<Response<Empty>, TStatus> {
        let key = req.into_inner().key;
        let resolver = open_settings_resolver().await?;
        resolver
            .unset(&key)
            .await
            .map_err(|e| TStatus::invalid_argument(format!("unset `{key}` failed: {e}")))?;
        notify_runtime_config_reload(&self.config_notify_tx).await;
        Ok(Response::new(Empty {}))
    }

    async fn list_sessions(
        &self,
        req: Request<ListSessionsRequest>,
    ) -> Result<Response<SessionList>, TStatus> {
        let include_archived = req.into_inner().include_archived;
        // Combine in-memory live sessions with the most recent persisted ones.
        let live: std::collections::HashSet<String> =
            self.registry.live_ids().into_iter().collect();
        let sessions = async {
            let db = Database::open().await?;
            // Over-fetch: archived rows are dropped below, and taking exactly
            // 100 here would let them eat the page budget and silently shrink
            // the listing. Asking for the archived ones doubles the reach,
            // since nothing is discarded to make room any more.
            let fetch = if include_archived { 600 } else { 300 };
            let rows = db.list_sessions(fetch).await?;
            // Two budgets rather than one: an old archive must not crowd active
            // conversations out of the page it was only invited to as an aside.
            let mut active_taken = 0usize;
            let mut archived_taken = 0usize;
            let mut kept = Vec::with_capacity(rows.len());
            for r in rows {
                // Archiving is a soft hide, and this listing is what it hides
                // from: `borg sessions list` and the TUI session picker both
                // render this response. A live session is never hidden — it
                // would vanish from the picker mid-conversation.
                let archived = r.archived_at.is_some() && !live.contains(&r.id);
                if archived && !include_archived {
                    continue;
                }
                // The page budget must never hide a pin: once 100 rows are
                // taken, only pinned sessions may still join. `list_sessions`
                // guarantees pinned rows are present past the recency window;
                // dropping them here would recreate the vanishing-pin bug one
                // layer up.
                let taken = if archived {
                    &mut archived_taken
                } else {
                    &mut active_taken
                };
                if *taken >= 100 && !r.pinned {
                    continue;
                }
                *taken += 1;
                kept.push((r, archived));
            }
            // One query each for counts and previews rather than two per row:
            // a hundred sessions used to mean a hundred round trips before the
            // list could render at all.
            let ids: Vec<String> = kept.iter().map(|(r, _)| r.id.clone()).collect();
            let counts = db
                .count_session_messages_batch(&ids)
                .await
                .unwrap_or_default();
            let previews = db.last_message_previews(&ids).await.unwrap_or_default();
            // Rows from before turns titled their session would otherwise list
            // as "Untitled" until a next turn that may never come. A live
            // session is left to its own turn, which titles it on the way out.
            let untitled: Vec<String> = kept
                .iter()
                .filter(|(r, _)| {
                    r.title.is_empty()
                        && !live.contains(&r.id)
                        && counts.get(&r.id).is_some_and(|&n| n > 0)
                })
                .map(|(r, _)| r.id.clone())
                .collect();
            let healed = match db.backfill_session_titles(&untitled).await {
                Ok(healed) => healed,
                Err(e) => {
                    tracing::warn!(error = %e, "could not title untitled sessions");
                    Default::default()
                }
            };
            let mut sessions = Vec::with_capacity(kept.len());
            for (r, archived) in kept {
                let count = counts.get(&r.id).copied().unwrap_or(0);
                let (last_message_role, last_message_preview) = previews
                    .get(&r.id)
                    .map(|(role, content)| (role.clone(), preview_line(content)))
                    .unwrap_or_default();
                let mut title = healed.get(&r.id).cloned().unwrap_or(r.title.clone());
                if live.contains(&r.id) && !title.contains("(live)") {
                    title = format!("{title} (live)");
                }
                // `peek`, not `get`: a listing is inspection, and refreshing
                // last-activity here would keep every listed session resident
                // past its TTL.
                let turn_active = self.registry.peek(&r.id).is_some_and(|h| h.is_busy());
                sessions.push(SessionSummary {
                    id: r.id,
                    title,
                    message_count: count as u64,
                    updated_unix_ts: r.updated_at as u64,
                    pinned: r.pinned,
                    turn_active,
                    archived,
                    last_message_preview,
                    last_message_role,
                });
            }
            anyhow::Ok(sessions)
        }
        .await
        .map_err(|e| TStatus::internal(format!("list_sessions failed: {e}")))?;
        Ok(Response::new(SessionList { sessions }))
    }

    async fn set_session_archived(
        &self,
        req: Request<SessionArchiveMutation>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        let db = Database::open()
            .await
            .map_err(|e| TStatus::internal(format!("set_session_archived failed: {e}")))?;
        // Reject an unknown id before touching the registry: closing a live
        // session is not undoable, and a request answered with an error must
        // not have cancelled a turn on its way out.
        let known = db
            .session_by_id(&req.id)
            .await
            .map_err(|e| TStatus::internal(format!("set_session_archived failed: {e}")))?
            .is_some();
        if !known {
            return Err(TStatus::not_found(format!(
                "session `{}` not found",
                req.id
            )));
        }
        // `list_sessions` above exempts live sessions from the archived filter
        // — hiding a conversation mid-turn would be worse than the clutter — so
        // archiving one has to close it too, or the row the user just archived
        // stays on screen.
        //
        // The close MUST precede the stamp, never follow it: `close()` finalizes
        // through the agent, and a finished turn calls `record_session_turn`,
        // which clears `archived_at`. Stamping first would let the close silently unarchive
        // the conversation the user just closed.
        if req.archived {
            if let Some(host) = self.registry.remove_host(&req.id) {
                host.close().await;
            }
        }
        let stamp = req.archived.then(|| chrono::Utc::now().timestamp());
        db.set_session_archived(&req.id, stamp)
            .await
            .map_err(|e| TStatus::internal(format!("set_session_archived failed: {e}")))?;
        Ok(Response::new(Empty {}))
    }

    async fn set_session_title(
        &self,
        req: Request<SessionTitleMutation>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        let title = req.title.trim();
        // A blank title renders as an unnamed row, which is exactly what the
        // rename was meant to fix — refuse rather than silently store it.
        if title.is_empty() {
            return Err(TStatus::invalid_argument("title must not be empty"));
        }
        // `update_session_title` is a no-op for an unknown id, so check first:
        // a rename that quietly did nothing looks identical to one that worked.
        let known = async {
            let db = Database::open().await?;
            let known = db.session_by_id(&req.id).await?.is_some();
            if known {
                db.update_session_title(&req.id, title).await?;
            }
            anyhow::Ok(known)
        }
        .await
        .map_err(map_internal("set_session_title"))?;
        if !known {
            return Err(TStatus::not_found(format!(
                "session `{}` not found",
                req.id
            )));
        }
        Ok(Response::new(Empty {}))
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
            let messages = db.load_session_lineage_rows(&row.id).await?;
            Ok(Some(SessionDetail {
                id: row.id,
                title: row.title,
                messages: messages
                    .into_iter()
                    .map(|m| MessageView {
                        text: display_text(&m.role, m.content.unwrap_or_default()),
                        role: m.role,
                        unix_ts: m.created_at as u64,
                        tool_calls_json: m.tool_calls_json.unwrap_or_default(),
                        tool_call_id: m.tool_call_id.unwrap_or_default(),
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
        req: Request<Empty>,
    ) -> Result<Response<ScheduledTaskList>, TStatus> {
        scheduled_tasks::list_scheduled_tasks(req).await
    }

    async fn create_scheduled_task(
        &self,
        req: Request<ScheduledTaskCreate>,
    ) -> Result<Response<ScheduledTask>, TStatus> {
        scheduled_tasks::create_scheduled_task(req).await
    }

    async fn delete_scheduled_task(
        &self,
        req: Request<ScheduledTaskRef>,
    ) -> Result<Response<Empty>, TStatus> {
        scheduled_tasks::delete_scheduled_task(req).await
    }

    async fn update_scheduled_task(
        &self,
        req: Request<ScheduledTaskUpdate>,
    ) -> Result<Response<ScheduledTask>, TStatus> {
        scheduled_tasks::update_scheduled_task(req).await
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
        // Not `update_task_status`: resuming must also refresh a recurring
        // task's stale `next_run`, or it fires the moment it turns active.
        scheduled_tasks::resume_scheduled_task(req).await
    }

    async fn run_scheduled_task_now(
        &self,
        req: Request<ScheduledTaskRef>,
    ) -> Result<Response<Empty>, TStatus> {
        scheduled_tasks::run_scheduled_task_now(req).await
    }

    async fn get_scheduled_task_runs(
        &self,
        req: Request<GetScheduledTaskRunsRequest>,
    ) -> Result<Response<ScheduledTaskRunList>, TStatus> {
        scheduled_tasks::get_scheduled_task_runs(req).await
    }

    async fn get_scheduled_task_detail(
        &self,
        req: Request<ScheduledTaskRef>,
    ) -> Result<Response<ScheduledTaskDetail>, TStatus> {
        scheduled_tasks::get_scheduled_task_detail(req).await
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
        // Redaction defaults ON; `no_redact` is the wire-level opt-out
        // (unset/false on old clients keeps them on the safe path).
        let redact = !req.no_redact;
        let (rendered, suggested) = async {
            use cortex::export::{export_session, ExportFormat, ExportOptions};
            use std::str::FromStr;
            let format = ExportFormat::from_str(&format_str)?;
            let db = Database::open().await?;
            export_session(&db, &session_id, ExportOptions { format, redact }).await
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
            uptime_secs: self.started_at.elapsed().as_secs(),
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

    async fn list_skills(&self, _req: Request<Empty>) -> Result<Response<PbSkillList>, TStatus> {
        let config = cortex::config::Config::load_from_db()
            .await
            .map_err(|e| TStatus::internal(format!("list_skills config load failed: {e}")))?;

        // The loader walks the on-disk skills dir and probes `$PATH` for every
        // declared binary, so it belongs off the runtime's worker threads.
        let listings = tokio::task::spawn_blocking(move || {
            let resolved = config.resolve_credentials();
            let skills = cortex::skills::load_all_skills(&resolved, &config.skills, None)?;
            anyhow::Ok(cortex::skills::offerable_skills(&skills))
        })
        .await
        .map_err(|e| TStatus::internal(format!("list_skills panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("list_skills failed: {e}")))?;

        Ok(Response::new(PbSkillList {
            skills: listings
                .into_iter()
                .map(|s| PbSkillEntry {
                    name: s.name,
                    description: s.description,
                    builtin: s.builtin,
                })
                .collect(),
        }))
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
        let executor = self
            .executor
            .get()
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
        let bus = self
            .heartbeat_bus
            .get()
            .ok_or_else(|| TStatus::failed_precondition("heartbeat bus not wired"))?;
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

    // Onboarding sentinel is the settings-table key `onboarded`. Routing the
    // write through the daemon (rather than direct `Database::open()`) keeps a
    // single writer so its settings cache stays coherent.

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

    async fn get_setup_status(
        &self,
        req: Request<Empty>,
    ) -> Result<Response<protocols::admin::SetupStatus>, TStatus> {
        setup::get_setup_status(req).await
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
        credentials::get_api_key(req).await
    }

    async fn set_api_key(&self, req: Request<ApiKeyValue>) -> Result<Response<Empty>, TStatus> {
        let res = credentials::set_api_key(req).await?;
        // A `credentials.{provider}` row feeds `llm.api_key`, so a saved key
        // is a config change like any other. Without this the running agent
        // keeps the old credential until something else reloads it.
        notify_runtime_config_reload(&self.config_notify_tx).await;
        Ok(res)
    }

    async fn unset_api_key(&self, req: Request<ApiKeyRef>) -> Result<Response<Empty>, TStatus> {
        let res = credentials::unset_api_key(req).await?;
        // "Forget" has to stop the daemon sending that key now, not later.
        notify_runtime_config_reload(&self.config_notify_tx).await;
        Ok(res)
    }

    async fn list_api_keys(&self, req: Request<Empty>) -> Result<Response<ApiKeyList>, TStatus> {
        credentials::list_api_keys(req).await
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
        credentials::add_provider(req).await
    }

    async fn remove_provider(
        &self,
        req: Request<RemoveProviderRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        credentials::remove_provider(req).await
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
        credentials::list_credentials(req).await
    }

    async fn add_credential(
        &self,
        req: Request<AddCredentialRequest>,
    ) -> Result<Response<CredentialEntry>, TStatus> {
        credentials::add_credential(self, req).await
    }

    async fn remove_credential(
        &self,
        req: Request<RemoveCredentialRequest>,
    ) -> Result<Response<CredentialEntry>, TStatus> {
        credentials::remove_credential(self, req).await
    }

    async fn reset_credentials(
        &self,
        req: Request<ResetCredentialsRequest>,
    ) -> Result<Response<ResetCredentialsResponse>, TStatus> {
        credentials::reset_credentials(self, req).await
    }

    async fn pair_credential(
        &self,
        req: Request<PairCredentialRequest>,
    ) -> Result<Response<PairCredentialResponse>, TStatus> {
        credentials::pair_credential(self, req).await
    }

    async fn set_credential_priority(
        &self,
        req: Request<SetCredentialPriorityRequest>,
    ) -> Result<Response<SetCredentialPriorityResponse>, TStatus> {
        credentials::set_credential_priority(self, req).await
    }

    async fn set_credential_strategy(
        &self,
        req: Request<SetCredentialStrategyRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        credentials::set_credential_strategy(self, req).await
    }

    async fn get_subprocesses(
        &self,
        req: Request<Empty>,
    ) -> Result<Response<SubprocessSnapshot>, TStatus> {
        agents::get_subprocesses(self, req).await
    }

    async fn set_subprocess_pinned(
        &self,
        req: Request<SetSubprocessPinnedRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        agents::set_subprocess_pinned(self, req).await
    }

    async fn request_handoff(
        &self,
        req: Request<HandoffRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        handoff::request_handoff(req).await
    }

    async fn claim_handoff(
        &self,
        req: Request<HandoffClaimRequest>,
    ) -> Result<Response<HandoffClaimResponse>, TStatus> {
        handoff::claim_handoff(req).await
    }

    async fn complete_handoff(&self, req: Request<SessionRef>) -> Result<Response<Empty>, TStatus> {
        handoff::complete_handoff(req).await
    }

    async fn fail_handoff(
        &self,
        req: Request<HandoffFailRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        handoff::fail_handoff(req).await
    }

    async fn browser_manage(
        &self,
        req: Request<BrowserManageRequest>,
    ) -> Result<Response<BrowserManageResponse>, TStatus> {
        browser::browser_manage(self, req).await
    }

    async fn get_handoff_status(
        &self,
        req: Request<SessionRef>,
    ) -> Result<Response<HandoffStatusReply>, TStatus> {
        handoff::get_handoff_status(req).await
    }

    async fn get_goal(&self, req: Request<GoalRef>) -> Result<Response<GoalDetail>, TStatus> {
        goals::get_goal(req).await
    }

    async fn set_goal(&self, req: Request<GoalMutation>) -> Result<Response<GoalDetail>, TStatus> {
        goals::set_goal(self, req).await
    }

    async fn mutate_goal_gates(
        &self,
        req: Request<protocols::admin::GoalGateMutation>,
    ) -> Result<Response<protocols::admin::GoalGateList>, TStatus> {
        goals::mutate_goal_gates(req).await
    }

    async fn mutate_session_heartbeat(
        &self,
        req: Request<protocols::admin::SessionHeartbeatMutation>,
    ) -> Result<Response<protocols::admin::SessionHeartbeatDetail>, TStatus> {
        goals::mutate_session_heartbeat(req).await
    }

    async fn clear_goal(&self, req: Request<GoalRef>) -> Result<Response<Empty>, TStatus> {
        goals::clear_goal(self, req).await
    }

    async fn pause_goal_on_interrupt(
        &self,
        req: Request<GoalRef>,
    ) -> Result<Response<GoalDetail>, TStatus> {
        goals::pause_goal_on_interrupt(self, req).await
    }

    type SubscribeGoalEventsStream = goals::GoalEventsStream;

    type SubscribeSessionActivityStream = SessionActivityStream;

    type SubscribeAgentTreeEventsStream = agents::AgentTreeEventsStream;

    async fn subscribe_goal_events(
        &self,
        req: Request<Empty>,
    ) -> Result<Response<Self::SubscribeGoalEventsStream>, TStatus> {
        goals::subscribe_goal_events(self, req).await
    }

    async fn subscribe_session_activity(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<Self::SubscribeSessionActivityStream>, TStatus> {
        let mut sub = self.registry.subscribe_activity();
        let (tx, rx) = mpsc::channel::<Result<PbSessionActivityEvent, TStatus>>(16);
        tokio::spawn(async move {
            loop {
                match sub.recv().await {
                    Ok(evt) => {
                        let pb = PbSessionActivityEvent {
                            session_id: evt.session_id,
                            turn_active: evt.turn_active,
                            emitted_at_unix: evt.emitted_at_unix,
                        };
                        if tx.send(Ok(pb)).await.is_err() {
                            return; // client gone
                        }
                    }
                    // Dropped transitions are recoverable: the client reads
                    // `ListSessions.turn_active` whenever this stream opens,
                    // so keep the feed alive rather than failing it.
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("session activity subscriber lagged by {n}; continuing");
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

    async fn reload(
        &self,
        req: Request<ReloadRequest>,
    ) -> Result<Response<ReloadResponse>, TStatus> {
        use protocols::admin::reload_request::Target;

        let target = Target::try_from(req.into_inner().target).unwrap_or(Target::Unspecified);
        // Unspecified is treated as `All` so older clients that leave the
        // field at proto3 default still get a sensible reload rather than a
        // no-op masquerading as success.
        let (reload_mcp, reload_skills) = match target {
            Target::Mcp => (true, false),
            Target::Skills => (false, true),
            Target::All | Target::Unspecified => (true, true),
        };

        let config = cortex::config::Config::load_from_db()
            .await
            .map_err(|e| TStatus::internal(format!("config load failed: {e}")))?;

        let mut resp = ReloadResponse::default();

        if reload_mcp {
            let report = cortex::mcp_clients::reload_from_config(&config)
                .await
                .map_err(|e| TStatus::internal(format!("mcp reload failed: {e}")))?;
            resp.mcp_added = report.added;
            resp.mcp_removed = report.removed;
            resp.mcp_unchanged = report.unchanged as u32;
            resp.mcp_failures = report
                .failed
                .into_iter()
                .map(|(server, error)| McpReloadFailure { server, error })
                .collect();
            resp.mcp_total_tools = report.total_tools as u32;
        }

        if reload_skills {
            let resolved_creds = config.resolve_credentials();
            let report = cortex::skills::reload_user_skills(&resolved_creds, &config.skills)
                .map_err(|e| TStatus::internal(format!("skills reload failed: {e}")))?;
            resp.skills_total = report.total as u32;
            resp.skills_user = report.user as u32;
            resp.skills_user_unavailable = report.user_unavailable as u32;
            resp.skills_disabled = report.disabled as u32;
        }

        // Only kick the config watcher when skills changed — the watcher
        // tick clears the per-session skills-context cache. MCP tool defs
        // are pulled fresh from `mcp_clients::tool_definitions()` each
        // turn so the MCP-only path needs no session-side invalidation.
        if reload_skills {
            notify_runtime_config_reload(&self.config_notify_tx).await;
        }

        Ok(Response::new(resp))
    }

    /// List sub-agents owned by `session_id`. Reads from `sub_agent_runs` so
    /// terminal agents (completed/errored/shutdown) and live agents both
    /// surface — the operator's view of "what did this turn spawn" needs
    /// the historical rows, not just the in-memory registry. Snapshots for
    /// the same session are tacked on as `history` so the TUI can populate
    /// its snapshot-cursor in one round-trip.
    async fn list_agents(
        &self,
        req: Request<AgentListRequest>,
    ) -> Result<Response<PbAgentList>, TStatus> {
        agents::list_agents(req).await
    }

    /// Kill an agent. Today returns Unimplemented — the in-memory registry
    /// handle the cancellation token lives behind is not yet plumbed through
    /// the daemon session executor. The wire format is stable so the CLI
    /// and TUI can be built against it and the executor lands separately.
    async fn kill_agent(&self, req: Request<AgentRef>) -> Result<Response<Empty>, TStatus> {
        agents::kill_agent(req).await
    }

    /// Kill an agent and every descendant. See `kill_agent` for the same
    /// executor-wiring caveat.
    async fn kill_agent_subtree(&self, req: Request<AgentRef>) -> Result<Response<Empty>, TStatus> {
        agents::kill_agent_subtree(req).await
    }

    /// Pause an agent at its next iteration boundary. See `kill_agent`.
    async fn pause_agent(&self, req: Request<AgentRef>) -> Result<Response<Empty>, TStatus> {
        agents::pause_agent(req).await
    }

    /// Resume a paused agent. See `kill_agent`.
    async fn resume_agent(&self, req: Request<AgentRef>) -> Result<Response<Empty>, TStatus> {
        agents::resume_agent(req).await
    }

    /// Stream tree-lifecycle events. Returns `Unimplemented` until the
    /// [`cortex::multi_agent::AgentTreeBus`] is wired into the daemon
    /// session executor — an immediately-closed stream would let clients
    /// confuse "no events yet" with "not wired".
    async fn subscribe_agent_tree_events(
        &self,
        req: Request<SessionRef>,
    ) -> Result<Response<Self::SubscribeAgentTreeEventsStream>, TStatus> {
        agents::subscribe_agent_tree_events(req).await
    }

    /// List persisted spawn-tree snapshots for a session (newest first,
    /// capped to the per-session ring of 10).
    async fn list_agent_snapshots(
        &self,
        req: Request<SessionRef>,
    ) -> Result<Response<AgentSnapshotList>, TStatus> {
        agents::list_agent_snapshots(req).await
    }

    /// Fetch the agents captured in one persisted snapshot. The snapshot
    /// JSON is decoded server-side so the wire shape matches `ListAgents`.
    async fn get_agent_snapshot(
        &self,
        req: Request<AgentSnapshotRefRequest>,
    ) -> Result<Response<PbAgentList>, TStatus> {
        agents::get_agent_snapshot(req).await
    }
}

mod agents;
mod browser;
mod credentials;
mod goals;
mod handoff;
mod helpers;
mod scheduled_tasks;
mod setup;
use helpers::{
    approved_sender_row_to_proto, build_primary_pool, credential_pool_to_proto, goal_to_detail,
    load_ambient_status, map_internal, map_invalid_arg, notify_runtime_config_reload,
    open_settings_resolver, opt_if_non_negative_i32, opt_if_not_empty, opt_if_positive_i64,
    pairing_row_to_proto, parse_snapshot_payload, pool_provider_matches, pooled_to_proto,
    process_entry_to_proto, require_provider_match, resolve_agent_session_id,
    resolve_goal_session_id, resolve_or_create_goal_session_id, scheduled_task_row_to_proto,
    snapshot_row_to_pb, snapshot_to_reply, sub_agent_row_to_pb, task_run_row_to_proto,
    update_task_status, ApiKeyRouting, SetGoalError, SetGoalOutcome, WiredSlot,
};

#[cfg(test)]
mod display_text_tests {
    use super::display_text;
    use cortex::security::sanitize::wrap_untrusted;

    #[test]
    fn a_reopened_turn_shows_the_sentence_the_person_typed() {
        let stored = format!(
            "{}\n\nsummarise",
            wrap_untrusted("notes.md", "pages of text")
        );
        assert_eq!(
            display_text("user", stored),
            "[attached notes.md]\n\nsummarise"
        );
    }

    #[test]
    fn a_tool_row_keeps_the_boundary_that_frames_its_output() {
        // The wrapper is the tool result's own framing. Summarizing it here
        // would blank out what the agent actually read.
        let stored = wrap_untrusted("fetch", "the page body");
        assert_eq!(display_text("tool", stored.clone()), stored);
        assert_eq!(display_text("assistant", stored.clone()), stored);
    }
}
