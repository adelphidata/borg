//! Credential cache and skill-runtime refresh helpers for `Agent`.
//!
//! Extracted from `agent/mod.rs` so credential resolution, the skill env
//! allowlist, and the "did a tool just write to ~/.borg/skills?" gate live
//! together in one focused module. All entry points are methods on `Agent`;
//! the trait-like surface is preserved through an additional `impl Agent`
//! block.

use std::collections::HashMap;

use crate::tool::names as tn;

use super::Agent;

impl Agent {
    /// Ensure credential cache is populated, resolving on first call or after config reload.
    /// Separated from the getter so the getter can return a borrow while leaving
    /// `&self` free for other immutable accesses.
    pub(super) fn ensure_credentials_cached(&mut self) {
        let config = &self.config;
        self.caches
            .ensure_credentials(|| config.resolve_credentials());
    }

    /// Borrow the cached credentials, falling back to an empty map if the cache
    /// was not populated yet. Callers inside this module always invoke
    /// [`Self::ensure_credentials_cached`] first; the fallback keeps the
    /// skill-loading path robust if that ordering ever regresses.
    pub(super) fn credentials_cached(&self) -> &HashMap<String, String> {
        self.caches.credentials_or_empty()
    }

    /// Invalidate skill-derived caches after a skill manifest changes on disk
    /// (e.g. `apply_skill_patch` succeeded). Resets the LRU skill context cache,
    /// re-resolves credentials so newly-declared skill env vars take effect,
    /// and recomputes the env allowlist exposed to `run_shell`.
    pub(super) fn refresh_skill_runtime_state(&mut self) {
        self.caches.on_skill_modified();
        self.ensure_credentials_cached();
        self.skill_env_allowlist = crate::skills::collect_required_env_vars(
            self.credentials_cached(),
            &self.config.skills,
        );
    }

    /// Inspect a just-completed tool call and refresh skill state if it
    /// successfully touched the skills tree. Cheap fast-path: only the two
    /// patch tools targeting the skills directory can mutate skill manifests,
    /// and a successful apply emits the literal "applied successfully" marker.
    pub(super) fn maybe_refresh_skills_after_tool(
        &mut self,
        tool_name: &str,
        args: &serde_json::Value,
        output: &str,
    ) {
        let touched_skills = match tool_name {
            tn::APPLY_SKILL_PATCH => true,
            tn::APPLY_PATCH => args["target"].as_str() == Some("skills"),
            _ => false,
        };
        if touched_skills && output.contains("applied successfully") {
            self.refresh_skill_runtime_state();
        }
    }
}
