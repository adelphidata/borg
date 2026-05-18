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

/// Computes embedding vectors for one or more inputs. The output order
/// MUST match the input order — the route layer assigns the response
/// `data[i].index` from input position and clients zip against it.
///
/// Implementations:
/// - `ConfigEmbeddingDispatcher` (production) — calls the configured
///   `cortex::memory::embeddings::EmbeddingProvider`, reusing the
///   memory subsystem's cache layer.
/// - Test fakes — return scripted vectors.
#[async_trait]
pub trait EmbeddingDispatcher: Send + Sync + 'static {
    /// Embed each input string and return the vectors in the same
    /// order. `model` is the alias the client asked for; production
    /// impls echo it back via the response, but provider selection is
    /// governed by Borg config, not this argument.
    async fn embed(
        &self,
        model: &str,
        inputs: Vec<String>,
    ) -> Result<Vec<Vec<f32>>, EmbeddingDispatchError>;
}

/// Failure modes the route layer must distinguish — `ProviderUnavailable`
/// is an operator/config issue (no API key configured) and surfaces as
/// 503; `Upstream` is a transient or remote-side fault and surfaces as
/// 502 so monitoring can alert on the two separately.
#[derive(Debug)]
pub enum EmbeddingDispatchError {
    /// No embedding provider is configured / enabled. Maps to HTTP 503
    /// with `error.type = "service_unavailable"`.
    ProviderUnavailable,
    /// Upstream provider call failed. Maps to HTTP 502 with
    /// `error.type = "api_error"` and the inner message in
    /// `error.message`.
    Upstream(String),
}

impl std::fmt::Display for EmbeddingDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProviderUnavailable => write!(f, "embedding provider unavailable"),
            Self::Upstream(msg) => write!(f, "upstream embedding error: {msg}"),
        }
    }
}

impl std::error::Error for EmbeddingDispatchError {}
