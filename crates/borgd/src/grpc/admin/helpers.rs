//! Free helpers + auxiliary types for the admin gRPC service.
//!
//! Extracted from `grpc/admin.rs` so the 82-method `impl Admin for AdminSvc`
//! block in that file isn't interleaved with row→proto converters and
//! ambient-state lookups. Visibility is `pub(super)` — the implementations
//! stay private to the admin RPC family.
//!
//! Three kinds of code live here:
//!   - **Row → proto converters** (`scheduled_task_row_to_proto`,
//!     `pairing_row_to_proto`, `pooled_to_proto`, etc.): pure clerical
//!     mapping from `cortex::db::*Row` / pool entries to the protocol
//!     types declared in `protocols::admin`.
//!   - **Resolvers + glue** (`resolve_goal_session_id`,
//!     `resolve_agent_session_id`, `notify_runtime_config_reload`,
//!     `load_ambient_status`): small async helpers each RPC handler
//!     would otherwise inline.
//!   - **Local error/outcome types** (`SetGoalOutcome`, `SetGoalError`,
//!     `ApiKeyRouting`): private to the goal + credential RPCs.

use std::sync::Mutex;

/// A setting whose value is also written into the agent's seeded memory
/// files: the agent's own name (`user.agent_name`) or the owner's
/// (`user.name`). A write is validated first, and carried into the files
/// after the row lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NameSetting {
    Agent,
    Owner,
}

impl NameSetting {
    /// The name setting `key` is, if it is one.
    pub(super) fn of(key: &str) -> Option<Self> {
        match key {
            "user.agent_name" => Some(Self::Agent),
            cortex::user_name::USER_NAME_KEY => Some(Self::Owner),
            _ => None,
        }
    }

    /// Refuse a name that would break the memory files it lands in.
    pub(super) fn validate(self, value: &str) -> Result<(), tonic::Status> {
        match self {
            Self::Agent => cortex::agent_name::validate(value),
            Self::Owner => cortex::user_name::validate(value),
        }
        .map_err(tonic::Status::invalid_argument)
    }

    /// The name before a write, for the memory-file rename to replace.
    /// Read before the row changes; "" when unset.
    pub(super) async fn current(self) -> Result<String, tonic::Status> {
        cortex::config::Config::load_from_db()
            .await
            .map(|c| match self {
                Self::Agent => c.user.agent_name,
                Self::Owner => c.user.name,
            })
            .map(Option::unwrap_or_default)
            .map_err(|e| tonic::Status::internal(format!("read current name: {e}")))
    }

    /// Carry a write from `old` to `new` into the memory files. The setting
    /// has already landed, so the error says the files lag rather than that
    /// the rename failed. The file work runs on the blocking pool, off the
    /// RPC worker.
    pub(super) async fn rename_in_memory(
        self,
        old: String,
        new: &str,
    ) -> Result<(), tonic::Status> {
        let new = new.to_string();
        tokio::task::spawn_blocking(move || {
            cortex::config::Config::data_dir().and_then(|dir| match self {
                Self::Agent => cortex::agent_name::rename_in_memory_files(&dir, &old, &new),
                Self::Owner => cortex::user_name::rename_in_memory_files(&dir, &old, &new),
            })
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("rename task failed: {e}")))
        .map_err(|e| {
            tonic::Status::internal(format!(
                "name saved, but the memory files could not be updated: {e}"
            ))
        })
    }
}

/// A daemon dependency wired into [`super::AdminSvc`] after construction via a
/// `with_*` builder. Centralizes the poison-tolerant lock dance that every
/// wired slot would otherwise repeat for its setter and getter.
///
/// Reads clone the value out (`T: Clone`); a slot stays `None` until wired, and
/// the matching RPC returns `FailedPrecondition` ("not wired") in that case.
pub(super) struct WiredSlot<T>(Mutex<Option<T>>);

impl<T: Clone> WiredSlot<T> {
    /// An unwired slot.
    pub(super) fn empty() -> Self {
        Self(Mutex::new(None))
    }

    /// Wire (or replace) the value.
    pub(super) fn set(&self, value: T) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(value);
    }

    /// Clone the wired value out, or `None` if not yet wired.
    pub(super) fn get(&self) -> Option<T> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

use protocols::admin::{
    AgentEntry as PbAgentEntry, AgentMetrics as PbAgentMetrics,
    AgentSnapshotRef as PbAgentSnapshotRef, AmbientStatus, ApprovedSender, CredentialEntry, Empty,
    GoalDetail, PairingRequest, ProviderCredentials, ScheduledTask, ScheduledTaskRun,
    Subprocess as PbSubprocess, TunnelStatusReply,
};
use tokio::sync::mpsc;
use tonic::{Response, Status as TStatus};

