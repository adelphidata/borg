//! Central default values for the entire application.
//!
//! All magic numbers live here. Config `Default` impls and runtime code
//! reference these constants instead of scattering literals.
//!
//! # Tiers
//!
//! Constants here fall into three tiers; each section below is annotated.
//! When adding a constant, pick the tier and place it under the matching
//! section divider:
//!
//! - **Tier A — user-facing defaults.** The corresponding setting is also
//!   registered in `config::settings_table::define_settings!` so a user
//!   can override it at runtime via `/settings`, `borg config set`, or the
//!   admin RPC. Examples: `TOOL_OUTPUT_MAX_TOKENS`, `DEFAULT_LLM_TEMPERATURE`,
//!   `MIN_HEARTBEAT_INTERVAL_SECS`, `BROWSER_DEFAULT_*`.
//!
//! - **Tier B — operational internals.** Tunable in principle, but not
//!   surfaced to end users. Backoff schedules, dedup capacities, retry
//!   factors, cooldowns, watchdog thresholds. Changing these is a code
//!   change, not a config change.
//!
//! - **Tier C — architectural caps.** Hard limits the system relies on for
//!   correctness or safety. `MAX_TOOL_CALLS_PER_RESPONSE`, embedding
//!   dimensions, token-overhead estimates, hash-chain checkpoints. Tweaking
//!   these can break invariants downstream — touch with care.

use std::time::Duration;

// ── Tier A: User-facing (also config keys) ──────────────────────────

/// Max tokens for tool output before truncation (head + tail preserved).
pub const TOOL_OUTPUT_MAX_TOKENS: usize = 10_000;

/// Token budget for skill metadata + instruction context in the system
/// prompt. Default cap for `SkillsConfig::max_context_tokens`; also the
/// threshold asserted by the built-in skills metadata test.
pub const SKILLS_MAX_CONTEXT_TOKENS: usize = 4000;

/// Tokens reserved for the compaction summary marker.
pub const COMPACTION_MARKER_TOKENS: usize = 200;

/// Max characters from the transcript sent to the LLM summarizer.
pub const MAX_TRANSCRIPT_CHARS: usize = 4000;

/// Safety margin for compaction trigger: compact at 85% of budget to leave
/// headroom for the next LLM response. Value is a fraction (0.0–1.0).
pub const COMPACTION_SAFETY_MARGIN: f64 = 0.85;

/// Max body size for gateway webhook requests (2 MB).
pub const GATEWAY_MAX_BODY_SIZE: usize = 2 * 1024 * 1024;

/// Telegram long-polling timeout in seconds.
pub const TELEGRAM_POLL_TIMEOUT_SECS: u64 = 30;

/// Consecutive failures before circuit breaker opens.
pub const TELEGRAM_CIRCUIT_FAILURE_THRESHOLD: u32 = 10;

/// Seconds the circuit breaker stays open after tripping.
pub const TELEGRAM_CIRCUIT_SUSPENSION_SECS: u64 = 300;

/// Capacity of the Telegram update deduplicator.
pub const TELEGRAM_DEDUP_CAPACITY: usize = 1000;

/// Capacity of the Slack event deduplicator.
pub const SLACK_DEDUP_CAPACITY: usize = 5000;

/// Capacity of the Discord interaction deduplicator.
pub const DISCORD_DEDUP_CAPACITY: usize = 5000;

/// Capacity of the Microsoft Teams activity deduplicator. The Bot Framework
/// retries deliveries on 5xx responses, so we dedup by activity ID.
pub const TEAMS_DEDUP_CAPACITY: usize = 2000;

/// Capacity of the Signal SSE message deduplicator.
pub const SIGNAL_DEDUP_CAPACITY: usize = 1000;

/// Capacity of the Slack echo cache (recently sent message hashes).
pub const SLACK_ECHO_CACHE_CAPACITY: usize = 100;

/// Max file download size for Slack attachments (50 MB).
pub const SLACK_MAX_FILE_SIZE: usize = 50 * 1024 * 1024;

/// Max characters shown as tool result preview in REPL.
pub const TOOL_RESULT_PREVIEW_CHARS: usize = 200;

// ── Script execution limits ──────────────────────────────────────────

/// Default timeout for channel script execution (15s).
pub const CHANNEL_DEFAULT_TIMEOUT_MS: u64 = 15_000;

/// Default max concurrent channel handlers.
pub const CHANNEL_DEFAULT_MAX_CONCURRENT: usize = 5;

/// Default timeout for tool execution (30s).
pub const TOOLS_DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Default wall-clock deadline for one builtin tool handler (300s). Clears
/// the slowest legitimate bounded builtins — web fetch, browser navigation,
/// media transcription — while still guaranteeing the turn makes progress.
pub const TOOLS_BUILTIN_DEADLINE_MS: u64 = 300_000;

/// Default timeout for user script execution (60s).
pub const SCRIPTS_DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// Default timeout for audio transcription requests (60s).
pub const AUDIO_DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// Default timeout for text-to-speech synthesis requests (30s).
pub const TTS_DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Default per-task / per-workflow-step execution timeout (5m).
/// Used by `scheduled_tasks.timeout_ms` and workflow step defaults.
pub const SCHEDULED_TASK_DEFAULT_TIMEOUT_MS: u64 = 300_000;

/// Default timeout for seeded nightly/weekly workflows (10m).
pub const SEEDED_WORKFLOW_TIMEOUT_MS: u64 = 600_000;

/// Default `pre_run_command` timeout for scheduled tasks (1m). Seeded as the
/// SQL column default and used when callers omit `pre_run_timeout_ms`.
pub const PRE_RUN_DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// SQLite `busy_timeout` for gateway DB opens (30s).
pub const GATEWAY_BUSY_TIMEOUT_MS: u64 = 30_000;

/// Minimum allowed heartbeat interval in seconds.
pub const MIN_HEARTBEAT_INTERVAL_SECS: u64 = 60;

// ── Tier B: Operational internals ───────────────────────────────────

/// Injection score at or above which content is considered high-risk.
pub const INJECTION_HIGH_RISK_THRESHOLD: u8 = 50;

/// Injection score at or above which content is flagged.
pub const INJECTION_FLAGGED_THRESHOLD: u8 = 20;

/// Rough bytes-per-token estimate for truncation math.
pub const APPROX_BYTES_PER_TOKEN: usize = 4;

/// Maximum input size (bytes) for injection scanning to prevent ReDoS.
pub const MAX_INJECTION_SCAN_BYTES: usize = 64 * 1024;

