//! ChatGPT Codex OAuth credential discovery.
//!
//! Looks at `~/.codex/auth.json`. The file is also written by the upstream
//! Codex CLI / VS Code extension and refresh tokens are single-use, so two
//! consumers sharing the file will race on refresh. The refresh path locks
//! the entry to mitigate; the discoverer just reads.
//!
//! File shape:
//! ```json
//! {
//!   "tokens": {
//!     "access_token": "ChatGPT…",
//!     "refresh_token": "ChatGPT…"
//!   },
//!   "last_refresh": "2025-…"
//! }
//! ```

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::llm::provider::Provider;

use super::{CredentialSource, DiscoveredCredential};

pub struct CodexSource;

#[derive(Deserialize, Serialize)]
struct CodexAuthFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens: Option<Tokens>,
    /// Round-trip unknown top-level fields verbatim so write-back doesn't drop
    /// data the upstream CLI writes (e.g. `last_refresh`).
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize, Serialize)]
struct Tokens {
    #[serde(default)]
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    /// Codex doesn't write an explicit expires_at; the refresh path computes
    /// it from token TTL. `None` is correct — the refresh path schedules a
    /// probe-refresh when expires_at is missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at_ms: Option<i64>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

fn codex_auth_path(home: &Path) -> PathBuf {
    home.join(".codex/auth.json")
}

impl CredentialSource for CodexSource {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn provider(&self) -> Provider {
        Provider::CodexOAuth
    }

    fn discover(&self, home: &Path) -> anyhow::Result<Vec<DiscoveredCredential>> {
        let path = codex_auth_path(home);
        if !path.exists() {
            return Ok(vec![]);
        }
        super::warn_if_world_readable(&path);
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let parsed: CodexAuthFile = serde_json::from_str(&raw)
            .with_context(|| format!("parse {} as Codex auth", path.display()))?;
        let Some(tokens) = parsed.tokens else {
            return Ok(vec![]);
        };
        if tokens.access_token.is_empty() {
            return Ok(vec![]);
        }
        let label = format!(
            "codex:{}",
            super::short_label_fingerprint(&tokens.access_token)
        );
        Ok(vec![DiscoveredCredential {
            label,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token.filter(|t| !t.is_empty()),
            expires_at_ms: tokens.expires_at_ms,
            // Codex requests target the ChatGPT backend, not api.openai.com.
            // Storing the override here keeps Provider::CodexOAuth's static
            // base_url as the api.openai.com fallback for any post-paired
            // hand-edited credentials.
            base_url: Some("https://chatgpt.com/backend-api/codex".to_string()),
            extra_json: None,
        }])
    }

    fn write_back(
        &self,
        home: &Path,
        access_token: &str,
        refresh_token: &str,
        expires_at_ms: i64,
    ) -> anyhow::Result<()> {
        let path = codex_auth_path(home);
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read {} for refresh write-back", path.display()))?;
        let mut parsed: CodexAuthFile = serde_json::from_str(&raw)
            .with_context(|| format!("parse {} for refresh write-back", path.display()))?;
        let block = parsed.tokens.get_or_insert(Tokens {
            access_token: String::new(),
            refresh_token: None,
            expires_at_ms: None,
            extra: serde_json::Map::new(),
        });
        block.access_token = access_token.to_string();
        block.refresh_token = Some(refresh_token.to_string());
        block.expires_at_ms = Some(expires_at_ms);
        let serialized =
            serde_json::to_string_pretty(&parsed).context("serialize rotated Codex credentials")?;
        super::write_atomically(&path, &serialized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::credential_sources::test_support::write_fixture;

    #[test]
    fn discovers_codex_tokens_and_overrides_base_url() {
        let home = tempfile::tempdir().unwrap();
        write_fixture(
            home.path(),
            ".codex/auth.json",
            r#"{"tokens": {"access_token": "at-XYZ789", "refresh_token": "rt-abc"}}"#,
        );
        let creds = CodexSource.discover(home.path()).unwrap();
        assert_eq!(creds.len(), 1);
        let c = &creds[0];
        assert_eq!(c.access_token, "at-XYZ789");
        assert_eq!(c.refresh_token.as_deref(), Some("rt-abc"));
        assert_eq!(
            c.base_url.as_deref(),
            Some("https://chatgpt.com/backend-api/codex")
        );
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let home = tempfile::tempdir().unwrap();
        assert!(CodexSource.discover(home.path()).unwrap().is_empty());
    }

    #[test]
    fn malformed_file_returns_error() {
        let home = tempfile::tempdir().unwrap();
        write_fixture(home.path(), ".codex/auth.json", "not json");
        assert!(CodexSource.discover(home.path()).is_err());
    }
}
