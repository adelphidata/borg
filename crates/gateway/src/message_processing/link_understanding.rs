use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use regex::Regex;
use tracing::debug;

use cortex::config::{LinkUnderstandingConfig, UrlPolicyConfig};
use cortex::url_policy::{self, ClassifyOutcome};

/// Number of redirect hops to follow before giving up. Each hop is re-checked
/// against the SSRF policy, so this only bounds redirect-chain length.
const REDIRECT_LIMIT: usize = 3;

/// Inbound link-unfurl acts on attacker-controlled URLs pulled from untrusted
/// messages, so it must NEVER reach private/loopback/internal space — regardless
/// of `allow_private_urls`, which is an operator convenience for the first-party
/// `web_fetch` tool, not something an inbound message sender should be able to
/// leverage. Force the flag off for every SSRF check on this path.
fn inbound_policy(policy: &UrlPolicyConfig) -> UrlPolicyConfig {
    UrlPolicyConfig {
        allow_private_urls: false,
        ..policy.clone()
    }
}

/// Extract URLs from text, deduplicate, cap at `max_links`, and drop any that
/// the offline SSRF pre-filter refuses.
pub fn extract_urls(text: &str, max_links: usize, policy: &UrlPolicyConfig) -> Vec<String> {
    static URL_RE: OnceLock<Option<Regex>> = OnceLock::new();
    let Some(re) = URL_RE.get_or_init(|| Regex::new(r#"https?://[^\s<>\)\]"']+"#).ok()) else {
        return Vec::new();
    };

    let policy = inbound_policy(policy);
    let mut seen = HashSet::new();
    let mut urls = Vec::new();

    for m in re.find_iter(text) {
        let url = m.as_str().trim_end_matches(['.', ',']);
        if seen.insert(url.to_string()) && is_admissible(url, &policy) {
            urls.push(url.to_string());
            if urls.len() >= max_links {
                break;
            }
        }
    }
    urls
}

/// Offline SSRF pre-filter for the extraction stage. Runs cortex's no-DNS
/// classifier (blocks cloud-metadata FQDNs, link-local/private/CGNAT IP
/// literals, non-http(s) schemes, and allow/deny rules) and additionally
/// rejects the loopback hostname aliases a name-based check can only catch
/// with a resolver. Name-based internal targets that *resolve* to private
/// space are still caught at fetch time by the DNS-resolving
/// [`url_policy::classify`] in [`fetch_link_content`].
fn is_admissible(url: &str, policy: &UrlPolicyConfig) -> bool {
    if !matches!(
        url_policy::classify_offline(url, policy),
        ClassifyOutcome::Allow { .. }
    ) {
        return false;
    }
    match url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
    {
        Some(host) => !matches!(
            host.as_str(),
            "localhost" | "ip6-localhost" | "ip6-loopback"
        ),
        None => false,
    }
}

/// Fetch a URL and extract its text content, truncated to `max_chars`.
///
/// SSRF is enforced with the same layering as the `web_fetch` tool: a full
/// DNS-resolving [`url_policy::classify`] pre-flight on the input URL; a
/// connect-time DNS guard ([`cortex::web::ssrf_guard_resolver`]) pinned into
/// the client so the addresses actually connected to — on the initial request
/// AND every redirect hop — are re-validated against the IP policy (this is what
/// catches a name-fronted internal host on an intermediate hop, which the
/// no-DNS offline re-check cannot); a fast offline per-hop reject for metadata
/// FQDNs / IP literals; and a post-fetch DNS re-check of the final landing host.
/// `allow_private_urls` is forced off here — see [`inbound_policy`].
pub async fn fetch_link_content(
    url: &str,
    max_chars: usize,
    timeout_ms: u64,
    policy: &UrlPolicyConfig,
) -> Result<String> {
    let policy = inbound_policy(policy);

    // Pre-flight: full classifier (scheme + SSRF + DNS). Honor the returned
    // effective URL so any policy transforms (e.g. force-https) apply.
    let effective_url = match url_policy::classify(url, &policy).await {
        ClassifyOutcome::Allow { effective_url } => effective_url,
        other => {
            let msg = url_policy::describe_block_or_throttle(&other)
                .unwrap_or_else(|| "URL policy refused request".to_string());
            anyhow::bail!("{msg}");
        }
    };

    // Fast offline reject per redirect hop (metadata FQDNs, internal IP
    // literals). This is a cheap first line; the connect-time DNS guard below
    // is the authoritative check that also catches name-fronted internal hosts.
    let redirect_cfg = policy.clone();
    let redirect = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= REDIRECT_LIMIT {
            return attempt.error("too many redirects");
        }
        let next = attempt.url().to_string();
        match url_policy::classify_offline(&next, &redirect_cfg) {
            ClassifyOutcome::Allow { .. } => attempt.follow(),
            ClassifyOutcome::Block { reason, rule } => attempt.error(format!(
                "URL policy blocked redirect to {next}: {reason} (rule: {rule})"
            )),
            // Throttle carries no meaning on a redirect hop — let it follow.
            ClassifyOutcome::Throttle { .. } => attempt.follow(),
        }
    });

    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; Borg/0.1)")
        .timeout(Duration::from_millis(timeout_ms))
        .redirect(redirect)
        // Pin SSRF enforcement into the connector: every host (initial + each
        // redirect hop) is resolved through this guard, and a resolve to
        // private/internal/metadata space fails the connection. Inbound unfurl
        // never permits private targets, so `allow_private = false`.
        .dns_resolver(cortex::web::ssrf_guard_resolver(false))
        .build()?;

    let resp = client.get(&effective_url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {} fetching {}", resp.status(), effective_url);
    }

    // Post-fetch: re-validate the final landing host with full DNS so a
    // redirect that resolved to internal space never returns a body.
    let final_url = resp.url().to_string();
    if !matches!(
        url_policy::classify_no_throttle(&final_url, &policy).await,
        ClassifyOutcome::Allow { .. }
    ) {
        anyhow::bail!("URL policy blocked final redirect target: {final_url}");
    }

    // Only process text content types
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if !content_type.contains("text/")
        && !content_type.contains("application/json")
        && !content_type.contains("application/xml")
    {
        anyhow::bail!("Non-text content type: {content_type}");
    }

    // Cap response body to prevent memory exhaustion
    const MAX_BODY_BYTES: usize = 2 * 1024 * 1024; // 2 MB

    // Early rejection via Content-Length header (before downloading)
    if let Some(cl) = resp.content_length() {
        if cl > MAX_BODY_BYTES as u64 {
            anyhow::bail!("Response too large (Content-Length: {cl} bytes)");
        }
    }

    let bytes = resp.bytes().await?;
    if bytes.len() > MAX_BODY_BYTES {
        anyhow::bail!("Response too large ({} bytes)", bytes.len());
    }
    let body = String::from_utf8_lossy(&bytes).into_owned();

    let text = if body.contains("<html") || body.contains("<HTML") || body.contains("<!DOCTYPE") {
        html_to_plain_text(&body)
    } else {
        body
    };

    // Truncate to max_chars at a char boundary (avoid UTF-8 panic)
    if text.len() > max_chars {
        let mut end = max_chars;
        while !text.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        Ok(text[..end].to_string())
    } else {
        Ok(text)
    }
}

