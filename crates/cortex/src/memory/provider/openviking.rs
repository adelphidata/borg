//! OpenViking memory provider.
//!
//! OpenViking exposes a hierarchical context store keyed by
//! `viking://<account>/<user>/<agent>/<scope>/<name>`. Borg's `(scope, name)`
//! tuples map directly onto the last two URI segments; the first three are
//! configured per-deployment.
//!
//! Endpoints (mounted under the operator-supplied `endpoint`):
//! - `PUT    {endpoint}/v1/notes/{path...}`     write
//! - `GET    {endpoint}/v1/notes/{path...}`     read
//! - `DELETE {endpoint}/v1/notes/{path...}`     delete
//! - `GET    {endpoint}/v1/notes/{prefix}?list` list
//! - `POST   {endpoint}/v1/search`               search

use anyhow::Result;
use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    ConsolidationOutcome, MemCtx, MemoryEntry, MemoryError, MemoryProvider, SearchHit, SearchQuery,
};
use crate::config::media::OpenVikingSettings;
use crate::memory::WriteMode;

const PROVIDER_NAME: &str = "openviking";

/// OpenViking-backed memory provider.
#[derive(Debug, Clone)]
pub struct OpenVikingProvider {
    client: Client,
    endpoint: String,
    api_key: String,
    account: String,
    user: String,
    agent: String,
}

