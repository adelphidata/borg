use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use regex::Regex;
use tracing::debug;

use cortex::config::LinkUnderstandingConfig;

/// Extract URLs from text, deduplicate, and cap at `max_links`.
pub fn extract_urls(text: &str, max_links: usize) -> Vec<String> {
    static URL_RE: OnceLock<Option<Regex>> = OnceLock::new();
    let Some(re) = URL_RE.get_or_init(|| Regex::new(r#"https?://[^\s<>\)\]"']+"#).ok()) else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    let mut urls = Vec::new();

    for m in re.find_iter(text) {
        let url = m.as_str().trim_end_matches(['.', ',']);
        if seen.insert(url.to_string()) && !is_private_url(url) {
            urls.push(url.to_string());
            if urls.len() >= max_links {
                break;
            }
        }
    }
    urls
}

/// Reject URLs that point to localhost or private IP ranges (SSRF protection).
fn is_private_url(url: &str) -> bool {
    if let Ok(parsed) = url::Url::parse(url) {
        if let Some(host) = parsed.host_str() {
            // Check for localhost variants
            if host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host == "0.0.0.0" {
                return true;
            }
            // Check for private IP addresses
            if let Ok(ip) = host.parse::<IpAddr>() {
                return match ip {
                    IpAddr::V4(v4) => {
                        v4.is_loopback()
                            || v4.is_private()
                            || v4.is_link_local()
                            || v4.is_unspecified()
                    }
                    IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
                };
            }
        }
    }
    false
}

/// Fetch a URL and extract its text content, truncated to `max_chars`.
pub async fn fetch_link_content(url: &str, max_chars: usize, timeout_ms: u64) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; Borg/0.1)")
        .timeout(Duration::from_millis(timeout_ms))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()?;

    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {} fetching {}", resp.status(), url);
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
) -> (String, Vec<String>) {
    if !config.enabled {
        return (text.to_string(), Vec::new());
    }

    let urls = extract_urls(text, config.max_links);
    if urls.is_empty() {
        return (text.to_string(), Vec::new());
    }

    let mut augmented = text.to_string();
    let mut fetched_urls = Vec::new();

    let futures: Vec<_> = urls
        .iter()
        .map(|url| fetch_link_content(url, config.max_chars_per_link, config.timeout_ms))
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

    #[test]
    fn extract_urls_basic() {
        let text = "Check out https://example.com and https://foo.bar/page";
        let urls = extract_urls(text, 10);
        assert_eq!(urls, vec!["https://example.com", "https://foo.bar/page"]);
    }

    #[test]
    fn extract_urls_deduplicates() {
        let text = "Visit https://example.com and also https://example.com again";
        let urls = extract_urls(text, 10);
        assert_eq!(urls.len(), 1);
    }

    #[test]
    fn extract_urls_respects_max_links() {
        let text = "https://a.com https://b.com https://c.com https://d.com";
        let urls = extract_urls(text, 2);
        assert_eq!(urls.len(), 2);
    }

    #[test]
    fn extract_urls_strips_trailing_punctuation() {
        let text = "See https://example.com. And https://foo.com, too";
        let urls = extract_urls(text, 10);
        assert_eq!(urls, vec!["https://example.com", "https://foo.com"]);
    }

    #[test]
    fn extract_urls_rejects_localhost() {
        let text = "http://localhost:8080/admin and https://example.com";
        let urls = extract_urls(text, 10);
        assert_eq!(urls, vec!["https://example.com"]);
    }

    #[test]
    fn extract_urls_rejects_private_ips() {
        let text = "http://192.168.1.1/admin http://10.0.0.1/secret https://example.com";
        let urls = extract_urls(text, 10);
        assert_eq!(urls, vec!["https://example.com"]);
    }

    #[test]
    fn extract_urls_rejects_loopback() {
        let text = "http://127.0.0.1:3000 http://0.0.0.0:8080 https://real.com";
        let urls = extract_urls(text, 10);
        assert_eq!(urls, vec!["https://real.com"]);
    }

    #[test]
    fn extract_urls_empty_text() {
        let urls = extract_urls("no urls here", 10);
        assert!(urls.is_empty());
    }

    #[test]
    fn extract_urls_in_markdown() {
        let text = "Check [this link](https://example.com) and https://other.com";
        let urls = extract_urls(text, 10);
        assert_eq!(urls.len(), 2);
        assert!(urls.contains(&"https://example.com".to_string()));
        assert!(urls.contains(&"https://other.com".to_string()));
    }

    #[test]
    fn is_private_url_checks() {
        assert!(is_private_url("http://localhost:8080"));
        assert!(is_private_url("http://127.0.0.1"));
        assert!(is_private_url("http://192.168.1.1"));
        assert!(is_private_url("http://10.0.0.1"));
        assert!(is_private_url("http://0.0.0.0"));
        assert!(!is_private_url("https://example.com"));
        assert!(!is_private_url("https://8.8.8.8"));
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
        let (result, urls) = augment_with_links("https://example.com", &config).await;
        assert_eq!(result, "https://example.com");
        assert!(urls.is_empty());
    }

    #[tokio::test]
    async fn augment_no_urls_is_noop() {
        let config = LinkUnderstandingConfig {
            enabled: true,
            ..Default::default()
        };
        let (result, urls) = augment_with_links("no links here", &config).await;
        assert_eq!(result, "no links here");
        assert!(urls.is_empty());
    }
}
