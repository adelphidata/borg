//! Claude Code OAuth credential discovery.
//!
//! Looks at `~/.claude/.credentials.json`. The macOS Keychain entry
//! `"Claude Code-credentials"` (Claude Code >= 2.1.114) is also a valid
//! source but lives behind a `security`-CLI shell-out and is deferred until
//! someone actually needs it.
//!
//! File shape:
//! ```json
//! {
//!   "claudeAiOauth": {
//!     "accessToken": "sk-ant-oat01-…",
//!     "refreshToken": "sk-ant-ort01-…",
//!     "expiresAt": 1730000000000
//!   }
//! }
//! ```

use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

use crate::provider::Provider;

use super::{CredentialSource, DiscoveredCredential};

pub struct ClaudeCodeSource;

#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OAuthBlock>,
}

#[derive(Deserialize)]
struct OAuthBlock {
    #[serde(rename = "accessToken", default)]
    access_token: String,
    #[serde(rename = "refreshToken", default)]
    refresh_token: Option<String>,
    #[serde(rename = "expiresAt", default)]
    expires_at: Option<i64>,
}

impl CredentialSource for ClaudeCodeSource {
    fn name(&self) -> &'static str {
        "claude_code"
    }

    fn provider(&self) -> Provider {
        Provider::ClaudeCodeOAuth
    }

    fn discover(&self, home: &Path) -> anyhow::Result<Vec<DiscoveredCredential>> {
        let path = home.join(".claude/.credentials.json");
        if !path.exists() {
            return Ok(vec![]);
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let parsed: CredentialsFile = serde_json::from_str(&raw)
            .with_context(|| format!("parse {} as Claude Code credentials", path.display()))?;
        let Some(oauth) = parsed.oauth else {
            return Ok(vec![]);
        };
        if oauth.access_token.is_empty() {
            return Ok(vec![]);
        }
        // Label: short fingerprint suffix of the access token. A richer
        // label would derive the email from the JWT payload, but that
        // requires a base64+JWT parser; last 6 chars is enough for
        // operators to disambiguate in `borg auth list` for now.
        let label = format!(
            "claude-code:{}",
            oauth
                .access_token
                .chars()
                .rev()
                .take(6)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        );
        Ok(vec![DiscoveredCredential {
            label,
            access_token: oauth.access_token,
            refresh_token: oauth.refresh_token.filter(|t| !t.is_empty()),
            expires_at_ms: oauth.expires_at.filter(|&t| t > 0),
            base_url: None,
            extra_json: None,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::credential_sources::test_support::write_fixture;

    #[test]
    fn discovers_well_formed_claude_code_credentials() {
        let home = tempfile::tempdir().unwrap();
        write_fixture(
            home.path(),
            ".claude/.credentials.json",
            r#"{
                "claudeAiOauth": {
                    "accessToken": "sk-ant-oat01-tokenABC123",
                    "refreshToken": "sk-ant-ort01-refresh",
                    "expiresAt": 1730000000000
                }
            }"#,
        );
        let creds = ClaudeCodeSource.discover(home.path()).unwrap();
        assert_eq!(creds.len(), 1);
        let c = &creds[0];
        assert_eq!(c.access_token, "sk-ant-oat01-tokenABC123");
        assert_eq!(c.refresh_token.as_deref(), Some("sk-ant-ort01-refresh"));
        assert_eq!(c.expires_at_ms, Some(1730000000000));
        // Label terminates with the last 6 chars of the access_token — gives
        // operators a way to disambiguate two paired Claude Code accounts in
        // `borg auth list` without us learning to parse JWTs.
        assert!(
            c.label.ends_with("C123"),
            "label should surface a token-suffix hint: {}",
            c.label
        );
    }

    #[test]
    fn returns_empty_when_file_missing() {
        let home = tempfile::tempdir().unwrap();
        assert!(ClaudeCodeSource.discover(home.path()).unwrap().is_empty());
    }

    #[test]
    fn returns_empty_when_oauth_block_absent() {
        // The file exists but has no `claudeAiOauth` key. Don't error —
        // returning Ok(vec![]) lets onboarding's "detected accounts" step
        // just say "none found" instead of bailing out.
        let home = tempfile::tempdir().unwrap();
        write_fixture(
            home.path(),
            ".claude/.credentials.json",
            r#"{"otherStuff": {"foo": "bar"}}"#,
        );
        assert!(ClaudeCodeSource.discover(home.path()).unwrap().is_empty());
    }

    #[test]
    fn empty_access_token_yields_no_credential() {
        // Half-populated files (refresh token written, access token wiped)
        // happen in the wild — Claude Code sometimes blanks the access while
        // the refresh is mid-flight. We don't want to seed a zero-length key.
        let home = tempfile::tempdir().unwrap();
        write_fixture(
            home.path(),
            ".claude/.credentials.json",
            r#"{"claudeAiOauth": {"accessToken": "", "refreshToken": "rt"}}"#,
        );
        assert!(ClaudeCodeSource.discover(home.path()).unwrap().is_empty());
    }

    #[test]
    fn malformed_json_surfaces_as_error() {
        // The pool catches the Err and logs+continues, so the agent still
        // boots. But the source itself must report the error rather than
        // silently swallowing it (CLAUDE.md: "If we report success to the
        // user, it MUST actually succeed").
        let home = tempfile::tempdir().unwrap();
        write_fixture(home.path(), ".claude/.credentials.json", "not json");
        assert!(ClaudeCodeSource.discover(home.path()).is_err());
    }
}
