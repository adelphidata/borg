//! Finding the Meet, Zoom or Teams link in a calendar event. Links hide in
//! the conference data, the location or the description, often HTML-escaped,
//! so each field is checked in the order the caller trusts it.

use std::sync::LazyLock;

use regex::Regex;

use super::JoinProvider;

static PATTERNS: LazyLock<Vec<(JoinProvider, Regex)>> = LazyLock::new(|| {
    [
        (
            JoinProvider::Meet,
            r"https://meet\.google\.com/[a-z]{3}-[a-z]{4}-[a-z]{3}\b",
        ),
        (
            JoinProvider::Zoom,
            r"https://(?:[a-z0-9-]+\.)*zoom\.us/(?:j|my|w)/[A-Za-z0-9.]+(?:\?pwd=[A-Za-z0-9._-]+)?",
        ),
        (
            JoinProvider::Teams,
            r#"https://teams\.(?:microsoft|live)\.com/(?:l/meetup-join|meet)/[^\s<>"')\]]+"#,
        ),
    ]
    .into_iter()
    .filter_map(|(p, re)| match Regex::new(re) {
        Ok(r) => Some((p, r)),
        Err(e) => {
            tracing::error!("join-link pattern failed to compile: {e}");
            None
        }
    })
    .collect()
});

/// The first join link in `fields`, checked in order, with its provider.
pub fn extract(fields: &[&str]) -> Option<(String, JoinProvider)> {
    for field in fields {
        if field.is_empty() {
            continue;
        }
        let text = field.replace("&amp;", "&");
        let found = PATTERNS
            .iter()
            .filter_map(|(provider, re)| re.find(&text).map(|m| (m.start(), m.as_str(), *provider)))
            .min_by_key(|(start, _, _)| *start);
        if let Some((_, url, provider)) = found {
            return Some((url.to_string(), provider));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_each_service_and_ignores_lookalikes() {
        let cases: &[(&str, Option<(&str, JoinProvider)>)] = &[
            (
                "Join: https://meet.google.com/abc-defg-hij please",
                Some(("https://meet.google.com/abc-defg-hij", JoinProvider::Meet)),
            ),
            (
                "https://adelphi.zoom.us/j/81234567890?pwd=AbC.123 (passcode 1)",
                Some(("https://adelphi.zoom.us/j/81234567890?pwd=AbC.123", JoinProvider::Zoom)),
            ),
            (
                "<a href=\"https://teams.microsoft.com/l/meetup-join/19%3ameeting_x%40thread.v2/0?context=%7b%7d\">Join</a>",
                Some((
                    "https://teams.microsoft.com/l/meetup-join/19%3ameeting_x%40thread.v2/0?context=%7b%7d",
                    JoinProvider::Teams,
                )),
            ),
            ("https://notzoom.us/j/123456789", None),
            ("https://meet.google.com.evil.example/abc-defg-hij", None),
            ("no link here", None),
        ];
        for (text, want) in cases {
            let got = extract(&[text]);
            assert_eq!(got.as_ref().map(|(u, p)| (u.as_str(), *p)), *want, "{text}");
        }
    }

    #[test]
    fn html_escaped_ampersands_are_restored() {
        let (url, _) =
            extract(&["https://teams.microsoft.com/l/meetup-join/abc?a=1&amp;b=2"]).unwrap();
        assert_eq!(url, "https://teams.microsoft.com/l/meetup-join/abc?a=1&b=2");
    }

    #[test]
    fn earlier_fields_win_over_later_ones() {
        let (url, provider) = extract(&[
            "",
            "https://zoom.us/j/123456789",
            "https://meet.google.com/abc-defg-hij",
        ])
        .unwrap();
        assert_eq!(provider, JoinProvider::Zoom);
        assert_eq!(url, "https://zoom.us/j/123456789");
    }
}
