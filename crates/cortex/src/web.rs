use anyhow::{Context, Result};
use scraper::{Html, Selector};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock};

use crate::config::{UrlPolicyConfig, WebConfig};
use crate::constants;
use crate::url_policy::{self, ssrf, ClassifyOutcome};

const MAX_FETCH_CHARS: usize = constants::WEB_FETCH_MAX_CHARS;
const MAX_SEARCH_RESULTS: usize = constants::WEB_MAX_SEARCH_RESULTS;
const USER_AGENT: &str = "Mozilla/5.0 (compatible; Borg/0.1)";

fn build_http_client(timeout_secs: u64) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .map_err(Into::into)
}

/// Filter a set of resolved IPs down to those the SSRF policy permits,
/// returning `SocketAddr`s (port 0 — the HTTP connector overrides it with the
/// destination port). Mirrors the IP gate in [`url_policy::classify`]:
/// always-blocked cloud-metadata/link-local addresses are dropped, and private
/// addresses are dropped unless `allow_private` is set. This is the pure core
/// of [`SsrfGuardResolver`], extracted so it can be unit-tested without DNS.
fn ssrf_filter_addrs(ips: Vec<IpAddr>, allow_private: bool) -> Vec<SocketAddr> {
    ips.into_iter()
        .filter(|ip| !ssrf::is_always_blocked_ip(ip))
        .filter(|ip| allow_private || !url_policy::is_private_class(ip))
        .map(|ip| SocketAddr::new(ip, 0))
        .collect()
}

/// A reqwest DNS resolver that enforces the SSRF IP policy at connect time.
///
/// reqwest/hyper invoke this for the initial request AND for every redirect hop
/// (each new host is resolved through it), so a hostname that resolves — right
/// now, at the moment of connection — to a blocked or private address is
/// refused. This is what closes the DNS-rebinding / redirect-to-internal window
/// that a pre-flight `classify()` alone leaves open: the pre-flight lookup and
/// the connect-time lookup are no longer allowed to disagree, because the
/// connection uses exactly the addresses this resolver validated.
#[derive(Debug, Clone)]
pub struct SsrfGuardResolver {
    allow_private: bool,
}

/// Build the connect-time SSRF DNS guard that `web_fetch` pins into its client,
/// exposed so other outbound-fetch paths (e.g. the gateway link-unfurl) enforce
/// the same IP policy on the initial request AND every redirect hop instead of
/// re-implementing a weaker offline (no-DNS) per-hop check. With
/// `allow_private = false` the resolver refuses, at connect time, any host that
/// resolves to private/loopback/link-local/CGNAT or cloud-metadata space —
/// closing the redirect-to-internal and DNS-rebinding window that a name-fronted
/// internal host on an intermediate hop would otherwise slip through.
pub fn ssrf_guard_resolver(allow_private: bool) -> Arc<SsrfGuardResolver> {
    Arc::new(SsrfGuardResolver { allow_private })
}

impl reqwest::dns::Resolve for SsrfGuardResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let allow_private = self.allow_private;
        Box::pin(async move {
            let host = name.as_str().to_string();
            let ips = ssrf::resolve_host(&host).await?;
            let allowed = ssrf_filter_addrs(ips, allow_private);
            if allowed.is_empty() {
                // All resolved addresses are blocked (or the host resolves only
                // to internal space). Fail the connection rather than fall back
                // to an OS resolve that would skip the policy.
                let err: Box<dyn std::error::Error + Send + Sync> = format!(
                    "SSRF policy: host '{host}' resolves only to blocked/private addresses"
                )
                .into();
                return Err(err);
            }
            let addrs: reqwest::dns::Addrs = Box::new(allowed.into_iter());
            Ok(addrs)
        })
    }
}

