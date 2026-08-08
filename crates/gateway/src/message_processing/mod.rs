//! Message-content transforms applied before delivery: per-channel chunking,
//! plain-text rendering, recent-message deduplication, and link-understanding
//! enrichment. `handler.rs` is the primary consumer.

/// Per-channel message chunking.
pub mod chunker;
/// Outbound dedup of near-duplicate consecutive messages.
pub mod dedup;
/// Inbound message scan/truncate/wrap pipeline.
pub mod inbound_prep;
/// Inbound link expansion/understanding.
pub mod link_understanding;
/// Outbound `[MEDIA:<path>]` attachment delivery + fallback notices.
pub(crate) mod media_egress;
/// Placeholders for inbound media that failed to download.
pub(crate) mod media_failure;
/// Plain-text rendering for SMS/iMessage and similar channels.
pub mod plain_text;
/// Shared inbound voice-transcription helpers.
pub(crate) mod voice;
