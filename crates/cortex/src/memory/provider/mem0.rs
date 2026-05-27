//! Mem0 memory provider.
//!
//! Talks to Mem0's REST API. The service does server-side LLM fact
//! extraction on writes — Borg sends raw `(scope, name, content)` tuples
//! and the service decides what to retain. Reads/searches return the
//! distilled facts.
//!
//! Authoritative endpoints used:
//! - `POST   {base}/v1/memories/`               write
//! - `POST   {base}/v1/memories/search/`        search
//! - `GET    {base}/v1/memories/?user_id=…`     list
//! - `DELETE {base}/v1/memories/{id}/`          delete
//!
//! The provider keys entries client-side as `"{scope}/{name}"` in the
//! `metadata.borg_key` field so subsequent reads/deletes can locate
//! a specific entry deterministically — the service's own ids are
//! server-assigned and not directly addressable from a tool call.

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    urlencode, ConsolidationOutcome, MemCtx, MemoryEntry, MemoryError, MemoryProvider, SearchHit,
    SearchQuery,
};
use crate::config::media::Mem0Settings;
use crate::memory::WriteMode;

const DEFAULT_BASE_URL: &str = "https://api.mem0.ai";
const PROVIDER_NAME: &str = "mem0";

/// Mem0-backed memory provider.
#[derive(Debug, Clone)]
pub struct Mem0Provider {
    client: Client,
    base_url: String,
    api_key: String,
    user_id: String,
    organization: Option<String>,
    project: Option<String>,
}

impl Mem0Provider {
    /// Build from resolved settings. Returns `Err` when `api_key` is unset
    /// so the registry can fall back to Native.
    pub fn from_settings(settings: &Mem0Settings) -> Result<Self, MemoryError> {
        let api_key = settings
            .api_key
            .as_ref()
            .ok_or_else(|| MemoryError::NotConfigured {
                provider: PROVIDER_NAME,
                detail: "set memory.mem0.api_key (e.g. {\"env\":\"MEM0_API_KEY\"})".to_string(),
            })?
            .resolve()
            .map_err(|_e| MemoryError::NotConfigured {
                provider: PROVIDER_NAME,
                // The underlying error from `SecretRef::resolve` can include
                // exec stderr or file contents — never echo it back into the
                // tool-output stream or daemon log.
                detail: "api_key resolve failed; check memory.mem0.api_key".to_string(),
            })?;
        let base_url = settings
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
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
            organization: settings.organization.clone(),
            project: settings.project.clone(),
        })
    }

    /// Test-only constructor: lets `wiremock_uri()` be injected without
    /// going through `SecretRef::resolve`. Not exposed outside the crate.
    #[cfg(test)]
    pub(crate) fn for_tests(base_url: impl Into<String>) -> Self {
        Self {
            client: super::http_client(),
            base_url: base_url.into(),
            api_key: "test-key".into(),
            user_id: "test-user".into(),
            organization: None,
            project: None,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    fn auth_header(&self) -> String {
        format!("Token {}", self.api_key)
    }

    fn borg_key(scope: &str, name: &str) -> String {
        format!("{scope}/{name}")
    }

    async fn send_json<T: for<'de> Deserialize<'de>>(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<T, MemoryError> {
        super::send_json(req, PROVIDER_NAME, &self.auth_header(), &self.api_key).await
    }
}

#[derive(Serialize)]
struct WriteRequest<'a> {
    messages: Vec<Mem0Message<'a>>,
    user_id: &'a str,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    metadata: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    org_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<&'a str>,
}

#[derive(Serialize)]
struct Mem0Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<SearchRow>,
}

#[derive(Deserialize)]
struct SearchRow {
    #[serde(default)]
    memory: String,
    #[serde(default)]
    score: Option<f32>,
    #[serde(default)]
    metadata: serde_json::Value,
}

#[derive(Deserialize)]
struct ListResponse {
    #[serde(default)]
    results: Vec<ListRow>,
}

#[derive(Deserialize)]
struct ListRow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    memory: String,
    #[serde(default)]
    metadata: serde_json::Value,
    #[serde(default)]
    updated_at: Option<String>,
}

#[async_trait]
impl MemoryProvider for Mem0Provider {
    fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    async fn write(
        &self,
        ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
        content: &str,
        mode: WriteMode,
    ) -> Result<(), MemoryError> {
        // Mem0's POST always creates a new memory — the server has no
        // contract for an "append" mode. Preserve borg's append semantics
        // (concatenate to existing) by reading the prior entry and
        // submitting the joined content as a fresh overwrite. Without
        // this, append writes silently become sibling entries and
        // read_memory returns only the latest payload.
        let resolved_content: String = match mode {
            WriteMode::Overwrite => content.to_string(),
            WriteMode::Append => match self.read(ctx, scope, name).await? {
                Some(existing) if !existing.content.is_empty() => {
                    format!("{}\n{}", existing.content, content)
                }
                _ => content.to_string(),
            },
        };
        let metadata = json!({
            "borg_key": Self::borg_key(scope, name),
            "scope": scope,
            "name": name,
        });
        let body = WriteRequest {
            messages: vec![Mem0Message {
                role: "user",
                content: &resolved_content,
            }],
            user_id: &self.user_id,
            metadata,
            org_id: self.organization.as_deref(),
            project_id: self.project.as_deref(),
        };
        let _: serde_json::Value = self
            .send_json(self.client.post(self.url("/v1/memories/")).json(&body))
            .await?;
        Ok(())
    }

