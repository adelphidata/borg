//! Filesystem tool handlers: apply_patch, list_dir, read_file, read_pdf.
//!
//! Split into focused submodules:
//! - [`patch`] — `apply_patch` (cwd / skills / channels), ghost-commit rollback,
//!   ACP text-bridge integration, patch-DSL path extraction & normalization.
//! - [`listing`] — `list_dir` with recursive walk, fuzzy matching, pagination.
//! - [`read`] — `read_file` (text + image multimodal) and `read_pdf`.
//! - [`document`] — Word/PowerPoint/Excel/OpenDocument/RTF/EPUB/PDF →
//!   Markdown conversion used by `read`.
//! - [`blocked`] — security blocklist enforcement shared by the others.
//! - [`completion`] — workspace-scoped prefix completion for client file
//!   pickers, answering to the same containment and blocklist rules.

mod blocked;
mod completion;
mod document;
mod listing;
mod patch;
mod read;

pub use blocked::{is_blocked_path, shell_command_targets_control_plane};
pub use completion::{complete_paths, PathSuggestion, MAX_PATH_SUGGESTIONS};
pub use listing::{handle_list_dir, handle_list_dir_with_context};
pub use patch::{
    handle_apply_patch, handle_apply_patch_unified, handle_apply_patch_unified_with_context,
    handle_apply_patch_with_context, handle_apply_skill_patch, handle_create_channel,
};
// Re-exported for the concurrency planner in `crate::tool::effects`, which must
// reserve exactly the paths these markers cause this handler to write.
pub(crate) use patch::PATCH_DSL_MARKERS;
pub use read::{handle_read_file, handle_read_file_with_context, handle_read_pdf};

/// Build a default `ExecutionContext` from the running process. Used by the
/// public `handle_X` thunks that don't take an explicit context.
pub(super) fn default_execution_context(
) -> anyhow::Result<crate::execution_context::ExecutionContext> {
    crate::execution_context::ExecutionContext::from_process()
}