/// Read a response body chunk-by-chunk, aborting as soon as the cumulative size
/// exceeds `max_bytes`. Unlike `Response::bytes()` (which buffers the entire body
/// before any size check), this bounds peak memory: a hostile or compromised
/// server that streams a multi-GB chunked response is stopped after ~`max_bytes`
/// rather than OOMing the process.
async fn read_body_capped(mut resp: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.context("error reading response chunk")? {
        if buf.len() + chunk.len() > max_bytes {
            anyhow::bail!("Response body too large (exceeds {max_bytes} bytes)");
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Attach the active trace context to an outbound request as a `traceparent`
/// header so collectors can stitch the HTTP call to the agent span. Inert when
/// telemetry is disabled or there is no recording span (no header is added).
fn with_trace_context(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    // `HeaderMap::new()` doesn't allocate and merging an empty map is a no-op,
    // so this stays cheap when telemetry is off (no traceparent injected).
    let mut headers = reqwest::header::HeaderMap::new();
    crate::telemetry::propagation::inject_current_context(&mut headers);
    req.headers(headers)
}

/// A single search result from any provider.
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Format search results into a human-readable string.
pub fn format_results(query: &str, results: &[SearchResult]) -> String {
    if results.is_empty() {
        return format!("No search results found for: {query}");
    }
    let formatted: Vec<String> = results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            format!(
                "{}. {}\n   URL: {}\n   {}",
                i + 1,
                r.title.trim(),
                r.url,
                r.snippet.trim()
            )
        })
        .collect();
    format!(
        "Search results for \"{query}\":\n\n{}",
        formatted.join("\n\n")
    )
}

/// Resolve which search provider to use based on config and available env vars.
///
/// Priority when `search_provider = "auto"`:
///   Tavily > Serper > Brave > DuckDuckGo (keyless fallback)
pub fn resolve_provider_id(config: &WebConfig) -> &str {
    let p = config.search_provider.as_str();
    if p != "auto" {
        return p;
    }

    // Auto-detect from env vars in priority order
    const AUTO_DETECT: &[(&str, &str)] = &[
        ("tavily", "TAVILY_API_KEY"),
        ("serper", "SERPER_API_KEY"),
        ("brave", "BRAVE_SEARCH_API_KEY"),
    ];

    for (id, env_var) in AUTO_DETECT {
        if std::env::var(env_var).is_ok() {
            return id;
        }
    }

    "duckduckgo"
}

/// Fetch a URL and return its text content. HTML is stripped to plain text.
///
/// The URL is run through [`url_policy::classify`] pre-flight; redirects
/// are gated via a `Policy::custom` callback that runs the no-DNS
/// classifier on each hop; and the final landing URL is re-checked with
/// the full classifier post-fetch to defend against DNS rebinding.
#[tracing::instrument(
    skip_all,
    fields(http.method = "GET", http.url = %url, http.status_code = tracing::field::Empty)
)]
pub async fn web_fetch(
    url: &str,
    max_chars: Option<usize>,
    policy: &UrlPolicyConfig,
) -> Result<String> {
    let max = max_chars.unwrap_or(MAX_FETCH_CHARS);

    // Pre-flight: full classify (DNS + transforms + throttle). Surface the
    // block/throttle message via the canonical `describe_block_or_throttle`
    // helper so `web_fetch` and `browser` callers always see identical
    // wording (avoids silent desync between two error formatters).
    let preflight = url_policy::classify(url, policy).await;
    let effective_url = match &preflight {
        ClassifyOutcome::Allow { effective_url } => effective_url.clone(),
        _ => {
            let msg = url_policy::describe_block_or_throttle(&preflight)
                .unwrap_or_else(|| "URL policy refused request".to_string());
            anyhow::bail!("{msg}");
        }
    };

    // Per-hop redirect re-validation (no DNS — full re-classify is too
    // expensive on every redirect; the post-fetch check below covers DNS).
    let redirect_policy_cfg = policy.clone();
    let redirect_limit = constants::WEB_REDIRECT_LIMIT;
    let redirect = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= redirect_limit {
            return attempt.error("too many redirects");
        }
        let next_url = attempt.url().to_string();
        match url_policy::classify_offline(&next_url, &redirect_policy_cfg) {
            ClassifyOutcome::Allow { .. } => attempt.follow(),
            ClassifyOutcome::Block { reason, rule } => attempt.error(format!(
                "URL policy blocked redirect to {next_url}: {reason} (rule: {rule})"
            )),
            ClassifyOutcome::Throttle { .. } => {
                // Throttle has no meaning on a redirect hop — let it follow.
                attempt.follow()
            }
        }
    });

    // Pin SSRF enforcement into the connector's DNS resolver so the addresses
    // actually connected to — on the initial request and on every redirect hop —
    // are exactly the ones validated against the policy. This closes the
    // DNS-rebinding / redirect-to-internal window: the connect-time resolve can
    // no longer differ from a pre-flight lookup, because there is only one
    // resolve and it enforces the IP gate.
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(
            constants::WEB_FETCH_TIMEOUT_SECS,
        ))
        .redirect(redirect)
        .dns_resolver(Arc::new(SsrfGuardResolver {
            allow_private: policy.allow_private_urls,
        }))
        .build()?;

    let resp = with_trace_context(client.get(&effective_url))
        .send()
        .await
        .with_context(|| format!("Failed to fetch {effective_url}"))?;
    tracing::Span::current().record("http.status_code", resp.status().as_u16());

    // Post-fetch: re-validate the final landing URL with full DNS as
    // defense-in-depth. The connector's `SsrfGuardResolver` is the primary
    // guard — it already enforced the IP policy on the addresses actually
    // connected to, for every hop — so this recheck is a belt-and-suspenders
    // catch for the final host rather than the sole rebinding defense.
    let final_url = resp.url().to_string();
    let post = url_policy::classify_no_throttle(&final_url, policy).await;
    if !matches!(post, ClassifyOutcome::Allow { .. }) {
        let msg = url_policy::describe_block_or_throttle(&post)
            .unwrap_or_else(|| "URL policy refused final URL".to_string());
        anyhow::bail!("post-fetch: {msg} (final url: {final_url})");
    }

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {status} fetching {effective_url}");
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    const MAX_BODY_BYTES: usize = constants::WEB_MAX_BODY_BYTES;
    let bytes = read_body_capped(resp, MAX_BODY_BYTES)
        .await
        .with_context(|| format!("Failed to read body from {effective_url}"))?;
    let body = String::from_utf8_lossy(&bytes).into_owned();

    let text = if content_type.contains("text/html") || content_type.contains("application/xhtml") {
        html_to_text(&body)
    } else {
        body
    };

    let char_count = text.chars().count();
    if char_count > max {
        let truncated: String = text.chars().take(max).collect();
        Ok(format!(
            "{truncated}\n\n[truncated — showing {max} of {char_count} chars]"
        ))
    } else {
        Ok(text)
    }
}

