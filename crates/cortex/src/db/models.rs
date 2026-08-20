/// Activity log entry from SQLite.
#[derive(Debug, Clone)]
pub struct ActivityEntry {
    /// Unique row identifier.
    pub id: i64,
    /// Log level (e.g. "info", "warn", "error").
    pub level: String,
    /// Activity category for filtering.
    pub category: String,
    /// Human-readable log message.
    pub message: String,
    /// Optional extended detail or context.
    pub detail: Option<String>,
    /// Unix timestamp when the entry was created.
    pub created_at: i64,
}

/// Session metadata row from SQLite.
#[derive(Debug, Clone)]
pub struct SessionRow {
    /// Unique session identifier.
    pub id: String,
    /// Unix timestamp when the session was created.
    pub created_at: i64,
    /// Unix timestamp when the session was last updated.
    pub updated_at: i64,
    /// Cumulative token count for the session.
    pub total_tokens: i64,
    /// LLM model used in this session.
    pub model: String,
    /// User-facing session title.
    pub title: String,
    /// Parent session id when this session was created via `/fork`.
    /// `None` for top-level sessions.
    pub parent_session_id: Option<String>,
    /// Anchor message rowid copied as the last message of this fork.
    /// `None` for top-level sessions.
    pub forked_from_message_id: Option<i64>,
    /// Whether the user pinned this session. Pinned sessions are exempt from
    /// the retention purge and the stale auto-archive sweep.
    pub pinned: bool,
    /// When the stale-session sweep archived this session, if it did.
    /// Archiving is a soft hide and is always reversible.
    pub archived_at: Option<i64>,
    /// Canonicalized working directory the session was opened from.
    /// `None` for sessions without a meaningful workspace (gateway
    /// channels, daemon-internal lanes, pre-v16 rows).
    pub cwd: Option<String>,
    /// Canonicalized toplevel of the git checkout enclosing `cwd`, when
    /// there is one. Drives workspace-scoped resume (`borg --continue`).
    pub git_repo_root: Option<String>,
}

impl SessionRow {
    /// Column list, in the exact order [`SessionRow::from_row`] reads. Every
    /// `SELECT ... FROM sessions` that maps into a `SessionRow` interpolates
    /// this so the column order and the field decoding never drift apart.
    pub const COLUMNS: &'static str = "id, created_at, updated_at, total_tokens, \
         model, title, parent_session_id, forked_from_message_id, pinned, archived_at, \
         cwd, git_repo_root";

    /// Decode a row produced by a `SELECT {COLUMNS} FROM sessions` query.
    pub fn from_row(row: &libsql::Row) -> anyhow::Result<Self> {
        Ok(SessionRow {
            id: row.get::<String>(0)?,
            created_at: row.get::<i64>(1)?,
            updated_at: row.get::<i64>(2)?,
            total_tokens: row.get::<i64>(3)?,
            model: row.get::<String>(4)?,
            title: row.get::<String>(5)?,
            parent_session_id: row.get::<Option<String>>(6)?,
            forked_from_message_id: row.get::<Option<i64>>(7)?,
            pinned: row.get::<Option<i64>>(8)?.unwrap_or(0) != 0,
            archived_at: row.get::<Option<i64>>(9)?,
            cwd: row.get::<Option<String>>(10)?,
            git_repo_root: row.get::<Option<String>>(11)?,
        })
    }
}

/// A held per-session lease (worker-pool / fallible executors).
///
/// A worker takes this before advancing a session and releases it after; on
/// crash it simply expires and any worker can reconstruct the session from the
/// log and continue. `epoch` fences a revived dead worker: a `renew`/`release`
/// only takes effect while the stored epoch still matches the one granted here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLease {
    /// Session this lease is over.
    pub session_id: String,
    /// Worker id that holds the lease.
    pub owner: String,
    /// Fencing token — advances on every takeover from a different/expired owner.
    pub epoch: i64,
    /// Unix timestamp after which the lease is considered expired.
    pub expiry: i64,
}

