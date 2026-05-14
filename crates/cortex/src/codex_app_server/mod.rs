//! Opt-in codex `app-server` runtime for OpenAI/Codex turns.
//!
//! When `model.openai_runtime == "codex_app_server"`, an OpenAI/Codex turn is
//! handed off to the `codex` CLI's `app-server` subprocess instead of running
//! through borg's native tool loop. Codex provides `shell`, `apply_patch`,
//! `update_plan`, `view_image`, and any installed codex plugins inside its own
//! seatbelt/landlock sandbox. Borg's MCP callback (separate module, lands in a
//! later stage) re-exposes borg-only tools like `web_fetch`, `browser`,
//! `memory_search`, etc., so the codex side can call back into borg.
//!
//! Default behavior is unchanged unless the runtime is explicitly enabled.
//!
//! Stage layout (see plan):
//!
//! - `protocol` — JSON-RPC wire types + framing. **Implemented.**
//! - `client`   — async subprocess + reader + request/response correlation.
//!   **Implemented.**
//! - `projector` — translates codex `item/*` notifications into borg `Message`s.
//!   Lands in Stage 2.
//! - `turn`     — runtime entrypoint that drives a turn end-to-end.
//!   Lands in Stage 3.
//! - `migration` — `~/.codex/config.toml` managed-block rewrite. Lands in Stage 5.
//! - `approval`  — auto-approve handler with audit-chain emission. Lands in Stage 3.

pub mod client;
pub mod protocol;

pub use client::{
    parse_codex_version, ClientError, CodexAppServerClient, Notification, ServerRequest,
    SpawnOptions, INITIALIZE_TIMEOUT, MIN_CODEX_VERSION,
};
pub use protocol::{InboundFrame, RequestId, RpcError};
