//! Zep memory provider.
//!
//! Zep pairs per-session conversational memory (with server-side enriched
//! summaries) with a temporal knowledge graph spanning all of a user's
//! sessions. Borg uses both tiers:
//!
//! - **Storage** is session-per-scope (like [`super::honcho`]): each
//!   `(scope, name)` entry is written as a message tagged with
//!   `borg_name` / `borg_scope` metadata into the scope's session.
//! - **Retrieval** (`search`) runs Zep's user-level **graph search** for
//!   semantically related facts, and additionally surfaces each in-scope
//!   session's **enriched summary** (`context`) as a synthesised hit —
//!   this is the signal a plain message search would miss.
//!
//! Endpoints (under `base_url`, default Zep Cloud):
//! - `POST   {base}/api/v2/sessions`                              create
//! - `POST   {base}/api/v2/sessions/{sid}/memory`                 write
//! - `GET    {base}/api/v2/sessions/{sid}/memory`                 list + summary
//! - `DELETE {base}/api/v2/sessions/{sid}/messages/{msgId}`       delete
//! - `POST   {base}/api/v2/graph/search`                          search
//!
//! Auth is the Zep Cloud `Authorization: Api-Key <key>` scheme.

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    ConsolidationOutcome, MemCtx, MemoryEntry, MemoryError, MemoryProvider, SearchHit, SearchQuery,
};
use crate::config::media::ZepSettings;
use crate::memory::WriteMode;

const DEFAULT_BASE_URL: &str = "https://api.getzep.com";
const PROVIDER_NAME: &str = "zep";
/// Score for the synthesised session-summary hit. Mirrors Honcho's
/// dialectic-answer fallback so summaries rank among, not above, real hits.
const SUMMARY_SCORE: f32 = 0.5;

/// Zep-backed memory provider.
#[derive(Debug, Clone)]
pub struct ZepProvider {
    client: Client,
    base_url: String,
    api_key: String,
    user_id: String,
}

