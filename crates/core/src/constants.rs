//! Central default values for the entire application.
//!
//! All magic numbers live here. Config `Default` impls and runtime code
//! reference these constants instead of scattering literals.

use std::time::Duration;

// ── Tier A: User-facing (also config keys) ──────────────────────────

/// Max tokens for tool output before truncation (head + tail preserved).
pub const TOOL_OUTPUT_MAX_TOKENS: usize = 4000;

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

// ── File reading defaults ──────────────────────────────────────────

/// Default max characters for file/PDF/URL reading tools.
pub const DEFAULT_READ_MAX_CHARS: usize = 50_000;

/// Max image file size for inline rendering (50 MB).
pub const MAX_IMAGE_FILE_SIZE: usize = 50 * 1024 * 1024;

/// Image compression target size (1 MB).
pub const IMAGE_COMPRESSION_TARGET: usize = 1_048_576;

// ── Gateway handler limits ─────────────────────────────────────────

/// Max response size from agent before truncation (256 KB).
pub const MAX_RESPONSE_SIZE: usize = 256 * 1024;

/// Max inbound text payload from webhook (32 KB).
pub const MAX_INBOUND_TEXT_BYTES: usize = 32 * 1024;

// ── Embedding provider defaults ───────────────────────────────────

/// Default embedding dimension for OpenAI models (text-embedding-3-small).
pub const OPENAI_EMBEDDING_DIM: usize = 1536;

/// Default embedding dimension for Gemini models (text-embedding-004).
pub const GEMINI_EMBEDDING_DIM: usize = 768;

/// Max characters sent to embedding API (~8000 tokens).
pub const MAX_EMBEDDING_INPUT_CHARS: usize = 32_000;

/// Max characters per message when building session transcripts.
pub const MAX_SESSION_MESSAGE_CHARS: usize = 2000;

// ── Filenames in ~/.borg/ ─────────────────────────────────────────

/// SQLite database filename under the data directory.
pub const DB_FILE: &str = "borg.db";

/// Agent identity / persona filename under the data directory.
pub const IDENTITY_FILE: &str = "IDENTITY.md";

/// Memory index filename under the data directory.
pub const MEMORY_INDEX_FILE: &str = "MEMORY.md";

/// Heartbeat checklist filename under the data directory.
pub const HEARTBEAT_FILE: &str = "HEARTBEAT.md";

// ── Agent reply tokens ────────────────────────────────────────────

/// Token the agent emits when it has nothing to say (heartbeat acks, silent replies).
pub const SILENT_REPLY_TOKEN: &str = "<SILENT>";

/// Token the agent emits to acknowledge a heartbeat poll with nothing to report.
pub const HEARTBEAT_OK_TOKEN: &str = "HEARTBEAT_OK";

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

// ── LLM / Agent ───────────────────────────────────────────────────

/// Default sampling temperature for LLM requests.
pub const DEFAULT_LLM_TEMPERATURE: f32 = 0.7;

/// Default maximum tokens in an LLM response.
pub const DEFAULT_LLM_MAX_TOKENS: u32 = 4096;

/// Default number of retry attempts on transient LLM errors.
pub const DEFAULT_LLM_MAX_RETRIES: u32 = 3;

/// Default initial retry delay (ms) for LLM requests.
pub const DEFAULT_LLM_INITIAL_RETRY_DELAY_MS: u64 = 200;

/// Default total request timeout (ms) for LLM requests (2 minutes).
pub const DEFAULT_LLM_REQUEST_TIMEOUT_MS: u64 = 120_000;

/// Default per-chunk SSE timeout (seconds) while streaming.
/// Thinking-capable models routinely pause >30s between chunks during reasoning,
/// so the default is generous; the inactivity timer (gateway) is the higher-level guard.
pub const DEFAULT_LLM_STREAM_CHUNK_TIMEOUT_SECS: u64 = 90;

/// Default LLM provider env var name (OpenRouter has the broadest model coverage).
pub const DEFAULT_LLM_API_KEY_ENV: &str = "OPENROUTER_API_KEY";

/// Default heartbeat check-in interval.
pub const DEFAULT_HEARTBEAT_INTERVAL: &str = "30m";

