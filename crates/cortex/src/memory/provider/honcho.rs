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
    /// Build from resolved settings.
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
        let resp = req
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .send()
            .await
            .map_err(|e| MemoryError::Transport {
                provider: PROVIDER_NAME,
                detail: e.to_string(),
            })?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| MemoryError::Transport {
            provider: PROVIDER_NAME,
            detail: e.to_string(),
        })?;
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes);
            let stripped = body.replace(&self.api_key, "<redacted>");
            let redacted = crate::security::secrets::redact_secrets(&stripped);
            return Err(MemoryError::Backend {
                provider: PROVIDER_NAME,
                detail: format!(
                    "{}: {}",
                    status.as_u16(),
                    redacted.chars().take(400).collect::<String>()
                ),
            });
        }
        serde_json::from_slice(&bytes).map_err(|e| MemoryError::Backend {
            provider: PROVIDER_NAME,
            detail: format!("response decode failed: {e}"),
        })
    }
}

/// Stable session id for a given scope. We use a single session per
/// scope so message lookup by entry-name metadata stays bounded.
fn session_id(scope: &str) -> String {
    format!("borg-{}", scope.replace(':', "-"))
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
        // Best-effort idempotent session create — 409 conflicts are fine.
        let create_url = format!(
            "{}/v1/apps/{}/users/{}/sessions/",
            self.base_url.trim_end_matches('/'),
            self.app_id,
            self.peer_id
        );
        let _ = self
            .client
            .post(&create_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&json!({ "id": session_id(scope) }))
            .send()
            .await;

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
        let url = self.url(&format!(
            "{}/dialectic/",
            self.session_path(ctx.default_scope)
        ));
        let body = json!({ "query": query.query, "limit": query.max_results });
        let resp: DialecticResponse = self.send_json(self.client.post(url).json(&body)).await?;
        let mut hits: Vec<SearchHit> = resp
            .citations
            .into_iter()
            .filter_map(|c| {
                let scope = c
                    .metadata
                    .get("borg_scope")
                    .and_then(|v| v.as_str())
                    .unwrap_or(ctx.default_scope)
                    .to_string();
                let name = c
                    .metadata
                    .get("borg_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("honcho")
                    .to_string();
                let score = c.score.unwrap_or(0.5).clamp(0.0, 1.0);
                if score < query.min_score {
                    return None;
                }
                Some(SearchHit {
                    scope,
                    name,
                    snippet: c.content,
                    score,
                })
            })
            .collect();
        // If citations were empty but the dialectic produced an answer,
        // surface it as a single synthesized hit so the agent still gets
        // signal.
        if hits.is_empty() && !resp.answer.is_empty() {
            hits.push(SearchHit {
                scope: ctx.default_scope.to_string(),
                name: "honcho-dialectic".to_string(),
                snippet: resp.answer,
                score: 0.5,
            });
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_replaces_colons() {
        // The colon in `project:abc` would break Honcho's path-segment
        // routing — pin the replacement.
        assert_eq!(session_id("global"), "borg-global");
        assert_eq!(session_id("project:abc"), "borg-project-abc");
    }
}
