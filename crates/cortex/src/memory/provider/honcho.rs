//! Honcho memory provider.
//!
//! Honcho models memory around `App → User → Session → Message`. Borg's
//! `(scope, name)` keys don't map 1:1 — we use a single session-per-scope
//! convention and write each `(scope, name)` entry as a message tagged
//! with the entry name in its metadata. Search uses Honcho's `dialectic`
//! Q&A endpoint when available, falling back to plain message search.
//!
//! Endpoints used:
//! - `POST {base}/v1/apps/{app}/users/{peer}/sessions/`               session create
//! - `POST {base}/v1/apps/{app}/users/{peer}/sessions/{sid}/messages/` write
//! - `GET  {base}/v1/apps/{app}/users/{peer}/sessions/{sid}/messages/` list
//! - `POST {base}/v1/apps/{app}/users/{peer}/sessions/{sid}/dialectic/` search

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    ConsolidationOutcome, MemCtx, MemoryEntry, MemoryError, MemoryProvider, SearchHit, SearchQuery,
};
use crate::config::media::HonchoSettings;
use crate::memory::WriteMode;

const DEFAULT_BASE_URL: &str = "https://api.honcho.dev";
const PROVIDER_NAME: &str = "honcho";

/// Honcho-backed memory provider.
#[derive(Debug, Clone)]
pub struct HonchoProvider {
    client: Client,
    base_url: String,
    api_key: String,
    app_id: String,
    peer_id: String,
}

impl HonchoProvider {
    pub fn from_settings(settings: &HonchoSettings) -> Result<Self, MemoryError> {
        let api_key = settings
            .api_key
            .as_ref()
            .ok_or_else(|| MemoryError::NotConfigured {
                provider: PROVIDER_NAME,
                detail: "set memory.honcho.api_key (e.g. {\"env\":\"HONCHO_API_KEY\"})".to_string(),
            })?
            .resolve()
            .map_err(|_e| MemoryError::NotConfigured {
                provider: PROVIDER_NAME,
                detail: "api_key resolve failed; check memory.honcho.api_key".to_string(),
            })?;
        let app_id = settings
            .app_id
            .clone()
            .ok_or_else(|| MemoryError::NotConfigured {
                provider: PROVIDER_NAME,
                detail: "set memory.honcho.app_id".to_string(),
            })?;
        let peer_id = settings
            .peer_id
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "borg".to_string());
        let base_url = settings
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        super::endpoint_guard::validate_endpoint_syntax(&base_url, PROVIDER_NAME)?;
        Ok(Self {
            client: super::http_client(),
            base_url,
            api_key,
            app_id,
            peer_id,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_tests(base_url: impl Into<String>) -> Self {
        Self {
            client: super::http_client(),
            base_url: base_url.into(),
            api_key: "test-key".into(),
            app_id: "app".into(),
            peer_id: "peer".into(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.api_key)
    }

    fn session_path(&self, scope: &str) -> String {
        format!(
            "/v1/apps/{}/users/{}/sessions/{}",
            self.app_id,
            self.peer_id,
            session_id(scope)
        )
    }

    async fn send_json<T: for<'de> Deserialize<'de>>(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<T, MemoryError> {
        super::send_json(req, PROVIDER_NAME, &self.auth_header(), &self.api_key).await
    }
}

/// Stable session id for a given scope. We use a single session per
/// scope so message lookup by entry-name metadata stays bounded. The
/// injective encoding lives in [`super::scoped_session_id`] (shared with
/// the Zep provider); Honcho uses the `borg-` namespace prefix.
fn session_id(scope: &str) -> String {
    super::scoped_session_id("borg-", scope)
}

#[derive(Serialize)]
struct MessageWrite<'a> {
    content: &'a str,
    role: &'a str,
    metadata: serde_json::Value,
}

#[derive(Deserialize)]
struct MessageRow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    metadata: serde_json::Value,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Deserialize)]
struct MessageList {
    #[serde(default)]
    items: Vec<MessageRow>,
}

#[derive(Deserialize)]
struct DialecticResponse {
    #[serde(default)]
    answer: String,
    #[serde(default)]
    citations: Vec<DialecticCitation>,
}

#[derive(Deserialize)]
struct DialecticCitation {
    #[serde(default)]
    content: String,
    #[serde(default)]
    metadata: serde_json::Value,
    #[serde(default)]
    score: Option<f32>,
}

#[async_trait]
impl MemoryProvider for HonchoProvider {
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
        // Best-effort idempotent session create — 409 conflicts are fine,
        // but other failures (401, transport errors, 5xx) should surface
        // a breadcrumb. Otherwise the follow-up POST fails with a
        // confusing "session not found" 404 and the real cause is lost.
        let create_url = format!(
            "{}/v1/apps/{}/users/{}/sessions/",
            self.base_url.trim_end_matches('/'),
            self.app_id,
            self.peer_id
        );
        match super::guarded_send(
            self.client
                .post(&create_url)
                .header("Authorization", self.auth_header())
                .header("Content-Type", "application/json")
                .json(&json!({ "id": session_id(scope) })),
            PROVIDER_NAME,
        )
        .await
        {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() && status.as_u16() != 409 {
                    tracing::warn!(
                        "honcho session create returned {} for {} \
                         — subsequent message write may 404",
                        status.as_u16(),
                        session_id(scope)
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    "honcho session create transport error for {}: {} \
                     — subsequent message write may 404",
                    session_id(scope),
                    e
                );
            }
        }

        let body = MessageWrite {
            content,
            role: "user",
            metadata: json!({ "borg_name": name, "borg_scope": scope }),
        };
        let url = self.url(&format!("{}/messages/", self.session_path(scope)));
        let _: serde_json::Value = self.send_json(self.client.post(url).json(&body)).await?;
        Ok(())
    }

