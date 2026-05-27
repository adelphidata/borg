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
        // Reuse the existing tool handler, which already runs hybrid
        // FTS+vector with MMR rerank. We parse its formatted output
        // back into structured hits here so the trait surface stays
        // typed — the handler-level formatter is only used for
        // direct LLM-facing rendering.
        let args = serde_json::json!({
            "query": query.query,
            "max_results": query.max_results as u64,
            "min_score": query.min_score as f64,
        });
        let rendered = crate::tool_handlers::handle_memory_search(&args, ctx.config)
            .await
            .map_err(|e| backend(e.to_string()))?;
        Ok(parse_rendered_hits(&rendered))
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

/// Parse the formatted block emitted by `format_search_results` /
/// `format_message_fts_hits` back into [`SearchHit`]s. Best-effort —
/// the formatter is the single source of truth so changes there
/// propagate here; the regression test in [`super::tests`] pins the
/// round-trip.
fn parse_rendered_hits(rendered: &str) -> Vec<SearchHit> {
    if rendered.trim().is_empty() || rendered.contains("No matching memories found") {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut lines = rendered.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        let Some(end) = rest.find(']') else { continue };
        let body = rest[end + 1..].trim();
        // body: "<name> (score: 0.42)" or "<name> (lines a-b, score: 0.42)"
        let Some(open) = body.rfind('(') else {
            continue;
        };
        let label = body[..open].trim().to_string();
        let inner = &body[open + 1..body.len().saturating_sub(1)];
        let score = inner
            .split(',')
            .find_map(|seg| seg.trim().strip_prefix("score:"))
            .and_then(|s| s.trim().parse::<f32>().ok())
            .unwrap_or(0.0);
        let snippet = lines
            .peek()
            .and_then(|l| l.strip_prefix("> "))
            .map(str::to_string)
            .unwrap_or_default();
        let (scope, name) = match label.split_once('/') {
            Some((s, n)) if s == "global" || s.starts_with("project:") => {
                (s.to_string(), n.to_string())
            }
            _ => ("global".to_string(), label),
        };
        out.push(SearchHit {
            scope,
            name,
            snippet,
            score,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rendered_hits_empty_string_returns_empty() {
        assert!(parse_rendered_hits("").is_empty());
    }

    #[test]
    fn parse_rendered_hits_no_matches_returns_empty() {
        assert!(parse_rendered_hits("No matching memories found.").is_empty());
    }

    #[test]
    fn parse_rendered_hits_extracts_name_score_and_snippet() {
        let rendered = "[1] notes.md (lines 1-10, score: 0.87)\n> Important decision";
        let hits = parse_rendered_hits(rendered);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "notes.md");
        assert!((hits[0].score - 0.87).abs() < 1e-6);
        assert_eq!(hits[0].snippet, "Important decision");
    }

    #[test]
    fn parse_rendered_hits_handles_scoped_label() {
        let rendered = "[1] project:abc/notes (score: 0.50)\n> body text";
        let hits = parse_rendered_hits(rendered);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].scope, "project:abc");
        assert_eq!(hits[0].name, "notes");
    }
}
