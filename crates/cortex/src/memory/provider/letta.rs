//! Letta (MemGPT lineage) memory provider.
//!
//! Letta gives each *agent* a hierarchy of memory tiers. Borg maps onto
//! the two operator-relevant ones:
//!
//! - **Archival memory** — out-of-context, embedding-searchable passages.
//!   This is the default store for every scope. Passages are plain text,
//!   so Borg's `(scope, name)` key is carried in a deterministic first-line
//!   marker (`borg-key: {scope}/{name}`) and parsed back out on
//!   read/list/search. Overwrite finds and deletes the prior passage(s)
//!   for the key before inserting; append concatenates onto them.
//! - **Core-memory blocks** — small, always-in-context, durable values.
//!   The configured `core_scope` (default `"core"`) routes here instead of
//!   archival, with `label = name` and `value = content`. `search` also
//!   surfaces matching core blocks, ranked above archival hits because
//!   they're the in-context tier.
//!
//! Endpoints (under the operator-supplied `base_url`, default Letta Cloud):
//! - `GET    {base}/v1/agents/{agent}/archival-memory?search={q}&limit={n}`
//! - `POST   {base}/v1/agents/{agent}/archival-memory`            `{text}`
//! - `DELETE {base}/v1/agents/{agent}/archival-memory/{id}`
//! - `GET    {base}/v1/agents/{agent}/core-memory/blocks`
//! - `PATCH  {base}/v1/agents/{agent}/core-memory/blocks/{label}` `{value}`

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::{
    urlencode, ConsolidationOutcome, MemCtx, MemoryEntry, MemoryError, MemoryProvider, SearchHit,
    SearchQuery,
};
use crate::config::media::LettaSettings;
use crate::memory::WriteMode;

const DEFAULT_BASE_URL: &str = "https://api.letta.com";
const DEFAULT_CORE_SCOPE: &str = "core";
const PROVIDER_NAME: &str = "letta";
/// First-line marker that carries the Borg `(scope, name)` key inside an
/// otherwise-plain archival passage. Followed by a blank line, then content.
const KEY_PREFIX: &str = "borg-key: ";
/// Upper bound on archival passages scanned client-side to locate a
/// `(scope, name)` key — Letta archival has no server-side metadata
/// filter, so write/read/list/delete all fetch-and-filter. Using one
/// constant everywhere keeps those paths in agreement about whether a key
/// is present; [`LettaProvider::fetch_passages`] warns when a full scan
/// hits this cap, since a key beyond it can't be located (a write would
/// then duplicate instead of overwrite).
const ARCHIVAL_SCAN_LIMIT: usize = 1000;
/// Score assigned to a core-memory-block hit — above every archival hit
/// because core blocks are the always-in-context tier.
const CORE_BLOCK_SCORE: f32 = 0.95;
/// Ceiling for synthesised archival-rank scores. Kept below
/// [`CORE_BLOCK_SCORE`] so the in-context tier always outranks archival
/// even when archival is the top semantic match.
const ARCHIVAL_MAX_SCORE: f32 = 0.9;

/// Letta-backed memory provider.
#[derive(Debug, Clone)]
pub struct LettaProvider {
    client: Client,
    base_url: String,
    api_key: String,
    agent_id: String,
    project: Option<String>,
    core_scope: String,
}

