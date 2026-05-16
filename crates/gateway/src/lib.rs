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
/// Ephemeral `/btw` side-question dispatcher shared across native channels.
pub mod btw_dispatch;
/// Shared channel infrastructure: trait, init, registry, routing.
pub mod channel;
pub mod commands;
pub mod constants;
pub mod crypto;
pub mod discord;
/// Typed error surface for the crate's public API.
pub mod error;
pub mod executor;
/// Generic signed-payload webhook surface — `POST /v1/webhook/{name}`
/// with HMAC-SHA256 verification. Lets users wire arbitrary upstream
/// JSON events into the agent loop via per-webhook JSON dot-paths.
pub mod generic_webhook;
/// Native Google Chat integration.
pub mod google_chat;
pub mod handler;
pub mod health;
#[cfg(target_os = "macos")]
pub mod imessage;
pub mod manifest;
/// MCP server — exposes Borg's read-only tools to MCP clients
/// (Claude Desktop, MCP Inspector) over stdio + HTTP transports.
pub mod mcp;
/// Message-content transforms (chunking, dedup, link understanding, plain-text).
pub mod message_processing;
/// OpenAI-compatible HTTP surface — `/v1/chat/completions`, `/v1/models`.
/// Bearer auth via `cortex::auth::tokens`. Lets ChatBox / Open
/// WebUI / the official `openai` SDK point at borg as if it were
/// OpenAI's API.
pub mod openai_compat;
/// Fault-tolerance primitives: backoff, retry, circuit breaker, rate
/// limit, HTTP retry, challenge throttling.
pub mod resilience;
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

pub use channel::registry::ChannelRegistry;
pub use error::{Error, Result};
pub use server::GatewayServer;
pub use session_executor::{InboundTurn, InlineImage, SessionExecutor, SessionExecutorHandle};