/// Minimum backoff duration for Telegram polling retries.
pub const TELEGRAM_MIN_BACKOFF: Duration = Duration::from_secs(2);
/// Maximum backoff duration for Telegram polling retries.
pub const TELEGRAM_MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Exponential backoff multiplier for Telegram polling.
pub const TELEGRAM_BACKOFF_FACTOR: f64 = 1.8;
/// Random jitter fraction applied to Telegram backoff delays.
pub const TELEGRAM_JITTER_FRACTION: f64 = 0.25;
/// Timeout after which a Telegram poll is considered stalled.
pub const TELEGRAM_STALL_TIMEOUT: Duration = Duration::from_secs(90);

/// Minimum backoff duration for Signal SSE reconnection.
pub const SIGNAL_SSE_MIN_BACKOFF: Duration = Duration::from_secs(1);
/// Maximum backoff duration for Signal SSE reconnection.
pub const SIGNAL_SSE_MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Exponential backoff multiplier for Signal SSE retries.
pub const SIGNAL_SSE_BACKOFF_FACTOR: f64 = 2.0;
/// Random jitter fraction applied to Signal SSE backoff delays.
pub const SIGNAL_SSE_JITTER_FRACTION: f64 = 0.2;
/// Timeout after which a Signal SSE connection is considered stalled.
pub const SIGNAL_SSE_STALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Max characters per Signal message chunk before splitting.
pub const SIGNAL_MESSAGE_CHUNK_SIZE: usize = 4000;
/// Timeout in seconds for Signal CLI RPC calls.
pub const SIGNAL_RPC_TIMEOUT_SECS: u64 = 10;
/// Maximum attachment size in bytes for Signal downloads (10 MB).
pub const SIGNAL_MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;

/// Maximum number of retries for gateway outbound requests.
pub const RETRY_MAX_RETRIES: u32 = 5;
/// Initial delay in milliseconds before the first retry.
pub const RETRY_INITIAL_DELAY_MS: u64 = 5000;
/// Maximum delay in milliseconds between retries.
pub const RETRY_MAX_DELAY_MS: u64 = 300_000;
/// Exponential backoff multiplier for gateway retries.
pub const RETRY_BACKOFF_FACTOR: f64 = 2.0;
/// Random jitter factor applied to gateway retry delays.
pub const RETRY_JITTER_FACTOR: f64 = 0.1;

/// TTL for iMessage echo cache entries matched by text content.
pub const ECHO_CACHE_TEXT_TTL: Duration = Duration::from_secs(5);
/// TTL for iMessage echo cache entries matched by message ID.
pub const ECHO_CACHE_ID_TTL: Duration = Duration::from_secs(60);

/// Maximum entries in the iMessage self-chat dedup cache.
pub const SELF_CHAT_CACHE_MAX_ENTRIES: usize = 512;
/// TTL for iMessage self-chat cache entries.
pub const SELF_CHAT_CACHE_TTL: Duration = Duration::from_secs(10);

/// Interactive session: tool call count that triggers a warning.
pub const RATE_TOOL_CALLS_WARN: u32 = 200;
/// Interactive session: tool call count that blocks further calls.
pub const RATE_TOOL_CALLS_BLOCK: u32 = 500;
/// Interactive session: shell command count that triggers a warning.
pub const RATE_SHELL_COMMANDS_WARN: u32 = 100;
/// Interactive session: shell command count that blocks further commands.
pub const RATE_SHELL_COMMANDS_BLOCK: u32 = 250;
/// Interactive session: file write count that triggers a warning.
pub const RATE_FILE_WRITES_WARN: u32 = 50;
/// Interactive session: file write count that blocks further writes.
pub const RATE_FILE_WRITES_BLOCK: u32 = 150;
/// Interactive session: memory write count that triggers a warning.
pub const RATE_MEMORY_WRITES_WARN: u32 = 20;
/// Interactive session: memory write count that blocks further writes.
pub const RATE_MEMORY_WRITES_BLOCK: u32 = 50;
/// Interactive session: web request count that triggers a warning.
pub const RATE_WEB_REQUESTS_WARN: u32 = 50;
/// Interactive session: web request count that blocks further requests.
pub const RATE_WEB_REQUESTS_BLOCK: u32 = 150;

/// Gateway session: tool call count that triggers a warning.
pub const GW_RATE_TOOL_CALLS_WARN: u32 = 30;
/// Gateway session: tool call count that blocks further calls.
pub const GW_RATE_TOOL_CALLS_BLOCK: u32 = 50;
/// Gateway session: shell command count that triggers a warning.
pub const GW_RATE_SHELL_COMMANDS_WARN: u32 = 10;
/// Gateway session: shell command count that blocks further commands.
pub const GW_RATE_SHELL_COMMANDS_BLOCK: u32 = 20;
/// Gateway session: file write count that triggers a warning.
pub const GW_RATE_FILE_WRITES_WARN: u32 = 10;
/// Gateway session: file write count that blocks further writes.
pub const GW_RATE_FILE_WRITES_BLOCK: u32 = 20;
/// Gateway session: memory write count that triggers a warning.
pub const GW_RATE_MEMORY_WRITES_WARN: u32 = 5;
/// Gateway session: memory write count that blocks further writes.
pub const GW_RATE_MEMORY_WRITES_BLOCK: u32 = 10;
/// Gateway session: web request count that triggers a warning.
pub const GW_RATE_WEB_REQUESTS_WARN: u32 = 10;
/// Gateway session: web request count that blocks further requests.
pub const GW_RATE_WEB_REQUESTS_BLOCK: u32 = 25;

// ── MCP per-server circuit breaker ────────────────────────────────

/// Consecutive failed `tools/call`s (an `isError` result or a transport
/// error) on one MCP server before its breaker opens. A success anywhere in
/// the run resets the count. Five is enough for the model to fix its own
/// arguments and low enough that a dead server stops eating a turn per call.
pub const MCP_BREAKER_STRIKE_THRESHOLD: u32 = 5;

/// How long an open MCP breaker refuses calls before letting one probe
/// through (half-open). The probe's outcome closes or re-opens it.
pub const MCP_BREAKER_COOLDOWN: Duration = Duration::from_secs(60);

// ── Evolution stage rate limits ───────────────────────────────────
//
// Rate-limit floors per evolution stage. `ActionLimits::apply_stage` uses
// `.max()` against these so explicit user config above the floor is preserved.
// Source of truth: `docs/evolution.md#action-limits-by-stage`.