impl OpenVikingProvider {
    /// Build from resolved settings.
    pub fn from_settings(settings: &OpenVikingSettings) -> Result<Self, MemoryError> {
        let endpoint = settings
            .endpoint
            .clone()
            .ok_or_else(|| MemoryError::NotConfigured {
                provider: PROVIDER_NAME,
                detail: "set memory.openviking.endpoint to your service URL".to_string(),
            })?;
        let api_key = settings
            .api_key
            .as_ref()
            .ok_or_else(|| MemoryError::NotConfigured {
                provider: PROVIDER_NAME,
                detail: "set memory.openviking.api_key".to_string(),
            })?
            .resolve()
            .map_err(|_e| MemoryError::NotConfigured {
                provider: PROVIDER_NAME,
                detail: "api_key resolve failed; check memory.openviking.api_key".to_string(),
            })?;
        let account = settings
            .account
            .clone()
            .ok_or_else(|| MemoryError::NotConfigured {
                provider: PROVIDER_NAME,
                detail: "set memory.openviking.account".to_string(),
            })?;
        let user = settings
            .user
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "borg".to_string());
        let agent = settings.agent.clone().unwrap_or_else(|| "borg".to_string());
        Ok(Self {
            client: super::http_client(),
            endpoint,
            api_key,
            account,
            user,
            agent,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_tests(endpoint: impl Into<String>) -> Self {
        Self {
            client: super::http_client(),
            endpoint: endpoint.into(),
            api_key: "test-key".into(),
            account: "acct".into(),
            user: "user".into(),
            agent: "borg".into(),
        }
    }

    fn note_url(&self, scope: &str, name: &str) -> String {
        format!(
            "{}/v1/notes/{}/{}/{}/{}/{}",
            self.endpoint.trim_end_matches('/'),
            self.account,
            self.user,
            self.agent,
            scope,
            name
        )
    }

    fn list_url(&self, scope: &str) -> String {
        format!(
            "{}/v1/notes/{}/{}/{}/{}?list=1",
            self.endpoint.trim_end_matches('/'),
            self.account,
            self.user,
            self.agent,
            scope
        )
    }

    fn search_url(&self) -> String {
        format!("{}/v1/search", self.endpoint.trim_end_matches('/'))
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, MemoryError> {
        req.header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .map_err(|e| MemoryError::Transport {
                provider: PROVIDER_NAME,
                detail: e.to_string(),
            })
    }

    async fn send_json<T: for<'de> Deserialize<'de>>(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<T, MemoryError> {
        let resp = self
            .send(req.header("Content-Type", "application/json"))
            .await?;
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

#[derive(Serialize)]
struct NoteWrite<'a> {
    content: &'a str,
    category: &'a str,
    mode: &'a str,
}

#[derive(Deserialize)]
struct NoteRead {
    #[serde(default)]
    content: String,
    #[serde(default)]
    updated_at: Option<i64>,
}

#[derive(Deserialize)]
struct ListPayload {
    #[serde(default)]
    notes: Vec<ListedNote>,
}

#[derive(Deserialize)]
struct ListedNote {
    name: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    updated_at: Option<i64>,
}

#[derive(Deserialize)]
struct SearchPayload {
    #[serde(default)]
    hits: Vec<SearchHitWire>,
}

#[derive(Deserialize)]
struct SearchHitWire {
    #[serde(default)]
    scope: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    score: Option<f32>,
}

#[async_trait]
impl MemoryProvider for OpenVikingProvider {
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
        let body = NoteWrite {
            content,
            category: "note",
            mode: match mode {
                WriteMode::Overwrite => "overwrite",
                WriteMode::Append => "append",
            },
        };
        let _: serde_json::Value = self
            .send_json(self.client.put(self.note_url(scope, name)).json(&body))
            .await?;
        Ok(())
    }

    async fn read(
        &self,
        _ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
    ) -> Result<Option<MemoryEntry>, MemoryError> {
        let resp = self
            .send(self.client.get(self.note_url(scope, name)))
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| MemoryError::Transport {
            provider: PROVIDER_NAME,
            detail: e.to_string(),
        })?;
        if !status.is_success() {
            return Err(MemoryError::Backend {
                provider: PROVIDER_NAME,
                detail: format!("{}", status.as_u16()),
            });
        }
        let row: NoteRead = serde_json::from_slice(&bytes).map_err(|e| MemoryError::Backend {
            provider: PROVIDER_NAME,
            detail: format!("response decode failed: {e}"),
        })?;
        Ok(Some(MemoryEntry {
            scope: scope.to_string(),
            name: name.to_string(),
            content: row.content,
            updated_at_epoch: row.updated_at,
        }))
    }

    async fn search(
        &self,
        _ctx: &MemCtx<'_>,
        query: &SearchQuery,
    ) -> Result<Vec<SearchHit>, MemoryError> {
        let body = json!({
            "query": query.query,
            "limit": query.max_results,
            "account": self.account,
            "user": self.user,
            "agent": self.agent,
        });
        let resp: SearchPayload = self
            .send_json(self.client.post(self.search_url()).json(&body))
            .await?;
        Ok(resp
            .hits
            .into_iter()
            .filter_map(|h| {
                let score = h.score.unwrap_or(0.0).clamp(0.0, 1.0);
                if score < query.min_score {
                    return None;
                }
                Some(SearchHit {
                    scope: if h.scope.is_empty() {
                        "global".to_string()
                    } else {
                        h.scope
                    },
                    name: h.name,
                    snippet: h.snippet,
                    score,
                })
            })
            .collect())
    }

    async fn list(&self, _ctx: &MemCtx<'_>, scope: &str) -> Result<Vec<MemoryEntry>, MemoryError> {
        let resp: ListPayload = self
            .send_json(self.client.get(self.list_url(scope)))
            .await?;
        Ok(resp
            .notes
            .into_iter()
            .map(|n| MemoryEntry {
                scope: scope.to_string(),
                name: n.name,
                content: n.content,
                updated_at_epoch: n.updated_at,
            })
            .collect())
    }

    async fn delete(
        &self,
        _ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
    ) -> Result<bool, MemoryError> {
        let resp = self
            .send(self.client.delete(self.note_url(scope, name)))
            .await?;
        match resp.status() {
            StatusCode::OK | StatusCode::NO_CONTENT => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            other => Err(MemoryError::Backend {
                provider: PROVIDER_NAME,
                detail: format!("delete returned {}", other.as_u16()),
            }),
        }
    }

    async fn consolidate(&self, _ctx: &MemCtx<'_>) -> Result<ConsolidationOutcome, MemoryError> {
        Ok(ConsolidationOutcome::Delegated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_url_encodes_scope_and_name() {
        let p = OpenVikingProvider {
            client: super::super::http_client(),
            endpoint: "http://x".into(),
            api_key: "k".into(),
            account: "acct".into(),
            user: "u".into(),
            agent: "borg".into(),
        };
        let u = p.note_url("project:abc", "notes");
        assert_eq!(u, "http://x/v1/notes/acct/u/borg/project:abc/notes");
    }
}
