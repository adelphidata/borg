//! URL-safe plain text wrapping.
//!
//! When the input contains a URL-like token, wrapping switches to
//! `WordSeparator::AsciiSpace` + `WordSplitter::NoHyphenation` +
//! `break_words: false`, which keeps URLs intact on a single line.
//! Other prose falls back to default `textwrap` behavior. Scoped to plain
//! `&str` wrap — line/span-slicing is intentionally out of scope.

use std::borrow::Cow;

/// Wrap `text` to `width` columns. If `text` contains a URL-like token,
/// use URL-preserving settings so the URL stays on one line; otherwise
/// use default textwrap behavior.
pub fn wrap(text: &str, width: usize) -> Vec<Cow<'_, str>> {
    if contains_url_like(text) {
        let opts = textwrap::Options::new(width)
            .word_separator(textwrap::WordSeparator::AsciiSpace)
            .word_splitter(textwrap::WordSplitter::NoHyphenation)
            .break_words(false);
        textwrap::wrap(text, opts)
    } else {
        textwrap::wrap(text, width)
    }
}

/// True if any whitespace-delimited token looks like a URL.
///
/// Conservative heuristic: scheme-based (`https://…`, `ftp://…`, custom
/// `myapp://…`), `www.example.com[/…]`, bare domain + path
/// (`example.com/path`), `localhost:PORT[/…]`, IPv4:port+path. File paths
/// like `src/main.rs` are rejected.
pub fn contains_url_like(text: &str) -> bool {
    text.split_ascii_whitespace().any(is_url_like_token)
}

fn is_url_like_token(raw: &str) -> bool {
    let token = raw.trim_matches(|c: char| {
        matches!(
            c,
            '(' | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | ','
                | '.'
                | ';'
                | ':'
                | '!'
                | '\''
                | '"'
        )
    });
    if token.is_empty() {
        return false;
    }
    is_absolute_url_like(token) || is_bare_url_like(token)
}

fn is_absolute_url_like(token: &str) -> bool {
    let Some((scheme, rest)) = token.split_once("://") else {
        return false;
    };
    if scheme.is_empty() || rest.is_empty() {
        return false;
    }
    let mut chars = scheme.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
}

fn is_bare_url_like(token: &str) -> bool {
    let (host_port, has_trailer) = match token.find(['/', '?', '#']) {
        Some(idx) => (&token[..idx], true),
        None => (token, false),
    };
    if host_port.is_empty() {
        return false;
    }
    let lower = host_port.to_ascii_lowercase();
    if !has_trailer && !lower.starts_with("www.") {
        return false;
    }
    let (host, port) = split_host_and_port(host_port);
    if host.is_empty() {
        return false;
    }
    if let Some(p) = port {
        if p.is_empty() || p.len() > 5 || !p.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        if p.parse::<u16>().is_err() {
            return false;
        }
    }
    host.eq_ignore_ascii_case("localhost") || is_ipv4(host) || is_domain_name(host)
}

fn split_host_and_port(host_port: &str) -> (&str, Option<&str>) {
    if host_port.starts_with('[') {
        return (host_port, None);
    }
    if let Some((h, p)) = host_port.rsplit_once(':') {
        if !h.is_empty() && !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) {
            return (h, Some(p));
        }
    }
    (host_port, None)
}

fn is_ipv4(host: &str) -> bool {
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.parse::<u8>().is_ok())
}

fn is_domain_name(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if !host.contains('.') {
        return false;
    }
    let mut labels = host.split('.');
    let Some(tld) = labels.next_back() else {
        return false;
    };
    if !((2..=63).contains(&tld.len()) && tld.chars().all(|c| c.is_ascii_alphabetic())) {
        return false;
    }
    labels.all(is_domain_label)
}

fn is_domain_label(label: &str) -> bool {
    if label.is_empty() || label.len() > 63 {
        return false;
    }
    let Some(first) = label.chars().next() else {
        return false;
    };
    let Some(last) = label.chars().next_back() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && last.is_ascii_alphanumeric()
        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_url_like_tokens() {
        let positives = [
            "https://example.com/a/b",
            "ftp://host/path",
            "www.example.com/path?x=1",
            "example.test/path#frag",
            "localhost:3000/api",
            "127.0.0.1:8080/health",
            "(https://example.com/wrapped-in-parens)",
            "see https://foo.bar/x for details",
        ];
        for t in positives {
            assert!(contains_url_like(t), "expected URL in {t:?}");
        }
    }

    #[test]
    fn rejects_non_urls() {
        let negatives = [
            "src/main.rs",
            "foo/bar",
            "key:value",
            "just-some-text-with-dashes",
            "hello.world", // no path/query/fragment and no www
        ];
        for t in negatives {
            assert!(!contains_url_like(t), "did not expect URL in {t:?}");
        }
    }

    #[test]
    fn url_stays_intact_when_wrapping_narrow() {
        let s =
            "see https://example.com/a-very-long-path-with-many-segments-and-query?x=1 for more";
        let wrapped = wrap(s, 24);
        assert!(
            wrapped.iter().any(|l| l
                .contains("https://example.com/a-very-long-path-with-many-segments-and-query?x=1")),
            "expected URL kept intact on one line: {wrapped:?}"
        );
    }

    #[test]
    fn non_url_text_still_wraps_normally() {
        let s = "alpha beta gamma delta epsilon zeta eta theta";
        let wrapped = wrap(s, 10);
        assert!(wrapped.len() > 1);
    }
}
