//! Caches that feed system-prompt assembly.
//!
//! All callers reach the underlying caches through typed event methods
//! (`on_memory_written`, `on_config_reloaded`, `on_skill_modified`)
//! rather than poking fields directly. The "which event invalidates
//! which cache" rule lives in this module — adding a new cache or
//! invalidation trigger doesn't require updating call sites scattered
//! across `agent/mod.rs` and the tool handlers.
//!
//! Per `RUST-IDIOMS.md`: this struct earns its keep by *owning* the
//! caches and *enforcing* the invariant that they are only mutated
//! through named events. No trait — single impl. Field access is
//! private; readers go through accessors.
//!
//! Borrow-checker note: methods take `&mut self` only when they
//! actually mutate. Pure readers take `&self` so they can coexist with
//! disjoint borrows of other `Agent` fields under the same outer
//! `&mut Agent`.
//!
//! ## Caches owned
//!
//! - **identity** — IDENTITY.md snapshot. Lazily loaded on first call to
//!   [`Self::identity_or_load`]; cleared by `on_memory_written` when the
//!   write targets `IDENTITY.md`.
//! - **credentials** — resolved env-var bag from `Config::resolve_credentials`.
//!   Populated lazily; replaced wholesale on `on_config_reloaded`.
//! - **project docs** — `AGENTS.md` / `CLAUDE.md` discovered relative to
//!   CWD. The outer `Option` records "have we tried to load yet?"; the
//!   inner records "did we find any?".
//! - **skills context + request** — paired: the cached rendered skills
//!   block and the user-request text it was rendered against. Both are
//!   cleared together by `on_config_reloaded` and `on_skill_modified`.
//! - **last skill drop signature** — sorted set of skills omitted on the
//!   most-recent budget pass. Suppresses duplicate warnings while the
//!   dropped set is unchanged. Cleared with the skills caches.

use crate::constants::IDENTITY_FILE;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Default)]
pub(crate) struct SystemPromptCache {
    identity: Option<String>,
    credentials: Option<HashMap<String, String>>,
    project_docs: Option<Option<String>>,
    skills_context: Option<String>,
    skills_request: Option<String>,
    /// Per-skill token costs recorded when `skills_context` was rendered.
    /// Lives and dies with the skills caches — same setter, same
    /// invalidation events — so it can never describe a stale render.
    skills_per_skill_tokens: Option<Vec<(String, usize)>>,
    last_skill_drop_signature: Option<Vec<String>>,
}

impl SystemPromptCache {
    /// Return the cached identity, computing and storing it on first call.
    pub(crate) fn identity_or_load<F>(&mut self, load: F) -> Result<String>
    where
        F: FnOnce() -> Result<String>,
    {
        if let Some(v) = self.identity.as_ref() {
            return Ok(v.clone());
        }
        let v = load()?;
        self.identity = Some(v.clone());
        Ok(v)
    }

    /// Return cached credentials, or a static empty map if never populated.
    /// The empty fallback exists so the skill-loading path can borrow
    /// credentials before `ensure_credentials` has been called.
    pub(crate) fn credentials_or_empty(&self) -> &HashMap<String, String> {
        static EMPTY: LazyLock<HashMap<String, String>> = LazyLock::new(HashMap::new);
        self.credentials.as_ref().unwrap_or(&EMPTY)
    }

    /// Populate the credentials cache if empty. Resolution closure runs
    /// only on the first call after construction or invalidation.
    pub(crate) fn ensure_credentials<F>(&mut self, resolve: F)
    where
        F: FnOnce() -> HashMap<String, String>,
    {
        if self.credentials.is_none() {
            self.credentials = Some(resolve());
        }
    }

    /// Read project docs, computing them on first call. The inner `Option`
    /// records whether any docs were found.
    pub(crate) fn project_docs_or_load<F>(&mut self, load: F) -> Option<String>
    where
        F: FnOnce() -> Option<String>,
    {
        if self.project_docs.is_none() {
            self.project_docs = Some(load());
        }
        self.project_docs.as_ref().and_then(Clone::clone)
    }

    /// Clear the cached project docs snapshot so it is reloaded on next access.
    pub(crate) fn clear_project_docs(&mut self) {
        self.project_docs = None;
    }

    /// True when no skills context is cached, or when the cached request
    /// no longer matches `request`.
    pub(crate) fn skills_stale_for(&self, request: &str) -> bool {
        self.skills_context.is_none() || self.skills_request.as_deref() != Some(request)
    }

    /// Return the cached skills context only if it was rendered for this
    /// exact request. Callers should fall back to re-rendering otherwise.
    pub(crate) fn skills_for_request(&self, request: &str) -> Option<&str> {
        match (self.skills_context.as_ref(), self.skills_request.as_ref()) {
            (Some(ctx), Some(req)) if req == request => Some(ctx.as_str()),
            _ => None,
        }
    }

