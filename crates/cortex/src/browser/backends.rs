//! Remote browser backends.
//!
//! A remote backend never ships its own browser driver. Each provider exposes
//! a small REST control plane: you `POST` a "create session" request and it
//! hands back a Chrome DevTools Protocol endpoint (`ws(s)://…`). Borg then
//! drives that endpoint through the *same* [`super::BrowserSession::connect`]
//! path used for `/browser connect`, so every existing browser action works
//! unchanged against a cloud session.
//!
//! The URL a provider returns is attacker-influenced (a compromised or
//! misconfigured provider could point us at a metadata endpoint), so the CDP
//! endpoint is run through the shared URL-policy classifier before we connect.

use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::config::{BrowserBackend, BrowserConfig, UrlPolicyConfig};
use crate::constants;
use crate::url_policy::{self, ClassifyOutcome};

/// A session created on a remote provider, ready to be driven over CDP.
#[derive(Debug, Clone)]
pub struct RemoteSession {
    /// CDP endpoint (`ws(s)://…` or a discovery `http(s)://…` URL) to connect to.
    pub cdp_url: String,
    /// Provider-side session identifier, used to release the session on close.
    pub session_id: Option<String>,
    /// Which backend minted this session.
    pub provider: BrowserBackend,
}

/// Create a remote browser session for the configured backend.
///
/// Returns `Ok(None)` for the `Local` / `CdpAttach` backends (those are served
/// by the existing launch / `cdp_url` connect paths — there is no remote
/// control plane to call). For a cloud backend it calls the provider's REST
/// API, validates the returned CDP endpoint against `url_policy`, and returns
/// the session to connect to.
pub async fn create_remote_session(
    cfg: &BrowserConfig,
    url_policy: &UrlPolicyConfig,
) -> Result<Option<RemoteSession>> {
    let mut session = match cfg.backend {
        BrowserBackend::Local | BrowserBackend::CdpAttach => return Ok(None),
        BrowserBackend::Browserbase => create_browserbase(cfg).await?,
        BrowserBackend::BrowserUse => create_browser_use(cfg).await?,
        BrowserBackend::Firecrawl => create_firecrawl(cfg).await?,
    };

    // SSRF gate: never connect to a provider-supplied endpoint that resolves to
    // a metadata service or (unless explicitly allowed) a private address. The
    // provider has already created (and is billing) the session at this point,
    // so a rejection must release it — otherwise a keep-alive session leaks.
    match guard_remote_cdp_url(&session.cdp_url, url_policy).await {
        Ok(guarded_url) => {
            // Honor the classifier's `effective_url` (a transform rule may have
            // rewritten the host); connect to that, not the raw provider URL.
            session.cdp_url = guarded_url;
            Ok(Some(session))
        }
        Err(e) => {
            close_remote_session(cfg, &session).await;
            Err(e)
        }
    }
}

/// Best-effort release of a remote session. Failures are logged, never
/// propagated — a keep-alive session left dangling times out provider-side,
/// and a failed release must not turn a successful `browser close` into an
/// error the agent sees.
pub async fn close_remote_session(cfg: &BrowserConfig, session: &RemoteSession) {
    let Some(id) = session.session_id.as_deref() else {
        // No id to address — nothing we can release explicitly.
        return;
    };
    let result = match session.provider {
        BrowserBackend::Browserbase => close_browserbase(cfg, id).await,
        BrowserBackend::BrowserUse => close_browser_use(cfg, id).await,
        BrowserBackend::Firecrawl => close_firecrawl(cfg, id).await,
        BrowserBackend::Local | BrowserBackend::CdpAttach => Ok(()),
    };
    if let Err(e) = result {
        tracing::warn!(
            provider = %session.provider,
            session_id = %id,
            error = %e,
            "failed to release remote browser session (it will time out provider-side)"
        );
    }
}

// ── Provider: Browserbase ────────────────────────────────────────────────

