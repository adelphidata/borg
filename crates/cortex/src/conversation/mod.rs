//! Conversation history management: token accounting, compaction strategies,
//! tool-result truncation, LLM-driven summarization, and history editing.
//!
//! Submodules:
//! - [`tokens`] — per-message and whole-history token estimation.
//! - [`truncation`] — non-LLM tool-result reduction (share-limit, age-based
//!   degradation, newest-first compaction). Used by the autocompaction
//!   pipeline before any LLM summary call.
//! - [`compaction`] — head/tail compaction planners (`plan_compaction`,
//!   `plan_compaction_v2`) and full `compact_history*` orchestrators that
//!   weave summarization in with the planner output.
//! - [`summarize`] — LLM-backed summarization prompts and chunkers.
//! - [`history`] — `undo_last_turn`, `rewind_to_nth_user`, `normalize_history`.
//! - [`event`] — the first-class session event log (`SessionEvent`) and the
//!   projection functions that reconstruct working state from it.

mod compaction;
mod event;
mod history;
mod summarize;
mod tokens;
mod truncation;

pub use compaction::{
    compact_history, compact_history_v2, compaction_checkpoint_range, effective_head_protection,
    effective_history_budget_for, extract_last_compaction_summary, is_compaction_marker,
    plan_compaction, plan_compaction_v2, render_compaction_marker, reserve_output_budget,
    CompactionPlan, COMPACTION_MARKER_CLOSE, COMPACTION_MARKER_HEADING, COMPACTION_MARKER_OPEN,
};
pub use event::{
    compute_session_event_hmac, events_from_rows, project_history, verify_session_chain,
    SessionEvent, SessionEventHashInput, SessionEventKind, COMPACTION_CHECKPOINT_KIND,
    SESSION_HMAC_DOMAIN,
};
pub use history::{normalize_history, rewind_to_nth_user, undo_last_turn};
pub use summarize::SummarizeFailure;
pub use tokens::history_tokens;
pub use truncation::{
    age_based_tool_result_degradation, compact_tool_results, degrade_old_tool_results,
    enforce_tool_result_share_limit, AGE_TIER1_TOKEN_THRESHOLD, AGE_TIER1_WINDOW,
    AGE_TIER2_TOKEN_THRESHOLD, AGE_TIER2_WINDOW,
};