    /// Record a freshly-rendered skills context against the request that
    /// produced it, along with the per-skill token costs of that render.
    pub(crate) fn set_skills(
        &mut self,
        request: String,
        context: String,
        per_skill_tokens: Vec<(String, usize)>,
    ) {
        self.skills_context = Some(context);
        self.skills_request = Some(request);
        self.skills_per_skill_tokens = Some(per_skill_tokens);
    }

    /// Per-skill token costs of the cached skills render, only if it was
    /// rendered for this exact request (same hit rule as
    /// [`Self::skills_for_request`]).
    pub(crate) fn skill_sizes_for_request(&self, request: &str) -> Option<&[(String, usize)]> {
        match (
            self.skills_per_skill_tokens.as_ref(),
            self.skills_request.as_ref(),
        ) {
            (Some(sizes), Some(req)) if req == request => Some(sizes.as_slice()),
            _ => None,
        }
    }

    /// True if the previously-recorded drop signature equals `candidate`.
    /// Used by the warn-once path to suppress duplicate warnings.
    pub(crate) fn skill_drop_signature_matches(&self, candidate: &[String]) -> bool {
        self.last_skill_drop_signature
            .as_ref()
            .is_some_and(|prev| prev.as_slice() == candidate)
    }

    pub(crate) fn set_skill_drop_signature(&mut self, signature: Vec<String>) {
        self.last_skill_drop_signature = Some(signature);
    }

    /// Event: a memory entry was written. `IDENTITY.md` invalidates the
    /// identity cache; other names are no-ops. The `scope` argument is
    /// reserved — today only the unscoped/global IDENTITY.md drives
    /// invalidation, but the parameter exists so future per-scope rules
    /// don't need to thread a new arg through every caller.
    pub(crate) fn on_memory_written(&mut self, _scope: &str, name: &str) {
        if name == IDENTITY_FILE {
            self.identity = None;
        }
    }

    /// Event: config was reloaded. Re-derive credentials and clear the
    /// skills caches — the env-var allowlist that filtered them may now
    /// be stale, so the next prompt rebuild renders from scratch.
    /// Identity and project docs survive (config does not affect them).
    pub(crate) fn on_config_reloaded(&mut self, fresh_credentials: HashMap<String, String>) {
        self.credentials = Some(fresh_credentials);
        self.skills_context = None;
        self.skills_request = None;
        self.skills_per_skill_tokens = None;
        self.last_skill_drop_signature = None;
    }

    /// Event: a skill was modified (apply_skill_patch, or apply_patch
    /// targeting `skills/`). Clears the skills caches so the next prompt
    /// rebuild picks up the change. Identity, credentials, and project
    /// docs survive.
    pub(crate) fn on_skill_modified(&mut self) {
        self.skills_context = None;
        self.skills_request = None;
        self.skills_per_skill_tokens = None;
        self.last_skill_drop_signature = None;
    }