async fn create_browserbase(cfg: &BrowserConfig) -> Result<RemoteSession> {
    let key = resolve_key(&cfg.browserbase_api_key, "browser.browserbase_api_key")?;
    let project_id = cfg
        .browserbase_project_id
        .clone()
        .context("browser.browserbase_project_id not set — required for the Browserbase backend")?;
    let base = base_url(
        &cfg.browserbase_base_url,
        constants::BROWSER_BROWSERBASE_BASE_URL,
    );

    let body = serde_json::json!({
        "projectId": project_id,
        "proxies": cfg.browserbase_proxies,
        "keepAlive": true,
    });

    let resp = http_client()?
        .post(format!("{base}/v1/sessions"))
        .timeout(req_timeout(cfg))
        .header("X-BB-API-Key", key)
        .json(&body)
        .send()
        .await
        .context("Browserbase: failed to reach session API")?;
    let json = parse_json(resp, "Browserbase").await?;

    let cdp_url = extract_cdp_url(&json)
        .context("Browserbase: session response contained no CDP connect URL")?;
    Ok(RemoteSession {
        cdp_url,
        session_id: extract_session_id(&json),
        provider: BrowserBackend::Browserbase,
    })
}

async fn close_browserbase(cfg: &BrowserConfig, id: &str) -> Result<()> {
    let key = resolve_key(&cfg.browserbase_api_key, "browser.browserbase_api_key")?;
    let base = base_url(
        &cfg.browserbase_base_url,
        constants::BROWSER_BROWSERBASE_BASE_URL,
    );
    let body = serde_json::json!({
        "projectId": cfg.browserbase_project_id.clone().unwrap_or_default(),
        "status": "REQUEST_RELEASE",
    });
    http_client()?
        .post(format!("{base}/v1/sessions/{id}"))
        .timeout(req_timeout(cfg))
        .header("X-BB-API-Key", key)
        .json(&body)
        .send()
        .await
        .context("Browserbase: release request failed")?
        .error_for_status()
        .context("Browserbase: release returned an error status")?;
    Ok(())
}

// ── Provider: Browser Use ────────────────────────────────────────────────

async fn create_browser_use(cfg: &BrowserConfig) -> Result<RemoteSession> {
    let key = resolve_key(&cfg.browser_use_api_key, "browser.browser_use_api_key")?;
    let base = base_url(
        &cfg.browser_use_base_url,
        constants::BROWSER_BROWSER_USE_BASE_URL,
    );

    let resp = http_client()?
        .post(format!("{base}/api/v3/browsers"))
        .timeout(req_timeout(cfg))
        .bearer_auth(key)
        .json(&serde_json::json!({}))
        .send()
        .await
        .context("Browser Use: failed to reach browser API")?;
    let json = parse_json(resp, "Browser Use").await?;

    let cdp_url =
        extract_cdp_url(&json).context("Browser Use: response contained no CDP connect URL")?;
    Ok(RemoteSession {
        cdp_url,
        session_id: extract_session_id(&json),
        provider: BrowserBackend::BrowserUse,
    })
}

async fn close_browser_use(cfg: &BrowserConfig, id: &str) -> Result<()> {
    let key = resolve_key(&cfg.browser_use_api_key, "browser.browser_use_api_key")?;
    let base = base_url(
        &cfg.browser_use_base_url,
        constants::BROWSER_BROWSER_USE_BASE_URL,
    );
    http_client()?
        .delete(format!("{base}/api/v3/browsers/{id}"))
        .timeout(req_timeout(cfg))
        .bearer_auth(key)
        .send()
        .await
        .context("Browser Use: release request failed")?
        .error_for_status()
        .context("Browser Use: release returned an error status")?;
    Ok(())
}

// ── Provider: Firecrawl ──────────────────────────────────────────────────