/// Basic HTML to plain text conversion.
fn html_to_plain_text(html: &str) -> String {
    use scraper::{Html, Selector};

    let document = Html::parse_document(html);
    // Try to get body content first, fall back to full document
    let root = Selector::parse("body")
        .ok()
        .and_then(|sel| {
            document
                .select(&sel)
                .next()
                .map(|el| el.text().collect::<Vec<_>>().join(" "))
        })
        .unwrap_or_else(|| document.root_element().text().collect::<Vec<_>>().join(" "));

    root.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Augment a message with fetched link content.
/// Returns the augmented text and a list of successfully fetched URLs.
pub async fn augment_with_links(
    text: &str,
    config: &LinkUnderstandingConfig,
    policy: &UrlPolicyConfig,
) -> (String, Vec<String>) {
    if !config.enabled {
        return (text.to_string(), Vec::new());
    }

    let urls = extract_urls(text, config.max_links, policy);
    if urls.is_empty() {
        return (text.to_string(), Vec::new());
    }

    let mut augmented = text.to_string();
    let mut fetched_urls = Vec::new();

    let futures: Vec<_> = urls
        .iter()
        .map(|url| fetch_link_content(url, config.max_chars_per_link, config.timeout_ms, policy))
        .collect();

    let results = futures::future::join_all(futures).await;

    for (url, result) in urls.iter().zip(results.into_iter()) {
        match result {
            Ok(content) if !content.trim().is_empty() => {
                augmented.push_str(&format!(
                    "\n\n<untrusted_web_content source=\"{url}\">\n{content}\n</untrusted_web_content>"
                ));
                fetched_urls.push(url.clone());
                debug!(
                    "Link understanding: fetched {} chars from {url}",
                    content.len()
                );
            }
            Ok(_) => {
                debug!("Link understanding: empty content from {url}");
            }
            Err(e) => {
                debug!("Link understanding: failed to fetch {url}: {e}");
            }
        }
    }

    (augmented, fetched_urls)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default policy has `enabled: true`, `allow_private_urls: false` — the
    /// production posture, so the SSRF gate is actually exercised.
    fn policy() -> UrlPolicyConfig {
        UrlPolicyConfig::default()
    }

    #[test]
    fn extract_urls_basic() {
        let text = "Check out https://example.com and https://foo.bar/page";
        let urls = extract_urls(text, 10, &policy());
        assert_eq!(urls, vec!["https://example.com", "https://foo.bar/page"]);
    }

    #[test]
    fn extract_urls_deduplicates() {
        let text = "Visit https://example.com and also https://example.com again";
        let urls = extract_urls(text, 10, &policy());
        assert_eq!(urls.len(), 1);
    }

    #[test]
    fn extract_urls_respects_max_links() {
        let text = "https://a.com https://b.com https://c.com https://d.com";
        let urls = extract_urls(text, 2, &policy());
        assert_eq!(urls.len(), 2);
    }

    #[test]
    fn extract_urls_strips_trailing_punctuation() {
        let text = "See https://example.com. And https://foo.com, too";
        let urls = extract_urls(text, 10, &policy());
        assert_eq!(urls, vec!["https://example.com", "https://foo.com"]);
    }

    #[test]
    fn extract_urls_rejects_localhost() {
        let text = "http://localhost:8080/admin and https://example.com";
        let urls = extract_urls(text, 10, &policy());
        assert_eq!(urls, vec!["https://example.com"]);
    }

    #[test]
    fn extract_urls_rejects_private_ips() {
        let text = "http://192.168.1.1/admin http://10.0.0.1/secret https://example.com";
        let urls = extract_urls(text, 10, &policy());
        assert_eq!(urls, vec!["https://example.com"]);
    }

    #[test]
    fn extract_urls_rejects_loopback() {
        let text = "http://127.0.0.1:3000 http://0.0.0.0:8080 https://real.com";
        let urls = extract_urls(text, 10, &policy());
        assert_eq!(urls, vec!["https://real.com"]);
    }

    #[test]
    fn extract_urls_rejects_cloud_metadata() {
        // The upgrade over the old literal-host guard: cloud-metadata FQDNs,
        // the IMDS link-local IP, and CGNAT space are all refused now.
        let text = "http://metadata.google.internal/ \
             http://169.254.169.254/latest/meta-data/ \
             http://100.100.100.200/ https://example.com";
        let urls = extract_urls(text, 10, &policy());
        assert_eq!(urls, vec!["https://example.com"]);
    }

    #[test]
    fn extract_urls_empty_text() {
        let urls = extract_urls("no urls here", 10, &policy());
        assert!(urls.is_empty());
    }

    #[test]
    fn extract_urls_in_markdown() {
        let text = "Check [this link](https://example.com) and https://other.com";
        let urls = extract_urls(text, 10, &policy());
        assert_eq!(urls.len(), 2);
        assert!(urls.contains(&"https://example.com".to_string()));
        assert!(urls.contains(&"https://other.com".to_string()));
    }

    #[test]
    fn extract_urls_rejects_bracketed_ipv6_loopback() {
        // Regression: `[::1]` is the IPv6 loopback authority; the bracketed
        // host must not survive the extraction pre-filter (normalize_host now
        // strips the brackets so the literal parses and the private-IP gate
        // fires).
        let text = "http://[::1]:8080/admin http://[::1]/ https://example.com";
        let urls = extract_urls(text, 10, &policy());
        assert_eq!(urls, vec!["https://example.com"]);
    }

    #[test]
    fn inbound_unfurl_ignores_allow_private_urls() {
        // Regression: inbound link-unfurl handles attacker-controlled URLs and
        // must block private/loopback space even when the operator enabled
        // `allow_private_urls` for the first-party web_fetch tool.
        let permissive = UrlPolicyConfig {
            allow_private_urls: true,
            ..UrlPolicyConfig::default()
        };
        let text = "http://192.168.1.1/secret http://127.0.0.1/admin https://example.com";
        let urls = extract_urls(text, 10, &permissive);
        assert_eq!(urls, vec!["https://example.com"]);
    }

    #[test]
    fn is_admissible_checks() {
        let p = policy();
        // Loopback names + literals, private, link-local, CGNAT, metadata all refused.
        assert!(!is_admissible("http://localhost:8080", &p));
        assert!(!is_admissible("http://127.0.0.1", &p));
        assert!(!is_admissible("http://192.168.1.1", &p));
        assert!(!is_admissible("http://10.0.0.1", &p));
        assert!(!is_admissible("http://0.0.0.0", &p));
        assert!(!is_admissible("http://169.254.169.254/", &p));
        assert!(!is_admissible("http://metadata.google.internal/", &p));
        // Non-http(s) schemes rejected by the classifier.
        assert!(!is_admissible("ftp://example.com", &p));
        // Public hosts admitted.
        assert!(is_admissible("https://example.com", &p));
        assert!(is_admissible("https://8.8.8.8", &p));
    }

    #[test]
    fn html_to_plain_text_basic() {
        let html = "<html><body><h1>Hello</h1><p>World</p></body></html>";
        let text = html_to_plain_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
    }

    #[test]
    fn html_to_plain_text_collapses_whitespace() {
        let html = "<html><body>  lots   of   spaces  </body></html>";
        let text = html_to_plain_text(html);
        assert_eq!(text, "lots of spaces");
    }

    #[tokio::test]
    async fn augment_disabled_is_noop() {
        let config = LinkUnderstandingConfig::default(); // enabled: false
        let (result, urls) = augment_with_links("https://example.com", &config, &policy()).await;
        assert_eq!(result, "https://example.com");
        assert!(urls.is_empty());
    }

    #[tokio::test]
    async fn augment_no_urls_is_noop() {
        let config = LinkUnderstandingConfig {
            enabled: true,
            ..Default::default()
        };
        let (result, urls) = augment_with_links("no links here", &config, &policy()).await;
        assert_eq!(result, "no links here");
        assert!(urls.is_empty());
    }
}