/// Base stage: tool call warn floor.
pub const EVOLUTION_BASE_TOOL_CALLS_WARN: u32 = 50;
/// Base stage: tool call block floor.
pub const EVOLUTION_BASE_TOOL_CALLS_BLOCK: u32 = 100;
/// Base stage: shell command warn floor.
pub const EVOLUTION_BASE_SHELL_WARN: u32 = 20;
/// Base stage: shell command block floor.
pub const EVOLUTION_BASE_SHELL_BLOCK: u32 = 50;
/// Base stage: file write warn floor.
pub const EVOLUTION_BASE_FILE_WRITES_WARN: u32 = 15;
/// Base stage: file write block floor.
pub const EVOLUTION_BASE_FILE_WRITES_BLOCK: u32 = 30;

/// Evolved stage: tool call warn floor.
pub const EVOLUTION_EVOLVED_TOOL_CALLS_WARN: u32 = 75;
/// Evolved stage: tool call block floor.
pub const EVOLUTION_EVOLVED_TOOL_CALLS_BLOCK: u32 = 150;
/// Evolved stage: shell command warn floor.
pub const EVOLUTION_EVOLVED_SHELL_WARN: u32 = 30;
/// Evolved stage: shell command block floor.
pub const EVOLUTION_EVOLVED_SHELL_BLOCK: u32 = 75;
/// Evolved stage: file write warn floor.
pub const EVOLUTION_EVOLVED_FILE_WRITES_WARN: u32 = 25;
/// Evolved stage: file write block floor.
pub const EVOLUTION_EVOLVED_FILE_WRITES_BLOCK: u32 = 50;

/// Final stage: tool call warn floor.
pub const EVOLUTION_FINAL_TOOL_CALLS_WARN: u32 = 100;
/// Final stage: tool call block floor.
pub const EVOLUTION_FINAL_TOOL_CALLS_BLOCK: u32 = 200;
/// Final stage: shell command warn floor.
pub const EVOLUTION_FINAL_SHELL_WARN: u32 = 50;
/// Final stage: shell command block floor.
pub const EVOLUTION_FINAL_SHELL_BLOCK: u32 = 100;
/// Final stage: file write warn floor.
pub const EVOLUTION_FINAL_FILE_WRITES_WARN: u32 = 40;
/// Final stage: file write block floor.
pub const EVOLUTION_FINAL_FILE_WRITES_BLOCK: u32 = 80;

// ── File reading defaults ──────────────────────────────────────────

/// Default max characters for file/PDF/URL reading tools.
pub const DEFAULT_READ_MAX_CHARS: usize = 50_000;

/// Max image file size for inline rendering (50 MB).
pub const MAX_IMAGE_FILE_SIZE: usize = 50 * 1024 * 1024;

/// Image compression target size (1 MB).
pub const IMAGE_COMPRESSION_TARGET: usize = 1_048_576;

// ── Outbound image ceiling (Anthropic-shaped providers) ────────────

/// Image count above which Anthropic holds *every* image in the request to
/// [`MANY_IMAGE_MAX_SIDE_PX`] per side. Not a count limit on its own.
pub const MANY_IMAGE_THRESHOLD: usize = 20;

/// Per-side pixel cap that applies to each image once a request carries more
/// than [`MANY_IMAGE_THRESHOLD`] images.
pub const MANY_IMAGE_MAX_SIDE_PX: u32 = 2000;

/// Hard maximum number of images Anthropic accepts in one request.
pub const MAX_IMAGES_PER_REQUEST: usize = 100;

/// How many of the oldest tool-result images are retired per step when a
/// request breaches the ceiling. Batching keeps the retired prefix stable
/// across several turns so the prompt cache is not rewritten every call.
pub const IMAGE_RETIRE_BATCH: usize = 8;

/// How many base64 characters of an image are decoded to read its header.
/// JPEG dimensions sit behind APP/EXIF segments that can span tens of KB;
/// a header not found within this window counts as unreadable.
pub const IMAGE_HEADER_B64_PREFIX_CHARS: usize = 96_000;

/// Max document file size for Markdown conversion (100 MB). Conversion reads
/// the whole file into memory and expands it into a document model, so the
/// ceiling guards peak RSS — it is not a correctness limit.
pub const MAX_DOCUMENT_FILE_SIZE: usize = 100 * 1024 * 1024;

/// Max on-disk size for ZIP-container documents — `.docx`, `.xlsx`, `.pptx`,
/// `.odt`, `.ods`, `.odp`, `.epub` (25 MB).
///
/// Deliberately far below [`MAX_DOCUMENT_FILE_SIZE`]: these are archives, so
/// the compressed size bounds nothing. A crafted archive of a few MB expands
/// to gigabytes of XML, and the parser holds all of it. Real documents of this
/// kind are a few MB at most; a 25 MB one is already an outlier.
pub const MAX_CONTAINER_DOCUMENT_FILE_SIZE: usize = 25 * 1024 * 1024;

/// Max Markdown a single document may expand to (64 MB).
///
/// The backstop for what the input ceilings cannot see: whatever the archive
/// held, the rendered text is refused past this rather than being carried
/// through the read path in memory.
pub const MAX_DOCUMENT_MARKDOWN_BYTES: usize = 64 * 1024 * 1024;

// ── Gateway handler limits ─────────────────────────────────────────

/// Max response size from agent before truncation (256 KB).
pub const MAX_RESPONSE_SIZE: usize = 256 * 1024;

/// Max inbound text payload from webhook (32 KB).
pub const MAX_INBOUND_TEXT_BYTES: usize = 32 * 1024;

/// Max bytes of any single free-text field written to the conversation log
/// at `~/.borg/logs/{date}.jsonl` (32 KB).
///
/// Every other capture path is already bounded — hook output at 8 KB, shell
/// output through a head/tail buffer that spills the middle to a file — but
/// the conversation log wrote whatever it was handed. One `fs_read` of a large
/// file, or an MCP result returning a blob, then lands in the daily log whole.
/// Sized well above a normal message so ordinary history is untouched, and far
/// below the point where the file stops being readable.
pub const MAX_LOG_FIELD_BYTES: usize = 32 * 1024;

// ── Embedding provider defaults ───────────────────────────────────

/// Default embedding dimension for OpenAI models (text-embedding-3-small).
pub const OPENAI_EMBEDDING_DIM: usize = 1536;

/// Default embedding dimension for Gemini models (text-embedding-004).
pub const GEMINI_EMBEDDING_DIM: usize = 768;