/// Scheduled task row from SQLite.
#[derive(Debug, Clone)]
pub struct ScheduledTaskRow {
    /// Unique task identifier.
    pub id: String,
    /// Human-readable task name.
    pub name: String,
    /// Prompt text (for LLM tasks) or shell command (for cron jobs).
    pub prompt: String,
    /// Schedule type (e.g. "cron", "interval").
    pub schedule_type: String,
    /// Schedule expression (cron expression or interval string).
    pub schedule_expr: String,
    /// IANA timezone for schedule evaluation.
    pub timezone: String,
    /// Current task status (e.g. "active", "paused", "completed").
    pub status: String,
    /// Unix timestamp of the next scheduled run, if any.
    pub next_run: Option<i64>,
    /// Unix timestamp when the task was created.
    pub created_at: i64,
    /// Maximum number of retry attempts on transient failure.
    pub max_retries: i32,
    /// Number of retries already attempted for the current failure.
    pub retry_count: i32,
    /// Unix timestamp after which a retry may be attempted.
    pub retry_after: Option<i64>,
    /// Error message from the most recent failed run.
    pub last_error: Option<String>,
    /// Per-task execution timeout in milliseconds.
    pub timeout_ms: i64,
    /// Channel to deliver results to (e.g. "telegram", "slack").
    pub delivery_channel: Option<String>,
    /// Target identifier within the delivery channel (e.g. chat ID).
    pub delivery_target: Option<String>,
    /// Comma-separated list of allowed tools for this task. None = all tools allowed.
    pub allowed_tools: Option<String>,
    /// Task type: "prompt" (LLM task) or "command" (shell cron job).
    pub task_type: String,
    /// Optional shell command to run before the LLM (for type=prompt).
    /// Stdout is injected into the prompt as `<preprocessed_context>`.
    pub pre_run_command: Option<String>,
    /// Optional working directory for `pre_run_command`. Defaults to $HOME.
    pub pre_run_cwd: Option<String>,
    /// Timeout in milliseconds for `pre_run_command` (default 60000).
    pub pre_run_timeout_ms: i64,
    /// One-shot bypass flag: when 1, the next dispatch ignores quiet hours
    /// and the per-task `min_interval_secs` floor. Cleared atomically when
    /// the dispatcher claims the row, so a concurrent second poke can't
    /// silently lose its override. Used today only by `task_type="heartbeat"`
    /// (set via `Admin.Poke` / `TriggerHeartbeat`).
    pub bypass_quiet_hours_once: bool,
    /// Per-task minimum-interval floor in seconds. NULL for tasks where the
    /// schedule expression is the only gate (regular `prompt`/`command`).
    /// Heartbeat populates this from `heartbeat.min_interval_secs` so two
    /// firings can't land within the configured window.
    pub min_interval_secs: Option<i64>,
    /// Whether each run should see what the previous runs produced.
    ///
    /// Off by default: carrying prior output costs context on every fire, and
    /// most scheduled tasks are stateless by design ("summarize today's
    /// mail"). On, the task can notice what changed since last time and
    /// avoid repeating itself.
    pub continuity: bool,
    /// Hash of the most recent delivered response. The heartbeat dispatch
    /// path checks this before broadcasting to suppress accidental repeats
    /// (model said the same thing twice). `[SILENT]` runs do NOT update
    /// this — only successful deliveries do.
    pub last_response_hash: Option<String>,
    /// User-set model pin for this task. `None` falls back to the
    /// scheduler-fleet default, then the global `llm.model`.
    ///
    /// Deliberately not settable through the agent-facing `schedule` tool: a
    /// model that could re-point its own scheduled work would silently change
    /// what the fleet costs and how it behaves. Set it via settings/CLI.
    pub model: Option<String>,
    /// Provider for [`Self::model`], when the pinned id needs a provider that
    /// differs from the global one. `None` uses the configured provider.
    pub model_provider: Option<String>,
    /// Monitor mode: when true and the task has a `pre_run_command`, a run
    /// whose successful pre-run output hashes identically to
    /// [`Self::last_pre_run_hash`] skips the LLM turn entirely and is
    /// recorded as a no-change run. Failed pre-runs never update or consult
    /// the baseline — the task runs normally with the failure surfaced.
    pub monitor_mode: bool,
    /// Baseline hash of the last successful `pre_run_command` output that
    /// reached the LLM. `None` until the first monitored run completes.
    pub last_pre_run_hash: Option<String>,
}

/// Task run log row from SQLite.
#[derive(Debug, Clone)]
pub struct TaskRunRow {
    /// Unique run identifier.
    pub id: i64,
    /// ID of the parent scheduled task.
    pub task_id: String,
    /// Unix timestamp when the run started.
    pub started_at: i64,
    /// Execution duration in milliseconds.
    pub duration_ms: i64,
    /// Output or result text on success.
    pub result: Option<String>,
    /// Error message on failure.
    pub error: Option<String>,
    /// Run outcome status (e.g. "success", "error", "timeout").
    pub status: String,
}