/// Default heartbeat quiet-hours window start (HH:MM, local time).
pub const DEFAULT_HEARTBEAT_QUIET_START: &str = "00:00";

/// Default heartbeat quiet-hours window end (HH:MM, local time).
pub const DEFAULT_HEARTBEAT_QUIET_END: &str = "06:00";

/// Default timeout (seconds) when waiting for a sub-agent to complete.
pub const DEFAULT_SUB_AGENT_TIMEOUT_SECS: u64 = 300;

/// SSE streaming buffer size (10 MB).
pub const MAX_SSE_BUFFER: usize = 10 * 1024 * 1024;

/// Hard upper bound on tool-call array indices during SSE stream parsing.
/// Prevents OOM from malformed events. Distinct from `MAX_TOOL_CALLS_PER_RESPONSE`
/// which caps how many tool calls are forwarded to execution.
pub const MAX_AGENT_TOOL_CALLS: usize = 128;

/// Character limit when truncating messages for memory flush transcripts.
pub const FLUSH_MESSAGE_TRUNCATE_CHARS: usize = 500;

/// Cap on total transcript characters for memory flush.
pub const FLUSH_TRANSCRIPT_CAP_CHARS: usize = 20_000;

// ── Conversation / Tokens ─────────────────────────────────────────

/// Conservative token estimate per image (OpenAI high-detail ≈ 765).
pub const IMAGE_TOKEN_ESTIMATE: usize = 765;

/// Minimum token estimate for audio content.
pub const AUDIO_TOKEN_ESTIMATE_MIN: usize = 200;

/// Bytes per token for audio content estimation.
pub const AUDIO_BYTES_PER_TOKEN: usize = 16;

/// Token threshold below which tool results are not compacted.
pub const TOOL_RESULT_COMPACT_THRESHOLD: usize = 20;

// ── Image generation ──────────────────────────────────────────────

/// Timeout in seconds for image generation API requests.
pub const IMAGE_GEN_TIMEOUT_SECS: u64 = 120;

// ── Web ───────────────────────────────────────────────────────────

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

// ── Project docs ──────────────────────────────────────────────────

/// Maximum total bytes to read from project docs (32 KiB).
pub const MAX_PROJECT_DOC_BYTES: usize = 32 * 1024;

// ── HMAC chain ────────────────────────────────────────────────────

/// How often to write HMAC chain checkpoints (every N verified events).
pub const HMAC_CHECKPOINT_INTERVAL: u32 = 100;

/// Seconds per hour, used for rate-limit hour bucketing.
pub const SECS_PER_HOUR: i64 = 3600;

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

/// Default Chrome DevTools Protocol port.
pub const BROWSER_DEFAULT_CDP_PORT: u16 = 9222;

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

/// Default port for a locally-running signal-cli JSON-RPC server.
pub const SIGNAL_CLI_PORT_DEFAULT: u16 = 8080;

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

// ── Self-healing ──────────────────────────────────────────────────

/// How often the daemon loop scans for scheduled tasks whose `next_run`
/// has drifted into the past (clock jumps, crashed runs, silent stalls).
pub const STALLED_TASK_SCAN_INTERVAL: Duration = Duration::from_secs(300);

/// A scheduled task is considered stalled when `next_run` is more than
/// this many seconds in the past. Set generously to avoid racing a task
/// that is about to fire on the normal cadence.
pub const STALLED_TASK_GRACE_SECS: i64 = 3600;

/// Warn when a memory entry exceeds this token count — writes succeed
/// but the entry is close to the hard cap.
pub const MEMORY_ENTRY_WARN_TOKENS: usize = 8_000;

