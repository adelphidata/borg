//! TUI rendering thresholds.
//!
//! These constants tune cell-collapse and preview-row budgets used across
//! history rendering, the events router, and other display surfaces. They
//! live here (not inline in `history.rs`) so a single edit re-tunes every
//! consumer and the values stay in sync.

/// Head/tail line count for collapsed long assistant messages. A finalized
/// assistant cell with more than `2 * ASSISTANT_PREVIEW_LINES` rendered lines
/// is shown as `head + ellipsis + tail`; the user reaches the full body via
/// the Ctrl+T transcript pager.
pub const ASSISTANT_PREVIEW_LINES: usize = 20;

/// Total-line threshold above which a finalized assistant cell collapses.
/// Derived: `2 * ASSISTANT_PREVIEW_LINES` (head + tail).
pub const ASSISTANT_COLLAPSE_THRESHOLD: usize = ASSISTANT_PREVIEW_LINES * 2;

/// Display rows of tool output shown before collapsing. Row-aware
/// (post-wrap) — a single logical line that wraps to many rows counts
/// against this budget, so a stray long URL can't blow past the preview.
/// Toggled with Ctrl+E.
pub const COLLAPSE_THRESHOLD: usize = 10;

/// Preview budget (display rows) when a tool result is collapsed.
pub const COLLAPSE_PREVIEW_LINES: usize = 8;

/// Per-stream preview rows for `run_shell` results when collapsed,
/// before emitting the `… +N lines (ctrl + t to view transcript)` footer.
/// Stdout and stderr each get their own budget so a noisy stderr can't
/// hide the head of stdout.
pub const SHELL_PREVIEW_LINES: usize = 5;
