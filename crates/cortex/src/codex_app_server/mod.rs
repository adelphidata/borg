//! Opt-in codex `app-server` runtime for OpenAI/Codex turns.
//!
//! When `model.openai_runtime == "codex_app_server"`, an OpenAI/Codex turn is
//! handed off to the `codex` CLI's `app-server` subprocess instead of running
//! through borg's native tool loop. The subprocess provides `shell`,
//! `apply_patch`, `update_plan`, `view_image`, and any installed plugins inside
//! its own seatbelt/landlock sandbox. Borg's MCP callback re-exposes borg-only
//! tools like `web_fetch`, `browser`, `memory_search`, etc., so the subprocess
//! can call back into borg.
//!
//! Default behavior is unchanged unless the runtime is explicitly enabled.
//!
//! Module layout:
//!
//! - `protocol` — JSON-RPC wire types + framing.
//! - `client`   — async subprocess + reader + request/response correlation.

pub mod client;
pub mod protocol;

pub use client::{
    parse_codex_version, ClientError, CodexAppServerClient, Notification, ServerRequest,
    SpawnOptions, INITIALIZE_TIMEOUT, MIN_CODEX_VERSION,
};
pub use protocol::{InboundFrame, RequestId, RpcError};
