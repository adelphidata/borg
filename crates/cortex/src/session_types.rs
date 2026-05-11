//! Shared per-turn payload types that cross the gateway ↔ daemon boundary.
//!
//! These are the canonical in-process types used by `gateway::InboundTurn`
//! and `borgd::session::backend::SessionBackend::run_turn`. The corresponding
//! wire types live in `protocols::session` and are converted at the gRPC
//! boundary.

/// One inline image attached to a multimodal turn.
#[derive(Debug, Clone)]
pub struct InlineImage {
    /// MIME type of the image (e.g. `image/png`).
    pub mime_type: String,
    /// Raw image bytes (not base64).
    pub data: Vec<u8>,
    /// Optional filename for display.
    pub filename: Option<String>,
}
