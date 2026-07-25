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

// ── Syntax-highlighting guardrails ───────────────────────────────────────────
//
// `syntect`'s regex engine is superlinear on pathological input, and the
// collapse constants above only trim what is *displayed* — they run after
// highlighting, so they do not bound its cost. Without these caps a single
// minified blob or long unterminated string in a tool result can wedge the
// render thread. Exceeding any of them falls back to plain unstyled text, which
// preserves the content verbatim; only the colors are lost.

/// Total input size above which highlighting is skipped.
pub const HIGHLIGHT_MAX_TOTAL_BYTES: usize = 512 * 1024;

/// Line count above which highlighting is skipped.
pub const HIGHLIGHT_MAX_LINES: usize = 10_000;

/// Length of a *single* line above which highlighting is skipped. The tightest
/// of the three in practice: a 2 MB one-line minified bundle passes both limits
/// above while being the worst case for the regex engine.
pub const HIGHLIGHT_MAX_LINE_BYTES: usize = 4 * 1024;