async fn create_firecrawl(cfg: &BrowserConfig) -> Result<RemoteSession> {
    let key = resolve_key(&cfg.firecrawl_api_key, "browser.firecrawl_api_key")?;
    let base = base_url(
        &cfg.firecrawl_base_url,
        constants::BROWSER_FIRECRAWL_BASE_URL,
    );

    let resp = http_client()?
        .post(format!("{base}/v2/browser"))
        .timeout(req_timeout(cfg))
        .bearer_auth(key)
        .json(&serde_json::json!({ "ttl": cfg.firecrawl_browser_ttl_secs }))
        .send()
        .await
        .context("Firecrawl: failed to reach browser API")?;
    let json = parse_json(resp, "Firecrawl").await?;

    let cdp_url =
        extract_cdp_url(&json).context("Firecrawl: response contained no CDP connect URL")?;
    Ok(RemoteSession {
        cdp_url,
        session_id: extract_session_id(&json),
        provider: BrowserBackend::Firecrawl,
    })
}

async fn close_firecrawl(cfg: &BrowserConfig, id: &str) -> Result<()> {
    let key = resolve_key(&cfg.firecrawl_api_key, "browser.firecrawl_api_key")?;
    let base = base_url(
        &cfg.firecrawl_base_url,
        constants::BROWSER_FIRECRAWL_BASE_URL,
    );
    http_client()?
        .delete(format!("{base}/v2/browser/{id}"))
        .timeout(req_timeout(cfg))
        .bearer_auth(key)
        .send()
        .await
        .context("Firecrawl: release request failed")?
        .error_for_status()
        .context("Firecrawl: release returned an error status")?;
    Ok(())
}

// ── Shared helpers ───────────────────────────────────────────────────────

/// Shared HTTP client for remote-backend control-plane calls. Built once and
/// reused so we don't pay TLS + connection-pool setup on every create/close.
/// Per-call timeouts are applied on the request builder (see [`req_timeout`]),
/// not baked into the client, so each call still honors `startup_timeout_ms`.
fn http_client() -> Result<&'static reqwest::Client> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    // Build outside `get_or_init` so a (rare) construction failure surfaces as
    // an error instead of poisoning the slot with a panic.
    let client = reqwest::Client::builder()
        .build()
        .context("failed to build HTTP client for remote browser backend")?;
    Ok(CLIENT.get_or_init(|| client))
}

/// Per-request timeout for a remote-backend control-plane call.
fn req_timeout(cfg: &BrowserConfig) -> Duration {
    Duration::from_millis(cfg.startup_timeout_ms)
}

/// Resolve an optional credential, mapping "unset" to a precise, actionable
/// error naming the setting the user must configure.
fn resolve_key(
    key: &Option<crate::security::secrets_resolve::SecretRef>,
    setting: &str,
) -> Result<String> {
    let sref = key
        .as_ref()
        .with_context(|| format!("{setting} not set — required for this browser backend"))?;
    sref.resolve()
        .with_context(|| format!("failed to resolve {setting}"))
}

fn base_url(override_url: &Option<String>, default: &str) -> String {
    override_url
        .clone()
        .unwrap_or_else(|| default.to_string())
        .trim_end_matches('/')
        .to_string()
}

async fn parse_json(resp: reqwest::Response, provider: &str) -> Result<serde_json::Value> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .with_context(|| format!("{provider}: failed to read session response body"))?;
    if !status.is_success() {
        // Body may echo the request; providers do not include the API key, but
        // keep the surfaced text short so we don't spill a large error page.
        let snippet: String = text.chars().take(300).collect();
        bail!("{provider}: session API returned {status}: {snippet}");
    }
    serde_json::from_str(&text)
        .with_context(|| format!("{provider}: session response was not valid JSON"))
}

/// Pull a CDP endpoint out of a provider response. Providers disagree on the
/// field name (and sometimes nest it under `data`/`session`), so probe the
/// known aliases rather than binding to one schema.
fn extract_cdp_url(json: &serde_json::Value) -> Option<String> {
    const KEYS: &[&str] = &[
        "connectUrl",
        "cdpUrl",
        "wsUrl",
        "webSocketDebuggerUrl",
        "url",
    ];
    find_string(json, KEYS)
}