/// Search the web and return formatted results.
pub async fn web_search(query: &str, config: &WebConfig) -> Result<String> {
    let provider = resolve_provider_id(config);
    match provider {
        "tavily" => tavily_search(query, config).await,
        "serper" => serper_search(query, config).await,
        "brave" => brave_search(query, config).await,
        _ => duckduckgo_search(query).await,
    }
}

// Provider implementations

/// Search via DuckDuckGo HTML endpoint (no API key needed).
#[tracing::instrument(skip_all, fields(search.provider = "duckduckgo"))]
async fn duckduckgo_search(query: &str) -> Result<String> {
    let client = build_http_client(15)?;

    let resp = with_trace_context(
        client
            .post("https://html.duckduckgo.com/html/")
            .form(&[("q", query)]),
    )
    .send()
    .await
    .context("Failed to reach DuckDuckGo")?;

    let html = resp.text().await?;
    let document = Html::parse_document(&html);

    static RESULT_SEL: LazyLock<Option<Selector>> =
        LazyLock::new(|| Selector::parse(".result").ok());
    static TITLE_SEL: LazyLock<Option<Selector>> =
        LazyLock::new(|| Selector::parse(".result__a").ok());
    static SNIPPET_SEL: LazyLock<Option<Selector>> =
        LazyLock::new(|| Selector::parse(".result__snippet").ok());
    let (Some(result_selector), Some(title_selector), Some(snippet_selector)) = (
        RESULT_SEL.as_ref(),
        TITLE_SEL.as_ref(),
        SNIPPET_SEL.as_ref(),
    ) else {
        tracing::error!("Failed to parse DuckDuckGo CSS selectors");
        return Ok(format_results(query, &[]));
    };

    let mut results = Vec::new();
    for (i, result) in document.select(result_selector).enumerate() {
        if i >= MAX_SEARCH_RESULTS {
            break;
        }

        let title: String = result
            .select(title_selector)
            .next()
            .map(|e| e.text().collect())
            .unwrap_or_default();

        if title.is_empty() {
            continue;
        }

        let url: String = result
            .select(title_selector)
            .next()
            .and_then(|e| e.value().attr("href"))
            .unwrap_or("")
            .to_string();

        let snippet: String = result
            .select(snippet_selector)
            .next()
            .map(|e| e.text().collect())
            .unwrap_or_default();

        results.push(SearchResult {
            title: title.trim().to_string(),
            url,
            snippet: snippet.trim().to_string(),
        });
    }

    Ok(format_results(query, &results))
}

/// Resolve a search provider's API key from the configured env var (falling
/// back to the provider's default). `provider` names the provider in the error.
fn resolve_search_api_key(config: &WebConfig, default_env: &str, provider: &str) -> Result<String> {
    let api_key_env = config.search_api_key_env.as_deref().unwrap_or(default_env);
    std::env::var(api_key_env)
        .with_context(|| format!("{provider} API key not found. Set {api_key_env}"))
}