/// Max characters sent to embedding API (~8000 tokens).
pub const MAX_EMBEDDING_INPUT_CHARS: usize = 32_000;

/// Max number of inputs in a single batched embeddings request. OpenAI
/// caps their own equivalent at 2048; we use a tighter bound because
/// the dispatcher fans each input out as a separate upstream call and
/// large batches turn into quota-burning loops on a stolen token.
pub const MAX_EMBEDDING_BATCH_SIZE: usize = 256;

/// Max characters per message when building session transcripts.
pub const MAX_SESSION_MESSAGE_CHARS: usize = 2000;

// ── Keychain service / account names ──────────────────────────────
//
// Cortex stores two distinct secrets in the OS keychain. The wire-format
// names below are also used as libsecret schema attributes and must match
// exactly what was persisted in prior installs — never change a value here.

/// Keychain service name for the script HMAC key (signs SKILL.md authors).
pub const KEYCHAIN_SERVICE_SCRIPTS: &str = "borg-scripts";

/// Keychain account name for the script HMAC key under [`KEYCHAIN_SERVICE_SCRIPTS`].
pub const KEYCHAIN_ACCOUNT_SCRIPTS_HMAC: &str = "hmac-key";

/// Keychain service name for the evolution Ed25519 signing key.
pub const KEYCHAIN_SERVICE_EVOLUTION: &str = "com.borg.evolution.signing";

/// Keychain account name for the evolution signing key under [`KEYCHAIN_SERVICE_EVOLUTION`].
pub const KEYCHAIN_ACCOUNT_EVOLUTION_SIGNING: &str = "ed25519-secret";

// ── Filenames in ~/.borg/ ─────────────────────────────────────────

/// SQLite database filename under the data directory.
pub const DB_FILE: &str = "borg.db";

/// Agent identity / persona filename under the data directory.
pub const IDENTITY_FILE: &str = "IDENTITY.md";

/// Memory index filename under the data directory.
pub const MEMORY_INDEX_FILE: &str = "MEMORY.md";

/// Heartbeat checklist filename under the data directory.
pub const HEARTBEAT_FILE: &str = "HEARTBEAT.md";

/// First-conversation instructions filename under the data directory.
/// Written by onboarding, consumed by `agent::build_system_prompt`, and
/// retired once the agent has shaped its identity in long-term memory.
pub const SETUP_FILE: &str = "SETUP.md";

/// Sibling marker created atomically by `consume_setup_template` on the
/// very first injection of `SETUP_FILE`. Its presence signals "turn-1
/// tool stripping already fired" so subsequent turns keep SETUP injected
/// but allow tools. Also gates the TUI's one-shot "I just finished
/// onboarding" auto-greeting so it doesn't refire on every restart
/// during the first conversation.
pub const SETUP_FIRST_TURN_MARKER_FILE: &str = "SETUP.md.first_turn_done";

// ── Agent reply tokens ────────────────────────────────────────────

/// Token the agent emits when it has nothing to say (heartbeat acks, silent replies).
pub const SILENT_REPLY_TOKEN: &str = "<SILENT>";

/// Token the agent emits to acknowledge a heartbeat poll with nothing to report.
pub const HEARTBEAT_OK_TOKEN: &str = "HEARTBEAT_OK";

/// How a heartbeat turn treats its `<proactive_nudges>` block. Shared by the
/// agentic heartbeat protocol and the tool-less check-in, so neither lane
/// acks a tick that carries a due nudge. The augmenter cooldowns decide when
/// a nudge is due; the model must not second-guess them from history.
pub const HEARTBEAT_NUDGES_RULE: &str = "A heartbeat tick that carries a <proactive_nudges> \
block always needs attention: Borg sends a nudge only when it is due, so relay every item to \
the owner in your own words, even if an earlier tick carried the same nudge. Never answer it \
with HEARTBEAT_OK or a silence marker.";

// ── Typing indicator ──────────────────────────────────────────────

/// Maximum duration (seconds) before auto-stopping a typing indicator.
pub const TYPING_MAX_TTL_SECS: u64 = 60;

/// Consecutive send failures before stopping the typing keepalive.
pub const TYPING_MAX_CONSECUTIVE_FAILURES: u32 = 2;

// ── Gateway session queues ─────────────────────────────────────────

/// Per-session idle timeout before the session consumer exits (seconds).
pub const SESSION_IDLE_TIMEOUT_SECS: u64 = 300;

/// Per-session message queue capacity before backpressure.
pub const SESSION_QUEUE_CAPACITY: usize = 64;

/// Max concurrent active sessions across all channels.
pub const MAX_ACTIVE_SESSIONS: usize = 10_000;

/// Brief window (ms) to coalesce rapid-fire messages from the same session.
pub const SESSION_COALESCE_WINDOW_MS: u64 = 200;

/// Max sessions a single sender can create across all channels/threads.
pub const MAX_SESSIONS_PER_SENDER: usize = 10;

// ── Session indexing ───────────────────────────────────────────────

/// Max transcript characters to index per session.
pub const MAX_SESSION_TRANSCRIPT_CHARS: usize = 500_000;

// Tool call validation limits
/// Max length of a tool call name from LLM response.
pub const MAX_TOOL_NAME_LEN: usize = 256;
/// Max length of tool call arguments JSON from LLM response (1 MB).
pub const MAX_TOOL_ARGS_LEN: usize = 1_000_000;
/// Max number of tool calls allowed in a single LLM response.
pub const MAX_TOOL_CALLS_PER_RESPONSE: usize = 50;

// Identical-tool-call cap and pre-block backoff live in
// `config::loop_guard::LoopGuardConfig` and `agent::tool_validation::ToolCallRepeatTracker`.

// ── LLM / Agent ───────────────────────────────────────────────────

/// Default sampling temperature for LLM requests.
pub const DEFAULT_LLM_TEMPERATURE: f32 = 0.7;

/// Default maximum tokens in an LLM response. Set high enough that single
/// large file writes (apply_patch with a several-hundred-line body) don't
/// hit the ceiling mid-tool-argument and produce truncated JSON. The 4096
/// historical default routinely truncated `apply_patch` calls because the
/// patch travels as a JSON-string-escaped argument, where every newline /
/// quote / backslash inflates the token count. Modern providers' model-level
/// caps are 8K–32K+; this default sits comfortably under all of them.
pub const DEFAULT_LLM_MAX_TOKENS: u32 = 16_384;