    async fn read(
        &self,
        ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
    ) -> Result<Option<MemoryEntry>, MemoryError> {
        let entries = self.list(ctx, scope).await?;
        Ok(entries.into_iter().find(|e| e.name == name))
    }

    async fn search(
        &self,
        ctx: &MemCtx<'_>,
        query: &SearchQuery,
    ) -> Result<Vec<SearchHit>, MemoryError> {
        // Honcho's session model is per-scope, so a single dialectic call
        // only sees the messages in one session. Borg's native search
        // enumerates every resolved scope (`global` + per-project) and
        // merges — match that surface here so multi-scope deployments
        // don't silently lose hits outside `default_scope`.
        let scopes = crate::memory::resolved_memory_scopes(ctx.config);
        let mut hits: Vec<SearchHit> = Vec::new();
        const SYNTHESIZED_SCORE: f32 = 0.5;
        for scope in &scopes {
            let url = self.url(&format!("{}/dialectic/", self.session_path(scope)));
            let body = json!({ "query": query.query, "limit": query.max_results });
            let resp: DialecticResponse =
                match self.send_json(self.client.post(url).json(&body)).await {
                    Ok(r) => r,
                    Err(MemoryError::Backend { detail, .. }) if detail.starts_with("404") => {
                        // Scope's session doesn't exist yet — not an error.
                        continue;
                    }
                    Err(e) => return Err(e),
                };
            for c in resp.citations {
                let cite_scope = c
                    .metadata
                    .get("borg_scope")
                    .and_then(|v| v.as_str())
                    .unwrap_or(scope)
                    .to_string();
                let name = c
                    .metadata
                    .get("borg_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("honcho")
                    .to_string();
                let score = c.score.unwrap_or(SYNTHESIZED_SCORE).clamp(0.0, 1.0);
                if score < query.min_score {
                    continue;
                }
                hits.push(SearchHit {
                    scope: cite_scope,
                    name,
                    snippet: c.content,
                    score,
                });
            }
            // Per-scope synthesized-answer fallback when this scope has no
            // citations but the dialectic returned a useful answer.
            if !resp.answer.is_empty() && SYNTHESIZED_SCORE >= query.min_score {
                hits.push(SearchHit {
                    scope: scope.clone(),
                    name: "honcho-dialectic".to_string(),
                    snippet: resp.answer,
                    score: SYNTHESIZED_SCORE,
                });
            }
        }
        super::rank_and_truncate(&mut hits, query.max_results);
        Ok(hits)
    }

    async fn list(&self, _ctx: &MemCtx<'_>, scope: &str) -> Result<Vec<MemoryEntry>, MemoryError> {
        let url = self.url(&format!("{}/messages/", self.session_path(scope)));
        let resp: MessageList = self.send_json(self.client.get(url)).await?;
        Ok(resp
            .items
            .into_iter()
            .map(|row| {
                let name = row
                    .metadata
                    .get("borg_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&row.id)
                    .to_string();
                let updated = row.created_at.and_then(|s| {
                    chrono::DateTime::parse_from_rfc3339(&s)
                        .ok()
                        .map(|dt| dt.timestamp())
                });
                MemoryEntry {
                    scope: scope.to_string(),
                    name,
                    content: row.content,
                    updated_at_epoch: updated,
                }
            })
            .collect())
    }

    async fn delete(
        &self,
        _ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
    ) -> Result<bool, MemoryError> {
        // Locate by message metadata; delete via individual message endpoint.
        let list_url = self.url(&format!("{}/messages/", self.session_path(scope)));
        let resp: MessageList = self.send_json(self.client.get(list_url)).await?;
        let target = resp.items.into_iter().find(|row| {
            row.metadata
                .get("borg_name")
                .and_then(|v| v.as_str())
                .map(|v| v == name)
                .unwrap_or(false)
        });
        let Some(row) = target else { return Ok(false) };
        let del_url = self.url(&format!(
            "{}/messages/{}/",
            self.session_path(scope),
            row.id
        ));
        let _: serde_json::Value = self.send_json(self.client.delete(del_url)).await?;
        Ok(true)
    }

    async fn consolidate(&self, _ctx: &MemCtx<'_>) -> Result<ConsolidationOutcome, MemoryError> {
        Ok(ConsolidationOutcome::Delegated)
    }

    async fn health(&self, _ctx: &MemCtx<'_>) -> Result<(), MemoryError> {
        // List the peer's sessions — exercises both the api_key and the
        // configured app_id without writing anything.
        let url = self.url(&format!(
            "/v1/apps/{}/users/{}/sessions/",
            self.app_id, self.peer_id
        ));
        let _: serde_json::Value = self.send_json(self.client.get(url)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_escapes_colons_distinctly_from_hyphens() {
        // The colon in `project:abc` would break Honcho's path-segment
        // routing if left literal; previously `replace(':', "-")` collided
        // `project:abc` with `project-abc`. Pin both the encoding shape
        // and the non-collision invariant.
        assert_eq!(session_id("global"), "borg-global");
        assert_eq!(session_id("project:abc"), "borg-project_cabc");
        assert_eq!(session_id("project-abc"), "borg-project-abc");
        assert_ne!(
            session_id("project:abc"),
            session_id("project-abc"),
            "colon-scope and hyphen-scope must produce distinct session ids"
        );
    }

    #[test]
    fn session_id_is_injective_under_underscore_input() {
        // Underscores in scope names must survive the escape — otherwise
        // a scope containing `_c` could collide with one containing `:`.
        assert_ne!(session_id("foo_cbar"), session_id("foo:bar"));
        assert_eq!(session_id("foo_bar"), "borg-foo__bar");
    }
}
