//! Pluggable memory provider abstraction.
//!
//! Borg's default `MemoryConfig::provider = Native` keeps the existing
//! SQLite-backed two-tier memory (working + long-term `memory_entries`) as
//! the source of truth. Operators who want fact extraction, dialectic
//! retrieval, or a hierarchical context store can flip `memory.provider`
//! to one of the bundled external providers ([`mem0`], [`honcho`],
//! [`openviking`], [`letta`], [`zep`]) — the three model-visible tools
//! (`write_memory`, `read_memory`, `memory_search`) and the per-turn
//! `<active_memory>` injection then route through the chosen provider.
//!
//! Scope caveat: the stable-prefix `<long_term_memory>` block in the
//! system prompt still loads from the local SQLite store regardless of
//! provider. External-provider content reaches the model through
//! `<active_memory>` (per-turn search) and on-demand via `read_memory` /
//! `memory_search`. Routing the prefix block through the trait is
//! tracked under "Memory provider — full prompt-block routing" in
//! `docs/ROADMAP.md`.
//!
//! Invariants common to every backend (prompt-injection scan + 20k-token
//! cap) live in [`write_guarded`] so each impl gets them for free. The
//! trait surface stays minimal; the elegance comes from keeping shared
//! safety once at the dispatch site.
//!
//! See `docs/memory.md` for the operator-facing surface.

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;

use crate::config::Config;
use crate::constants::{MEMORY_ENTRY_REJECT_TOKENS, MEMORY_ENTRY_WARN_TOKENS};
use crate::memory::injection::scan_for_injection;
use crate::memory::tokenizer::estimate_tokens;
use crate::memory::WriteMode;

/// Cap on how much of a failed backend response body lands in the error
/// string: enough to identify the fault, short enough that a hostile or
/// verbose provider cannot flood the transcript with attacker-chosen text.
const ERROR_BODY_PREVIEW_CHARS: usize = 400;

pub(crate) mod endpoint_guard;
pub mod honcho;
pub mod letta;
pub mod mem0;
pub mod native;
pub mod openviking;
pub mod registry;
pub mod zep;

#[cfg(test)]
mod tests;

pub use native::NativeProvider;
pub use registry::resolve;

/// A single memory entry as returned by a provider. The shape is intentionally
/// minimal — every backend can populate `scope`, `name`, and `content`;
/// `updated_at_epoch` is optional because not every cloud store exposes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    /// Logical scope (`"global"`, `"project:abc"`, …).
    pub scope: String,
    /// Hierarchical key inside the scope.
    pub name: String,
    /// Stored markdown content.
    pub content: String,
    /// Last-update unix epoch seconds, when the backend supplies it.
    pub updated_at_epoch: Option<i64>,
}

/// A single search hit. Score is normalized to `[0.0, 1.0]` so the dispatch
/// layer can merge hits across stores without per-provider rescaling.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// Scope of the hit (`"global"`, `"project:abc"`, …).
    pub scope: String,
    /// Name of the matched entry.
    pub name: String,
    /// Snippet to render to the LLM.
    pub snippet: String,
    /// Normalized score in `[0.0, 1.0]`.
    pub score: f32,
}

/// Parameters for a search call. Borg's existing `memory_search` tool
/// supplies `max_results` and `min_score`; both are passed through so the
/// provider can choose whether to honor them server-side.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    /// Free-form query text.
    pub query: String,
    /// Caller-requested cap on returned hits.
    pub max_results: usize,
    /// Minimum normalized score; provider implementations may ignore.
    pub min_score: f32,
}

/// Errors a provider may surface. Kept narrow so the dispatch layer can map
/// to user-facing messages without leaking transport details.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    /// The chosen provider is missing required credentials. Surfaces as a
    /// hard fail in `write_memory` so the operator sees the misconfiguration
    /// immediately; the [`resolve`] factory pre-empts this for the prompt
    /// path by falling back to Native.
    #[error("memory provider '{provider}' is not configured: {detail}")]
    NotConfigured {
        /// Provider name (e.g. `"mem0"`).
        provider: &'static str,
        /// Operator-actionable hint.
        detail: String,
    },
    /// Backend rejected the request — auth, quota, or schema. Detail is
    /// already secret-redacted by the calling impl.
    #[error("memory provider '{provider}' rejected request: {detail}")]
    Backend {
        /// Provider name.
        provider: &'static str,
        /// Already-redacted detail string.
        detail: String,
    },
    /// Transport failure (timeout, DNS, TLS, etc.).
    #[error("memory provider '{provider}' transport failure: {detail}")]
    Transport {
        /// Provider name.
        provider: &'static str,
        /// Already-redacted detail string.
        detail: String,
    },
    /// Invariant violation surfaced from [`write_guarded`].
    #[error("{0}")]
    Invariant(String),
}

