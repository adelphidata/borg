//! Shared low-level primitives with no domain coupling: hex encoding, Unix
//! filesystem helpers, retry/backoff, panic-message extraction, and text
//! truncation. Grouped here to keep the crate root focused on domain modules.

/// ANSI/terminal escape stripping for recalled text re-entering model context.
pub mod ansi;
/// Lowercase hex encode/decode helpers.
pub mod hex;
/// Shared helpers for panic-isolated supervised loops.
pub mod panic_utils;
/// Process-group spawn/kill helpers so abandoned children don't orphan trees.
pub(crate) mod process_tree;
/// Retry utilities with exponential backoff.
pub mod retry;
/// Wall-clock helpers.
pub mod time;
/// Text truncation with head+tail preservation.
pub mod truncate;
/// Tiny Unix filesystem helpers (chmod) shared across crates.
pub mod unix_ext;
/// Incremental UTF-8 decoding for streamed byte chunks.
pub(crate) mod utf8_stream;
