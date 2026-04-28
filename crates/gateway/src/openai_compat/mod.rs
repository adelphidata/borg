//! OpenAI-compatible HTTP surface.
//!
//! Exposes `POST /v1/chat/completions` (stream + non-stream) and
//! `GET /v1/models`. Auth is HTTP `Authorization: Bearer <token>` via
//! [`cortex::auth::tokens`]. Each request is normalized into a
//! [`cortex::ingress::IngressMessage`], gated through
//! [`cortex::ingress::IngressGate`] for rate-limit + sanitization +
//! audit, and dispatched into the agent loop via the `ChatDispatcher`
//! trait. Tests inject a fake dispatcher; production wires
//! `AgentDispatcher` which spawns a `cortex::agent::Agent` per
//! request (mirrors the existing channel handler).
//!
//! Tool-call passthrough: agent `ToolExecuting` events are surfaced as
//! OpenAI `tool_calls` deltas in the streaming response. Client tool
//! results (role=`tool` messages on the next request) are not yet
//! threaded back into the agent's tool loop — clients that want
//! agent-driven tool execution should let the agent run its tools
//! server-side and consume the resulting text.

mod agent_dispatcher;
mod dispatcher;
mod routes;
mod types;

pub use agent_dispatcher::{arc_dispatcher, AgentDispatcher};
pub use dispatcher::{AgentEventChunk, ChatDispatcher};
pub use routes::{router, OpenAiState};
pub use types::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, ModelInfo,
    ModelsList,
};
