//! Generic signed-payload webhook surface.
//!
//! Defines `POST /v1/webhook/{name}`: the user pre-registers a webhook
//! definition (name + HMAC secret + JSON dot-paths for sender/thread/
//! body), then arbitrary upstream services post signed payloads.
//! Signatures are HMAC-SHA256 over `{timestamp}.{body}`, with a 5-min
//! replay window. Bodies above 1 MiB are rejected.
//!
//! v1 supports JSON dot-path extraction (`$.user.id`, `$.payload.text`).
//! Full JSONPath (filters, slices, recursive descent) is deferred to
//! v2 — most webhook payloads have stable shapes that dot-paths cover.

mod agent_sink;
pub mod definitions;
pub mod extract;
pub mod routes;

pub use agent_sink::{arc_sink, AgentSink};
pub use routes::{router, AdmitSink, WebhookState};
