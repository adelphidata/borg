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

/// A task that has been atomically claimed for execution, with its associated run ID.
#[derive(Debug, Clone)]
pub struct ClaimedTask {
    /// The scheduled task that was claimed.
    pub task: ScheduledTaskRow,
    /// ID of the task run created for this claim.
    pub run_id: i64,
}

/// Persisted message row from SQLite.
#[derive(Debug, Clone)]
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
    /// Pairing code the sender must present for approval.
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

/// Lifecycle status of a project.
///
/// Stored as a lowercase string in the `projects.status` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectStatus {
    /// Project is open; new workflows may target it.
    Active,
    /// Project is closed; hidden from the default `list` and not acceptable as a
    /// target for new workflows.
    Archived,
}

impl ProjectStatus {
    /// All variants, in the order they should appear in JSON schemas.
    pub const ALL: &'static [Self] = &[Self::Active, Self::Archived];

    /// SQLite/JSON string form of this status.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
        }
    }
}

impl std::fmt::Display for ProjectStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ProjectStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "archived" => Ok(Self::Archived),
            other => Err(format!(
                "Invalid status: {other}. Use 'active' or 'archived'."
            )),
        }
    }
}

/// Project row from SQLite — groups related workflows.
#[derive(Debug, Clone)]
pub struct ProjectRow {
    /// Unique project identifier.
    pub id: String,
    /// Human-readable project name.
    pub name: String,
    /// Project description.
    pub description: String,
    /// Current status (active, archived).
    pub status: String,
    /// Unix timestamp when the project was created.
    pub created_at: i64,
    /// Unix timestamp when the project was last updated.
    pub updated_at: i64,
}

/// Workflow row from SQLite.
#[derive(Debug, Clone)]
pub struct WorkflowRow {
    /// Unique workflow identifier.
    pub id: String,
    /// Human-readable workflow title.
    pub title: String,
    /// Full description of the workflow's objective.
    pub goal: String,
    /// Current workflow status (pending, running, completed, failed, cancelled).
    pub status: String,
    /// Index of the step currently being executed (0-based).
    pub current_step: i64,
    /// Unix timestamp when the workflow was created.
    pub created_at: i64,
    /// Unix timestamp when the workflow was last updated.
    pub updated_at: i64,
    /// Unix timestamp when the workflow completed, if finished.
    pub completed_at: Option<i64>,
    /// Error message if the workflow failed.
    pub error: Option<String>,
    /// Session that created this workflow (FK to sessions).
    pub session_id: Option<String>,
    /// Project this workflow belongs to (FK to projects).
    pub project_id: Option<String>,
    /// Channel to deliver final results to.
    pub delivery_channel: Option<String>,
    /// Target identifier within the delivery channel.
    pub delivery_target: Option<String>,
}

/// Workflow step row from SQLite.
#[derive(Debug, Clone)]
pub struct WorkflowStepRow {
    /// Unique step identifier.
    pub id: i64,
    /// ID of the parent workflow.
    pub workflow_id: String,
    /// Zero-based ordering index within the workflow.
    pub step_index: i64,
    /// Human-readable step title.
    pub title: String,
    /// Prompt/instructions for the agent executing this step.
    pub instructions: String,
    /// Current step status (pending, running, completed, failed, skipped).
    pub status: String,
    /// Accumulated output/result from this step.
    pub output: Option<String>,
    /// Error message if the step failed.
    pub error: Option<String>,
    /// Unix timestamp when the step started executing.
    pub started_at: Option<i64>,
    /// Unix timestamp when the step completed.
    pub completed_at: Option<i64>,
    /// Maximum retry attempts for this step.
    pub max_retries: i32,
    /// Number of retries already attempted.
    pub retry_count: i32,
    /// Per-step execution timeout in milliseconds.
    pub timeout_ms: i64,
}

/// Parameters for creating a new workflow step.
pub struct NewWorkflowStep {
    /// Human-readable step title.
    pub title: String,
    /// Prompt/instructions for the agent executing this step.
    pub instructions: String,
    /// Maximum retry attempts for this step (default 3).
    pub max_retries: i32,
    /// Per-step execution timeout in milliseconds (default 300000).
    pub timeout_ms: i64,
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
