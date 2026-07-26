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

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::llm::provider::Provider;

use super::{CredentialSource, DiscoveredCredential};

pub struct ClaudeCodeSource;

#[derive(Deserialize, Serialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OAuthBlock>,
    /// Anything we don't recognize — round-tripped verbatim so write-back
    /// doesn't truncate operator-added fields the upstream CLI might use.
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize, Serialize)]
struct OAuthBlock {
    #[serde(rename = "accessToken", default)]
    access_token: String,
    #[serde(
        rename = "refreshToken",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    refresh_token: Option<String>,
    #[serde(rename = "expiresAt", default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

fn claude_creds_path(home: &Path) -> PathBuf {
    home.join(".claude/.credentials.json")
}

impl CredentialSource for ClaudeCodeSource {
    fn name(&self) -> &'static str {
        "claude_code"
    }

    fn provider(&self) -> Provider {
        Provider::ClaudeCodeOAuth
    }

    fn discover(&self, home: &Path) -> anyhow::Result<Vec<DiscoveredCredential>> {
        let path = claude_creds_path(home);
        if !path.exists() {
            return Ok(vec![]);
        }
        super::warn_if_world_readable(&path);
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let parsed: CredentialsFile = serde_json::from_str(&raw)
            .with_context(|| format!("parse {} as Claude Code credentials", path.display()))?;
        let Some(oauth) = parsed.oauth else {
            return Ok(vec![]);
        };
        if oauth.access_token.is_empty() {
            return Ok(vec![]);
        }
        // Label format: `claude-code:<6-hex>` where `<6-hex>` is a fingerprint
        // prefix derived from the access token. Stable across rotations of
        // the access_token (fingerprint is BLAKE3-truncated) and unique enough
        // that two paired accounts won't collide in `borg auth list`.
        let label = format!(
            "claude-code:{}",
            super::short_label_fingerprint(&oauth.access_token)
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

    /// Write rotated tokens back to `~/.claude/.credentials.json`.
    ///
    /// Cross-process race: Claude Code CLI may also be running and burning
    /// refresh tokens. We can't fully prevent that without a file lock both
    /// sides honor; writing back at least keeps `borgd`'s next reload from
    /// using a refresh_token the previous session already consumed. Atomic
    /// rename ensures a parallel reader never sees a half-written file.
    fn write_back(
        &self,
        home: &Path,
        access_token: &str,
        refresh_token: &str,
        expires_at_ms: i64,
    ) -> anyhow::Result<()> {
        let path = claude_creds_path(home);
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read {} for refresh write-back", path.display()))?;
        let mut parsed: CredentialsFile = serde_json::from_str(&raw)
            .with_context(|| format!("parse {} for refresh write-back", path.display()))?;
        let block = parsed.oauth.get_or_insert(OAuthBlock {
            access_token: String::new(),
            refresh_token: None,
            expires_at: None,
            extra: serde_json::Map::new(),
        });
        block.access_token = access_token.to_string();
        block.refresh_token = Some(refresh_token.to_string());
        block.expires_at = Some(expires_at_ms);
        let serialized = serde_json::to_string_pretty(&parsed)
            .context("serialize rotated Claude Code credentials")?;
        super::write_atomically(&path, &serialized)
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
        // Label is `claude-code:<6-hex>` where the hex is a fingerprint prefix
        // of the access token. Fingerprint is stable across access-token
        // rotations and collision-unique enough to disambiguate two paired
        // accounts in `borg auth list`.
        assert!(
            c.label.starts_with("claude-code:"),
            "label prefix: {}",
            c.label
        );
        let suffix = c.label.trim_start_matches("claude-code:");
        assert_eq!(suffix.len(), 6, "fingerprint prefix should be 6 hex chars");
        assert!(
            suffix.chars().all(|ch| ch.is_ascii_hexdigit()),
            "fingerprint prefix must be hex: {suffix}"
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
        // silently swallowing it — a success return here would be a lie.
        let home = tempfile::tempdir().unwrap();
        write_fixture(home.path(), ".claude/.credentials.json", "not json");
        assert!(ClaudeCodeSource.discover(home.path()).is_err());
    }
}