/// Per-task failure summary over a time window. Built by joining
/// `task_runs` (status='failed') back to `scheduled_tasks` so the
/// heartbeat can name failed tasks instead of just counting them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedRunGroup {
    /// Scheduled task ID — same value the `schedule` tool's `action='runs'` accepts.
    pub task_id: String,
    /// Human-readable task name from `scheduled_tasks.name`.
    pub task_name: String,
    /// Number of failed runs in the window.
    pub failures: i64,
    /// Most recent failure's `error` column, if any.
    pub latest_error: Option<String>,
    /// `started_at` of the most recent failure in the window.
    pub latest_at: i64,
}

/// A task that has been atomically claimed for execution, with its associated run ID.
#[derive(Debug, Clone)]
pub struct ClaimedTask {
    /// The scheduled task that was claimed.
    pub task: ScheduledTaskRow,
    /// ID of the task run created for this claim.
    pub run_id: i64,
}

/// Persisted message row from SQLite.
#[derive(Debug, Clone, Default)]
pub struct MessageRow {
    /// Unique row identifier.
    pub id: i64,
    /// ID of the session this message belongs to.
    pub session_id: String,
    /// Message role (e.g. "system", "user", "assistant", "tool").
    pub role: String,
    /// Plain text content of the message.
    pub content: Option<String>,
    /// JSON-serialized multipart content (images, etc.).
    pub content_parts_json: Option<String>,
    /// JSON-serialized tool calls made in this message.
    pub tool_calls_json: Option<String>,
    /// ID of the tool call this message is a response to.
    pub tool_call_id: Option<String>,
    /// RFC3339 timestamp for temporal reasoning.
    pub timestamp: Option<String>,
    /// Unix timestamp when the message was persisted.
    pub created_at: i64,
    /// Fine-grained event tag (e.g. `tool_call_started`). `None` for
    /// conversational message rows, which decode from `role`/`content`.
    pub event_kind: Option<String>,
    /// Typed payload for non-message events, serialized as
    /// [`crate::conversation::SessionEventKind`]. `None` for message rows.
    pub event_json: Option<String>,
    /// Per-session HMAC chain link over this event's immutable fields. Nullable
    /// in the schema only; every persist path sets it, and `None` fails chain
    /// verification.
    pub hmac: Option<String>,
    /// HMAC of the previous event in this session's chain (`"0"` for the first
    /// chained event). Same nullability caveat as [`Self::hmac`].
    pub prev_hmac: Option<String>,
}

/// Delivery queue row from SQLite.
#[derive(Debug, Clone)]
pub struct DeliveryRow {
    /// Unique delivery identifier.
    pub id: String,
    /// Name of the delivery channel (e.g. "telegram", "slack").
    pub channel_name: String,
    /// Sender identifier within the channel.
    pub sender_id: String,
    /// Channel-specific conversation or chat ID.
    pub channel_id: Option<String>,
    /// Associated agent session ID, if any.
    pub session_id: Option<String>,
    /// JSON-serialized delivery payload.
    pub payload_json: String,
    /// Current delivery status (e.g. "pending", "delivered", "failed").
    pub status: String,
    /// Number of delivery retries attempted so far.
    pub retry_count: i32,
    /// Maximum number of delivery retry attempts.
    pub max_retries: i32,
    /// Unix timestamp after which the next retry may be attempted.
    pub next_retry_at: Option<i64>,
    /// Unix timestamp when the delivery was enqueued.
    pub created_at: i64,
    /// Unix timestamp of the last status update.
    pub updated_at: i64,
    /// Error message from the most recent failed delivery attempt.
    pub error: Option<String>,
}

/// Plugin row from SQLite.
#[derive(Debug, Clone)]
pub struct PluginRow {
    /// Unique plugin identifier.
    pub id: String,
    /// Plugin display name.
    pub name: String,
    /// Plugin kind (e.g. "native", "script").
    pub kind: String,
    /// Plugin category (e.g. "messaging", "email", "productivity").
    pub category: String,
    /// Installation status (e.g. "installed", "uninstalled").
    pub status: String,
    /// Installed plugin version string.
    pub version: String,
    /// Unix timestamp when the plugin was installed.
    pub installed_at: i64,
    /// Unix timestamp when the plugin was last verified, if any.
    pub verified_at: Option<i64>,
}

