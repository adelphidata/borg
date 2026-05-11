//! Pre-flight URL transforms: rewrite the URL before it is fetched.
//!
//! Each rule matches on the original host; matching rules are applied in
//! declared order, so users can stack `ForceHttps` + `StripQueryParams`
//! on the same host predictably. After every step the URL is re-parsed so
//! a `RewriteHost` followed by a `StripFragment` sees the new host.

use crate::config::{TransformAction, TransformRule};

use super::rules::{matches_rule, normalize_host, normalize_rule};

/// Apply every matching transform in `rules` to `url` and return the
/// resulting URL. Returns `Err` if a transform produces a malformed URL
/// (e.g. `RewriteHost { to: "" }`).
pub fn apply(url: &url::Url, rules: &[TransformRule]) -> Result<url::Url, String> {
    let mut current = url.clone();
    for rule in rules {
        let pattern = match normalize_rule(&rule.pattern) {
            Some(p) => p,
            None => continue,
        };
        let host = match current.host_str() {
            Some(h) => normalize_host(h),
            None => continue,
        };
        if !matches_rule(&host, &pattern) {
            continue;
        }
        current = apply_action(&current, &rule.action)?;
    }
    Ok(current)
}

fn apply_action(url: &url::Url, action: &TransformAction) -> Result<url::Url, String> {
    match action {
        TransformAction::StripQueryParams { names } => {
            let kept: Vec<(String, String)> = url
                .query_pairs()
                .filter(|(k, _)| !names.iter().any(|n| n == k.as_ref()))
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            let mut next = url.clone();
            if kept.is_empty() {
                next.set_query(None);
            } else {
                let mut q = next.query_pairs_mut();
                q.clear();
                for (k, v) in &kept {
                    q.append_pair(k, v);
                }
                drop(q);
            }
            Ok(next)
        }
        TransformAction::RewriteHost { to } => {
            if to.trim().is_empty() {
                return Err("RewriteHost.to is empty".into());
            }
            let mut next = url.clone();
            next.set_host(Some(to))
                .map_err(|e| format!("RewriteHost set_host failed: {e}"))?;
            Ok(next)
        }
        TransformAction::ForceHttps => {
            if url.scheme() == "https" {
                return Ok(url.clone());
            }
            let mut next = url.clone();
            // `Url::set_scheme` returns Err(()) when changing between
            // special and non-special schemes; http<->https is allowed.
            next.set_scheme("https")
                .map_err(|_| "ForceHttps: set_scheme rejected by url crate".to_string())?;
            // If the original had an explicit `:80`, drop it after the
            // upgrade so we don't end up talking https on port 80.
            // `set_port(None)` on a parsed http/https URL is infallible
            // — only fails for cannot-be-a-base URLs, which we already
            // exclude via the scheme gate.
            if next.port() == Some(80) {
                let _ = next.set_port(None);
            }
            Ok(next)
        }
        TransformAction::StripFragment => {
            let mut next = url.clone();
            next.set_fragment(None);
            Ok(next)
        }
    }
}