    async fn read(
        &self,
        _ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
    ) -> Result<Option<MemoryEntry>, MemoryError> {
        let key = Self::borg_key(scope, name);
        let body = json!({
            "query": key,
            "user_id": self.user_id,
            "filters": { "metadata": { "borg_key": key } },
            "limit": 1,
        });
        let resp: SearchResponse = self
            .send_json(
                self.client
                    .post(self.url("/v1/memories/search/"))
                    .json(&body),
            )
            .await?;
        Ok(resp.results.into_iter().next().map(|row| MemoryEntry {
            scope: scope.to_string(),
            name: name.to_string(),
            content: row.memory,
            updated_at_epoch: None,
        }))
    }

    async fn search(
        &self,
        _ctx: &MemCtx<'_>,
        query: &SearchQuery,
    ) -> Result<Vec<SearchHit>, MemoryError> {
        let body = json!({
            "query": query.query,
            "user_id": self.user_id,
            "limit": query.max_results,
        });
        let resp: SearchResponse = self
            .send_json(
                self.client
                    .post(self.url("/v1/memories/search/"))
                    .json(&body),
            )
            .await?;
        Ok(resp
            .results
            .into_iter()
            .filter_map(|row| {
                let key = row
                    .metadata
                    .get("borg_key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("global/mem0");
                let (scope, name) = key.split_once('/').unwrap_or(("global", key));
                let score = row.score.unwrap_or(0.0).clamp(0.0, 1.0);
                if score < query.min_score {
                    return None;
                }
                Some(SearchHit {
                    scope: scope.to_string(),
                    name: name.to_string(),
                    snippet: row.memory,
                    score,
                })
            })
            .collect())
    }

    async fn list(&self, _ctx: &MemCtx<'_>, scope: &str) -> Result<Vec<MemoryEntry>, MemoryError> {
        let url = format!(
            "{}?user_id={}",
            self.url("/v1/memories/"),
            urlencode(&self.user_id)
        );
        let resp: ListResponse = self.send_json(self.client.get(url)).await?;
        Ok(resp
            .results
            .into_iter()
            .filter_map(|row| {
                // Rows written through borg carry an explicit `borg_key`
                // metadata field shaped as `"{scope}/{name}"`. Rows
                // created by Mem0's server-side fact extraction don't,
                // and would otherwise be invisible to `list` — defeating
                // the point of selecting Mem0. Synthesise a
                // `(scope=global, name=mem0-<id-prefix>)` key for those
                // so distilled facts surface alongside operator writes.
                let (row_scope, row_name) =
                    match row.metadata.get("borg_key").and_then(|v| v.as_str()) {
                        Some(key) => match key.split_once('/') {
                            Some((s, n)) => (s.to_string(), n.to_string()),
                            None => ("global".to_string(), key.to_string()),
                        },
                        None => {
                            let suffix: String = row.id.chars().take(8).collect();
                            ("global".to_string(), format!("mem0-{suffix}"))
                        }
                    };
                if row_scope != scope {
                    return None;
                }
                let updated = row.updated_at.and_then(|s| {
                    chrono::DateTime::parse_from_rfc3339(&s)
                        .ok()
                        .map(|dt| dt.timestamp())
                });
                Some(MemoryEntry {
                    scope: row_scope,
                    name: row_name,
                    content: row.memory,
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
        // Mem0 deletes by id; we lookup-by-metadata first.
        let key = Self::borg_key(scope, name);
        let lookup = json!({
            "query": key,
            "user_id": self.user_id,
            "filters": { "metadata": { "borg_key": key } },
            "limit": 1,
        });
        let resp: serde_json::Value = self
            .send_json(
                self.client
                    .post(self.url("/v1/memories/search/"))
                    .json(&lookup),
            )
            .await?;
        let Some(id) = resp
            .get("results")
            .and_then(|r| r.as_array())
            .and_then(|arr| arr.first())
            .and_then(|row| row.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            return Ok(false);
        };
        let _: serde_json::Value = self
            .send_json(self.client.delete(self.url(&format!("/v1/memories/{id}/"))))
            .await?;
        Ok(true)
    }

    async fn consolidate(&self, _ctx: &MemCtx<'_>) -> Result<ConsolidationOutcome, MemoryError> {
        // The service consolidates server-side; nothing to do here.
        Ok(ConsolidationOutcome::Delegated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borg_key_format_is_stable() {
        // Pinned: parsing back via `split_once('/')` must round-trip,
        // otherwise the search-result mapping in `search()` silently
        // misroutes hits to the wrong scope.
        let key = Mem0Provider::borg_key("project:abc", "notes");
        let (scope, name) = key.split_once('/').unwrap();
        assert_eq!(scope, "project:abc");
        assert_eq!(name, "notes");
    }
}
