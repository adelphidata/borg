//! Domain rule matching shared by allowlist, denylist, throttle, and
//! transforms. Rule syntax:
//!
//! * `example.com` — matches `example.com` and any subdomain
//!   (`a.example.com`, `a.b.example.com`).
//! * `*.example.com` — matches **strict** subdomains only. `example.com`
//!   itself does NOT match; `a.example.com` does.
//! * URL-shaped rules (`https://example.com/path`) are accepted; only the
//!   host portion is used.
//!
//! Matching is case-insensitive and trailing dots are stripped on both
//! sides (`example.com.` and `example.com` are equivalent).

/// Normalize a host for comparison: lowercase, strip trailing dots. We
/// deliberately do **not** strip a leading `www.` — `www.example.com` and
/// `example.com` are different hosts at the DNS level, and the
/// "bare rule matches all subdomains" semantic already covers the common
/// case (`example.com` rule matches `www.example.com` host).
pub fn normalize_host(host: &str) -> String {
    let mut h = host.trim().to_ascii_lowercase();
    while h.ends_with('.') {
        h.pop();
    }
    h
}

/// Normalize a rule to its host portion. `https://x.com/path` → `x.com`.
/// Returns `None` for empty/comment lines so callers can iterate user
/// input safely.
pub fn normalize_rule(rule: &str) -> Option<String> {
    let raw = rule.trim();
    if raw.is_empty() || raw.starts_with('#') {
        return None;
    }
    // Strip scheme if present.
    let after_scheme = match raw.find("://") {
        Some(i) => &raw[i + 3..],
        None => raw,
    };
    // Strip path/query/fragment — keep only the netloc segment.
    let netloc = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    if netloc.is_empty() {
        return None;
    }
    let mut value = netloc.to_ascii_lowercase();
    while value.ends_with('.') {
        value.pop();
    }
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// True if `host` matches `rule`. Both are expected pre-normalized via the
/// functions above; this function does the actual pattern logic.
pub fn matches_rule(host: &str, rule: &str) -> bool {
    if host.is_empty() || rule.is_empty() {
        return false;
    }
    if let Some(suffix) = rule.strip_prefix("*.") {
        // Wildcard: strict subdomain only.
        return host != suffix && host.ends_with(&format!(".{suffix}"));
    }
    // Bare rule: exact OR subdomain.
    host == rule || host.ends_with(&format!(".{rule}"))
}

/// True if `host` matches any rule in `rules` (after normalization).
pub fn any_match(host: &str, rules: &[String]) -> bool {
    first_match(host, rules).is_some()
}

/// Returns the first rule (post-normalization) that matches `host`, or
/// `None`. The returned string is the normalized form, suitable for use
/// as a rule slug in error messages.
pub fn first_match(host: &str, rules: &[String]) -> Option<String> {
    for raw in rules {
        if let Some(rule) = normalize_rule(raw) {
            if matches_rule(host, &rule) {
                return Some(rule);
            }
        }
    }
    None
}
