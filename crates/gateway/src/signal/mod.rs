/// Signal REST API client for sending messages.
pub mod api;
/// Message deduplication by timestamp.
pub mod dedup;
/// Markdown-to-Signal text formatting with style ranges.
pub mod format;
/// Signal message parsing into inbound messages.
pub mod parse;
/// Server-Sent Events listener for Signal CLI REST API.
pub mod sse;
/// Signal API type definitions.
pub mod types;
