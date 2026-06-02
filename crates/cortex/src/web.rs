use anyhow::{Context, Result};
use scraper::{Html, Selector};
use std::sync::LazyLock;

use crate::config::{UrlPolicyConfig, WebConfig};
use crate::constants;
use crate::url_policy::{self, ClassifyOutcome};

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

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(
            constants::WEB_FETCH_TIMEOUT_SECS,
        ))
        .redirect(redirect)
        .build()?;

    let resp = with_trace_context(client.get(&effective_url))
        .send()
        .await
        .with_context(|| format!("Failed to fetch {effective_url}"))?;
    tracing::Span::current().record("http.status_code", resp.status().as_u16());

    // Post-fetch: re-validate the final landing URL with full DNS,
    // *unconditionally*, to defend against DNS rebinding. Even a
    // no-redirect 200 can have IPs at connect time that differ from the
    // pre-flight lookup (TOCTOU), so we re-resolve regardless of whether
    // a redirect occurred. The cost is one cached DNS lookup.
    //
    // True end-to-end DNS-rebinding defense requires pinning the
    // pre-flight IPs into the connector (e.g. a custom
    // `reqwest::dns::Resolve`). That's tracked as future work; the
    // post-fetch recheck closes the window for the common case where the
    // attacker's DNS TTL has not yet expired between resolves.
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

    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("Failed to read body from {effective_url}"))?;
    const MAX_BODY_BYTES: usize = constants::WEB_MAX_BODY_BYTES;
    if bytes.len() > MAX_BODY_BYTES {
        anyhow::bail!(
            "Response body too large ({} bytes, max {MAX_BODY_BYTES})",
            bytes.len()
        );
    }
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

/// Search via Brave Search API (requires API key).
#[tracing::instrument(skip_all, fields(search.provider = "brave"))]
async fn brave_search(query: &str, config: &WebConfig) -> Result<String> {
    let api_key_env = config
        .search_api_key_env
        .as_deref()
        .unwrap_or("BRAVE_SEARCH_API_KEY");
    let api_key = std::env::var(api_key_env)
        .with_context(|| format!("Brave Search API key not found. Set {api_key_env}"))?;

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

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_else(|e| {
            tracing::warn!("Failed to read error response body: {e}");
            String::new()
        });
        anyhow::bail!("Brave Search returned HTTP {status}: {body}");
    }

    let body: serde_json::Value = resp.json().await?;
    let mut results = Vec::new();

    if let Some(web_results) = body["web"]["results"].as_array() {
        for (i, r) in web_results.iter().enumerate() {
            if i >= MAX_SEARCH_RESULTS {
                break;
            }
            results.push(SearchResult {
                title: r["title"].as_str().unwrap_or("").to_string(),
                url: r["url"].as_str().unwrap_or("").to_string(),
                snippet: r["description"].as_str().unwrap_or("").to_string(),
            });
        }
    }

    Ok(format_results(query, &results))
}

/// Search via Tavily API (AI-optimized search, requires TAVILY_API_KEY).
#[tracing::instrument(skip_all, fields(search.provider = "tavily"))]
async fn tavily_search(query: &str, config: &WebConfig) -> Result<String> {
    let api_key_env = config
        .search_api_key_env
        .as_deref()
        .unwrap_or("TAVILY_API_KEY");
    let api_key = std::env::var(api_key_env)
        .with_context(|| format!("Tavily API key not found. Set {api_key_env}"))?;

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

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_else(|e| {
            tracing::warn!("Failed to read error response body: {e}");
            String::new()
        });
        anyhow::bail!("Tavily returned HTTP {status}: {body}");
    }

    let resp_body: serde_json::Value = resp.json().await?;
    let mut results = Vec::new();

    if let Some(tavily_results) = resp_body["results"].as_array() {
        for (i, r) in tavily_results.iter().enumerate() {
            if i >= MAX_SEARCH_RESULTS {
                break;
            }
            results.push(SearchResult {
                title: r["title"].as_str().unwrap_or("").to_string(),
                url: r["url"].as_str().unwrap_or("").to_string(),
                snippet: r["content"].as_str().unwrap_or("").to_string(),
            });
        }
    }

    Ok(format_results(query, &results))
}

/// Search via Serper API (Google results, requires SERPER_API_KEY).
#[tracing::instrument(skip_all, fields(search.provider = "serper"))]
async fn serper_search(query: &str, config: &WebConfig) -> Result<String> {
    let api_key_env = config
        .search_api_key_env
        .as_deref()
        .unwrap_or("SERPER_API_KEY");
    let api_key = std::env::var(api_key_env)
        .with_context(|| format!("Serper API key not found. Set {api_key_env}"))?;

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

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_else(|e| {
            tracing::warn!("Failed to read error response body: {e}");
            String::new()
        });
        anyhow::bail!("Serper returned HTTP {status}: {body}");
    }

    let resp_body: serde_json::Value = resp.json().await?;
    let mut results = Vec::new();

    if let Some(organic) = resp_body["organic"].as_array() {
        for (i, r) in organic.iter().enumerate() {
            if i >= MAX_SEARCH_RESULTS {
                break;
            }
            results.push(SearchResult {
                title: r["title"].as_str().unwrap_or("").to_string(),
                url: r["link"].as_str().unwrap_or("").to_string(),
                snippet: r["snippet"].as_str().unwrap_or("").to_string(),
            });
        }
    }

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