impl LettaProvider {
    /// Build from resolved settings. Returns `Err` when `api_key` or
    /// `agent_id` is unset so the registry can fall back to Native.
    pub fn from_settings(settings: &LettaSettings) -> Result<Self, MemoryError> {
        let api_key = settings
            .api_key
            .as_ref()
            .ok_or_else(|| MemoryError::NotConfigured {
                provider: PROVIDER_NAME,
                detail: "set memory.letta.api_key (e.g. {\"env\":\"LETTA_API_KEY\"})".to_string(),
            })?
            .resolve()
            .map_err(|_e| MemoryError::NotConfigured {
                provider: PROVIDER_NAME,
                // `SecretRef::resolve` errors can echo exec stderr / file
                // contents — never surface them.
                detail: "api_key resolve failed; check memory.letta.api_key".to_string(),
            })?;
        let agent_id = settings
            .agent_id
            .clone()
            .ok_or_else(|| MemoryError::NotConfigured {
                provider: PROVIDER_NAME,
                detail: "set memory.letta.agent_id (memory in Letta is agent-scoped)".to_string(),
            })?;
        let base_url = settings
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let core_scope = settings
            .core_scope
            .clone()
            .unwrap_or_else(|| DEFAULT_CORE_SCOPE.to_string());
        Ok(Self {
            client: super::http_client(),
            base_url,
            api_key,
            agent_id,
            project: settings.project.clone(),
            core_scope,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_tests(base_url: impl Into<String>) -> Self {
        Self {
            client: super::http_client(),
            base_url: base_url.into(),
            api_key: "test-key".into(),
            agent_id: "agent".into(),
            project: None,
            core_scope: DEFAULT_CORE_SCOPE.to_string(),
        }
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.api_key)
    }

    fn archival_url(&self) -> String {
        format!(
            "{}/v1/agents/{}/archival-memory",
            self.base_url.trim_end_matches('/'),
            urlencode(&self.agent_id)
        )
    }

    fn blocks_url(&self) -> String {
        format!(
            "{}/v1/agents/{}/core-memory/blocks",
            self.base_url.trim_end_matches('/'),
            urlencode(&self.agent_id)
        )
    }

    /// Attach the optional `X-Project` header (Letta Cloud project scoping)
    /// before the shared sender adds `Authorization` + `Content-Type`.
    fn with_project(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.project {
            Some(p) => req.header("X-Project", p),
            None => req,
        }
    }

    async fn send_json<T: for<'de> Deserialize<'de>>(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<T, MemoryError> {
        super::send_json(
            self.with_project(req),
            PROVIDER_NAME,
            &self.auth_header(),
            &self.api_key,
        )
        .await
    }

    async fn send_ok(&self, req: reqwest::RequestBuilder) -> Result<(), MemoryError> {
        super::send_ok(
            self.with_project(req),
            PROVIDER_NAME,
            &self.auth_header(),
            &self.api_key,
        )
        .await
    }

    /// Fetch all archival passages, parsed back into `(scope, name, content,
    /// passage_id)` tuples. `search` is an optional server-side query.
    async fn fetch_passages(
        &self,
        search: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String, String, String)>, MemoryError> {
        let mut url = format!("{}?limit={limit}", self.archival_url());
        if let Some(q) = search {
            url.push_str(&format!("&search={}", urlencode(q)));
        }
        let passages: Vec<Passage> = self.send_json(self.client.get(url)).await?;
        // A full scan (no server-side query) that hits the cap may be
        // truncated — keys past the window become invisible to
        // read/write/delete. Warn so the operator can prune; don't warn on
        // bounded `search` calls, where hitting `limit` is expected.
        if search.is_none() && passages.len() >= limit {
            tracing::warn!(
                "letta archival scan returned {} passages (cap {}); entries beyond \
                 the cap can't be located by key, so writes may duplicate instead of \
                 overwrite — consider pruning archival memory",
                passages.len(),
                limit
            );
        }
        Ok(passages
            .into_iter()
            .map(|p| {
                let (scope, name, content) = parse_passage(&p.text);
                (scope, name, content, p.id)
            })
            .collect())
    }

    async fn fetch_blocks(&self) -> Result<Vec<Block>, MemoryError> {
        self.send_json(self.client.get(self.blocks_url())).await
    }

    fn is_core(&self, scope: &str) -> bool {
        scope == self.core_scope
    }
}

#[derive(Deserialize)]
struct Passage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    text: String,
}

#[derive(Serialize)]
struct InsertPassage<'a> {
    text: &'a str,
}

#[derive(Deserialize)]
struct Block {
    #[serde(default)]
    label: String,
    #[serde(default)]
    value: String,
}

#[derive(Serialize)]
struct UpdateBlock<'a> {
    value: &'a str,
}

/// Encode a `(scope, name, content)` tuple into an archival passage body.
fn encode_passage(scope: &str, name: &str, content: &str) -> String {
    format!("{KEY_PREFIX}{scope}/{name}\n\n{content}")
}

/// Parse an archival passage body back into `(scope, name, content)`.
/// Passages not written by Borg (no marker) surface under
/// `(global, "letta")` so server-side or externally-created passages
/// still reach the model instead of being silently dropped.
fn parse_passage(text: &str) -> (String, String, String) {
    if let Some(rest) = text.strip_prefix(KEY_PREFIX) {
        let (key_line, body) = rest.split_once('\n').unwrap_or((rest, ""));
        // Drop the single blank separator line that `encode_passage` adds.
        let body = body.strip_prefix('\n').unwrap_or(body);
        let (scope, name) = key_line.split_once('/').unwrap_or(("global", key_line));
        return (scope.to_string(), name.to_string(), body.to_string());
    }
    ("global".to_string(), "letta".to_string(), text.to_string())
}

