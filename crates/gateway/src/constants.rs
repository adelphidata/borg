use std::time::Duration;

/// Default HTTP timeout for gateway API clients. Re-export of
/// `cortex::constants::DEFAULT_HTTP_TIMEOUT` so call sites can name it
/// in gateway-flavored language without forking the value.
pub use cortex::constants::DEFAULT_HTTP_TIMEOUT as GATEWAY_HTTP_TIMEOUT;

/// Sentinel binding id assigned when no `[[gateway.bindings]]` rule matched
/// the inbound message. Compared in routing logic to distinguish a
/// fallback route from a user-configured one.
pub const DEFAULT_BINDING_ID: &str = "default";

/// Default message chunk size for platforms with ~4000 char limits.
/// Why: Telegram caps a single sendMessage at 4096 chars; rounding to 4000
/// leaves headroom for trailing metadata appended by the sender path.
pub const DEFAULT_MESSAGE_CHUNK_SIZE: usize = 4000;

/// Peer kind for direct (1:1) messages.
pub const PEER_KIND_DIRECT: &str = "direct";

/// Peer kind for group/channel messages.
pub const PEER_KIND_GROUP: &str = "group";

/// HTTP timeout for the daemon-internal `/internal/poke` request.
/// Why: this is a loopback call to our own gateway; 5s is generous for a
/// local round-trip while still bounding a hung handler.
pub const INTERNAL_POKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Tick cadence for the per-session inactivity monitor loop.
/// Why: 1s gives second-resolution checks against the (config-driven)
/// inactivity warning/notify/timeout thresholds without busy-spinning.
pub const INACTIVITY_MONITOR_TICK: Duration = Duration::from_secs(1);

// ── Telegram ──────────────────────────────────────────────────────

/// Telegram API base URL.
pub const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

/// Telegram HTTP connect timeout.
pub const TELEGRAM_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Chunk budget applied to the *markdown source* before per-chunk HTML
/// rendering on the Telegram delivery path. Smaller than
/// [`DEFAULT_MESSAGE_CHUNK_SIZE`] to leave headroom for HTML expansion
/// (entity escaping, `<pre>`/`<a>` tags) so a rendered chunk still fits
/// Telegram's 4096-char message cap without a second, markup-blind split.
pub const TELEGRAM_MARKDOWN_CHUNK_SIZE: usize = 3500;

/// Per-request deadline for Telegram media uploads (photo/video/document/
/// voice/animation). These POST multipart bodies up to
/// [`TELEGRAM_MAX_FILE_SIZE_BYTES`] and then wait for server-side
/// processing before the response arrives — the client-wide
/// [`GATEWAY_HTTP_TIMEOUT`] (30s whole-request) routinely aborts a large
/// upload on a slow uplink. Applied as a reqwest per-request override.
pub const TELEGRAM_MEDIA_SEND_TIMEOUT: Duration = Duration::from_secs(120);

/// Signal HTTP connect timeout for the signal-cli JSON-RPC daemon.
pub const SIGNAL_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default port for a locally-running signal-cli JSON-RPC server.
pub const SIGNAL_CLI_PORT_DEFAULT: u16 = 8080;

/// Signal API circuit breaker: trips after N consecutive failures.
/// Why: signal-cli RPC bounces during restarts; 5 absorbs short hiccups
/// without leaving real outages undetected for too long.
pub const SIGNAL_CIRCUIT_FAILURE_THRESHOLD: u32 = 5;

/// Signal API circuit breaker: suspension duration in seconds.
/// Why: matches typical signal-cli restart window so the breaker re-probes
/// roughly when the daemon is ready again.
pub const SIGNAL_CIRCUIT_SUSPENSION_SECS: u64 = 60;

/// Max characters preserved for the inbound `thread_id` after sanitization.
/// The session key format `{sender_id}:{thread_id}` cannot tolerate unbounded
/// growth, so the sanitizer truncates here.
pub const MAX_THREAD_ID_LEN: usize = 128;

/// Max characters of Discord guild channel history to inject as context
/// before the current message.
/// Why: ~2k tokens worth of prior chat is enough for thread coherence
/// without blowing the per-turn prompt budget.
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
/// Why: Telegram occasionally returns retry_after >> minutes during flood waits;
/// 300s caps the wait so a stuck send doesn't pin a worker indefinitely.
pub const TELEGRAM_MAX_RETRY_AFTER_SECS: u64 = 300;

/// Max bytes we'll download from Telegram's getFile endpoint to avoid memory exhaustion.
/// Why: Telegram's Bot API caps getFile at 20 MiB; 25 MiB matches the documented
/// ceiling with a small margin for header/envelope overhead.
pub const TELEGRAM_MAX_FILE_SIZE_BYTES: u64 = 25 * 1024 * 1024;

/// Max bytes downloaded for a channel attachment (Teams / Twilio /
/// Google Chat voice-message audio and similar) before the fetch is aborted.
/// Why: these arrive from provider CDNs whose Content-Length can lie or be
/// absent, so the cap is enforced while streaming; 25 MiB comfortably fits
/// every voice note while bounding memory.
pub const ATTACHMENT_DOWNLOAD_MAX_BYTES: u64 = 25 * 1024 * 1024;

// ── Slack ─────────────────────────────────────────────────────────

/// Slack API base URL.
pub const SLACK_API_BASE: &str = "https://slack.com/api";

/// Slack signature replay window in seconds.
/// Why: matches Slack's documented signing window (5 min); requests with a
/// timestamp outside this window must be rejected as replays.
pub const SLACK_REPLAY_WINDOW_SECS: u64 = 300;

/// Discord interaction replay window in seconds.
/// Why: Discord signs `timestamp + body` with Ed25519, so the timestamp is
/// tamper-proof — but the signature alone does not stop a captured, validly
/// signed request from being replayed to our endpoint. Rejecting requests whose
/// signed timestamp is outside this window closes that replay gap.
pub const DISCORD_REPLAY_WINDOW_SECS: u64 = 300;

/// Slack typing indicator circuit breaker: trips after N consecutive failures.
/// Why: typing indicators are cosmetic; bail fast (2 strikes) rather than
/// burn rate-limit budget that real sends will need.
pub const SLACK_TYPING_CB_FAILURE_THRESHOLD: u32 = 2;

/// Slack typing indicator circuit breaker: suspension duration in seconds.
/// Why: short enough to recover quickly when the API comes back, long enough
/// to skip several would-be typing pings during an outage.
pub const SLACK_TYPING_CB_SUSPENSION_SECS: u64 = 60;

// ── Discord ───────────────────────────────────────────────────────

/// Discord REST API base URL.
pub const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

/// Discord message max length.
/// Why: Discord's REST API hard-rejects content > 2000 chars on non-Nitro sends.
pub const DISCORD_MESSAGE_CHUNK_SIZE: usize = 2000;

// ── Twilio ────────────────────────────────────────────────────────

/// Twilio REST API base URL (2010-04-01 API version).
pub const TWILIO_API_BASE: &str = "https://api.twilio.com/2010-04-01";

// ── Google Chat ───────────────────────────────────────────────────

/// Google Chat API base URL.
pub const GOOGLE_CHAT_API_BASE: &str = "https://chat.googleapis.com/v1";

/// Google Chat max message length (4096 chars).
/// Why: Google Chat enforces a 4096-char ceiling per message body.
pub const GOOGLE_CHAT_MESSAGE_CHUNK_SIZE: usize = 4096;
