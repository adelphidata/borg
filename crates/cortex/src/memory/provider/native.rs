//! In-process SQLite-backed memory provider.
//!
//! Wraps the existing free functions in [`crate::memory`] so the default
//! path is just another impl of [`MemoryProvider`]. The actual storage
//! layer (chunked entries, FTS, embeddings, MMR rerank) is unchanged —
//! this file is the smallest possible adapter onto the trait.

use anyhow::Result;
use async_trait::async_trait;

use super::{
    ConsolidationOutcome, MemCtx, MemoryEntry, MemoryError, MemoryProvider, SearchHit, SearchQuery,
};
use crate::memory::WriteMode;

/// Adapter onto Borg's in-process SQLite memory store.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeProvider;

impl NativeProvider {
    /// Build a fresh instance.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl MemoryProvider for NativeProvider {
    fn name(&self) -> &'static str {
        "native"
    }

    async fn write(
        &self,
        _ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
        content: &str,
        mode: WriteMode,
    ) -> Result<(), MemoryError> {
        // The free function already runs `scan_for_injection` and the
        // 20k-token cap; `write_guarded` upstream runs the same checks.
        // Running them twice is cheap and keeps Native correct even if
        // a future caller bypasses the wrapper.
        crate::memory::write_memory_db(name, content, mode, scope)
            .await
            .map(|_| ())
            .map_err(|e| MemoryError::Backend {
                provider: "native",
                detail: e.to_string(),
            })
    }

    async fn read(
        &self,
        _ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
    ) -> Result<Option<MemoryEntry>, MemoryError> {
        let db = crate::db::Database::open()
            .await
            .map_err(|e| transport(e.to_string()))?;
        let row = db
            .get_memory_entry(scope, name)
            .await
            .map_err(|e| backend(e.to_string()))?;
        Ok(row.map(|r| MemoryEntry {
            scope: r.scope,
            name: r.name,
            content: r.content,
            updated_at_epoch: Some(r.updated_at),
        }))
    }

    async fn search(
        &self,
        ctx: &MemCtx<'_>,
        query: &SearchQuery,
    ) -> Result<Vec<SearchHit>, MemoryError> {
        // Call the typed helper directly, NOT through the tool handler.
        // If we routed via `handle_memory_search`, a fallback-to-native
        // (external provider selected but unconfigured) would loop back
        // through the trait dispatch and recurse until stack overflow.
        let db = crate::db::Database::open()
            .await
            .map_err(|e| transport(e.to_string()))?;
        let results = crate::tool_handlers::native_memory_search(
            ctx.config,
            &db,
            &query.query,
            query.max_results,
            query.min_score,
        )
        .await
        .map_err(|e| backend(e.to_string()))?;
        Ok(results
            .into_iter()
            .take(query.max_results)
            .map(|r| {
                // `filename` carries the entry name. For long-term memory
                // entries it's already `name`; for knowledge files it's
                // a markdown path. Map to a global scope unless the
                // formatter convention indicated otherwise.
                let (scope, name) = match r.filename.split_once('/') {
                    Some((s, n)) if s == "global" || s.starts_with("project:") => {
                        (s.to_string(), n.to_string())
                    }
                    _ => ("global".to_string(), r.filename),
                };
                SearchHit {
                    scope,
                    name,
                    snippet: r.snippet,
                    score: r.score,
                }
            })
            .collect())
    }

    async fn list(&self, _ctx: &MemCtx<'_>, scope: &str) -> Result<Vec<MemoryEntry>, MemoryError> {
        let db = crate::db::Database::open()
            .await
            .map_err(|e| transport(e.to_string()))?;
        let rows = db
            .list_memory_entries(scope)
            .await
            .map_err(|e| backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|r| MemoryEntry {
                scope: r.scope,
                name: r.name,
                content: r.content,
                updated_at_epoch: Some(r.updated_at),
            })
            .collect())
    }

    async fn delete(
        &self,
        _ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
    ) -> Result<bool, MemoryError> {
        crate::memory::delete_memory_db(name, scope)
            .await
            .map_err(|e| backend(e.to_string()))
    }

    async fn consolidate(&self, _ctx: &MemCtx<'_>) -> Result<ConsolidationOutcome, MemoryError> {
        // The actual seeded-agent loop is owned by the daemon scheduler;
        // signalling `Local` tells the scheduler to run it.
        Ok(ConsolidationOutcome::Local)
    }
}

fn backend(detail: String) -> MemoryError {
    MemoryError::Backend {
        provider: "native",
        detail,
    }
}

fn transport(detail: String) -> MemoryError {
    MemoryError::Transport {
        provider: "native",
        detail,
    }
}