#[async_trait]
impl MemoryProvider for LettaProvider {
    fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    async fn write(
        &self,
        _ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
        content: &str,
        mode: WriteMode,
    ) -> Result<(), MemoryError> {
        if self.is_core(scope) {
            // Core-memory tier: the entry is a named block. Append reads
            // the current value and concatenates; overwrite replaces.
            let resolved = match mode {
                WriteMode::Overwrite => content.to_string(),
                WriteMode::Append => {
                    let existing = self
                        .fetch_blocks()
                        .await?
                        .into_iter()
                        .find(|b| b.label == name)
                        .map(|b| b.value)
                        .unwrap_or_default();
                    if existing.is_empty() {
                        content.to_string()
                    } else {
                        format!("{existing}\n{content}")
                    }
                }
            };
            let url = format!("{}/{}", self.blocks_url(), urlencode(name));
            return self
                .send_ok(
                    self.client
                        .patch(url)
                        .json(&UpdateBlock { value: &resolved }),
                )
                .await;
        }

        // Archival tier: locate existing passages for this key so a write
        // is idempotent on the key rather than piling up sibling passages.
        let existing: Vec<(String, String, String, String)> = self
            .fetch_passages(None, ARCHIVAL_SCAN_LIMIT)
            .await?
            .into_iter()
            .filter(|(s, n, _, _)| s == scope && n == name)
            .collect();
        let resolved = match mode {
            WriteMode::Overwrite => content.to_string(),
            WriteMode::Append => {
                let prior: Vec<String> = existing
                    .iter()
                    .map(|(_, _, c, _)| c.clone())
                    .filter(|c| !c.is_empty())
                    .collect();
                if prior.is_empty() {
                    content.to_string()
                } else {
                    format!("{}\n{content}", prior.join("\n"))
                }
            }
        };
        for (_, _, _, id) in &existing {
            self.send_ok(
                self.client
                    .delete(format!("{}/{}", self.archival_url(), urlencode(id))),
            )
            .await?;
        }
        let body = encode_passage(scope, name, &resolved);
        self.send_ok(
            self.client
                .post(self.archival_url())
                .json(&InsertPassage { text: &body }),
        )
        .await
    }

    async fn read(
        &self,
        _ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
    ) -> Result<Option<MemoryEntry>, MemoryError> {
        if self.is_core(scope) {
            return Ok(self
                .fetch_blocks()
                .await?
                .into_iter()
                .find(|b| b.label == name)
                .map(|b| MemoryEntry {
                    scope: scope.to_string(),
                    name: name.to_string(),
                    content: b.value,
                    updated_at_epoch: None,
                }));
        }
        // Locate the entry the same way write/delete do — a full scan
        // filtered by key — so all three agree on whether a key is present.
        // A server-side `search` ranks by semantic similarity and can drop
        // the exact passage below its cap, making read disagree with
        // write/delete about existence.
        Ok(self
            .fetch_passages(None, ARCHIVAL_SCAN_LIMIT)
            .await?
            .into_iter()
            .find(|(s, n, _, _)| s == scope && n == name)
            .map(|(s, n, content, _)| MemoryEntry {
                scope: s,
                name: n,
                content,
                updated_at_epoch: None,
            }))
    }