/// Outcome of a consolidation pass. External providers run their own
/// consolidation server-side, so the trait surface is "did we do work here
/// or delegate?" — useful to the daemon scheduler so it can skip the
/// expensive seeded-agent path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsolidationOutcome {
    /// The provider performed local consolidation; the seeded agent loop
    /// ran. Used by [`NativeProvider`].
    Local,
    /// The provider delegates consolidation to a remote service.
    Delegated,
}

/// Storage-agnostic context passed on every call. Carries the running
/// `Config` so impls can reach `memory.embeddings.*` knobs (Native) or
/// enumerate `resolved_memory_scopes` (Honcho).
#[derive(Debug)]
pub struct MemCtx<'a> {
    /// Active config (read-only).
    pub config: &'a Config,
}

impl<'a> MemCtx<'a> {
    pub fn from_config(config: &'a Config) -> Self {
        Self { config }
    }
}

/// The trait every memory backend implements. Async-trait keeps the
/// surface clean; impls hold their own clients in `Arc<…>` and own
/// concurrency themselves.
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    /// Stable name surfaced in errors and `borg doctor`.
    fn name(&self) -> &'static str;

    /// Write or append a single memory entry. Callers should use
    /// [`write_guarded`] rather than calling this directly so the
    /// injection-scan + size cap fire uniformly.
    async fn write(
        &self,
        ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
        content: &str,
        mode: WriteMode,
    ) -> Result<(), MemoryError>;

    /// Fetch a single entry by `scope` + `name`. Returns `Ok(None)` on miss.
    async fn read(
        &self,
        ctx: &MemCtx<'_>,
        scope: &str,
        name: &str,
    ) -> Result<Option<MemoryEntry>, MemoryError>;

    /// Hybrid search across the active scopes for the chosen backend.
    async fn search(
        &self,
        ctx: &MemCtx<'_>,
        query: &SearchQuery,
    ) -> Result<Vec<SearchHit>, MemoryError>;

    /// List all entries in `scope`. Used by `/memory` inspection surfaces.
    async fn list(&self, ctx: &MemCtx<'_>, scope: &str) -> Result<Vec<MemoryEntry>, MemoryError>;

    /// Delete an entry. `Ok(false)` for a no-op miss; `Ok(true)` on success.
    async fn delete(&self, ctx: &MemCtx<'_>, scope: &str, name: &str) -> Result<bool, MemoryError>;

    /// Run the long-term-memory consolidation pass for this backend.
    /// External providers typically return [`ConsolidationOutcome::Delegated`]
    /// because the service consolidates server-side.
    async fn consolidate(&self, _ctx: &MemCtx<'_>) -> Result<ConsolidationOutcome, MemoryError> {
        Ok(ConsolidationOutcome::Delegated)
    }

    /// Cheap reachability + credential probe surfaced by `borg doctor`.
    /// Native is always healthy; external providers override with a
    /// lightweight authenticated request so a wrong key or unreachable
    /// endpoint shows up *before* a real `write_memory` silently fails.
    /// Errors are already secret-redacted by the impl's `send_json`.
    async fn health(&self, _ctx: &MemCtx<'_>) -> Result<(), MemoryError> {
        Ok(())
    }
}

/// Common-path guard wrapper for [`MemoryProvider::write`]. Every backend
/// gets the same prompt-injection scan and 20k-token cap — defined once
/// here so a new provider can't silently skip either invariant.
///
/// Mirrors the legacy `write_memory_db` behavior (`memory::write_memory_db`)
/// so swapping providers doesn't change failure messages.
pub async fn write_guarded(
    provider: &dyn MemoryProvider,
    ctx: &MemCtx<'_>,
    scope: &str,
    name: &str,
    content: &str,
    mode: WriteMode,
) -> Result<String, MemoryError> {
    scan_for_injection(content).map_err(|e| MemoryError::Invariant(e.to_string()))?;
    let projected = match mode {
        WriteMode::Overwrite => estimate_tokens(content),
        WriteMode::Append => {
            // Best-effort prefix size: external providers may not expose
            // a pre-write read. We log + treat as 0 rather than `.ok()`
            // silently — a transient HTTP failure here could otherwise
            // admit a payload that blows past the 20k cap on the server.
            // The operator gets a breadcrumb; the cap stays as a backstop.
            let existing = match provider.read(ctx, scope, name).await {
                Ok(Some(entry)) => estimate_tokens(&entry.content),
                Ok(None) => 0,
                Err(e) => {
                    tracing::warn!(
                        "write_guarded append: pre-read of {scope}/{name} failed: {e}; \
                         token cap will only count new payload"
                    );
                    0
                }
            };
            existing + estimate_tokens(content)
        }
    };
    if projected >= MEMORY_ENTRY_REJECT_TOKENS {
        return Err(MemoryError::Invariant(format!(
            "memory entry '{name}' (scope {scope}) would be ~{projected} tokens, \
             over the {MEMORY_ENTRY_REJECT_TOKENS}-token cap. Split into multiple topic \
             entries — oversized entries get silently dropped from the context window."
        )));
    }
    if projected >= MEMORY_ENTRY_WARN_TOKENS {
        tracing::warn!(
            "memory entry {scope}/{name} is ~{projected} tokens — approaching \
             the {MEMORY_ENTRY_REJECT_TOKENS}-token cap; consider splitting"
        );
    }
    provider.write(ctx, scope, name, content, mode).await?;
    Ok(format!("Written to memory: {name} (scope: {scope})"))
}

/// Convenience: format a vec of [`SearchHit`] using the same shape the
/// legacy `format_search_results` produces. Keeps tool-output stable
/// regardless of provider.
pub fn format_hits(hits: &[SearchHit]) -> String {
    if hits.is_empty() {
        return "No matching memories found.".to_string();
    }
    let mut output = String::new();
    for (i, h) in hits.iter().enumerate() {
        let label = if h.scope == "global" || h.scope.is_empty() {
            h.name.clone()
        } else {
            format!("{}/{}", h.scope, h.name)
        };
        let snippet: String = h.snippet.chars().take(500).collect();
        output.push_str(&format!(
            "[{}] {} (score: {:.2})\n> {}\n\n",
            i + 1,
            label,
            h.score,
            snippet
        ));
    }
    output.trim_end().to_string()
}

/// Shared `reqwest::Client` builder for the three external providers.
/// Sets the user-agent + a 20s default timeout. Centralised so a future
/// telemetry/cookie/policy change only needs editing once. Falls back to
/// `Client::new()` on the (effectively impossible) builder failure path —
/// `reqwest::Client::new()` itself uses the same defaults, so the worst
/// case is "no custom user-agent" rather than a panic.
///
/// Redirects are DISABLED: the [`endpoint_guard`] request gate validates
/// only the initial URL, so a followed redirect would let a malicious
/// endpoint bounce the request (with its Authorization header) to a
/// metadata service or downgrade https→http. These JSON APIs never need
/// redirects; a 3xx surfaces as a non-success status from `send_json`/
/// `send_ok`. The fallback arm builds with the same policy — never
/// `Client::new()`, which would silently re-enable following.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("borg/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|e| {
            tracing::warn!(
                "memory provider http client builder failed ({e}); using minimal fallback"
            );
            // Fail closed rather than fall back to `Client::new()`: a default
            // client follows redirects, silently dropping the security policy
            // above. A bare builder with only the redirect policy cannot fail
            // in practice (no TLS/proxy/resolver options involved).
            #[allow(clippy::expect_used)]
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("bare reqwest client with redirect policy must build")
        })
}