impl ZepProvider {
    /// Build from resolved settings. Returns `Err` when `api_key` is unset
    /// so the registry can fall back to Native.
    pub fn from_settings(settings: &ZepSettings) -> Result<Self, MemoryError> {
        let api_key = settings
            .api_key
            .as_ref()
            .ok_or_else(|| MemoryError::NotConfigured {
                provider: PROVIDER_NAME,
                detail: "set memory.zep.api_key (e.g. {\"env\":\"ZEP_API_KEY\"})".to_string(),
            })?
            .resolve()
            .map_err(|_e| MemoryError::NotConfigured {
                provider: PROVIDER_NAME,
                // `SecretRef::resolve` errors can echo exec stderr / file
                // contents — never surface them.
                detail: "api_key resolve failed; check memory.zep.api_key".to_string(),
            })?;
        let base_url = settings
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        super::endpoint_guard::validate_endpoint_syntax(&base_url, PROVIDER_NAME)?;
        let user_id = settings
            .user_id
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "borg".to_string());
        Ok(Self {
            client: super::http_client(),
            base_url,
            api_key,
            user_id,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_tests(base_url: impl Into<String>) -> Self {
        Self {
            client: super::http_client(),
            base_url: base_url.into(),
            api_key: "test-key".into(),
            user_id: "test-user".into(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    fn auth_header(&self) -> String {
        format!("Api-Key {}", self.api_key)
    }

    fn session_id(scope: &str) -> String {
        super::scoped_session_id("borg-", scope)
    }

    async fn send_json<T: for<'de> Deserialize<'de>>(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<T, MemoryError> {
        super::send_json(req, PROVIDER_NAME, &self.auth_header(), &self.api_key).await
    }

    async fn send_ok(&self, req: reqwest::RequestBuilder) -> Result<(), MemoryError> {
        super::send_ok(req, PROVIDER_NAME, &self.auth_header(), &self.api_key).await
    }

    /// Best-effort idempotent session create. A pre-existing session (409 /
    /// 400) is fine; other failures get a breadcrumb because the follow-up
    /// memory write would otherwise 404 with the real cause lost.
    async fn ensure_session(&self, scope: &str) {
        let url = self.url("/api/v2/sessions");
        let body = json!({ "session_id": Self::session_id(scope), "user_id": self.user_id });
        match super::guarded_send(
            self.client
                .post(url)
                .header("Authorization", self.auth_header())
                .header("Content-Type", "application/json")
                .json(&body),
            PROVIDER_NAME,
        )
        .await
        {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() && status.as_u16() != 409 && status.as_u16() != 400 {
                    tracing::warn!(
                        "zep session create returned {} for {} — memory write may 404",
                        status.as_u16(),
                        Self::session_id(scope)
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    "zep session create transport error for {}: {e} — memory write may 404",
                    Self::session_id(scope)
                );
            }
        }
    }

    async fn fetch_memory(&self, scope: &str) -> Result<ZepMemory, MemoryError> {
        let url = self.url(&format!(
            "/api/v2/sessions/{}/memory",
            Self::session_id(scope)
        ));
        self.send_json(self.client.get(url)).await
    }
}

#[derive(Serialize)]
struct AddMemory<'a> {
    messages: Vec<WriteMsg<'a>>,
}

#[derive(Serialize)]
struct WriteMsg<'a> {
    role: &'a str,
    role_type: &'a str,
    content: &'a str,
    metadata: serde_json::Value,
}

#[derive(Deserialize, Default)]
struct ZepMemory {
    #[serde(default)]
    messages: Vec<ZepMessage>,
    /// Server-built enriched summary of the session.
    #[serde(default)]
    context: String,
}

#[derive(Deserialize)]
struct ZepMessage {
    #[serde(default)]
    uuid: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    metadata: serde_json::Value,
    #[serde(default)]
    created_at: Option<String>,
}

impl ZepMessage {
    fn borg_name(&self) -> Option<&str> {
        self.metadata.get("borg_name").and_then(|v| v.as_str())
    }

    fn created_epoch(&self) -> Option<i64> {
        self.created_at.as_ref().and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.timestamp())
        })
    }
}

#[derive(Deserialize)]
struct GraphSearchResponse {
    #[serde(default)]
    edges: Vec<GraphEdge>,
}

#[derive(Deserialize)]
struct GraphEdge {
    #[serde(default)]
    fact: String,
    #[serde(default)]
    score: Option<f32>,
}

#[async_trait]
impl MemoryProvider for ZepProvider {
    fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    async fn write(
        &self,
        _ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
        content: &str,
        _mode: WriteMode,
    ) -> Result<(), MemoryError> {
        // Zep memory is an append-oriented message log, like Honcho — a
        // write adds a message; `read`/`list` resolve the latest message
        // per `borg_name`. `WriteMode` therefore has no distinct overwrite
        // path (the newest write wins on read).
        self.ensure_session(scope).await;
        let url = self.url(&format!(
            "/api/v2/sessions/{}/memory",
            Self::session_id(scope)
        ));
        let body = AddMemory {
            messages: vec![WriteMsg {
                role: "user",
                role_type: "user",
                content,
                metadata: json!({ "borg_name": name, "borg_scope": scope }),
            }],
        };
        self.send_ok(self.client.post(url).json(&body)).await
    }

    async fn read(
        &self,
        ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
    ) -> Result<Option<MemoryEntry>, MemoryError> {
        // Resolve to the most-recently-written message for this name so a
        // re-write shadows the stale value instead of returning the oldest.
        Ok(self
            .list(ctx, scope)
            .await?
            .into_iter()
            .filter(|e| e.name == name)
            .max_by_key(|e| e.updated_at_epoch.unwrap_or(i64::MIN)))
    }