use cortex::db::Database;
use cortex::process_registry::ProcessEntry;

// ── Sentinel conversions ────────────────────────────────────────────────
//
// Protobuf has no native "absent" for primitive fields, so the admin RPCs
// reserve sentinel values (empty string, non-positive ints) to mean "leave
// unchanged" or "use default". These helpers turn those sentinels into the
// `Option<&str> / Option<i64>` shape the DB layer wants, without each
// handler re-inlining the same `(!s.is_empty()).then_some(s)` dance.

/// `Some(s)` when non-empty, else `None`. Used for proto string fields where
/// "" means "leave unchanged" (most update/patch requests).
#[inline]
pub(super) fn opt_if_not_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

/// `Some(v)` for positive values; `0` and negatives map to `None`. Used for
/// `next_run_unix` / `timeout_ms` proto fields where `0` means "unset".
#[inline]
pub(super) fn opt_if_positive_i64(v: i64) -> Option<i64> {
    (v > 0).then_some(v)
}

/// `Some(v)` for non-negative values; negative maps to `None`. Used for
/// `max_retries` where a `-1` sentinel means "leave to the DB default".
#[inline]
pub(super) fn opt_if_non_negative_i32(v: i32) -> Option<i32> {
    (v >= 0).then_some(v)
}

// ── Status-code mappers ─────────────────────────────────────────────────
//
// The handlers repeat the same `.map_err(|e| TStatus::*(format!("{verb} \
// failed: {e}")))` recipe for every RPC. These helpers keep the verb at the
// call site (the part that's actually distinct) and centralize the code
// classification.

/// Wrap an internal error with a verb prefix, surfacing as
/// `tonic::Code::Internal`.
#[inline]
pub(super) fn map_internal<E: std::fmt::Display>(verb: &'static str) -> impl FnOnce(E) -> TStatus {
    move |e| TStatus::internal(format!("{verb} failed: {e}"))
}

/// Wrap a validation/input error with a verb prefix, surfacing as
/// `tonic::Code::InvalidArgument`.
#[inline]
pub(super) fn map_invalid_arg<E: std::fmt::Display>(
    verb: &'static str,
) -> impl FnOnce(E) -> TStatus {
    move |e| TStatus::invalid_argument(format!("{verb} failed: {e}"))
}

// ── Settings-resolver lifecycle ─────────────────────────────────────────

/// Load the `SettingsResolver`, mapping load failures to a consistent
/// `Internal` status. Every settings RPC opens the same way; this kills the
/// per-handler boilerplate.
pub(super) async fn open_settings_resolver() -> Result<cortex::settings::SettingsResolver, TStatus>
{
    cortex::settings::SettingsResolver::load()
        .await
        .map_err(map_internal("settings load"))
}

pub(super) fn snapshot_to_reply(
    snap: crate::tunnel_supervisor::TunnelSnapshot,
) -> TunnelStatusReply {
    TunnelStatusReply {
        provider: snap.provider,
        running: snap.running,
        public_url: snap.public_url,
        error: snap.last_error,
    }
}

/// Resolve the session id `ListAgents` / `ListAgentSnapshots` operates on.
/// Empty input means "the most recently active session" — matches
/// `resolve_goal_session_id`. Returns `None` when no session row exists.
pub(super) async fn resolve_agent_session_id(
    db: &cortex::db::Database,
    requested: &str,
) -> anyhow::Result<Option<String>> {
    let trimmed = requested.trim();
    if !trimmed.is_empty() {
        return Ok(Some(trimmed.to_string()));
    }
    Ok(db.list_sessions(1).await?.into_iter().next().map(|r| r.id))
}

/// Convert a stored `SubAgentRunRow` into the protobuf `AgentEntry` the
/// CLI/TUI consume. Status strings match `SubAgentStatus::as_str`'s wire
/// values so clients can pattern-match without re-encoding.
pub(super) fn sub_agent_row_to_pb(row: &cortex::db::SubAgentRunRow) -> PbAgentEntry {
    let files_touched: Vec<String> = serde_json::from_str(&row.files_touched).unwrap_or_default();
    PbAgentEntry {
        id: row.id.clone(),
        nickname: row.nickname.clone(),
        role: row.role.clone(),
        model: row.model.clone(),
        parent_session_id: row.parent_session_id.clone(),
        session_id: row.session_id.clone(),
        depth: row.depth,
        status: row.status.clone(),
        created_at: row.created_at,
        ended_at: row.completed_at.unwrap_or(0),
        error: row.error_text.clone().unwrap_or_default(),
        result: row.result_text.clone().unwrap_or_default(),
        metrics: Some(PbAgentMetrics {
            input_tokens: row.input_tokens.max(0) as u64,
            output_tokens: row.output_tokens.max(0) as u64,
            reasoning_tokens: row.reasoning_tokens.max(0) as u64,
            cost_usd: row.cost_usd,
            tool_calls: row.tool_calls.max(0) as u64,
            files_touched,
        }),
    }
}

