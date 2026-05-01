//! Webhook gateway for messaging channel integrations.

#![cfg_attr(
    test,
    allow(
        clippy::approx_constant,
        clippy::assertions_on_constants,
        clippy::const_is_empty,
        clippy::expect_used,
        clippy::field_reassign_with_default,
        clippy::identity_op,
        clippy::items_after_test_module,
        clippy::len_zero,
        clippy::manual_range_contains,
        clippy::needless_borrow,
        clippy::needless_collect,
        clippy::redundant_clone,
        clippy::redundant_closure_for_method_calls,
        clippy::uninlined_format_args,
        clippy::unnecessary_cast,
        clippy::unnecessary_map_or,
        clippy::unwrap_used,
        clippy::useless_format,
        clippy::useless_vec
    )
)]

pub mod auto_reply;
pub mod backoff;
pub mod challenge_throttle;
mod channel_init;
pub mod channel_trait;
pub mod chunker;
pub mod circuit_breaker;
pub mod commands;
pub mod constants;
pub mod crypto;
pub mod dedup;
pub mod discord;
pub mod error_dedup;
pub mod executor;
/// Generic signed-payload webhook surface — `POST /v1/webhook/{name}`
/// with HMAC-SHA256 verification. Lets users wire arbitrary upstream
/// JSON events into the agent loop via per-webhook JSON dot-paths.
pub mod generic_webhook;
/// Native Google Chat integration.
pub mod google_chat;
pub mod handler;
pub mod health;
pub mod http_retry;
#[cfg(target_os = "macos")]
pub mod imessage;
pub mod link_understanding;
pub mod manifest;
/// MCP server — exposes Borg's read-only tools to MCP clients
/// (Claude Desktop, MCP Inspector) over stdio + HTTP transports.
pub mod mcp;
/// OpenAI-compatible HTTP surface — `/v1/chat/completions`, `/v1/models`.
/// Bearer auth via `cortex::auth::tokens`. Lets ChatBox / Open
/// WebUI / the official `openai` SDK point at borg as if it were
/// OpenAI's API.
pub mod openai_compat;
/// Helpers for plain-text channels that cannot render Markdown code fences.
pub mod plain_text;
pub mod rate_limit;
pub mod registry;
pub mod retry;
pub mod routing;
pub mod server;
pub mod session_executor;
pub mod session_queue;
pub mod signal;
pub mod slack;
pub mod teams;
pub mod telegram;
pub mod twilio;
pub(crate) mod typing_keepalive;
pub mod verify_common;
pub mod voice;

pub use registry::ChannelRegistry;
pub use server::GatewayServer;
pub use session_executor::{InboundTurn, InlineImage, SessionExecutor, SessionExecutorHandle};