    async fn search(
        &self,
        _ctx: &MemCtx<'_>,
        query: &SearchQuery,
    ) -> Result<Vec<SearchHit>, MemoryError> {
        let mut hits: Vec<SearchHit> = Vec::new();

        // Core-memory tier: surface blocks whose label or value match the
        // query, ranked above archival. Best-effort — a deployment without
        // the endpoint shouldn't sink the whole search.
        match self.fetch_blocks().await {
            Ok(blocks) => {
                let needle = query.query.to_lowercase();
                for b in blocks {
                    let hay = format!("{} {}", b.label, b.value).to_lowercase();
                    if hay.contains(&needle) && CORE_BLOCK_SCORE >= query.min_score {
                        hits.push(SearchHit {
                            scope: self.core_scope.clone(),
                            name: b.label,
                            snippet: b.value,
                            score: CORE_BLOCK_SCORE,
                        });
                    }
                }
            }
            Err(e) => {
                tracing::warn!("letta core-memory search skipped: {e}");
            }
        }

        // Archival tier: server-side semantic search. Letta returns rank
        // order without scores, so synthesise a descending score and apply
        // the caller's `min_score` floor.
        let passages = self
            .fetch_passages(Some(&query.query), query.max_results)
            .await?;
        let total = passages.len().max(1) as f32;
        for (i, (scope, name, content, _)) in passages.into_iter().enumerate() {
            let score = (ARCHIVAL_MAX_SCORE * (total - i as f32) / total).clamp(0.0, 1.0);
            if score < query.min_score {
                continue;
            }
            hits.push(SearchHit {
                scope,
                name,
                snippet: content,
                score,
            });
        }

        super::rank_and_truncate(&mut hits, query.max_results);
        Ok(hits)
    }

    async fn list(&self, _ctx: &MemCtx<'_>, scope: &str) -> Result<Vec<MemoryEntry>, MemoryError> {
        if self.is_core(scope) {
            return Ok(self
                .fetch_blocks()
                .await?
                .into_iter()
                .map(|b| MemoryEntry {
                    scope: scope.to_string(),
                    name: b.label,
                    content: b.value,
                    updated_at_epoch: None,
                })
                .collect());
        }
        Ok(self
            .fetch_passages(None, ARCHIVAL_SCAN_LIMIT)
            .await?
            .into_iter()
            .filter(|(s, _, _, _)| s == scope)
            .map(|(s, n, content, _)| MemoryEntry {
                scope: s,
                name: n,
                content,
                updated_at_epoch: None,
            })
            .collect())
    }

    async fn delete(
        &self,
        _ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
    ) -> Result<bool, MemoryError> {
        if self.is_core(scope) {
            // Core blocks are structural; "delete" clears the value rather
            // than detaching the block from the agent.
            let exists = self
                .fetch_blocks()
                .await?
                .into_iter()
                .any(|b| b.label == name);
            if !exists {
                return Ok(false);
            }
            let url = format!("{}/{}", self.blocks_url(), urlencode(name));
            self.send_ok(self.client.patch(url).json(&UpdateBlock { value: "" }))
                .await?;
            return Ok(true);
        }
        let matches: Vec<String> = self
            .fetch_passages(None, ARCHIVAL_SCAN_LIMIT)
            .await?
            .into_iter()
            .filter(|(s, n, _, _)| s == scope && n == name)
            .map(|(_, _, _, id)| id)
            .collect();
        if matches.is_empty() {
            return Ok(false);
        }
        for id in matches {
            self.send_ok(
                self.client
                    .delete(format!("{}/{}", self.archival_url(), urlencode(&id))),
            )
            .await?;
        }
        Ok(true)
    }

    async fn consolidate(&self, _ctx: &MemCtx<'_>) -> Result<ConsolidationOutcome, MemoryError> {
        Ok(ConsolidationOutcome::Delegated)
    }

    async fn health(&self, _ctx: &MemCtx<'_>) -> Result<(), MemoryError> {
        // Probe the archival tier — the default store for every non-core
        // scope, so this validates the path real writes take (a core-blocks
        // probe could pass while archival is misconfigured). A `limit=1` GET
        // exercises the api_key, the optional project header, and the
        // configured agent_id cheaply.
        let url = format!("{}?limit=1", self.archival_url());
        let _: serde_json::Value = self.send_json(self.client.get(url)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passage_marker_round_trips() {
        // The first-line key marker must survive encode → parse so reads,
        // deletes, and search-hit scoping locate the right entry.
        let body = encode_passage("project:abc", "notes", "line one\nline two");
        let (scope, name, content) = parse_passage(&body);
        assert_eq!(scope, "project:abc");
        assert_eq!(name, "notes");
        assert_eq!(content, "line one\nline two");
    }

    #[test]
    fn passage_without_marker_falls_back_to_global() {
        // Server-extracted passages carry no marker — they must still be
        // addressable rather than dropped.
        let (scope, name, content) = parse_passage("a fact the server distilled");
        assert_eq!(scope, "global");
        assert_eq!(name, "letta");
        assert_eq!(content, "a fact the server distilled");
    }
}