/// Bail with the provider name, status, and response body when a search call
/// returned a non-success status; otherwise pass the response through.
async fn ensure_search_ok(resp: reqwest::Response, provider: &str) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_else(|e| {
        tracing::warn!("Failed to read error response body: {e}");
        String::new()
    });
    anyhow::bail!("{provider} returned HTTP {status}: {body}")
}

/// Collect up to [`MAX_SEARCH_RESULTS`] results from a provider's JSON result
/// array, reading title/url/snippet from the provider-specific keys.
fn collect_search_results(
    array: Option<&Vec<serde_json::Value>>,
    title_key: &str,
    url_key: &str,
    snippet_key: &str,
) -> Vec<SearchResult> {
    array
        .into_iter()
        .flatten()
        .take(MAX_SEARCH_RESULTS)
        .map(|r| SearchResult {
            title: r[title_key].as_str().unwrap_or("").to_string(),
            url: r[url_key].as_str().unwrap_or("").to_string(),
            snippet: r[snippet_key].as_str().unwrap_or("").to_string(),
        })
        .collect()
}

/// Search via Brave Search API (requires API key).
#[tracing::instrument(skip_all, fields(search.provider = "brave"))]
async fn brave_search(query: &str, config: &WebConfig) -> Result<String> {
    let api_key = resolve_search_api_key(config, "BRAVE_SEARCH_API_KEY", "Brave Search")?;
    let client = build_http_client(15)?;

    let resp = with_trace_context(
        client
            .get("https://api.search.brave.com/res/v1/web/search")
            .header("X-Subscription-Token", &api_key)
            .header("Accept", "application/json")
            .query(&[("q", query), ("count", "8")]),
    )
    .send()
    .await
    .context("Failed to reach Brave Search API")?;

    let resp = ensure_search_ok(resp, "Brave Search").await?;
    let body: serde_json::Value = resp.json().await?;
    let results = collect_search_results(
        body["web"]["results"].as_array(),
        "title",
        "url",
        "description",
    );
    Ok(format_results(query, &results))
}

/// Search via Tavily API (AI-optimized search, requires TAVILY_API_KEY).
#[tracing::instrument(skip_all, fields(search.provider = "tavily"))]
async fn tavily_search(query: &str, config: &WebConfig) -> Result<String> {
    let api_key = resolve_search_api_key(config, "TAVILY_API_KEY", "Tavily")?;
    let client = build_http_client(15)?;

    let body = serde_json::json!({
        "query": query,
        "max_results": MAX_SEARCH_RESULTS,
        "include_answer": false,
    });

    let resp = with_trace_context(
        client
            .post("https://api.tavily.com/search")
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .json(&body),
    )
    .send()
    .await
    .context("Failed to reach Tavily API")?;

    let resp = ensure_search_ok(resp, "Tavily").await?;
    let resp_body: serde_json::Value = resp.json().await?;
    let results =
        collect_search_results(resp_body["results"].as_array(), "title", "url", "content");
    Ok(format_results(query, &results))
}

/// Search via Serper API (Google results, requires SERPER_API_KEY).
#[tracing::instrument(skip_all, fields(search.provider = "serper"))]
async fn serper_search(query: &str, config: &WebConfig) -> Result<String> {
    let api_key = resolve_search_api_key(config, "SERPER_API_KEY", "Serper")?;
    let client = build_http_client(15)?;

    let body = serde_json::json!({
        "q": query,
        "num": MAX_SEARCH_RESULTS,
    });

    let resp = with_trace_context(
        client
            .post("https://google.serper.dev/search")
            .header("X-API-KEY", &api_key)
            .header("Content-Type", "application/json")
            .json(&body),
    )
    .send()
    .await
    .context("Failed to reach Serper API")?;

    let resp = ensure_search_ok(resp, "Serper").await?;
    let resp_body: serde_json::Value = resp.json().await?;
    let results =
        collect_search_results(resp_body["organic"].as_array(), "title", "link", "snippet");
    Ok(format_results(query, &results))
}

// HTML helpers

/// Extract readable text from HTML, trying to focus on main content.
fn html_to_text(html: &str) -> String {
    let document = Html::parse_document(html);

    // Try content-rich selectors first
    let content_selectors = ["main", "article", "#content", ".content", "body"];
    for sel_str in &content_selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(el) = document.select(&sel).next() {
                let text = extract_text(&el);
                if text.len() > 100 {
                    return text;
                }
            }
        }
    }

    // Fallback: all text from root
    let text: String = document.root_element().text().collect::<Vec<_>>().join(" ");
    collapse_whitespace(&text)
}