/// Convert a stored snapshot row into its protobuf reference.
pub(super) fn snapshot_row_to_pb(
    row: &cortex::db::multi_agent::AgentTreeSnapshotRow,
) -> PbAgentSnapshotRef {
    PbAgentSnapshotRef {
        turn_id: row.turn_id.clone(),
        label: row.label.clone(),
        started_at: row.started_at,
        finished_at: row.finished_at,
    }
}

/// Decode a snapshot payload, accepting either a bare JSON array of agent
/// entries or an object with an `agents` field. Forward-compatible — future
/// writers can wrap the array in a richer envelope without breaking older
/// readers.
pub(super) fn parse_snapshot_payload(json: &str) -> anyhow::Result<Vec<PbAgentEntry>> {
    #[derive(serde::Deserialize)]
    struct StoredAgent {
        id: String,
        nickname: String,
        role: String,
        #[serde(default)]
        model: String,
        parent_session_id: String,
        session_id: String,
        depth: u32,
        status: String,
        #[serde(default)]
        created_at: i64,
        #[serde(default)]
        ended_at: i64,
        #[serde(default)]
        error: String,
        #[serde(default)]
        result: String,
        #[serde(default)]
        input_tokens: u64,
        #[serde(default)]
        output_tokens: u64,
        #[serde(default)]
        reasoning_tokens: u64,
        #[serde(default)]
        cost_usd: f64,
        #[serde(default)]
        tool_calls: u64,
        #[serde(default)]
        files_touched: Vec<String>,
    }
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Payload {
        Bare(Vec<StoredAgent>),
        Wrapped { agents: Vec<StoredAgent> },
    }
    let payload: Payload = serde_json::from_str(json)?;
    let agents = match payload {
        Payload::Bare(a) => a,
        Payload::Wrapped { agents } => agents,
    };
    Ok(agents
        .into_iter()
        .map(|a| PbAgentEntry {
            id: a.id,
            nickname: a.nickname,
            role: a.role,
            model: a.model,
            parent_session_id: a.parent_session_id,
            session_id: a.session_id,
            depth: a.depth,
            status: a.status,
            created_at: a.created_at,
            ended_at: a.ended_at,
            error: a.error,
            result: a.result,
            metrics: Some(PbAgentMetrics {
                input_tokens: a.input_tokens,
                output_tokens: a.output_tokens,
                reasoning_tokens: a.reasoning_tokens,
                cost_usd: a.cost_usd,
                tool_calls: a.tool_calls,
                files_touched: a.files_touched,
            }),
        })
        .collect())
}

pub(super) struct SetGoalOutcome {
    pub goal: Option<cortex::goals::Goal>,
    pub kind: &'static str,
}