/// Reject memory writes that would produce an entry larger than this.
/// Large entries get silently dropped from the context-window budget;
/// failing at write time forces the caller to split into topics.
pub const MEMORY_ENTRY_REJECT_TOKENS: usize = 20_000;

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
    fn tool_output_max_tokens_positive() {
        assert!(TOOL_OUTPUT_MAX_TOKENS > 0);
    }

    #[test]
    fn compaction_safety_margin_in_range() {
        assert!(COMPACTION_SAFETY_MARGIN > 0.0);
        assert!(COMPACTION_SAFETY_MARGIN < 1.0);
    }

    #[test]
    fn gateway_max_body_size_reasonable() {
        assert!(GATEWAY_MAX_BODY_SIZE >= 1024 * 1024); // at least 1 MB
    }

    #[test]
    fn max_tool_name_len_and_args_positive() {
        assert!(MAX_TOOL_NAME_LEN > 0);
        assert!(MAX_TOOL_ARGS_LEN > 0);
        assert!(MAX_TOOL_CALLS_PER_RESPONSE > 0);
    }

    #[test]
    fn backoff_factors_greater_than_one() {
        assert!(TELEGRAM_BACKOFF_FACTOR > 1.0);
        assert!(SIGNAL_SSE_BACKOFF_FACTOR > 1.0);
        assert!(RETRY_BACKOFF_FACTOR > 1.0);
    }

    #[test]
    fn jitter_fractions_in_zero_one_range() {
        assert!(TELEGRAM_JITTER_FRACTION > 0.0 && TELEGRAM_JITTER_FRACTION < 1.0);
        assert!(SIGNAL_SSE_JITTER_FRACTION > 0.0 && SIGNAL_SSE_JITTER_FRACTION < 1.0);
        assert!(RETRY_JITTER_FACTOR > 0.0 && RETRY_JITTER_FACTOR < 1.0);
    }

    #[test]
    fn echo_cache_ttls_positive() {
        assert!(!ECHO_CACHE_TEXT_TTL.is_zero());
        assert!(!ECHO_CACHE_ID_TTL.is_zero());
        assert!(!SELF_CHAT_CACHE_TTL.is_zero());
    }

    #[test]
    fn dedup_capacities_positive() {
        assert!(TELEGRAM_DEDUP_CAPACITY > 0);
        assert!(SLACK_DEDUP_CAPACITY > 0);
        assert!(DISCORD_DEDUP_CAPACITY > 0);
        assert!(TEAMS_DEDUP_CAPACITY > 0);
        assert!(SLACK_ECHO_CACHE_CAPACITY > 0);
        assert!(SELF_CHAT_CACHE_MAX_ENTRIES > 0);
    }

    #[test]
    fn agent_tool_calls_exceeds_per_response() {
        assert!(MAX_AGENT_TOOL_CALLS > MAX_TOOL_CALLS_PER_RESPONSE);
    }

    #[test]
    fn flush_limits_positive() {
        assert!(FLUSH_MESSAGE_TRUNCATE_CHARS > 0);
        assert!(FLUSH_TRANSCRIPT_CAP_CHARS > FLUSH_MESSAGE_TRUNCATE_CHARS);
    }

    #[test]
    fn token_estimates_positive() {
        assert!(IMAGE_TOKEN_ESTIMATE > 0);
        assert!(AUDIO_TOKEN_ESTIMATE_MIN > 0);
        assert!(AUDIO_BYTES_PER_TOKEN > 0);
        assert!(TOOL_RESULT_COMPACT_THRESHOLD > 0);
    }

    #[test]
    fn web_constants_positive() {
        assert!(WEB_FETCH_TIMEOUT_SECS > 0);
        assert!(WEB_FETCH_MAX_CHARS > 0);
        assert!(WEB_MAX_SEARCH_RESULTS > 0);
        assert!(WEB_MAX_BODY_BYTES >= 1024 * 1024);
        assert!(WEB_REDIRECT_LIMIT > 0);
    }

    #[test]
    fn embedding_dims_positive() {
        assert!(OPENAI_EMBEDDING_DIM > 0);
        assert!(GEMINI_EMBEDDING_DIM > 0);
        assert!(MAX_EMBEDDING_INPUT_CHARS > 0);
    }

    #[test]
    fn session_and_project_limits_positive() {
        assert!(MAX_SESSION_MESSAGE_CHARS > 0);
        assert!(MAX_PROJECT_DOC_BYTES > 0);
    }

    #[test]
    fn hmac_constants_valid() {
        assert!(HMAC_CHECKPOINT_INTERVAL > 0);
        assert_eq!(SECS_PER_HOUR, 3600);
    }
}