/// Default number of retry attempts on transient LLM errors.
pub const DEFAULT_LLM_MAX_RETRIES: u32 = 3;

/// Default initial retry delay (ms) for LLM requests.
pub const DEFAULT_LLM_INITIAL_RETRY_DELAY_MS: u64 = 200;

/// Default total request timeout (ms) for LLM requests (2 minutes).
///
/// Intentionally generous (120s) so slow thinking-mode requests reach first
/// token. Callers that operate under a shorter external deadline (e.g. gRPC
/// `Send` with a client deadline) should derive a tighter per-call timeout
/// via [`LLM_DEADLINE_SAFETY_MARGIN_MS`] rather than lowering this default.
pub const DEFAULT_LLM_REQUEST_TIMEOUT_MS: u64 = 120_000;

/// Headroom (ms) reserved for translating a deadline-cancel into an
/// `AgentEvent::Error` and flushing it through the gRPC stream before the
/// client's deadline fires. Without this, a stalled LLM call eats the entire
/// budget and the client sees `DEADLINE_EXCEEDED` instead of a structured
/// error event. The deadline watcher in `borgd::session::start_turn` scales
/// this down to `min(margin, remaining_budget / 2)` for tight client
/// deadlines so a fast turn under a small budget isn't tagged with a spurious
/// "deadline approaching" Error.
pub const LLM_DEADLINE_SAFETY_MARGIN_MS: u64 = 5_000;

/// Default per-chunk SSE timeout (seconds) while streaming.
/// Thinking-capable models routinely pause >30s between chunks during reasoning,
/// so the default is generous; the inactivity timer (gateway) is the higher-level guard.
pub const DEFAULT_LLM_STREAM_CHUNK_TIMEOUT_SECS: u64 = 90;

/// Default LLM provider env var name (OpenRouter has the broadest model coverage).
pub const DEFAULT_LLM_API_KEY_ENV: &str = "OPENROUTER_API_KEY";

/// Default heartbeat check-in interval.
pub const DEFAULT_HEARTBEAT_INTERVAL: &str = "60m";

/// Default heartbeat quiet-hours window start (HH:MM, local time).
pub const DEFAULT_HEARTBEAT_QUIET_START: &str = "00:00";

/// Default heartbeat quiet-hours window end (HH:MM, local time).
pub const DEFAULT_HEARTBEAT_QUIET_END: &str = "06:00";

/// SSE streaming buffer size (10 MB).
pub const MAX_SSE_BUFFER: usize = 10 * 1024 * 1024;

/// Cooldown base (seconds) for permanent provider faults: auth and billing.
/// Larger than the transient base so the failover circuit does not flap on
/// 401/403/payment errors that won't self-heal in a few seconds.
pub const PROVIDER_COOLDOWN_AUTH_BASE_SECS: f64 = 300.0;

/// Cooldown base (seconds) for transient provider faults (rate limit, 5xx,
/// network). Short so a brief upstream blip yields back to the primary fast.
pub const PROVIDER_COOLDOWN_TRANSIENT_BASE_SECS: f64 = 60.0;

/// Max exponent applied to the auth/billing cooldown (2^N multiplier cap).
pub const PROVIDER_COOLDOWN_AUTH_MAX_EXP: u32 = 3;

/// Max exponent applied to the transient cooldown (2^N multiplier cap).
pub const PROVIDER_COOLDOWN_TRANSIENT_MAX_EXP: u32 = 4;

/// Upper bound (seconds) on how far a server `Retry-After` may push a
/// provider's failover cooldown. The header parser accepts up to a day; a
/// failed-over client still re-probes the primary within the hour so one
/// absurd header cannot strand it on a fallback.
pub const PROVIDER_COOLDOWN_RETRY_AFTER_MAX_SECS: u64 = 60 * 60;

/// Default token budget for active memory injection into the system prompt.
pub const ACTIVE_INJECTION_DEFAULT_MAX_TOKENS: usize = 1500;

/// Default top-k for active memory injection search.
pub const ACTIVE_INJECTION_DEFAULT_TOP_K: usize = 5;

/// Hard upper bound on tool-call array indices during SSE stream parsing.
/// Prevents OOM from malformed events. Distinct from `MAX_TOOL_CALLS_PER_RESPONSE`
/// which caps how many tool calls are forwarded to execution.
pub const MAX_AGENT_TOOL_CALLS: usize = 128;

/// Character limit when truncating messages for memory flush transcripts.
pub const FLUSH_MESSAGE_TRUNCATE_CHARS: usize = 500;

/// Cap on total transcript characters for memory flush.
pub const FLUSH_TRANSCRIPT_CAP_CHARS: usize = 20_000;

/// Max bytes read from an LLM provider's error response body (64 KiB). Real
/// error bodies are a few hundred bytes; a larger one (an HTML error page, a
/// misbehaving proxy) is classified on its status alone.
pub const LLM_ERROR_BODY_MAX_BYTES: u64 = 64 * 1024;

/// Max bytes read from an OAuth token-refresh response (64 KiB). A token
/// response is well under 8 KiB.
pub const OAUTH_TOKEN_BODY_MAX_BYTES: u64 = 64 * 1024;

/// Max bytes of a remote model-catalog manifest (1 MiB). The embedded
/// snapshot is a few KiB; anything near this cap is not a catalog.
pub const MODEL_CATALOG_MAX_BYTES: u64 = 1024 * 1024;

// ── Conversation / Tokens ─────────────────────────────────────────

/// Approximate token overhead per chat message for role + formatting framing
/// (cl100k_base estimate; close enough for budget math).
pub const MESSAGE_ROLE_TOKEN_OVERHEAD: usize = 4;

/// Conservative token estimate per image (OpenAI high-detail ≈ 765).
pub const IMAGE_TOKEN_ESTIMATE: usize = 765;

/// Minimum token estimate for audio content.
pub const AUDIO_TOKEN_ESTIMATE_MIN: usize = 200;

/// Bytes per token for audio content estimation.
pub const AUDIO_BYTES_PER_TOKEN: usize = 16;

/// Approximate token overhead per tool call for the JSON envelope that wraps
/// each call (id, `"type":"function"`, and the surrounding `{}`/`":"` framing).
/// The model pays for these structural tokens too, so a per-call estimate that
/// counts only the function name and arguments undercounts multi-call turns and
/// mis-times compaction. The `id`/`type` field *values* are counted separately
/// against their actual contents; this constant covers only the framing.
pub const TOOL_CALL_ENVELOPE_TOKEN_OVERHEAD: usize = 8;