/// Build and send a request after passing the [`endpoint_guard`] request
/// gate. Every external-provider request MUST go through here (directly or
/// via [`send_json`]/[`send_ok`]) — it is the chokepoint that keeps a
/// credential from being sent to a metadata endpoint or a cleartext public
/// host, no matter what the configured base_url resolves to at request time.
pub(crate) async fn guarded_send(
    req: reqwest::RequestBuilder,
    provider: &'static str,
) -> Result<reqwest::Response, MemoryError> {
    let (client, request) = req.build_split();
    let request = request.map_err(|e| MemoryError::Transport {
        provider,
        detail: format!("request build failed: {e}"),
    })?;
    endpoint_guard::check_request_url(request.url(), provider).await?;
    client
        .execute(request)
        .await
        .map_err(|e| MemoryError::Transport {
            provider,
            detail: e.to_string(),
        })
}

/// Send an authenticated JSON request and decode the response. Shared by
/// every external provider — collapses the three near-identical send_json
/// impls and centralises the two-pass secret redaction (literal api_key
/// strip → `redact_secrets`). `auth_header` is the full `Authorization`
/// value (e.g. `"Token abc"` or `"Bearer abc"`); `api_key` is passed
/// separately so it can be stripped from error bodies before
/// the workspace redactor runs.
pub(crate) async fn send_json<T: for<'de> Deserialize<'de>>(
    req: reqwest::RequestBuilder,
    provider: &'static str,
    auth_header: &str,
    api_key: &str,
) -> Result<T, MemoryError> {
    let resp = guarded_send(
        req.header("Authorization", auth_header)
            .header("Content-Type", "application/json"),
        provider,
    )
    .await?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| MemoryError::Transport {
        provider,
        detail: e.to_string(),
    })?;
    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes);
        let stripped = body.replace(api_key, "<redacted>");
        let redacted = crate::security::secrets::redact_secrets(&stripped);
        return Err(MemoryError::Backend {
            provider,
            detail: format!(
                "{}: {}",
                status.as_u16(),
                redacted
                    .chars()
                    .take(ERROR_BODY_PREVIEW_CHARS)
                    .collect::<String>()
            ),
        });
    }
    serde_json::from_slice(&bytes).map_err(|e| MemoryError::Backend {
        provider,
        detail: format!("response decode failed: {e}"),
    })
}

