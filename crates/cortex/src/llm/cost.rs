//! Cost-event types shared between the LLM client and daemon transport.

/// One machine-readable cost sample for a completed LLM call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostEvent {
    /// UUID v4 generated per logical LLM call.
    pub request_id: String,
    /// Borg session/conversation id, or empty when none exists.
    pub session_id: String,
    /// Provider id (`openrouter`, `openai`, `anthropic`, ...).
    pub provider: String,
    /// Model id sent to the provider.
    pub model: String,
    /// Decimal USD string when known.
    pub cost_usd: Option<String>,
    /// False means the cost is unknown rather than `$0`.
    pub cost_usd_present: bool,
    /// Source label for the cost value.
    pub cost_source: String,
    /// Prompt tokens reported by the provider, if any.
    pub prompt_tokens: u32,
    /// Completion tokens reported by the provider, if any.
    pub completion_tokens: u32,
    /// Unix timestamp in milliseconds.
    pub timestamp_unix_ms: i64,
    /// Call kind (`chat`, ...).
    pub kind: String,
}

/// Best-effort sink for daemon cost events.
pub trait CostEventSink: Send + Sync + 'static {
    /// Accept one emitted event. Implementations must not panic.
    fn emit(&self, event: CostEvent);
}

/// Per-call metadata supplied by the daemon.
#[derive(Debug, Clone)]
pub struct CostEventContext {
    /// Borg session/conversation id, or empty when none exists.
    pub session_id: String,
    /// Call kind (`chat`, ...).
    pub kind: String,
}