    async fn search(
        &self,
        ctx: &MemCtx<'_>,
        query: &SearchQuery,
    ) -> Result<Vec<SearchHit>, MemoryError> {
        let mut hits: Vec<SearchHit> = Vec::new();

        // User-level knowledge-graph search — spans every session/scope.
        let url = self.url("/api/v2/graph/search");
        let body = json!({
            "user_id": self.user_id,
            "query": query.query,
            "scope": "edges",
            "limit": query.max_results,
        });
        // A user whose knowledge graph hasn't been built yet returns 404 —
        // treat that as "no graph facts" and still surface the per-scope
        // session summaries below, rather than failing the whole search.
        let resp: GraphSearchResponse =
            match self.send_json(self.client.post(url).json(&body)).await {
                Ok(r) => r,
                Err(MemoryError::Backend { detail, .. }) if detail.starts_with("404") => {
                    GraphSearchResponse { edges: Vec::new() }
                }
                Err(e) => return Err(e),
            };
        for (i, edge) in resp.edges.into_iter().enumerate() {
            if edge.fact.is_empty() {
                continue;
            }
            // Fall back to a descending rank score when Zep omits one.
            let rank = 1.0 - (i as f32 / query.max_results.max(1) as f32);
            let score = edge.score.unwrap_or(rank).clamp(0.0, 1.0);
            if score < query.min_score {
                continue;
            }
            hits.push(SearchHit {
                scope: "global".to_string(),
                name: "zep-fact".to_string(),
                snippet: edge.fact,
                score,
            });
        }

        // Per-scope enriched session summaries. A missing session (404) is
        // not an error — that scope just has nothing stored yet.
        if SUMMARY_SCORE >= query.min_score {
            for scope in crate::memory::resolved_memory_scopes(ctx.config) {
                match self.fetch_memory(&scope).await {
                    Ok(mem) if !mem.context.is_empty() => hits.push(SearchHit {
                        scope: scope.clone(),
                        name: "zep-summary".to_string(),
                        snippet: mem.context,
                        score: SUMMARY_SCORE,
                    }),
                    Ok(_) => {}
                    Err(MemoryError::Backend { detail, .. }) if detail.starts_with("404") => {}
                    Err(e) => return Err(e),
                }
            }
        }

        super::rank_and_truncate(&mut hits, query.max_results);
        Ok(hits)
    }

    async fn list(&self, _ctx: &MemCtx<'_>, scope: &str) -> Result<Vec<MemoryEntry>, MemoryError> {
        let mem = match self.fetch_memory(scope).await {
            Ok(m) => m,
            // No session for this scope yet — empty, not an error.
            Err(MemoryError::Backend { detail, .. }) if detail.starts_with("404") => {
                return Ok(Vec::new())
            }
            Err(e) => return Err(e),
        };
        Ok(mem
            .messages
            .into_iter()
            .filter_map(|m| {
                let name = m.borg_name()?.to_string();
                let updated = m.created_epoch();
                Some(MemoryEntry {
                    scope: scope.to_string(),
                    name,
                    content: m.content,
                    updated_at_epoch: updated,
                })
            })
            .collect())
    }

    async fn delete(
        &self,
        _ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
    ) -> Result<bool, MemoryError> {
        let mem = match self.fetch_memory(scope).await {
            Ok(m) => m,
            Err(MemoryError::Backend { detail, .. }) if detail.starts_with("404") => {
                return Ok(false)
            }
            Err(e) => return Err(e),
        };
        // Newest-first so a delete removes the live value, not a shadowed one.
        let mut matches: Vec<ZepMessage> = mem
            .messages
            .into_iter()
            .filter(|m| m.borg_name() == Some(name))
            .collect();
        matches.sort_by_key(|m| std::cmp::Reverse(m.created_epoch().unwrap_or(i64::MIN)));
        let Some(target) = matches.into_iter().find(|m| !m.uuid.is_empty()) else {
            return Ok(false);
        };
        let url = self.url(&format!(
            "/api/v2/sessions/{}/messages/{}",
            Self::session_id(scope),
            super::urlencode(&target.uuid)
        ));
        self.send_ok(self.client.delete(url)).await?;
        Ok(true)
    }

    async fn consolidate(&self, _ctx: &MemCtx<'_>) -> Result<ConsolidationOutcome, MemoryError> {
        Ok(ConsolidationOutcome::Delegated)
    }

    async fn health(&self, _ctx: &MemCtx<'_>) -> Result<(), MemoryError> {
        // Graph search with the configured user is the cheapest call that
        // exercises both the api_key and the user_id without writing. A 404
        // (user has no graph yet) still proves creds + endpoint are good, so
        // it counts as healthy.
        let url = self.url("/api/v2/graph/search");
        let body =
            json!({ "user_id": self.user_id, "query": "health", "scope": "edges", "limit": 1 });
        match self
            .send_json::<serde_json::Value>(self.client.post(url).json(&body))
            .await
        {
            Ok(_) => Ok(()),
            Err(MemoryError::Backend { detail, .. }) if detail.starts_with("404") => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_uses_shared_injective_encoder() {
        // Colon-bearing scopes must not collide with hyphen-bearing ones —
        // otherwise a write to one scope's session lands in another's.
        assert_eq!(ZepProvider::session_id("global"), "borg-global");
        assert_ne!(
            ZepProvider::session_id("project:abc"),
            ZepProvider::session_id("project-abc"),
        );
    }
}
