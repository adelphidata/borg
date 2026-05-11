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

    /// Write rotated OAuth tokens back to the source's on-disk file. The
    /// refresh flow calls this after a successful refresh so the next reload
    /// (or a parallel CLI consumer) sees the new refresh_token. Default impl
    /// is a no-op for sources that don't refresh (`qwen_cli`).
    ///
    /// Best-effort: a write failure logs `warn!` but does not surface as a
    /// refresh error — the in-memory rotation already succeeded.
    fn write_back(
        &self,
        _home: &Path,
        _access_token: &str,
        _refresh_token: &str,
        _expires_at_ms: i64,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// All discoverers, indexed by their stable `name()`. Used by both the pool's
/// load path (`discoverers_for_provider`) and the `pair_credential` RPC's
/// kind-keyed dispatch — the latter can't rely on provider lookup because
/// Qwen-CLI feeds `Provider::Custom`, which would also match any other future
/// custom-endpoint source.
pub fn all_discoverers() -> &'static [&'static dyn CredentialSource] {
    &[
        &claude_code::ClaudeCodeSource,
        &codex::CodexSource,
        &qwen_cli::QwenCliSource,
    ]
}

/// Look up a discoverer by its stable `name()` (e.g. `"claude_code"`,
/// `"codex"`, `"qwen_cli"`). Accepts the underscore form only — callers
/// (CLI / RPC) normalize hyphens to underscores before invoking.
pub fn discoverer_by_name(name: &str) -> Option<&'static dyn CredentialSource> {
    all_discoverers().iter().find(|d| d.name() == name).copied()
}

/// Return the set of discoverers that auto-seed for a given provider during
/// pool load. Returning `&[]` for non-OAuth providers keeps the load path
/// a single iteration loop.
///
/// Qwen-CLI is intentionally *not* auto-seeded under `Provider::Custom` —
/// `Custom` is the catch-all for any operator-defined OpenAI-compatible
/// endpoint, and auto-loading Qwen would inject credentials into pools that
/// have no semantic relationship to Qwen-CLI. Operators pair Qwen-CLI
/// explicitly via `borg auth pair qwen`.
pub fn discoverers_for(provider: Provider) -> &'static [&'static dyn CredentialSource] {
    match provider {
        Provider::ClaudeCodeOAuth => &[&claude_code::ClaudeCodeSource],
        Provider::CodexOAuth => &[&codex::CodexSource],
        _ => &[],
    }
}

/// 6-hex prefix of the access-token fingerprint, for display in
/// `borg auth list`. Stable across access-token rotations (the fingerprint
/// is a one-way BLAKE3 truncation of the original token), and collision-
/// unique enough that two paired accounts won't share a label.
pub(crate) fn short_label_fingerprint(token: &str) -> String {
    let fp = crate::llm::credential_pool::fingerprint(token);
    fp.chars().take(6).collect()
}

/// Best-effort atomic write for credential-source write-back. Public to the
/// crate so `codex.rs` and future sources can share it.
pub(crate) fn write_atomically(path: &Path, contents: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("credentials")
    ));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .with_context(|| format!("open {} for write", tmp.display()))?;
        f.write_all(contents.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        if let Err(e) = f.sync_all() {
            tracing::warn!(
                target: "cortex::llm::credential_sources",
                path = %tmp.display(),
                error = %e,
                "fsync of rotated credentials failed — write may not be durable"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // 0o600: refresh tokens are bearer credentials. A chmod failure
            // is rare but not silent — without this log a parallel-CLI race
            // can leave the file group/world-readable until the next
            // `warn_if_world_readable` pass.
            if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
                tracing::warn!(
                    target: "cortex::llm::credential_sources",
                    path = %tmp.display(),
                    error = %e,
                    "chmod 600 on rotated credentials failed — refresh token may be world-readable"
                );
            }
        }
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Warn (don't fail) when an OAuth credential file is group/world-readable.
/// Operators on shared boxes deserve to know refresh tokens are exposed, but
/// borg shouldn't refuse to start over a permissions nit. Unix-only.
pub(crate) fn warn_if_world_readable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                tracing::warn!(
                    target: "cortex::llm::credential_sources",
                    path = %path.display(),
                    mode = format!("{mode:o}"),
                    "OAuth credentials file is group/world-readable — \
                     consider `chmod 600` to protect the refresh token"
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
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