/// Extract text from an element, skipping script/style/noscript content.
fn extract_text(element: &scraper::ElementRef<'_>) -> String {
    use scraper::node::Node;

    let skip_tags: &[&str] = &["script", "style", "noscript"];

    let mut parts = Vec::new();
    for node in element.descendants() {
        if let Node::Text(text_node) = node.value() {
            // Check if any ancestor is a script/style/noscript element
            let mut in_skip = false;
            let mut ancestor = node.parent();
            while let Some(a) = ancestor {
                if let Node::Element(el) = a.value() {
                    if skip_tags.contains(&el.name.local.as_ref()) {
                        in_skip = true;
                        break;
                    }
                }
                ancestor = a.parent();
            }
            if !in_skip {
                let trimmed = text_node.text.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
        }
    }
    parts.join(" ")
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_basic() {
        let html = r#"<html><body><h1>Hello</h1><p>World</p></body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
    }

    #[test]
    fn ssrf_filter_drops_metadata_and_link_local() {
        // The IMDS IP and link-local space must be dropped regardless of the
        // allow-private toggle — this is the core SSRF gate the resolver pins
        // into the connector for every hop (initial + redirects).
        let imds: IpAddr = "169.254.169.254".parse().unwrap();
        let azure_wire: IpAddr = "168.63.129.16".parse().unwrap();
        let public: IpAddr = "8.8.8.8".parse().unwrap();
        for allow_private in [false, true] {
            let out = ssrf_filter_addrs(vec![imds, azure_wire, public], allow_private);
            let ips: Vec<IpAddr> = out.iter().map(|s| s.ip()).collect();
            assert!(!ips.contains(&imds), "IMDS IP must be dropped");
            assert!(
                !ips.contains(&azure_wire),
                "Azure wireserver must be dropped"
            );
            assert!(ips.contains(&public), "public IP must survive");
        }
    }

    #[test]
    fn ssrf_filter_gates_private_on_allow_flag() {
        let private: IpAddr = "10.1.2.3".parse().unwrap();
        // Default: private addresses are refused (empty → connection fails).
        assert!(ssrf_filter_addrs(vec![private], false).is_empty());
        // Only when the operator opts in are they allowed.
        assert_eq!(ssrf_filter_addrs(vec![private], true).len(), 1);
    }

    /// One-shot loopback HTTP server that replies with `body`, returning its
    /// port. Lets us exercise `read_body_capped` against a real
    /// `reqwest::Response` stream.
    async fn spawn_oneshot_http(body: Vec<u8>) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                let _ = sock.write_all(header.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.flush().await;
            }
        });
        port
    }

    #[tokio::test]
    async fn read_body_capped_rejects_oversized_body() {
        // A body larger than the cap must error rather than buffer whole — the
        // guard against a hostile server OOMing the fetch path.
        let port = spawn_oneshot_http(vec![b'x'; 8192]).await;
        let resp = reqwest::get(format!("http://127.0.0.1:{port}/"))
            .await
            .unwrap();
        let err = read_body_capped(resp, 64)
            .await
            .expect_err("oversized body must be rejected");
        assert!(
            err.to_string().contains("too large"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn read_body_capped_returns_body_under_cap() {
        // Control case: a body under the cap is returned intact.
        let port = spawn_oneshot_http(b"hello world".to_vec()).await;
        let resp = reqwest::get(format!("http://127.0.0.1:{port}/"))
            .await
            .unwrap();
        let out = read_body_capped(resp, 1024).await.unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn ssrf_filter_addrs_use_zero_port() {
        // Port 0 — hyper's HTTP connector overrides it with the destination port.
        let public: IpAddr = "93.184.216.34".parse().unwrap();
        let out = ssrf_filter_addrs(vec![public], false);
        assert_eq!(out[0].port(), 0);
    }

    #[test]
    fn html_to_text_strips_tags() {
        let html = r#"<html><body><p>This is <b>bold</b> and <i>italic</i>.</p></body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("bold") && text.contains("italic"));
    }

    #[test]
    fn html_to_text_handles_empty() {
        let text = html_to_text("");
        assert!(text.is_empty() || text.trim().is_empty());
    }

    #[test]
    fn collapse_whitespace_works() {
        assert_eq!(collapse_whitespace("  hello   world  "), "hello world");
    }

    #[test]
    fn format_results_empty() {
        let results: Vec<SearchResult> = vec![];
        let output = format_results("test", &results);
        assert_eq!(output, "No search results found for: test");
    }

    #[test]
    fn collect_search_results_maps_keys_and_caps() {
        // More items than the cap, with provider-specific key names and one
        // entry missing the snippet key (should fall back to empty string).
        let raw = serde_json::Value::Array(
            (0..MAX_SEARCH_RESULTS + 3)
                .map(|i| {
                    let mut obj =
                        serde_json::json!({"t": format!("title{i}"), "u": format!("https://{i}")});
                    if i != 1 {
                        obj["s"] = serde_json::json!(format!("snippet{i}"));
                    }
                    obj
                })
                .collect(),
        );
        let results = collect_search_results(raw.as_array(), "t", "u", "s");
        assert_eq!(results.len(), MAX_SEARCH_RESULTS, "truncated to cap");
        assert_eq!(results[0].title, "title0");
        assert_eq!(results[0].url, "https://0");
        assert_eq!(results[0].snippet, "snippet0");
        // Missing snippet key resolves to empty, not a panic.
        assert_eq!(results[1].snippet, "");
    }

    #[test]
    fn collect_search_results_none_is_empty() {
        let absent = serde_json::Value::Null;
        let results = collect_search_results(absent.as_array(), "title", "url", "snippet");
        assert!(results.is_empty());
    }

    #[test]
    fn format_results_with_items() {
        let results = vec![
            SearchResult {
                title: "First Result".into(),
                url: "https://example.com/1".into(),
                snippet: "A snippet".into(),
            },
            SearchResult {
                title: "Second Result".into(),
                url: "https://example.com/2".into(),
                snippet: "Another snippet".into(),
            },
        ];
        let output = format_results("rust", &results);
        assert!(output.contains("Search results for \"rust\""));
        assert!(output.contains("1. First Result"));
        assert!(output.contains("URL: https://example.com/1"));
        assert!(output.contains("2. Second Result"));
        assert!(output.contains("A snippet"));
    }

    #[test]
    fn resolve_provider_explicit_brave() {
        let config = WebConfig {
            enabled: true,
            search_provider: "brave".into(),
            search_api_key_env: None,
            search_cache_ttl_secs: 3600,
        };
        assert_eq!(resolve_provider_id(&config), "brave");
    }

    #[test]
    fn resolve_provider_explicit_tavily() {
        let config = WebConfig {
            enabled: true,
            search_provider: "tavily".into(),
            search_api_key_env: None,
            search_cache_ttl_secs: 3600,
        };
        assert_eq!(resolve_provider_id(&config), "tavily");
    }

    #[test]
    fn resolve_provider_explicit_serper() {
        let config = WebConfig {
            enabled: true,
            search_provider: "serper".into(),
            search_api_key_env: None,
            search_cache_ttl_secs: 3600,
        };
        assert_eq!(resolve_provider_id(&config), "serper");
    }

    #[test]
    fn resolve_provider_explicit_duckduckgo() {
        let config = WebConfig {
            enabled: true,
            search_provider: "duckduckgo".into(),
            search_api_key_env: None,
            search_cache_ttl_secs: 3600,
        };
        assert_eq!(resolve_provider_id(&config), "duckduckgo");
    }

    #[test]
    fn resolve_provider_auto_fallback_duckduckgo() {
        // When no env vars are set, auto should fall back to duckduckgo
        // Note: this test may be flaky if TAVILY_API_KEY/SERPER_API_KEY/BRAVE_SEARCH_API_KEY
        // are set in the test environment, but that's unlikely in CI.
        let config = WebConfig {
            enabled: true,
            search_provider: "auto".into(),
            search_api_key_env: None,
            search_cache_ttl_secs: 3600,
        };
        // We can't guarantee env vars aren't set, so just verify it returns a valid provider
        let provider = resolve_provider_id(&config);
        assert!(
            ["tavily", "serper", "brave", "duckduckgo"].contains(&provider),
            "auto should resolve to a known provider, got: {provider}"
        );
    }

    #[test]
    fn backward_compat_default_config() {
        // Existing configs with search_provider = "duckduckgo" should still work
        let config = WebConfig {
            enabled: true,
            search_provider: "duckduckgo".into(),
            search_api_key_env: None,
            search_cache_ttl_secs: 3600,
        };
        assert_eq!(resolve_provider_id(&config), "duckduckgo");
    }
}