/// WASM plugin row from SQLite.
#[derive(Debug, Clone)]
pub struct WasmPluginRow {
    /// Plugin identifier.
    pub plugin_id: String,
    /// Canonical manifest JSON.
    pub manifest_json: String,
    /// Manifest SHA-256 hex digest.
    pub manifest_sha256: String,
    /// WASM SHA-256 hex digest.
    pub wasm_sha256: String,
    /// Base64 Ed25519 publisher verifying key.
    pub publisher_key: String,
    /// Detached signature over the canonical manifest.
    pub signature: String,
    /// Local HMAC seal over manifest and WASM bytes.
    pub local_hmac: String,
    /// Installed manifest path.
    pub manifest_path: String,
    /// Installed WASM path.
    pub wasm_path: String,
    /// Trust state (`verified` or `quarantined`).
    pub status: String,
    /// Quarantine reason, if any.
    pub quarantine_reason: Option<String>,
    /// Unix timestamp when the plugin was installed.
    pub installed_at: i64,
    /// Unix timestamp when the plugin was last verified.
    pub verified_at: Option<i64>,
}

/// New WASM plugin metadata to persist.
pub struct NewWasmPlugin<'a> {
    /// Plugin identifier.
    pub plugin_id: &'a str,
    /// Canonical manifest JSON.
    pub manifest_json: &'a str,
    /// Manifest SHA-256 hex digest.
    pub manifest_sha256: &'a str,
    /// WASM SHA-256 hex digest.
    pub wasm_sha256: &'a str,
    /// Base64 Ed25519 publisher verifying key.
    pub publisher_key: &'a str,
    /// Detached signature over the canonical manifest.
    pub signature: &'a str,
    /// Local HMAC seal.
    pub local_hmac: &'a str,
    /// Installed manifest path.
    pub manifest_path: &'a str,
    /// Installed WASM path.
    pub wasm_path: &'a str,
}

/// WASM plugin tool definition row.
#[derive(Debug, Clone)]
pub struct WasmToolDefinitionRow {
    /// Tool name.
    pub name: String,
    /// Tool description.
    pub description: String,
    /// JSON schema string.
    pub input_schema_json: String,
}

/// WASM plugin runtime lookup row.
#[derive(Debug, Clone)]
pub struct WasmToolRuntimeRow {
    /// Owning plugin id.
    pub plugin_id: String,
    /// Tool name.
    pub tool_name: String,
    /// Canonical manifest JSON.
    pub manifest_json: String,
    /// Installed manifest path.
    pub manifest_path: String,
    /// Installed WASM path.
    pub wasm_path: String,
    /// Local HMAC seal.
    pub local_hmac: String,
}

/// Memory embedding row from SQLite.
#[derive(Debug, Clone)]
pub struct EmbeddingRow {
    /// Unique row identifier.
    pub id: i64,
    /// Memory scope (e.g. "global", "local").
    pub scope: String,
    /// Source memory filename.
    pub filename: String,
    /// Hash of the content that was embedded.
    pub content_hash: String,
    /// Raw embedding vector bytes.
    pub embedding: Vec<u8>,
    /// Number of dimensions in the embedding vector.
    pub dimension: usize,
    /// Embedding model used to generate the vector.
    pub model: String,
    /// Unix timestamp when the embedding was created.
    pub created_at: i64,
}

/// Chunk row from SQLite (for chunked/FTS memory search).
#[derive(Debug, Clone)]
pub struct ChunkRow {
    /// Unique row identifier.
    pub id: i64,
    /// Memory scope (e.g. "global", "local").
    pub scope: String,
    /// Source memory filename.
    pub filename: String,
    /// Zero-based index of this chunk within the file.
    pub chunk_index: i64,
    /// Starting line number in the source file, if known.
    pub start_line: Option<i64>,
    /// Ending line number in the source file, if known.
    pub end_line: Option<i64>,
    /// Text content of the chunk.
    pub content: String,
    /// Hash of the chunk content for deduplication.
    pub content_hash: String,
    /// Optional embedding vector bytes for this chunk.
    pub embedding: Option<Vec<u8>>,
    /// Unix timestamp when the chunk was created.
    pub created_at: i64,
}

