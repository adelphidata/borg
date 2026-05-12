//! Qwen-CLI credential discovery.
//!
//! Qwen-CLI writes API-key-style credentials (not refreshable OAuth) at
//! `~/.qwen/oauth_creds.json`. Despite the filename, the payload is an API
//! key + base_url combo — surfaced as an `AuthType::ApiKey` credential with
//! a `base_url` override, not as OAuth.
//!
//! File shape:
//! ```json
//! {"api_key": "qwen-…", "expires_at_ms": …, "base_url": "https://…", "source": "qwen-cli"}
//! ```

use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

use crate::llm::provider::Provider;

use super::{CredentialSource, DiscoveredCredential};

pub struct QwenCliSource;

#[derive(Deserialize)]
struct QwenFile {
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    expires_at_ms: Option<i64>,
    #[serde(default)]
    base_url: Option<String>,
    /// Optional operator-supplied label baked into the file (some Qwen-CLI
    /// versions write this; older ones don't).
    #[serde(default)]
    auth_file: Option<String>,
}

impl CredentialSource for QwenCliSource {
    fn name(&self) -> &'static str {
        "qwen_cli"
    }

    fn provider(&self) -> Provider {
        // Qwen runs through `Provider::Custom` because its endpoint is
        // operator-configurable (could be Alibaba Cloud, could be a self-host).
        // The base_url from the file drives the request URL.
        Provider::Custom
    }

    fn discover(&self, home: &Path) -> anyhow::Result<Vec<DiscoveredCredential>> {
        let path = home.join(".qwen/oauth_creds.json");
        if !path.exists() {
            return Ok(vec![]);
        }
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let parsed: QwenFile = serde_json::from_str(&raw)
            .with_context(|| format!("parse {} as Qwen-CLI credentials", path.display()))?;
        if parsed.api_key.is_empty() {
            return Ok(vec![]);
        }
        let label = parsed
            .auth_file
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "qwen-cli".to_string());
        Ok(vec![DiscoveredCredential {
            label,
            access_token: parsed.api_key,
            refresh_token: None,
            expires_at_ms: parsed.expires_at_ms,
            base_url: parsed.base_url.filter(|u| !u.is_empty()),
            extra_json: None,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::credential_sources::test_support::write_fixture;

    #[test]
    fn discovers_qwen_api_key_with_base_url() {
        let home = tempfile::tempdir().unwrap();
        write_fixture(
            home.path(),
            ".qwen/oauth_creds.json",
            r#"{
                "api_key": "qwen-key-xyz",
                "base_url": "https://dashscope.aliyuncs.com/api/v1",
                "auth_file": "qwen-alice"
            }"#,
        );
        let creds = QwenCliSource.discover(home.path()).unwrap();
        assert_eq!(creds.len(), 1);
        let c = &creds[0];
        assert_eq!(c.access_token, "qwen-key-xyz");
        assert_eq!(c.refresh_token, None, "Qwen API keys don't refresh");
        assert_eq!(
            c.base_url.as_deref(),
            Some("https://dashscope.aliyuncs.com/api/v1")
        );
        assert_eq!(c.label, "qwen-alice");
    }

    #[test]
    fn missing_optional_label_defaults_to_qwen_cli() {
        let home = tempfile::tempdir().unwrap();
        write_fixture(
            home.path(),
            ".qwen/oauth_creds.json",
            r#"{"api_key": "k", "base_url": "https://example.com"}"#,
        );
        let creds = QwenCliSource.discover(home.path()).unwrap();
        assert_eq!(creds[0].label, "qwen-cli");
    }

    #[test]
    fn empty_api_key_yields_no_credential() {
        let home = tempfile::tempdir().unwrap();
        write_fixture(
            home.path(),
            ".qwen/oauth_creds.json",
            r#"{"api_key": "", "base_url": "x"}"#,
        );
        assert!(QwenCliSource.discover(home.path()).unwrap().is_empty());
    }
}
