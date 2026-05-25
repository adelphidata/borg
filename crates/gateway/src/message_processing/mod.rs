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
/// Plain-text rendering for SMS/iMessage and similar channels.
pub mod plain_text;