/// Input data for upserting a chunk.
#[derive(Debug, Clone)]
pub struct ChunkData {
    /// Zero-based index of this chunk within the file.
    pub chunk_index: i64,
    /// Text content of the chunk.
    pub content: String,
    /// Hash of the chunk content for deduplication.
    pub content_hash: String,
    /// Optional embedding vector bytes.
    pub embedding: Option<Vec<u8>>,
    /// Embedding vector dimension, if an embedding is present.
    pub dimension: Option<usize>,
    /// Embedding model name, if an embedding is present.
    pub model: Option<String>,
    /// Starting line number in the source file, if known.
    pub start_line: Option<i64>,
    /// Ending line number in the source file, if known.
    pub end_line: Option<i64>,
}

/// A persistent memory entry stored in SQLite.
#[derive(Debug, Clone)]
pub struct MemoryEntryRow {
    /// Unique row identifier.
    pub id: i64,
    /// Memory scope (e.g. "global", "project:{id}").
    pub scope: String,
    /// Logical name / topic (e.g. "INDEX", "rust-patterns", "daily/2025-04-12").
    pub name: String,
    /// Full text content of the memory entry.
    pub content: String,
    /// SHA-256 hash of the content for change detection.
    pub content_hash: String,
    /// Unix timestamp when the entry was first created.
    pub created_at: i64,
    /// Unix timestamp when the entry was last modified.
    pub updated_at: i64,
}

/// Pairing request row from SQLite.
#[derive(Debug, Clone)]
pub struct PairingRequestRow {
    /// Unique pairing request identifier.
    pub id: String,
    /// Channel the pairing request originated from.
    pub channel_name: String,
    /// Sender identifier requesting access.
    pub sender_id: String,
    /// Hex-encoded SHA-256 of the lowercased pairing code. Plaintext is
    /// never persisted; admin surfaces should display sender_id / display_name
    /// to identify pending pairings, not this hash.
    pub code: String,
    /// Request status (e.g. "pending", "approved", "expired").
    pub status: String,
    /// Optional human-readable name of the sender.
    pub display_name: Option<String>,
    /// Unix timestamp when the request was created.
    pub created_at: i64,
    /// Unix timestamp when the pairing code expires.
    pub expires_at: i64,
    /// Unix timestamp when the request was approved, if any.
    pub approved_at: Option<i64>,
}

/// Approved sender row from SQLite.
#[derive(Debug, Clone)]
pub struct ApprovedSenderRow {
    /// Unique row identifier.
    pub id: i64,
    /// Channel the sender is approved for.
    pub channel_name: String,
    /// Approved sender identifier.
    pub sender_id: String,
    /// Optional human-readable name of the sender.
    pub display_name: Option<String>,
    /// Unix timestamp when the sender was approved.
    pub approved_at: i64,
}

/// Agent role row from SQLite.
#[derive(Debug, Clone)]
pub struct AgentRoleRow {
    /// Unique role name (e.g. "coder", "researcher").
    pub name: String,
    /// Human-readable description of this role's purpose.
    pub description: String,
    /// Optional LLM model override for this role.
    pub model: Option<String>,
    /// Optional LLM provider override for this role.
    pub provider: Option<String>,
    /// Optional temperature override for this role.
    pub temperature: Option<f32>,
    /// Optional custom system instructions for this role.
    pub system_instructions: Option<String>,
    /// Comma-separated list of tools this role may use.
    pub tools_allowed: Option<String>,
    /// Maximum agent loop iterations for this role.
    pub max_iterations: Option<i64>,
    /// Whether this role is a compiled-in built-in.
    pub is_builtin: bool,
    /// Unix timestamp when the role was created.
    pub created_at: i64,
    /// Unix timestamp when the role was last updated.
    pub updated_at: i64,
}