    #[cfg(test)]
    pub(crate) fn identity_for_test(&self) -> Option<&str> {
        self.identity.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn skills_context_for_test(&self) -> Option<&str> {
        self.skills_context.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn skills_request_for_test(&self) -> Option<&str> {
        self.skills_request.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn last_skill_drop_signature_for_test(&self) -> Option<&[String]> {
        self.last_skill_drop_signature.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn with_test_state(
        identity: Option<String>,
        credentials: Option<HashMap<String, String>>,
        skills_context: Option<String>,
        skills_request: Option<String>,
        last_skill_drop_signature: Option<Vec<String>>,
    ) -> Self {
        Self {
            identity,
            credentials,
            project_docs: None,
            skills_context,
            skills_request,
            skills_per_skill_tokens: None,
            last_skill_drop_signature,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_memory_written_clears_identity_only_for_identity_file() {
        let mut c = SystemPromptCache::with_test_state(
            Some("identity".into()),
            Some(HashMap::from([("X".into(), "y".into())])),
            Some("ctx".into()),
            Some("req".into()),
            Some(vec!["a".into()]),
        );

        c.on_memory_written("global", "notes.md");
        assert_eq!(c.identity_for_test(), Some("identity"));

        c.on_memory_written("global", IDENTITY_FILE);
        assert_eq!(
            c.identity_for_test(),
            None,
            "IDENTITY.md must clear identity"
        );
        assert!(
            c.credentials_or_empty().contains_key("X"),
            "credentials survive identity invalidation"
        );
        assert_eq!(
            c.skills_context_for_test(),
            Some("ctx"),
            "skills survive identity invalidation"
        );
    }

    #[test]
    fn on_config_reloaded_replaces_credentials_and_clears_skills() {
        let mut c = SystemPromptCache::with_test_state(
            Some("identity".into()),
            Some(HashMap::from([("OLD".into(), "v".into())])),
            Some("ctx".into()),
            Some("req".into()),
            Some(vec!["a".into()]),
        );

        c.on_config_reloaded(HashMap::from([("NEW".into(), "v".into())]));

        assert!(c.credentials_or_empty().contains_key("NEW"));
        assert!(!c.credentials_or_empty().contains_key("OLD"));
        assert_eq!(c.skills_context_for_test(), None);
        assert_eq!(c.skills_request_for_test(), None);
        assert_eq!(c.last_skill_drop_signature_for_test(), None);
        assert_eq!(
            c.identity_for_test(),
            Some("identity"),
            "identity survives config reload"
        );
    }

    #[test]
    fn on_skill_modified_clears_skills_only() {
        let mut c = SystemPromptCache::with_test_state(
            Some("identity".into()),
            Some(HashMap::from([("X".into(), "v".into())])),
            Some("ctx".into()),
            Some("req".into()),
            Some(vec!["a".into()]),
        );

        c.on_skill_modified();

        assert_eq!(c.skills_context_for_test(), None);
        assert_eq!(c.skills_request_for_test(), None);
        assert_eq!(c.last_skill_drop_signature_for_test(), None);
        assert_eq!(c.identity_for_test(), Some("identity"));
        assert!(c.credentials_or_empty().contains_key("X"));
    }

    #[test]
    fn skills_for_request_hits_only_on_exact_request_match() {
        let mut c = SystemPromptCache::default();
        c.set_skills("hello".into(), "ctx".into(), vec![("git".into(), 42)]);
        assert_eq!(c.skills_for_request("hello"), Some("ctx"));
        assert_eq!(c.skills_for_request("hello world"), None);
        assert!(!c.skills_stale_for("hello"));
        assert!(c.skills_stale_for("hello world"));
        // Per-skill sizes obey the exact same hit rule as the rendered text —
        // a mismatch would let /context attribute stale sizes to a fresh render.
        assert_eq!(
            c.skill_sizes_for_request("hello"),
            Some(&[("git".to_string(), 42usize)][..])
        );
        assert_eq!(c.skill_sizes_for_request("hello world"), None);
    }

    #[test]
    fn ensure_credentials_runs_resolver_only_on_first_call() {
        let mut c = SystemPromptCache::default();
        let mut calls = 0;
        c.ensure_credentials(|| {
            calls += 1;
            HashMap::from([("FOO".into(), "bar".into())])
        });
        c.ensure_credentials(|| {
            calls += 1;
            HashMap::new()
        });
        assert_eq!(calls, 1, "resolver must only run on the first call");
        assert_eq!(
            c.credentials_or_empty().get("FOO"),
            Some(&"bar".to_string())
        );
    }

    #[test]
    fn skill_drop_signature_matches_compares_by_slice() {
        let mut c = SystemPromptCache::default();
        let sig = vec!["a".to_string(), "b".to_string()];
        assert!(
            !c.skill_drop_signature_matches(&sig),
            "no signature recorded yet"
        );
        c.set_skill_drop_signature(sig.clone());
        assert!(c.skill_drop_signature_matches(&sig));
        assert!(!c.skill_drop_signature_matches(&["a".into(), "c".into()]));
    }

    #[test]
    fn project_docs_or_load_runs_loader_once() {
        let mut c = SystemPromptCache::default();
        let mut calls = 0;
        let r1 = c.project_docs_or_load(|| {
            calls += 1;
            Some("docs".into())
        });
        let r2 = c.project_docs_or_load(|| {
            calls += 1;
            Some("other".into())
        });
        assert_eq!(r1, Some("docs".into()));
        assert_eq!(r2, Some("docs".into()), "second call returns cached");
        assert_eq!(calls, 1);
    }

    #[test]
    fn project_docs_caches_negative_result_too() {
        let mut c = SystemPromptCache::default();
        let mut calls = 0;
        let _ = c.project_docs_or_load(|| {
            calls += 1;
            None
        });
        let _ = c.project_docs_or_load(|| {
            calls += 1;
            Some("late".into())
        });
        assert_eq!(
            calls, 1,
            "a None result must still be cached so we don't re-discover every turn"
        );
    }

    #[test]
    fn identity_or_load_propagates_loader_error() {
        let mut c = SystemPromptCache::default();
        let err = c
            .identity_or_load(|| Err(anyhow::anyhow!("boom")))
            .expect_err("loader error must propagate");
        assert!(err.to_string().contains("boom"));
        assert_eq!(
            c.identity_for_test(),
            None,
            "failed loader must not poison the cache"
        );
    }
}