/// Token threshold below which tool results are not compacted.
pub const TOOL_RESULT_COMPACT_THRESHOLD: usize = 20;

/// Tool result stored when a call is refused because Plan mode blocks
/// mutations. Compaction matches on it to keep the stub marked `denied`.
pub const PLAN_MODE_MUTATION_DENIED: &str = "Plan mode: mutating operations are not allowed. Use read-only tools (read_file, list_dir, list, memory_search, web_fetch, web_search) to explore the codebase and formulate your plan.";

/// Tool result stored when a `PreToolUse` hook blocks a call without a reason.
pub const TOOL_SKIPPED_BY_HOOK: &str = "[tool call skipped by hook]";

/// Tool result stored when the user cancels the turn before a call runs.
pub const TOOL_CANCELLED_BY_USER: &str = "[tool call cancelled by user]";

// ── Image generation ──────────────────────────────────────────────

/// Timeout in seconds for image generation API requests.
pub const IMAGE_GEN_TIMEOUT_SECS: u64 = 120;

// ── Web ───────────────────────────────────────────────────────────

/// Default outbound HTTP/RPC timeout for non-streaming clients (provider
/// REST calls, image-gen queue submissions, third-party APIs). Streaming
/// clients use `DEFAULT_LLM_REQUEST_TIMEOUT_MS` / per-chunk timeouts.
pub const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout in seconds for web fetch requests.
pub const WEB_FETCH_TIMEOUT_SECS: u64 = 30;

/// Max characters returned from web fetch.
pub const WEB_FETCH_MAX_CHARS: usize = 50_000;

/// Max number of search results to return.
pub const WEB_MAX_SEARCH_RESULTS: usize = 8;

/// Max response body size for web fetches (10 MB).
pub const WEB_MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Max HTTP redirects to follow.
pub const WEB_REDIRECT_LIMIT: usize = 5;

/// Redirect cap for the media fetchers (`vision_analyze`, `video_analyze`).
/// Higher than [`WEB_REDIRECT_LIMIT`] because CDN-hosted assets routinely
/// chain several hops; still bounded so a hostile host cannot stall the fetch
/// forever. Both fetchers must share it — a security bound kept in two copies
/// is the classic drift hazard.
pub const MEDIA_FETCH_REDIRECT_LIMIT: usize = 10;

// ── Project docs ──────────────────────────────────────────────────

/// Maximum total bytes to read from project docs (32 KiB).
pub const MAX_PROJECT_DOC_BYTES: usize = 32 * 1024;

// ── HMAC chain ────────────────────────────────────────────────────

/// How often to write HMAC chain checkpoints (every N verified events).
pub const HMAC_CHECKPOINT_INTERVAL: u32 = 100;

/// Seconds per minute. Used by `usage.rs` and friends for relative-time
/// formatting; the bare `60` left several callers ambiguous (RPM bucket?
/// retry delay? minute-since?).
pub const SECS_PER_MIN: i64 = 60;

/// Seconds per hour, used for rate-limit hour bucketing.
pub const SECS_PER_HOUR: i64 = 3600;

/// Seconds per day, used for interval parsing.
pub const SECS_PER_DAY: i64 = 86_400;

/// Maximum pairing code generation attempts per sender per hour.
pub const PAIRING_MAX_ATTEMPTS_PER_HOUR: u32 = 20;

// ── Browser (CDP) defaults ────────────────────────────────────────

/// Default CDP page-level operation timeout in milliseconds.
pub const BROWSER_DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Default CDP startup/connect timeout in milliseconds.
pub const BROWSER_STARTUP_TIMEOUT_MS: u64 = 15_000;

/// Default inner JS evaluation timeout (ms) for Promise.race wrapper.
pub const BROWSER_JS_EVAL_TIMEOUT_MS: u64 = 10_000;

/// Default capacity of the console log ring buffer.
pub const BROWSER_CONSOLE_BUFFER_SIZE: usize = 500;

/// Default capacity of the page error ring buffer.
pub const BROWSER_ERROR_BUFFER_SIZE: usize = 200;

/// Default capacity of the network request ring buffer.
pub const BROWSER_NETWORK_BUFFER_SIZE: usize = 500;

/// Conventional Chrome DevTools Protocol port. Used by the CDP *attach*
/// flow (`borg browser connect` and its auto-launched debug Chrome).
pub const BROWSER_DEFAULT_CDP_PORT: u16 = 9222;

/// CDP port for locally *launched* (headless) browsers: `0` makes Chrome
/// bind an ephemeral port, which chromiumoxide discovers from the child's
/// stderr. A fixed port here would make concurrent sessions (daemon + TUI,
/// parallel sub-agents, test runners) fail to launch with a bind error.
pub const BROWSER_LOCAL_CDP_PORT_EPHEMERAL: u16 = 0;

/// Default cap on elements returned by `browser.snapshot`. Dense pages
/// otherwise blow LLM context.
pub const BROWSER_SNAPSHOT_MAX_ELEMENTS: usize = 100;

/// Default REST API base for the Browserbase remote-browser backend.
pub const BROWSER_BROWSERBASE_BASE_URL: &str = "https://api.browserbase.com";

/// Default REST API base for the Browser Use remote-browser backend.
pub const BROWSER_BROWSER_USE_BASE_URL: &str = "https://api.browser-use.com";

/// Default REST API base for the Firecrawl remote-browser backend.
pub const BROWSER_FIRECRAWL_BASE_URL: &str = "https://api.firecrawl.dev";

/// Default remote-session lifetime (seconds) requested from a cloud
/// browser backend that accepts a TTL (Firecrawl).
pub const BROWSER_FIRECRAWL_TTL_SECS: u64 = 300;

// ── Gateway defaults ──────────────────────────────────────────────

/// Per-sender error notification cooldown (ms). Default: 4 hours.
pub const ERROR_POLICY_COOLDOWN_MS: u64 = 14_400_000;

/// Default rate limit, per-minute, for gateway inbound messages.
pub const GATEWAY_RATE_LIMIT_PER_MINUTE_DEFAULT: u32 = 60;

/// Default TTL (seconds) for sender pairing codes.
pub const PAIRING_CODE_TTL_SECS: i64 = 3600;

/// Default max characters to extract per link in link-understanding.
pub const LINK_UNDERSTANDING_MAX_CHARS: usize = 5000;

// ── Image generation ──────────────────────────────────────────────

/// Default image generation output dimensions (e.g. "1024x1024").
pub const IMAGE_GEN_DEFAULT_SIZE: &str = "1024x1024";