/// Sub-agent run row from SQLite.
#[derive(Debug, Clone)]
pub struct SubAgentRunRow {
    /// Unique sub-agent run identifier.
    pub id: String,
    /// Short display name for this sub-agent instance.
    pub nickname: String,
    /// Role assigned to this sub-agent.
    pub role: String,
    /// Session ID of the parent agent that spawned this run.
    pub parent_session_id: String,
    /// Session ID used by the sub-agent itself.
    pub session_id: String,
    /// Nesting depth (0 = top-level sub-agent).
    pub depth: u32,
    /// Run status (e.g. "running", "completed", "failed").
    pub status: String,
    /// Final result text on success.
    pub result_text: Option<String>,
    /// Error text on failure.
    pub error_text: Option<String>,
    /// Unix timestamp when the run was created.
    pub created_at: i64,
    /// Unix timestamp when the run completed, if finished.
    pub completed_at: Option<i64>,
    /// LLM model in use for this agent.
    pub model: String,
    /// Accumulated input/prompt tokens.
    pub input_tokens: i64,
    /// Accumulated output/completion tokens.
    pub output_tokens: i64,
    /// Accumulated reasoning tokens (provider-dependent).
    pub reasoning_tokens: i64,
    /// Estimated USD cost.
    pub cost_usd: f64,
    /// Count of tool dispatches.
    pub tool_calls: i64,
    /// JSON-encoded list of file paths touched.
    pub files_touched: String,
}

/// Aggregated token usage per model from SQLite.
#[derive(Debug)]
pub struct ModelUsageRow {
    /// LLM provider name.
    pub provider: String,
    /// LLM model identifier.
    pub model: String,
    /// Total prompt (input) tokens consumed.
    pub prompt_tokens: u64,
    /// Total completion (output) tokens consumed.
    pub completion_tokens: u64,
    /// Combined prompt + completion tokens.
    pub total_tokens: u64,
    /// Estimated total cost in USD, if pricing is available.
    pub total_cost_usd: Option<f64>,
}

/// Aggregated token totals over a time window. `cost` is `None` and
/// `cost_present` is `false` when no row in the window had a non-null
/// `cost_usd` — the formatter uses this to hide the cost section rather
/// than render a misleading `$0.00`.
#[derive(Debug, Default, PartialEq)]
pub struct TokenSummary {
    /// Sum of `prompt_tokens` over the window.
    pub prompt: u64,
    /// Sum of `completion_tokens` over the window.
    pub completion: u64,
    /// Sum of `total_tokens` over the window.
    pub total: u64,
    /// Sum of `cached_input_tokens` over the window (Anthropic cache hits).
    pub cached_input: u64,
    /// Sum of `cache_creation_tokens` over the window.
    pub cache_creation: u64,
    /// Aggregate cost in USD across rows that carried a `cost_usd`.
    /// `None` when no rate-card-priced rows fell in the window.
    pub cost: Option<f64>,
    /// True when at least one row in the window had a non-null `cost_usd`.
    pub cost_present: bool,
}

/// Per-channel activity over a time window. `last_active_unix` is the MAX
/// `created_at` for the channel in the window.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelActivityRow {
    /// Channel name (`telegram`, `slack`, …).
    pub channel_name: String,
    /// Total `channel_messages` rows for this channel in the window.
    pub message_count: u64,
    /// Most recent message timestamp (Unix seconds) in the window.
    pub last_active_unix: i64,
}

/// Parameters for creating a new scheduled task.
pub struct NewTask<'a> {
    /// Unique task identifier.
    pub id: &'a str,
    /// Human-readable task name.
    pub name: &'a str,
    /// Prompt text or shell command for the task.
    pub prompt: &'a str,
    /// Schedule type (e.g. "cron", "interval").
    pub schedule_type: &'a str,
    /// Schedule expression (cron expression or interval string).
    pub schedule_expr: &'a str,
    /// IANA timezone for schedule evaluation.
    pub timezone: &'a str,
    /// Unix timestamp of the first scheduled run, if any.
    pub next_run: Option<i64>,
    /// Maximum retry attempts on transient failure.
    pub max_retries: Option<i32>,
    /// Per-task execution timeout in milliseconds.
    pub timeout_ms: Option<i64>,
    /// Channel to deliver results to.
    pub delivery_channel: Option<&'a str>,
    /// Target identifier within the delivery channel.
    pub delivery_target: Option<&'a str>,
    /// Comma-separated tool allowlist. None = all tools allowed.
    pub allowed_tools: Option<&'a str>,
    /// Task type: "prompt" (LLM task) or "command" (shell cron job). Defaults to "prompt".
    pub task_type: &'a str,
    /// Optional shell preamble for type=prompt tasks. Stdout injected as
    /// `<preprocessed_context>`.
    pub pre_run_command: Option<&'a str>,
    /// Optional working directory for `pre_run_command`. Defaults to $HOME.
    pub pre_run_cwd: Option<&'a str>,
    /// Timeout (ms) for `pre_run_command`. Defaults to 60_000.
    pub pre_run_timeout_ms: Option<i64>,
    /// Per-task minimum-interval floor in seconds. Used by the heartbeat row
    /// to enforce `heartbeat.min_interval_secs` at dispatch time. None for
    /// regular tasks.
    pub min_interval_secs: Option<i64>,
    /// Whether each run should see what the previous runs produced. See
    /// [`ScheduledTaskRow::continuity`].
    pub continuity: bool,
}

