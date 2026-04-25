use std::time::Duration;

/// Default HTTP timeout for gateway API clients.
pub const GATEWAY_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Default message chunk size for platforms with ~4000 char limits.
pub const DEFAULT_MESSAGE_CHUNK_SIZE: usize = 4000;

/// Peer kind for direct (1:1) messages.
pub const PEER_KIND_DIRECT: &str = "direct";

/// Peer kind for group/channel messages.
pub const PEER_KIND_GROUP: &str = "group";

// ── Telegram ──────────────────────────────────────────────────────

/// Telegram API base URL.
pub const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

/// Telegram HTTP connect timeout.
pub const TELEGRAM_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Signal HTTP connect timeout for the signal-cli JSON-RPC daemon.
pub const SIGNAL_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Max characters preserved for the inbound `thread_id` after sanitization.
/// The session key format `{sender_id}:{thread_id}` cannot tolerate unbounded
/// growth, so the sanitizer truncates here.
pub const MAX_THREAD_ID_LEN: usize = 128;

/// Max characters of Discord guild channel history to inject as context
/// before the current message.
pub const DISCORD_CHANNEL_CONTEXT_MAX_CHARS: usize = 8000;

// ── Poll loop (Discord/etc.) ──────────────────────────────────────

/// Initial backoff applied after the first consecutive poll error.
pub const POLL_INITIAL_BACKOFF: Duration = Duration::from_secs(5);

/// Cap on per-cycle poll backoff; also the pause length when the
/// consecutive-error budget is exhausted before recycling.
pub const POLL_MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Consecutive poll errors tolerated before the loop pauses for one
/// `POLL_MAX_BACKOFF` window and resets the error counter.
pub const POLL_MAX_CONSECUTIVE_ERRORS: u32 = 10;

/// Max retries for Telegram send operations.
pub const TELEGRAM_MAX_SEND_RETRIES: u32 = 5;

/// Max retry_after seconds we'll honor from Telegram before giving up.
pub const TELEGRAM_MAX_RETRY_AFTER_SECS: u64 = 300;

// ── Slack ─────────────────────────────────────────────────────────

/// Slack API base URL.
pub const SLACK_API_BASE: &str = "https://slack.com/api";

/// Slack signature replay window in seconds.
pub const SLACK_REPLAY_WINDOW_SECS: u64 = 300;

/// Slack typing indicator circuit breaker: trips after N consecutive failures.
pub const SLACK_TYPING_CB_FAILURE_THRESHOLD: u32 = 2;

/// Slack typing indicator circuit breaker: suspension duration in seconds.
pub const SLACK_TYPING_CB_SUSPENSION_SECS: u64 = 60;

// ── Discord ───────────────────────────────────────────────────────

/// Discord REST API base URL.
pub const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

/// Discord message max length.
pub const DISCORD_MESSAGE_CHUNK_SIZE: usize = 2000;

// ── Twilio ────────────────────────────────────────────────────────

/// Twilio REST API base URL (2010-04-01 API version).
pub const TWILIO_API_BASE: &str = "https://api.twilio.com/2010-04-01";

// ── Google Chat ───────────────────────────────────────────────────

/// Google Chat API base URL.
pub const GOOGLE_CHAT_API_BASE: &str = "https://chat.googleapis.com/v1";

/// Google Chat max message length (4096 chars).
pub const GOOGLE_CHAT_MESSAGE_CHUNK_SIZE: usize = 4096;
