//! OAuth credential discovery sources.
//!
//! Each module here looks at a well-known on-disk location for an existing
//! OAuth account (Claude Code, ChatGPT Codex, Qwen-CLI, GitHub Copilot) and
//! returns a [`DiscoveredCredential`] the pool can seed. This is the
//! file-discovery path — interactive OAuth flows (PKCE, device-code) are
//! deliberately out of scope for now; they live behind `borg auth pair`.

use std::path::Path;

use crate::provider::Provider;

pub mod claude_code;
pub mod codex;
pub mod copilot;
pub mod qwen_cli;

/// One credential discovered on disk. The shape mirrors what callers need to
/// construct a `PooledCredential` row directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCredential {
    /// Display label for `borg auth list` (typically the account email or a
    /// distinctive token suffix when the source doesn't expose an email).
    pub label: String,
    /// Current OAuth access token. Stored in `PooledCredential.key`.
    pub access_token: String,
    /// Rotated on each refresh. None when the source is unrefreshable (a stale
    /// file with no refresh token — caller may still seed but will lose the
    /// credential once `expires_at_ms` passes).
    pub refresh_token: Option<String>,
    /// Wall-clock millis at which the access token expires.
    pub expires_at_ms: Option<i64>,
    /// Per-credential `base_url` override (Codex uses ChatGPT's backend URL,
    /// Qwen has a per-account custom endpoint).
    pub base_url: Option<String>,
    /// Opaque per-source metadata (PKCE verifier, OAuth client_id, etc.).
    /// Used by the OAuth-refresh flow.
    pub extra_json: Option<String>,
}

/// A discoverable on-disk credential store. Each impl is `Send + Sync` so the
/// pool can hold them behind `Arc<dyn ...>` without contortions.
pub trait CredentialSource: Send + Sync {
    /// Stable source-name string. Matches `CredentialSource::OAuth { name }`
    /// in the pool — keeps suppression rows portable across restarts.
    fn name(&self) -> &'static str;

    /// Provider these credentials authenticate against.
    fn provider(&self) -> Provider;

    /// Look at `$HOME` (or a test-injected dir) and return whatever
    /// credentials this source finds. Returns `Ok(vec![])` for "looked, found
    /// nothing" — `Err` is reserved for "looked, parser blew up", which the
    /// caller logs and continues past.
    fn discover(&self, home: &Path) -> anyhow::Result<Vec<DiscoveredCredential>>;
}

/// Return the default set of discoverers for a given OAuth-capable provider.
/// `Provider::ClaudeCodeOAuth` runs the Claude Code source; `Provider::Custom`
/// has no discoverer (operators pair custom endpoints via `borg auth add
/// --base-url`). Returning `&[]` for non-OAuth providers keeps the load path
/// a single iteration loop.
pub fn discoverers_for(provider: Provider) -> &'static [&'static dyn CredentialSource] {
    match provider {
        Provider::ClaudeCodeOAuth => &[&claude_code::ClaudeCodeSource],
        Provider::CodexOAuth => &[&codex::CodexSource],
        Provider::CopilotOAuth => &[&copilot::CopilotSource],
        Provider::Custom => &[],
        // Qwen runs through `Provider::Custom` (custom OpenAI-compatible
        // base_url) rather than getting its own enum variant — the
        // discoverer exists for explicit invocation via the CLI but isn't
        // auto-wired to a provider variant.
        _ => &[],
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::fs;
    use std::path::Path;

    /// Write `contents` to `home/<rel_path>`, creating parents. Test helper.
    pub fn write_fixture(home: &Path, rel_path: &str, contents: &str) {
        let path = home.join(rel_path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
    }
}