// ── Multi-agent ───────────────────────────────────────────────────

/// Buffered capacity of the sub-agent event channel.
pub const AGENT_EVENT_CHANNEL_CAPACITY: usize = 256;

// ── External service defaults ─────────────────────────────────────

/// Default port for a locally-running Ollama server.
pub const OLLAMA_PORT_DEFAULT: u16 = 11434;

// ── Daemon loop ───────────────────────────────────────────────────

/// Main daemon loop tick interval.
pub const DAEMON_LOOP_INTERVAL: Duration = Duration::from_secs(60);

/// Watchdog tick interval (how often the watchdog thread checks the main loop).
pub const WATCHDOG_TICK_INTERVAL: Duration = Duration::from_secs(30);

/// If the main loop hasn't updated its heartbeat within this window, the watchdog
/// assumes a deadlock and exits the daemon.
pub const WATCHDOG_STALL_THRESHOLD: Duration = Duration::from_secs(180);

/// Wall-clock gap between ticks above which we assume the host slept/woke.
/// Must be larger than `DAEMON_LOOP_INTERVAL` to avoid false positives.
pub const SLEEP_DRIFT_THRESHOLD: Duration = Duration::from_secs(120);

/// Consecutive gateway crashes in rapid succession before we stop respawning.
pub const GATEWAY_MAX_CRASH_RESPAWNS: u32 = 5;

/// Base delay after a gateway restart/crash before respawning.
pub const GATEWAY_RESPAWN_BASE_DELAY: Duration = Duration::from_millis(250);

/// Consecutive daemon-lock refresh failures before the daemon exits (lock stolen).
pub const DAEMON_LOCK_MAX_REFRESH_FAILURES: u32 = 3;

/// Seconds without a heartbeat refresh before a daemon lock is considered
/// stale. Past this — with a dead holder PID — the lock-takeover path reclaims
/// it and `borg doctor` reports it as orphaned. Single source for the
/// detection grace and the takeover grace so the two can never drift.
pub const DAEMON_LOCK_STALE_SECS: i64 = 300;

/// How long an unclean daemon exit stays worth reporting in `borg doctor`.
/// Past this the information is archaeology, not a diagnosis, and a warning
/// nobody can act on is a warning people learn to ignore.
pub const DAEMON_UNCLEAN_EXIT_WARN_WINDOW_SECS: i64 = 7 * SECS_PER_DAY;

/// `meta` key holding the unix timestamp of the last detected unclean exit.
pub const META_LAST_UNCLEAN_SHUTDOWN_AT: &str = "last_unclean_shutdown_at";
/// `meta` key holding the unix timestamp of the last graceful shutdown.
pub const META_LAST_CLEAN_SHUTDOWN_AT: &str = "last_clean_shutdown_at";

/// `meta` key holding the operator-supplied reason for the current
/// emergency pause (`borg pause --reason …`). Cleared on `borg resume`.
pub const META_EMERGENCY_PAUSE_REASON: &str = "emergency_pause_reason";
/// `meta` key holding the unix timestamp at which the current emergency
/// pause was engaged. Cleared on `borg resume`.
pub const META_EMERGENCY_PAUSE_SINCE: &str = "emergency_pause_since";

/// launchd service label (macOS): the daemon's reverse-DNS identifier; the
/// installed plist is `<label>.plist`. Shared by the service installer
/// (`borgd_launcher`) and the `borg doctor` service-install check so neither
/// probes a diverging path.
pub const LAUNCHD_SERVICE_LABEL: &str = "com.borg.daemon";

/// systemd unit filename (Linux) for the daemon. Same single-source rationale
/// as [`LAUNCHD_SERVICE_LABEL`].
pub const SYSTEMD_UNIT_NAME: &str = "borg.service";

// ── Self-healing ──────────────────────────────────────────────────

/// How often the daemon loop scans for scheduled tasks whose `next_run`
/// has drifted into the past (clock jumps, crashed runs, silent stalls).
pub const STALLED_TASK_SCAN_INTERVAL: Duration = Duration::from_secs(300);

/// A scheduled task is considered stalled when `next_run` is more than
/// this many seconds in the past. Set generously to avoid racing a task
/// that is about to fire on the normal cadence.
pub const STALLED_TASK_GRACE_SECS: i64 = 3600;

/// Lookback window (24h) used by the `<scheduled_tasks>` system-prompt
/// snapshot when counting recent failed/succeeded `task_runs`. Matches the
/// heartbeat augmenter's window so the two surfaces stay in sync.
pub const SCHEDULED_TASKS_WINDOW_SECS: i64 = SECS_PER_DAY;

/// Cap on per-task lines emitted into the `<scheduled_tasks>` snapshot to
/// keep prompt size bounded when many tasks are failing simultaneously.
pub const SCHEDULED_TASKS_MAX_LISTED: usize = 5;

/// Warn when a memory entry exceeds this token count — writes succeed
/// but the entry is close to the hard cap.
pub const MEMORY_ENTRY_WARN_TOKENS: usize = 8_000;

/// Reject memory writes that would produce an entry larger than this.
/// Large entries get silently dropped from the context-window budget;
/// failing at write time forces the caller to split into topics.
pub const MEMORY_ENTRY_REJECT_TOKENS: usize = 20_000;

// ── Telemetry & rate-limit fallbacks ──────────────────────────────

/// How often the prod OTLP exporter flushes accumulated metrics. Below
/// 60s and the collector starts complaining; above it and dashboard
/// freshness suffers. Tests in `telemetry::tests::*` synthesize a
/// longer interval because they don't actually export.
pub const TELEMETRY_FLUSH_INTERVAL: Duration = Duration::from_secs(60);

/// "Retry after this long" returned by the MCP + WASM-plugin rate-limit
/// gates when the operator misconfigured `per_minute=0` or `burst=0`.
/// Failing closed with a finite retry hint keeps the agent's
/// fallback path running rather than silently treating `0` as
/// "unlimited" (the historical footgun).
pub const RATE_LIMIT_MISCONFIGURED_RETRY: Duration = Duration::from_secs(60);