/// Parameters for enqueuing a delivery.
pub struct NewDelivery<'a> {
    /// Unique delivery identifier.
    pub id: &'a str,
    /// Name of the delivery channel.
    pub channel_name: &'a str,
    /// Sender identifier within the channel.
    pub sender_id: &'a str,
    /// Channel-specific conversation or chat ID.
    pub channel_id: Option<&'a str>,
    /// Associated agent session ID, if any.
    pub session_id: Option<&'a str>,
    /// JSON-serialized delivery payload.
    pub payload_json: &'a str,
    /// Maximum number of delivery retry attempts.
    pub max_retries: i32,
}

/// Parameters for updating an existing scheduled task. `None` fields are left unchanged.
pub struct UpdateTask<'a> {
    /// New task name, if changing.
    pub name: Option<&'a str>,
    /// New prompt or command text, if changing.
    pub prompt: Option<&'a str>,
    /// New schedule type, if changing.
    pub schedule_type: Option<&'a str>,
    /// New schedule expression, if changing.
    pub schedule_expr: Option<&'a str>,
    /// New timezone, if changing.
    pub timezone: Option<&'a str>,
}

/// Script row from SQLite.
#[derive(Debug, Clone)]
pub struct ScriptRow {
    /// Unique script identifier.
    pub id: String,
    /// Human-readable script name.
    pub name: String,
    /// Description of what the script does.
    pub description: String,
    /// Script runtime (e.g. "python", "node", "bash").
    pub runtime: String,
    /// Path to the script's entry point file.
    pub entrypoint: String,
    /// Sandbox profile applied when running this script.
    pub sandbox_profile: String,
    /// Whether the script is allowed network access.
    pub network_access: bool,
    /// Comma-separated filesystem read paths allowed.
    pub fs_read: String,
    /// Comma-separated filesystem write paths allowed.
    pub fs_write: String,
    /// Whether the script is ephemeral (auto-deleted after use).
    pub ephemeral: bool,
    /// HMAC integrity hash for tamper detection.
    pub hmac: String,
    /// Unix timestamp when the script was created.
    pub created_at: i64,
    /// Unix timestamp when the script was last updated.
    pub updated_at: i64,
    /// Unix timestamp of the last execution, if any.
    pub last_run_at: Option<i64>,
    /// Total number of times the script has been executed.
    pub run_count: i64,
}

/// Parameters for creating a new script.
pub struct NewScript<'a> {
    /// Unique script identifier.
    pub id: &'a str,
    /// Human-readable script name.
    pub name: &'a str,
    /// Description of what the script does.
    pub description: &'a str,
    /// Script runtime (e.g. "python", "node", "bash").
    pub runtime: &'a str,
    /// Path to the script's entry point file.
    pub entrypoint: &'a str,
    /// Sandbox profile to apply when running the script.
    pub sandbox_profile: &'a str,
    /// Whether the script is allowed network access.
    pub network_access: bool,
    /// Comma-separated filesystem read paths allowed.
    pub fs_read: &'a str,
    /// Comma-separated filesystem write paths allowed.
    pub fs_write: &'a str,
    /// Whether the script is ephemeral (auto-deleted after use).
    pub ephemeral: bool,
    /// HMAC integrity hash for tamper detection.
    pub hmac: &'a str,
    /// Unix timestamp when the script was created.
    pub created_at: i64,
    /// Unix timestamp when the script was last updated.
    pub updated_at: i64,
}

/// Health report for all event-sourced HMAC chains.
#[derive(Debug)]
pub struct ChainHealth {
    /// Whether the vitals HMAC chain is intact.
    pub vitals_valid: bool,
    /// Number of events in the vitals chain.
    pub vitals_count: u32,
    /// Whether the bond HMAC chain is intact.
    pub bond_valid: bool,
    /// Number of events in the bond chain.
    pub bond_count: u32,
    /// Whether the evolution HMAC chain is intact.
    pub evolution_valid: bool,
    /// Number of events in the evolution chain.
    pub evolution_count: u32,
}