/// Classified failures from `set_goal` so the boundary picks the right
/// `tonic::Status` code. Without this every error collapses to
/// `InvalidArgument`, hiding real DB / infra problems behind a client-error
/// shaped response and breaking retry/alerting heuristics.
pub(super) enum SetGoalError {
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
pub(super) async fn resolve_goal_session_id(
    db: &cortex::db::Database,
    requested: &str,
) -> anyhow::Result<Option<String>> {
    let trimmed = requested.trim();
    if !trimmed.is_empty() {
        return Ok(Some(trimmed.to_string()));
    }
    Ok(db.list_sessions(1).await?.into_iter().next().map(|r| r.id))
}

/// Resolver for the `Create` / `Replace` goal arms. Same as
/// [`resolve_goal_session_id`] when a session exists, but autocreates a
/// fresh anonymous session row when none does — defensive fallback for
/// headless callers (`borg ask`, custom gRPC clients) that may set a goal
/// without ever opening a SessionHost.
///
/// The TUI path doesn't hit this fallback because `AgentBackend::new`
/// upserts a `sessions` row on open, so `list_sessions(1)` returns the
/// real session id from the very first frame. The orphan-session footgun
/// — autocreate id ≠ the id `messages.session_id` later uses — is
/// avoided as a result.
pub(super) async fn resolve_or_create_goal_session_id(
    db: &cortex::db::Database,
    requested: &str,
) -> anyhow::Result<String> {
    if let Some(id) = resolve_goal_session_id(db, requested).await? {
        return Ok(id);
    }
    let new_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().timestamp();
    db.upsert_session(&new_id, now, now, 0, "", "").await?;
    Ok(new_id)
}

/// Convert the core goal struct to its wire projection. `Option<i64>` budget
/// is encoded as `(present, value)` so the proto can distinguish "no
/// budget" from "budget == 0" (which `validate_budget` rejects anyway, but
/// the wire is self-describing).
pub(super) fn goal_to_detail(g: &cortex::goals::Goal) -> GoalDetail {
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

pub(super) fn process_entry_to_proto(entry: ProcessEntry) -> PbSubprocess {
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

pub(super) async fn notify_runtime_config_reload(
    config_notify_tx: &WiredSlot<mpsc::Sender<cortex::config::Config>>,
) {
    let Some(tx) = config_notify_tx.get() else {
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

/// Routing JSON persisted at `credentials.{provider}`. Lives in cortex so the
/// LLM config resolves the same row the daemon writes.
pub(super) use cortex::security::secrets_resolve::ApiKeyRouting;

pub(super) async fn load_ambient_status() -> anyhow::Result<AmbientStatus> {
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

pub(super) async fn update_task_status(
    id: String,
    status: &'static str,
) -> Result<Response<Empty>, TStatus> {
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

pub(super) fn scheduled_task_row_to_proto(r: cortex::db::ScheduledTaskRow) -> ScheduledTask {
    ScheduledTask {
        system: cortex::task_ids::is_system_task_id(&r.id),
        session_id: crate::task_runner::task_session_of(&r).unwrap_or_default(),
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

pub(super) fn task_run_row_to_proto(r: cortex::db::TaskRunRow) -> ScheduledTaskRun {
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

pub(super) fn pairing_row_to_proto(r: cortex::db::PairingRequestRow) -> PairingRequest {
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

pub(super) fn approved_sender_row_to_proto(r: cortex::db::ApprovedSenderRow) -> ApprovedSender {
    ApprovedSender {
        id: r.id,
        channel_name: r.channel_name,
        sender_id: r.sender_id,
        display_name: r.display_name.unwrap_or_default(),
        approved_at: r.approved_at as u64,
    }
}

// ── Credential pool helpers ─────────────────────────────────────────────

pub(super) async fn build_primary_pool(
    config: &cortex::config::Config,
    _db: &cortex::db::Database,
) -> anyhow::Result<std::sync::Arc<cortex::llm::credential_pool::CredentialPool>> {
    let (provider, keys) = config.resolve_api_keys()?;
    let pool = cortex::llm::credential_pool::try_build_from_keys(
        provider,
        keys,
                    &config.llm,
    )
    .await
    .ok_or_else(|| {
        anyhow::anyhow!(
            "could not build credential pool for primary provider — check DB path and llm.api_keys config"
        )
    })?;
    Ok(pool)
}

pub(super) fn pool_provider_matches(
    pool: &cortex::llm::credential_pool::CredentialPool,
    requested: &str,
) -> bool {
    let req = requested.trim().to_ascii_lowercase();
    req.is_empty() || pool.provider_str().eq_ignore_ascii_case(&req)
}

pub(super) fn require_provider_match(
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

pub(super) fn credential_pool_to_proto(
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

pub(super) fn pooled_to_proto(
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

#[cfg(test)]
mod tests {
    use super::NameSetting;

    #[test]
    fn both_name_settings_are_recognised_and_nothing_else() {
        assert_eq!(NameSetting::of("user.agent_name"), Some(NameSetting::Agent));
        assert_eq!(NameSetting::of("user.name"), Some(NameSetting::Owner));
        assert_eq!(NameSetting::of("user.timezone"), None);
        assert_eq!(NameSetting::of("model"), None);
    }

    /// A line break would split the `- Name:` line in MEMORY.md; the write
    /// must be refused as the caller's mistake, before the row lands.
    #[test]
    fn a_name_that_breaks_the_files_is_an_invalid_argument() {
        for named in [NameSetting::Agent, NameSetting::Owner] {
            let err = named.validate("Mi\nke").unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "{named:?}");
            assert!(named.validate("Mike").is_ok());
            assert!(named.validate("").is_ok(), "empty clears the name");
        }
    }
}