/// Fallback heartbeat interval when `heartbeat.interval` is unparseable.
/// Matches the historical default ("30m × 2 = 1h staleness window").
pub const HEARTBEAT_FALLBACK_INTERVAL: Duration = Duration::from_secs(1800);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_warn_less_than_block() {
        assert!(RATE_TOOL_CALLS_WARN < RATE_TOOL_CALLS_BLOCK);
        assert!(RATE_SHELL_COMMANDS_WARN < RATE_SHELL_COMMANDS_BLOCK);
        assert!(RATE_FILE_WRITES_WARN < RATE_FILE_WRITES_BLOCK);
        assert!(RATE_MEMORY_WRITES_WARN < RATE_MEMORY_WRITES_BLOCK);
        assert!(RATE_WEB_REQUESTS_WARN < RATE_WEB_REQUESTS_BLOCK);
    }

    #[test]
    fn gateway_rate_warn_less_than_block() {
        assert!(GW_RATE_TOOL_CALLS_WARN < GW_RATE_TOOL_CALLS_BLOCK);
        assert!(GW_RATE_SHELL_COMMANDS_WARN < GW_RATE_SHELL_COMMANDS_BLOCK);
        assert!(GW_RATE_FILE_WRITES_WARN < GW_RATE_FILE_WRITES_BLOCK);
        assert!(GW_RATE_MEMORY_WRITES_WARN < GW_RATE_MEMORY_WRITES_BLOCK);
        assert!(GW_RATE_WEB_REQUESTS_WARN < GW_RATE_WEB_REQUESTS_BLOCK);
    }

    #[test]
    fn gateway_rates_stricter_than_interactive() {
        assert!(GW_RATE_TOOL_CALLS_BLOCK < RATE_TOOL_CALLS_BLOCK);
        assert!(GW_RATE_SHELL_COMMANDS_BLOCK < RATE_SHELL_COMMANDS_BLOCK);
        assert!(GW_RATE_FILE_WRITES_BLOCK < RATE_FILE_WRITES_BLOCK);
        assert!(GW_RATE_MEMORY_WRITES_BLOCK < RATE_MEMORY_WRITES_BLOCK);
        assert!(GW_RATE_WEB_REQUESTS_BLOCK < RATE_WEB_REQUESTS_BLOCK);
    }

    #[test]
    fn injection_thresholds_ordered() {
        assert!(INJECTION_FLAGGED_THRESHOLD < INJECTION_HIGH_RISK_THRESHOLD);
    }

    #[test]
    fn telegram_backoff_min_less_than_max() {
        assert!(TELEGRAM_MIN_BACKOFF < TELEGRAM_MAX_BACKOFF);
    }

    #[test]
    fn signal_sse_backoff_min_less_than_max() {
        assert!(SIGNAL_SSE_MIN_BACKOFF < SIGNAL_SSE_MAX_BACKOFF);
    }

    #[test]
    fn retry_initial_less_than_max() {
        assert!(RETRY_INITIAL_DELAY_MS < RETRY_MAX_DELAY_MS);
    }

    #[test]
    fn llm_deadline_safety_margin_is_smaller_than_default_budget() {
        // Pinned by smorgasborg session-send-...05-09 ticket: the deadline
        // watcher's safety margin must be small enough that a healthy default
        // budget still has meaningful runway after subtracting it. A regression
        // that grew the margin past the default budget would make every
        // gRPC-deadline-bounded `Send` fire the watcher immediately on entry.
        assert!(
            LLM_DEADLINE_SAFETY_MARGIN_MS > 0,
            "safety margin must be positive — zero margin defeats the watcher"
        );
        assert!(
            LLM_DEADLINE_SAFETY_MARGIN_MS * 2 < DEFAULT_LLM_REQUEST_TIMEOUT_MS,
            "LLM_DEADLINE_SAFETY_MARGIN_MS ({LLM_DEADLINE_SAFETY_MARGIN_MS}) must be well \
             under DEFAULT_LLM_REQUEST_TIMEOUT_MS ({DEFAULT_LLM_REQUEST_TIMEOUT_MS}) — at \
             least 2x headroom — so a typical LLM call has time to make progress before the \
             watcher fires"
        );
    }

    #[test]
    fn agent_tool_calls_exceeds_per_response() {
        assert!(MAX_AGENT_TOOL_CALLS > MAX_TOOL_CALLS_PER_RESPONSE);
    }

    #[test]
    fn flush_transcript_cap_exceeds_truncate() {
        assert!(FLUSH_TRANSCRIPT_CAP_CHARS > FLUSH_MESSAGE_TRUNCATE_CHARS);
    }

    #[test]
    fn web_max_body_at_least_1mb() {
        assert!(WEB_MAX_BODY_BYTES >= 1024 * 1024);
    }

    #[test]
    fn secs_constants_consistent() {
        assert_eq!(SECS_PER_HOUR, 3600);
        assert_eq!(SECS_PER_DAY, SECS_PER_HOUR * 24);
    }

    #[test]
    fn evolution_stages_monotonically_unlock() {
        assert!(EVOLUTION_BASE_TOOL_CALLS_WARN <= EVOLUTION_BASE_TOOL_CALLS_BLOCK);
        assert!(EVOLUTION_EVOLVED_TOOL_CALLS_WARN <= EVOLUTION_EVOLVED_TOOL_CALLS_BLOCK);
        assert!(EVOLUTION_FINAL_TOOL_CALLS_WARN <= EVOLUTION_FINAL_TOOL_CALLS_BLOCK);

        assert!(EVOLUTION_BASE_TOOL_CALLS_BLOCK <= EVOLUTION_EVOLVED_TOOL_CALLS_BLOCK);
        assert!(EVOLUTION_EVOLVED_TOOL_CALLS_BLOCK <= EVOLUTION_FINAL_TOOL_CALLS_BLOCK);
        assert!(EVOLUTION_BASE_SHELL_BLOCK <= EVOLUTION_EVOLVED_SHELL_BLOCK);
        assert!(EVOLUTION_EVOLVED_SHELL_BLOCK <= EVOLUTION_FINAL_SHELL_BLOCK);
        assert!(EVOLUTION_BASE_FILE_WRITES_BLOCK <= EVOLUTION_EVOLVED_FILE_WRITES_BLOCK);
        assert!(EVOLUTION_EVOLVED_FILE_WRITES_BLOCK <= EVOLUTION_FINAL_FILE_WRITES_BLOCK);
    }
}

/// Serializes tests that mutate [`DEFAULT_LLM_API_KEY_ENV`] in the process
/// environment.
///
/// It lives beside the constant on purpose: several test modules reach for the
/// same variable, and two privately-held locks would serialize nothing while
/// looking like they did. Under a parallel runner one test then observes
/// another's `set_var`/`remove_var`.
#[cfg(test)]
pub(crate) fn default_llm_api_key_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