/// Like [`send_json`] but discards a successful response body. For
/// endpoints (DELETE, PATCH, some inserts) that legitimately return an
/// empty `204`/`200` body, where [`send_json`]'s mandatory JSON decode
/// would otherwise fail. Error bodies get the same two-pass redaction.
pub(crate) async fn send_ok(
    req: reqwest::RequestBuilder,
    provider: &'static str,
    auth_header: &str,
    api_key: &str,
) -> Result<(), MemoryError> {
    let resp = guarded_send(
        req.header("Authorization", auth_header)
            .header("Content-Type", "application/json"),
        provider,
    )
    .await?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| MemoryError::Transport {
        provider,
        detail: e.to_string(),
    })?;
    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes);
        let stripped = body.replace(api_key, "<redacted>");
        let redacted = crate::security::secrets::redact_secrets(&stripped);
        return Err(MemoryError::Backend {
            provider,
            detail: format!(
                "{}: {}",
                status.as_u16(),
                redacted
                    .chars()
                    .take(ERROR_BODY_PREVIEW_CHARS)
                    .collect::<String>()
            ),
        });
    }
    Ok(())
}

/// Percent-encode a URL path segment or query-parameter value. RFC 3986
/// unreserved set passes through; everything else is `%HH`-encoded.
/// Shared so each provider's URL builder can defend against `/`, `?`,
/// `#`, and `..` in caller-supplied scope/name strings.
pub(crate) fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Sort search hits by descending score (NaN compares equal) and truncate
/// to `max`. Shared by the session/cloud `search` impls so result ordering
/// stays byte-identical across providers — the merge order is part of the
/// model-visible output.
pub(crate) fn rank_and_truncate(hits: &mut Vec<SearchHit>, max: usize) {
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(max);
}

/// Build a stable, injective session id for a `scope`, used by the
/// session-per-scope providers ([`honcho`], [`zep`]). A single session
/// per scope keeps entry lookup by `borg_name` metadata bounded.
///
/// The encoding must be injective: `project:abc` and `project-abc` were
/// previously colliding under a naive `replace(':', "-")` because both
/// mapped to the same id, so a write to one scope landed in the other's
/// session. We escape `_` first (so the escape marker itself can't be
/// confused with literal input) and then map `:` to `_c`. Hyphens pass
/// through untouched — they're already legal in session ids and the most
/// common scope character. `prefix` distinguishes providers/namespaces
/// (e.g. `"borg-"`).
pub(crate) fn scoped_session_id(prefix: &str, scope: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + scope.len() + 4);
    out.push_str(prefix);
    for c in scope.chars() {
        match c {
            '_' => out.push_str("__"),
            ':' => out.push_str("_c"),
            other => out.push(other),
        }
    }
    out
}

/// Build an `Arc<dyn MemoryProvider>` from config. Wraps [`resolve`] so
/// callers don't have to import the registry module.
pub fn from_config(config: &Config) -> Arc<dyn MemoryProvider> {
    resolve(config)
}

#[cfg(test)]
mod helper_tests {
    use super::scoped_session_id;

    #[test]
    fn scoped_session_id_is_injective_across_separators() {
        // Colon-, hyphen-, and underscore-bearing scopes must all map to
        // distinct ids — a collision routes one scope's writes into
        // another's session.
        assert_eq!(scoped_session_id("borg-", "global"), "borg-global");
        assert_eq!(
            scoped_session_id("borg-", "project:abc"),
            "borg-project_cabc"
        );
        assert_eq!(
            scoped_session_id("borg-", "project-abc"),
            "borg-project-abc"
        );
        assert_eq!(scoped_session_id("borg-", "foo_bar"), "borg-foo__bar");
        assert_ne!(
            scoped_session_id("borg-", "project:abc"),
            scoped_session_id("borg-", "project-abc"),
        );
        assert_ne!(
            scoped_session_id("borg-", "foo_cbar"),
            scoped_session_id("borg-", "foo:bar"),
        );
        // Prefix participates in the namespace.
        assert_ne!(
            scoped_session_id("borg-", "x"),
            scoped_session_id("zep-", "x"),
        );
    }
}
