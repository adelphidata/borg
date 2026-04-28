//! Normalized ingress schema and pre-dispatch gate.
//!
//! Every ingress surface (native channels, OpenAI-compat HTTP, generic
//! webhook, MCP server) parses its protocol into `IngressMessage` and
//! runs it through `IngressGate::admit` before handing off to the agent
//! loop. The gate centralizes three cross-surface concerns:
//!
//! 1. Rate limiting (per-principal sliding window).
//! 2. Prompt-injection sanitization (existing 5-layer defense applied
//!    once, here, instead of duplicated per-surface).
//! 3. Audit-write hook — wired to the daemon's hash-chained audit log
//!    when a real `AuditSink` is injected; defaults to a no-op for
//!    in-process tests.
//!
//! Session resolution is co-located here (`session_key`) so every
//! surface derives `{sender_id}:{thread_id}` identically.

mod gate;
mod message;

pub use gate::{AdmittedIngress, AuditSink, IngressGate, IngressReject, NoopAuditSink};
pub use message::{
    session_key, Attachment, IngressMessage, IngressSource, MessageBody, PrincipalId,
};
