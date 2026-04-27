//! Dispatcher trait that the OpenAI-compat routes use to drive the
//! agent loop. Production wiring will provide an `AgentDispatcher`
//! that spawns a `cortex::agent::Agent`; tests inject a fake.

use async_trait::async_trait;
use cortex::ingress::AdmittedIngress;
use futures::stream::BoxStream;

/// One chunk of agent output translated into the OpenAI streaming
/// shape. Intermediate between the rich [`cortex::agent::AgentEvent`]
/// and the OpenAI wire format — keeps the route layer free of agent
/// internals and the trait testable without a real LLM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEventChunk {
    /// Incremental text delta from the LLM.
    Text(String),
    /// Tool execution starting. Surfaced as an OpenAI `tool_calls`
    /// delta on the wire.
    ToolCall {
        /// Stable id within this completion (matches what OpenAI
        /// clients echo back in a tool-result message).
        id: String,
        /// Tool name.
        name: String,
        /// JSON-encoded arguments.
        arguments: String,
    },
    /// Turn finished cleanly — emit `finish_reason: "stop"`.
    Done,
    /// Error occurred mid-turn — emit a final chunk with the message
    /// in `content` and `finish_reason: "stop"`.
    Error(String),
}

/// Hands an admitted ingress to the agent loop and yields agent
/// events translated into the OpenAI chunk shape.
///
/// Implementations:
/// - `AgentDispatcher` (production) — spawns `cortex::agent::Agent`.
/// - Test fakes — yield canned chunk vectors.
#[async_trait]
pub trait ChatDispatcher: Send + Sync + 'static {
    /// Dispatch the admitted message and stream chunks back. The
    /// stream MUST end with a terminal [`AgentEventChunk::Done`] or
    /// [`AgentEventChunk::Error`]; the route relies on that to close
    /// the SSE response.
    async fn dispatch(&self, admitted: AdmittedIngress) -> BoxStream<'static, AgentEventChunk>;
}