/// Pull the provider-side session identifier, used to release the session.
fn extract_session_id(json: &serde_json::Value) -> Option<String> {
    const KEYS: &[&str] = &["id", "sessionId", "session_id", "browserId"];
    find_string(json, KEYS)
}

/// Look up any of `keys` at the top level of `json` or under a `data`/`session`
/// wrapper, returning the first non-empty string value found.
fn find_string(json: &serde_json::Value, keys: &[&str]) -> Option<String> {
    let candidates = [Some(json), json.get("data"), json.get("session")];
    for obj in candidates.into_iter().flatten() {
        for key in keys {
            if let Some(s) = obj.get(key).and_then(serde_json::Value::as_str) {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

/// Run a provider-supplied CDP endpoint through the shared URL-policy
/// classifier and return the endpoint to actually connect to.
///
/// The classifier only understands http/https, so a `ws(s)://` endpoint is
/// rewritten to `http(s)://` for the check (scheme match is case-insensitive —
/// a provider or proxy may return `WSS://`). Only the host is inspected (SSRF
/// blocklist, metadata endpoints, denylist, DNS→private-IP), which is exactly
/// what we need to gate before connecting.
///
/// The user's `url_policy.allowlist` is intentionally cleared for this check:
/// it is a `web_fetch`/`web_search` restriction, and a cloud provider's CDP
/// host legitimately won't be on it — applying it would silently disable every
/// cloud browser backend. Every other control (SSRF, denylist, private-IP)
/// still applies.
///
/// On success the classifier's `effective_url` is returned (a transform rule
/// may have rewritten the host), with the `ws(s)://` scheme restored so the
/// CDP client connects over the right transport.
async fn guard_remote_cdp_url(cdp_url: &str, policy: &UrlPolicyConfig) -> Result<String> {
    let lower = cdp_url.to_ascii_lowercase();
    let (probe, ws_scheme) = if lower.starts_with("wss://") {
        (
            format!("https://{}", &cdp_url["wss://".len()..]),
            Some("wss"),
        )
    } else if lower.starts_with("ws://") {
        (format!("http://{}", &cdp_url["ws://".len()..]), Some("ws"))
    } else {
        (cdp_url.to_string(), None)
    };

    let ssrf_policy = UrlPolicyConfig {
        allowlist: Vec::new(),
        ..policy.clone()
    };

    match url_policy::classify_no_throttle(&probe, &ssrf_policy).await {
        ClassifyOutcome::Allow { effective_url } => {
            Ok(restore_ws_scheme(&effective_url, ws_scheme))
        }
        ClassifyOutcome::Throttle { host, .. } => {
            bail!("remote browser CDP endpoint is throttled: {host}")
        }
        ClassifyOutcome::Block { reason, rule } => {
            bail!("remote browser CDP endpoint rejected by URL policy [{rule}]: {reason}")
        }
    }
}

/// Map a classifier-approved `http(s)://` URL back to the `ws(s)://` scheme the
/// provider handed us, so the CDP client dials the correct transport. A URL
/// that wasn't rewritten (already `http(s)://`) is returned unchanged.
fn restore_ws_scheme(url: &str, ws_scheme: Option<&str>) -> String {
    match ws_scheme {
        Some("wss") => url
            .strip_prefix("https://")
            .map(|rest| format!("wss://{rest}"))
            .unwrap_or_else(|| url.to_string()),
        Some("ws") => url
            .strip_prefix("http://")
            .map(|rest| format!("ws://{rest}"))
            .unwrap_or_else(|| url.to_string()),
        _ => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UrlPolicyConfig;
    use crate::security::secrets_resolve::SecretRef;

    fn cfg_with(backend: BrowserBackend) -> BrowserConfig {
        BrowserConfig {
            backend,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn local_and_cdp_attach_have_no_remote_session() {
        let policy = UrlPolicyConfig::default();
        for backend in [BrowserBackend::Local, BrowserBackend::CdpAttach] {
            let out = create_remote_session(&cfg_with(backend), &policy)
                .await
                .expect("resolve must not error");
            assert!(out.is_none(), "{backend} must not create a remote session");
        }
    }

    #[tokio::test]
    async fn missing_credential_errors_precisely() {
        let policy = UrlPolicyConfig::default();
        // Browserbase with no key set → error naming the exact setting, not a
        // silent fall-through to a local launch.
        let err = create_remote_session(&cfg_with(BrowserBackend::Browserbase), &policy)
            .await
            .expect_err("missing key must be an error");
        assert!(
            err.to_string().contains("browser.browserbase_api_key"),
            "error should name the missing setting, got: {err}"
        );
    }

    #[test]
    fn extract_cdp_url_probes_aliases_and_nesting() {
        // top-level connectUrl
        assert_eq!(
            extract_cdp_url(&serde_json::json!({"connectUrl": "wss://a/1"})).as_deref(),
            Some("wss://a/1")
        );
        // alias cdpUrl
        assert_eq!(
            extract_cdp_url(&serde_json::json!({"cdpUrl": "wss://b/2"})).as_deref(),
            Some("wss://b/2")
        );
        // nested under data
        assert_eq!(
            extract_cdp_url(&serde_json::json!({"data": {"wsUrl": "ws://c/3"}})).as_deref(),
            Some("ws://c/3")
        );
        // empty string is not a match
        assert_eq!(
            extract_cdp_url(&serde_json::json!({"connectUrl": ""})),
            None
        );
        // nothing usable
        assert_eq!(extract_cdp_url(&serde_json::json!({"foo": "bar"})), None);
    }

    #[test]
    fn extract_session_id_probes_aliases() {
        assert_eq!(
            extract_session_id(&serde_json::json!({"id": "s1"})).as_deref(),
            Some("s1")
        );
        assert_eq!(
            extract_session_id(&serde_json::json!({"session": {"sessionId": "s2"}})).as_deref(),
            Some("s2")
        );
        assert_eq!(extract_session_id(&serde_json::json!({})), None);
    }

    #[test]
    fn base_url_applies_override_and_trims_slash() {
        assert_eq!(
            base_url(&None, "https://api.example.com/"),
            "https://api.example.com"
        );
        assert_eq!(
            base_url(
                &Some("https://local.test/".into()),
                "https://api.example.com"
            ),
            "https://local.test"
        );
    }

    #[tokio::test]
    async fn ssrf_gate_blocks_metadata_endpoint() {
        let policy = UrlPolicyConfig::default();
        // A provider that tries to hand us the cloud metadata service must be
        // rejected before we ever connect.
        let err = guard_remote_cdp_url("wss://metadata.google.internal/cdp", &policy)
            .await
            .expect_err("metadata host must be blocked");
        assert!(
            err.to_string().contains("URL policy"),
            "expected a policy rejection, got: {err}"
        );
    }

    #[tokio::test]
    async fn ssrf_gate_allows_public_endpoint_and_restores_ws_scheme() {
        let policy = UrlPolicyConfig::default();
        // A normal public provider endpoint passes the gate (rewritten to https
        // for the host check) and comes back with its ws scheme restored. A
        // literal public IP keeps the test hermetic — `resolve_host` short-
        // circuits IP literals, so no DNS/network under a default `cargo test`.
        let guarded = guard_remote_cdp_url("wss://1.1.1.1:9222/session", &policy)
            .await
            .expect("public provider endpoint should pass the SSRF gate");
        assert_eq!(guarded, "wss://1.1.1.1:9222/session");
    }

    #[tokio::test]
    async fn ssrf_gate_accepts_uppercase_ws_scheme() {
        let policy = UrlPolicyConfig::default();
        // A provider/proxy may return a mixed-case scheme; the rewrite must be
        // case-insensitive so a legitimate endpoint isn't wrongly blocked.
        let guarded = guard_remote_cdp_url("WSS://1.1.1.1:9222/session", &policy)
            .await
            .expect("uppercase scheme must still pass the gate");
        assert_eq!(guarded, "wss://1.1.1.1:9222/session");
    }

    #[tokio::test]
    async fn ssrf_gate_ignores_web_fetch_allowlist() {
        // A user's web_fetch/web_search allowlist must NOT disable cloud browser
        // backends: the provider's CDP host won't be on that list, but selecting
        // the backend implies authorization. Every other SSRF control still runs.
        let policy = UrlPolicyConfig {
            allowlist: vec!["example.com".into()],
            ..UrlPolicyConfig::default()
        };
        let guarded = guard_remote_cdp_url("wss://1.1.1.1:9222/session", &policy)
            .await
            .expect("allowlist must not gate the remote CDP host");
        assert_eq!(guarded, "wss://1.1.1.1:9222/session");
    }

    #[tokio::test]
    async fn ssrf_gate_still_blocks_metadata_even_with_allowlist_set() {
        // Clearing the allowlist must not weaken the metadata/SSRF block.
        let policy = UrlPolicyConfig {
            allowlist: vec!["example.com".into()],
            ..UrlPolicyConfig::default()
        };
        let err = guard_remote_cdp_url("wss://169.254.169.254/cdp", &policy)
            .await
            .expect_err("cloud-metadata IP must stay blocked");
        assert!(
            err.to_string().contains("URL policy"),
            "expected a policy rejection, got: {err}"
        );
    }

    #[test]
    fn restore_ws_scheme_maps_back_and_passes_through() {
        assert_eq!(restore_ws_scheme("https://h/p", Some("wss")), "wss://h/p");
        assert_eq!(restore_ws_scheme("http://h/p", Some("ws")), "ws://h/p");
        // No original ws scheme → returned unchanged (discovery http(s) URL).
        assert_eq!(restore_ws_scheme("https://h/p", None), "https://h/p");
    }

    #[tokio::test]
    async fn browserbase_create_sends_auth_and_parses_connect_url() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/sessions"))
            .and(header("x-bb-api-key", "secret-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "sess-123",
                "connectUrl": "wss://connect.browserbase.com/sess-123",
            })))
            .mount(&server)
            .await;

        let mut cfg = cfg_with(BrowserBackend::Browserbase);
        cfg.browserbase_api_key = Some(SecretRef::Env {
            var: "BORG_TEST_BB_KEY".into(),
        });
        cfg.browserbase_project_id = Some("proj-1".into());
        cfg.browserbase_base_url = Some(server.uri());
        // SecretRef::Env resolves from the process env at call time.
        std::env::set_var("BORG_TEST_BB_KEY", "secret-key");

        let session = create_browserbase(&cfg)
            .await
            .expect("create should succeed against the mock");
        std::env::remove_var("BORG_TEST_BB_KEY");

        assert_eq!(session.provider, BrowserBackend::Browserbase);
        assert_eq!(session.session_id.as_deref(), Some("sess-123"));
        assert_eq!(session.cdp_url, "wss://connect.browserbase.com/sess-123");
    }

    #[tokio::test]
    async fn provider_http_error_surfaces_status() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/browser"))
            .respond_with(ResponseTemplate::new(402).set_body_string("payment required"))
            .mount(&server)
            .await;

        let mut cfg = cfg_with(BrowserBackend::Firecrawl);
        cfg.firecrawl_api_key = Some(SecretRef::Env {
            var: "BORG_TEST_FC_KEY".into(),
        });
        cfg.firecrawl_base_url = Some(server.uri());
        std::env::set_var("BORG_TEST_FC_KEY", "fc-key");

        let err = create_firecrawl(&cfg)
            .await
            .expect_err("402 must surface as an error");
        std::env::remove_var("BORG_TEST_FC_KEY");
        assert!(
            err.to_string().contains("402"),
            "error should carry the status, got: {err}"
        );
    }
}
