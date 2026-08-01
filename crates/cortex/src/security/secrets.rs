use regex::Regex;
use std::sync::LazyLock;

struct SecretPattern {
    regex: Regex,
    label: &'static str,
    /// If true, only the first capture group is replaced (preserves leading context).
    capture_only: bool,
    /// Post-match validator over the full match text; a `false` verdict keeps
    /// the match unredacted. Used where the false-positive boundary can't be
    /// expressed in the pattern itself (the regex crate has no lookarounds).
    validator: Option<fn(&str) -> bool>,
}

/// Credential keywords a key may end with, including real-world concatenated
/// compounds ("authtoken" ngrok, "secretkey" minio) that would otherwise fail
/// the word-boundary check below.
const CREDENTIAL_KEY_SUFFIXES: &[&str] = &[
    "password",
    "passphrase",
    "secret",
    "token",
    "api_key",
    "apikey",
    "apitoken",
    "authtoken",
    "authkey",
    "accesstoken",
    "secretkey",
    "key",
];

/// True at a word-start boundary inside an identifier: the key's edge, after
/// a non-letter (`_ - . 3`), or at a camelCase/acronym juncture
/// (`clientSecret`, `APIKey`).
fn is_word_start(s: &str, i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let b = s.as_bytes();
    let prev = b[i - 1] as char;
    let cur = b[i] as char;
    if !prev.is_ascii_alphabetic() {
        return true;
    }
    if cur.is_ascii_uppercase() && prev.is_ascii_lowercase() {
        return true; // camelCase: clientSecret
    }
    // Acronym run ending: APIKey — the 'K' begins a word when the preceding
    // run is uppercase and the following char is lowercase.
    if cur.is_ascii_uppercase()
        && prev.is_ascii_uppercase()
        && b.get(i + 1)
            .is_some_and(|n| (*n as char).is_ascii_lowercase())
    {
        return true;
    }
    false
}

/// Validate a Credential Assignment match: the key (text before `=`) must
/// carry a credential keyword at a word boundary, not merely embed one.
///
/// The pattern's key class allows arbitrary alphanumeric prefixes so real
/// keys like `client_secret` and `spring.datasource.password` match — the
/// side effect is that ordinary words that merely *end* in a keyword
/// (`turkey=`, `mypassword=` in prose-shaped text) get value-masked,
/// corrupting legitimate content on every sink that runs redaction.
///
/// ALL-CAPS keys keep the legacy embedded matching (`MYTOKEN=…`, `MONKEY=…`
/// included) — an all-caps name is env-var-shaped and almost never prose, so
/// over-masking there is the safer failure.
fn credential_assignment_key_is_real(full_match: &str) -> bool {
    let Some(eq) = full_match.find('=') else {
        return true; // defensive: pattern guarantees a '='
    };
    let key = full_match[..eq]
        .trim_start_matches([' ', '\t', '\n', ';'])
        .trim_start_matches("export ")
        .trim();
    let letters: Vec<char> = key.chars().filter(char::is_ascii_alphabetic).collect();
    if !letters.is_empty() && letters.iter().all(char::is_ascii_uppercase) {
        return true;
    }
    let lower = key.to_ascii_lowercase();
    CREDENTIAL_KEY_SUFFIXES
        .iter()
        .any(|kw| lower.ends_with(kw) && is_word_start(key, key.len() - kw.len()))
}

static SECRET_PATTERNS: LazyLock<Vec<SecretPattern>> = LazyLock::new(|| {
    type Validator = Option<fn(&str) -> bool>;
    let patterns: Vec<(&str, &str, bool, Validator)> = vec![
        // AWS key IDs share a 4-letter prefix + 16 base32 chars. Beyond the
        // long-lived `AKIA` access key, cover temporary/STS creds (`ASIA`) and
        // the unique-ID prefixes for users/roles/groups (`AIDA`/`AROA`/`AGPA`)
        // so a leaked temporary credential is masked too.
        (
            r"(?:AKIA|ASIA|AIDA|AROA|AGPA)[0-9A-Z]{16}",
            "AWS Access Key",
            false,
            None,
        ),
        (
            r"(?:ghp|ghs|gho|ghu|ghm)_[A-Za-z0-9_]{36,}",
            "GitHub Token",
            false,
            None,
        ),
        // GitHub fine-grained personal access tokens (`github_pat_...`) — a
        // distinct shape from the classic `ghp_` prefix above.
        (r"github_pat_[0-9A-Za-z_]{20,}", "GitHub Token", false, None),
        // Google API keys are `AIza` + 35 url-safe base64 chars. The prefix is
        // distinctive enough that a 30+ char lower bound won't false-positive.
        (r"AIza[0-9A-Za-z_-]{30,}", "Google API Key", false, None),
        // Stripe live secret/restricted keys (`sk_live_`/`rk_live_`). Test-mode
        // keys are deliberately left alone.
        (r"[sr]k_live_[0-9A-Za-z]{16,}", "Stripe Key", false, None),
        (
            r"sk[-_]ant[-_][A-Za-z0-9_-]{32,}",
            "Anthropic API Key",
            false,
            None,
        ),
        (
            r"sk[-_]proj[-_][A-Za-z0-9_-]{32,}",
            "OpenAI Project Key",
            false,
            None,
        ),
        (r"sk-[A-Za-z0-9]{48,}", "API Key", false, None),
        (
            r"eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
            "JWT",
            false,
            None,
        ),
        (
            r"-----BEGIN (?:RSA |EC |DSA )?PRIVATE KEY-----",
            "Private Key",
            false,
            None,
        ),
        (r"xox[bpsar]-[A-Za-z0-9-]{24,}", "Slack Token", false, None),
        // Slack app-level tokens (`xapp-1-...`) are distinct from the `xox*`
        // bot/user tokens above.
        (r"xapp-[0-9]-[A-Za-z0-9-]{10,}", "Slack Token", false, None),
        // `(?i)` so `Password:`/`PASSWORD:` are caught, not just lowercase —
        // the case asymmetry previously let a capitalized doc line leak while
        // masking the same text lowercased. `\b` keeps the keyword from
        // matching mid-word (`wordpassword…`); underscore/dotted keys like
        // `db_password=` are the Credential Assignment pattern's job.
        (
            r#"(?i)\b(?:password|passwd|pwd|passphrase)\s*[=:]\s*"[^"]{8,}""#,
            "Quoted Password",
            false,
            None,
        ),
        (
            r"(?i)\b(?:password|passwd|pwd|passphrase)\s*[=:]\s*(\S{8,64})",
            "Password Assignment",
            true,
            None,
        ),
        (
            r"(?:postgres(?:ql)?|mysql|mongodb(?:\+srv)?|redis|amqp|mssql)://[^\s]{8,}",
            "Database Connection String",
            false,
            None,
        ),
        // Bare-token credential in a URL's userinfo, e.g.
        // `git remote set-url origin https://ghp_xxx@github.com/org/repo`. The
        // `user:pass@` form (with a colon) is left to the connection-string /
        // generic patterns; this targets the single-token form a colon-based
        // pattern misses. Requiring 20+ userinfo chars avoids redacting an
        // ordinary `https://user@host` username. The whole URL is masked
        // (not just the token) because the secret sits mid-match — preserving
        // the host would require a lookahead the regex crate lacks.
        (
            r"(?:https?|ssh|git\+https?)://[A-Za-z0-9._~%+-]{20,}@\S+",
            "URL Credential",
            false,
            None,
        ),
        // HTTP auth headers. capture_only=true masks only the credential value and
        // keeps the header name + scheme word visible for debuggability, e.g.
        // `Authorization: Bearer [REDACTED Authorization Header]`. Covers the common
        // schemes (Bearer/Basic/Digest/token); opaque no-scheme values are left alone
        // to avoid clobbering the scheme word on a second pass.
        (
            r"(?i)authorization\s*:\s*(?:bearer|basic|digest|token)\s+(\S{4,})",
            "Authorization Header",
            true,
            None,
        ),
        // Opaque API-key headers (`x-api-key:`, `api-key:`, `apikey:`) carry the secret
        // directly after the colon.
        (
            r"(?i)(?:x-api-key|api[-_]?key)\s*:\s*(\S{4,})",
            "API Key Header",
            true,
            None,
        ),
        // Generic assignment patterns — run last so specific patterns above take priority.
        // capture_only=true: only the captured value (group 1) is replaced, preserving the
        // variable name prefix so the surrounding text remains readable.
        // The replace closure skips values already starting with "[REDACTED" to avoid
        // double-redacting values already handled by an earlier specific pattern
        // (Rust's regex crate does not support lookahead assertions).
        //
        // The key prefix is `[\w.]*` (not `\w*`) so dotted/namespaced config keys
        // — `spring.datasource.password=…`, `app.config.api_key=…`, Java
        // `.properties` files — are covered, not just bare `API_KEY=…`. Dots are
        // a superset of the prior word-char prefix, so no existing match is lost.
        // `PASSPHRASE` is included so the master-KEK env line
        // (`BORG_KEK_PASSPHRASE=...`, written to ~/.borg/secrets/kek.env) is
        // masked on every sink. It matches neither `KEY` (that's "KEK") nor the
        // `password|passwd|pwd` value patterns above, so without it the KEK
        // passphrase leaked through redaction entirely.
        // The post-match validator rejects keys that merely EMBED a keyword
        // (`turkey=`, `mypassword=` prose shapes) — the keyword must sit at a
        // word boundary in the key. See `credential_assignment_key_is_real`.
        (
            r"(?i)(?:^|[\s;])(?:export\s+)?(?:[\w.]*(?:PASSWORD|PASSPHRASE|SECRET|TOKEN|KEY|APIKEY|API_KEY))\s*=\s*(\S{8,})",
            "Credential Assignment",
            true,
            Some(credential_assignment_key_is_real as fn(&str) -> bool),
        ),
    ];

    patterns
        .into_iter()
        .filter_map(
            |(pattern, label, capture_only, validator)| match Regex::new(pattern) {
                Ok(regex) => Some(SecretPattern {
                    regex,
                    label,
                    capture_only,
                    validator,
                }),
                Err(e) => {
                    tracing::error!("Failed to compile secret pattern '{label}': {e} — skipping");
                    None
                }
            },
        )
        .collect()
});

/// One-pass prefilter over the full pattern list. Redaction runs on every
/// TUI transcript insert, log line, and outbound channel message; without
/// the prefilter each call walked all patterns sequentially, producing a
/// fresh full copy of the text per pattern. The set is built from the SAME
/// surviving `SECRET_PATTERNS` entries (via `Regex::as_str`), so set index
/// == pattern index by construction and the two cannot drift. `None` (set
/// compile failure — unreachable when every member compiled individually)
/// degrades to running every pattern, never to skipping one.
static SECRET_PATTERN_SET: LazyLock<Option<regex::RegexSet>> = LazyLock::new(|| {
    match regex::RegexSet::new(SECRET_PATTERNS.iter().map(|p| p.regex.as_str())) {
        Ok(set) => Some(set),
        Err(e) => {
            tracing::error!("Failed to compile secret pattern set: {e} — prefilter disabled");
            None
        }
    }
});

/// Indices of patterns that match `text`, in evaluation order.
fn matching_pattern_indices(text: &str) -> Vec<usize> {
    match SECRET_PATTERN_SET.as_ref() {
        Some(set) => set.matches(text).into_iter().collect(),
        None => (0..SECRET_PATTERNS.len()).collect(),
    }
}

/// A single per-pattern audit record produced by [`redact_with_audit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionHit {
    pub label: &'static str,
    pub count: usize,
}

/// Result of an audited redaction pass: the rewritten text plus a per-pattern
/// hit summary. `hits` is empty when nothing matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionOutcome {
    pub text: String,
    pub hits: Vec<RedactionHit>,
}

impl RedactionOutcome {
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }
}

/// Redact detected secrets in text, replacing matches with `[REDACTED <label>]`.
pub fn redact_secrets(text: &str) -> String {
    redact_with_audit(text).text
}

/// Redact detected secrets and report per-pattern match counts. The hit list
/// preserves pattern evaluation order so callers writing audit rows get a
/// deterministic ordering.
pub fn redact_with_audit(text: &str) -> RedactionOutcome {
    // Prefilter against the original text: only patterns the set saw run
    // their replace pass. Earlier passes only ever substitute "[REDACTED …]"
    // markers, which cannot form a new secret for a later pattern (and the
    // capture-only branch skips already-redacted values explicitly), so a
    // pattern the set didn't match can never become relevant mid-loop.
    let matched = matching_pattern_indices(text);
    if matched.is_empty() {
        return RedactionOutcome {
            text: text.to_string(),
            hits: Vec::new(),
        };
    }

    let mut result = text.to_string();
    let mut hits: Vec<RedactionHit> = Vec::new();
    for pattern in matched.into_iter().map(|i| &SECRET_PATTERNS[i]) {
        let label = pattern.label;
        let mut count: usize = 0;
        if pattern.capture_only {
            // Replace only the first capture group (the secret value), preserving the prefix.
            // Skip values that already start with "[REDACTED" to avoid corrupting output
            // from an earlier specific pattern (Rust's regex crate has no lookahead).
            result = pattern
                .regex
                .replace_all(&result, |caps: &regex::Captures| {
                    let full = &caps[0];
                    let value = &caps[1];
                    if value.starts_with("[REDACTED") {
                        return full.to_string();
                    }
                    if pattern.validator.is_some_and(|v| !v(full)) {
                        return full.to_string();
                    }
                    count += 1;
                    let prefix_len = full.len() - value.len();
                    format!("{}[REDACTED {label}]", &full[..prefix_len])
                })
                .into_owned();
        } else {
            result = pattern
                .regex
                .replace_all(&result, |caps: &regex::Captures| {
                    if pattern.validator.is_some_and(|v| !v(&caps[0])) {
                        return caps[0].to_string();
                    }
                    count += 1;
                    format!("[REDACTED {label}]")
                })
                .into_owned();
        }
        if count > 0 {
            hits.push(RedactionHit { label, count });
        }
    }
    RedactionOutcome { text: result, hits }
}

/// Check whether text contains any detectable secrets. Honors per-pattern
/// validators so a match `redact_with_audit` would leave unredacted doesn't
/// count as a secret here either.
pub fn contains_secrets(text: &str) -> bool {
    matching_pattern_indices(text)
        .into_iter()
        .map(|i| &SECRET_PATTERNS[i])
        .any(|p| match p.validator {
            None => p.regex.is_match(text),
            Some(v) => p.regex.find_iter(text).any(|m| v(m.as_str())),
        })
}

/// One representative sample per secret pattern. Used by cross-module tests
/// (notably the output-redaction policy) to assert every pattern the
/// detector knows about also gets stripped on the output side, without
/// duplicating the pattern list. The order matches `SECRET_PATTERNS`.
#[cfg(test)]
pub const SECRET_PATTERNS_FOR_TEST: &[&str] = &[
    "AKIAIOSFODNN7EXAMPLE",
    "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijk",
    "github_pat_11AAAAAAA0aBcDeFgHiJkLmNoPqRsTuVwXyZ0123",
    "AIzaSyA1234567890abcdefghijklmnopqrstuvwx",
    "sk_live_0123456789abcdefABCDEFghijkl",
    "sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678",
    "sk-proj-aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678",
    "sk-aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678aBcDeFgHiJkLm",
    "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
    "-----BEGIN RSA PRIVATE KEY-----",
    "xoxb-1234567890-abcdefghijklmnopqrstuvwx",
    "xapp-1-A012345678BCDEFGHIJ",
    r#"password="super-secret-shh""#,
    "password=hunter2trustno1",
    "postgresql://user:hunter2pw@localhost:5432/mydb",
    // A userinfo token NOT matched by any earlier pattern (not a ghp_/AWS/etc.
    // shape) so this sample exercises the URL Credential pattern itself.
    "https://AbCdEf0123456789GhIjKlMnOpQr@example.com/org/repo",
    "Authorization: Bearer abcDEF123456ghiJKL789",
    "x-api-key: AbCdEf0123456789TokenValue",
    "API_KEY=AbCdEf0123456789TokenValue",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_aws_key() {
        let input = "key=AKIAIOSFODNN7EXAMPLE";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED AWS Access Key]"));
        assert!(!result.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn redacts_github_token() {
        let input = "token: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijk";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED GitHub Token]"));
        assert!(
            !result.contains("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijk"),
            "raw token must not survive redaction: {result}"
        );
    }

    #[test]
    fn redacts_bare_token_url_credential() {
        // A bare token in URL userinfo (e.g. from `git remote set-url`) must not
        // leak. The token deliberately is NOT a ghp_/AWS/sk- shape, so an earlier
        // pattern can't claim it first — this proves the URL Credential pattern
        // is the one doing the work (regression: a ghp_ token here was redacted
        // by the GitHub Token pattern, leaving the new pattern untested).
        let token = "AbCdEf0123456789GhIjKlMnOpQr";
        let input = format!("remote: https://{token}@example.com/org/repo");
        let result = redact_secrets(&input);
        assert!(
            !result.contains(token),
            "bare URL token must not survive redaction: {result}"
        );
        assert!(
            result.contains("[REDACTED URL Credential]"),
            "must be masked by the URL Credential pattern specifically: {result}"
        );
    }

    #[test]
    fn redacts_dotted_config_key_credential() {
        // Namespaced/dotted keys ending in a credential word were missed by the
        // word-char-only prefix; they must now be masked.
        for input in [
            "spring.datasource.password=hunter2trustno1",
            "app.config.api_key=AbCdEf0123456789TokenValue",
        ] {
            let result = redact_secrets(input);
            assert!(
                result.contains("[REDACTED"),
                "dotted config key not redacted: {input} -> {result}"
            );
            assert!(
                !result.contains("hunter2trustno1")
                    && !result.contains("AbCdEf0123456789TokenValue"),
                "dotted config value leaked: {result}"
            );
        }
    }

    #[test]
    fn redacts_authorization_bearer_keeps_scheme() {
        let input = "Authorization: Bearer abcDEF123456ghiJKL789";
        let result = redact_secrets(input);
        assert!(
            result.contains("[REDACTED Authorization Header]"),
            "token must be redacted: {result}"
        );
        assert!(
            !result.contains("abcDEF123456ghiJKL789"),
            "raw bearer token must not survive: {result}"
        );
        assert!(
            result.contains("Authorization: Bearer "),
            "scheme word must stay visible for debuggability: {result}"
        );
    }

    #[test]
    fn redacts_authorization_basic_and_token_schemes() {
        for scheme in ["Basic", "Digest", "token"] {
            let input = format!("authorization: {scheme} c3VwZXJzZWNyZXR2YWx1ZQ==");
            let result = redact_secrets(&input);
            assert!(
                result.contains("[REDACTED Authorization Header]"),
                "{scheme} credential must be redacted: {result}"
            );
            assert!(
                !result.contains("c3VwZXJzZWNyZXR2YWx1ZQ=="),
                "{scheme} credential must not survive: {result}"
            );
        }
    }

    #[test]
    fn redacts_x_api_key_header() {
        for header in ["x-api-key", "X-Api-Key", "api-key", "apikey"] {
            let input = format!("{header}: AbCdEf0123456789TokenValue");
            let result = redact_secrets(&input);
            assert!(
                result.contains("[REDACTED API Key Header]"),
                "{header} value must be redacted: {result}"
            );
            assert!(
                !result.contains("AbCdEf0123456789TokenValue"),
                "{header} value must not survive: {result}"
            );
        }
    }

    #[test]
    fn authorization_redaction_does_not_touch_unrelated_text() {
        // No over-blocking: a sentence merely mentioning authorization stays intact.
        let input = "The user needs authorization to proceed with the request.";
        assert_eq!(redact_secrets(input), input);
    }

    #[test]
    fn redacts_anthropic_key() {
        let input = "ANTHROPIC_API_KEY=sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED Anthropic API Key]"));
        assert!(
            !result.contains("sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678"),
            "raw key must not survive redaction: {result}"
        );
    }

    #[test]
    fn redacts_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let input = format!("Bearer {jwt}");
        let result = redact_secrets(&input);
        assert!(result.contains("[REDACTED JWT]"));
        assert!(
            !result.contains(jwt),
            "raw JWT must not survive redaction: {result}"
        );
    }

    #[test]
    fn redacts_private_key() {
        let input = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAK...";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED Private Key]"));
    }

    #[test]
    fn no_false_positive_on_normal_text() {
        let input = "Hello world, this is a normal message with no secrets.";
        assert!(!contains_secrets(input));
        assert_eq!(redact_secrets(input), input);
    }

    #[test]
    fn credential_assignment_spares_prose_words_embedding_keywords() {
        // Words that merely END in a credential keyword are document text,
        // not keys — masking them corrupts legitimate content on every sink
        // that runs redaction (tool output, logs, channel replies).
        for input in [
            "turkey=delicious-roast-recipe",
            "the monkey=cheeky-little-fellow story",
            "mypassword=is-a-common-phrase-in-docs", // embedded, lowercase word shape
            "donkey=eeyore-the-gloomy",
        ] {
            assert_eq!(redact_secrets(input), input, "wrongly masked: {input}");
            assert!(!contains_secrets(input), "false secret hit: {input}");
        }
    }

    #[test]
    fn credential_assignment_still_masks_real_key_shapes() {
        // Word-boundary keys, dotted config keys, camelCase, acronym runs,
        // concatenated compounds, and ALL-CAPS env shapes must all keep
        // masking. (ALL-CAPS keeps legacy embedded matching — env-var-shaped
        // names are near-never prose, so over-masking is the safer failure.)
        for input in [
            "client_secret=abcd1234efgh5678",
            "spring.datasource.password=hunter22hunter22",
            "clientSecret=abcd1234efgh5678",
            "APIKey=abcd1234efgh5678",
            "authtoken=ngrok-1234567890",
            "export DB_PASSWORD=supersecret99",
            "MONKEY=allcaps-env-shape-stays-masked",
        ] {
            let out = redact_secrets(input);
            assert!(
                out.contains("[REDACTED"),
                "real key shape not masked: {input} -> {out}"
            );
        }
    }

    #[test]
    fn password_assignment_is_case_insensitive_and_word_bounded() {
        // `Password:` previously escaped while `password:` was masked — the
        // asymmetry leaked capitalized doc lines. `\b` keeps the keyword from
        // firing mid-word.
        let out = redact_secrets("Password: hunter2trustno1");
        assert!(out.contains("[REDACTED Password Assignment]"), "{out}");
        let out = redact_secrets("PASSWORD=hunter2trustno1");
        assert!(out.contains("[REDACTED"), "{out}");
        // Mid-word: "crosswordpassword" must not anchor at "password".
        let input = "crosswordpassword: not-a-credential-here";
        assert_eq!(redact_secrets(input), input);
    }

    #[test]
    fn contains_secrets_detects() {
        assert!(contains_secrets("my key AKIAIOSFODNN7EXAMPLE is here"));
        assert!(!contains_secrets("just a normal string"));
    }

    #[test]
    fn redacts_slack_token() {
        let input = "SLACK_TOKEN=xoxb-1234567890-abcdefghijklmnopqrstuvwx";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED Slack Token]"));
        assert!(
            !result.contains("xoxb-1234567890-abcdefghijklmnopqrstuvwx"),
            "raw Slack token must not survive redaction: {result}"
        );
    }

    #[test]
    fn redacts_database_connection_string() {
        let input = "DATABASE_URL=postgresql://user:pass@localhost:5432/mydb";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED Database Connection String]"));
        assert!(!result.contains("pass@localhost"));
    }

    #[test]
    fn redacts_mongodb_connection_string() {
        let input = "MONGO=mongodb+srv://admin:secret@cluster0.example.net/db";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED Database Connection String]"));
        assert!(
            !result.contains("admin:secret@cluster0.example.net"),
            "raw connection credentials must not survive redaction: {result}"
        );
    }

    #[test]
    fn redacts_underscore_variant_api_keys() {
        let input = "key=sk_ant_api03_aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED Anthropic API Key]"));
        assert!(
            !result.contains("sk_ant_api03_aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678"),
            "raw underscore-variant key must not survive redaction: {result}"
        );
    }

    #[test]
    fn redacts_credential_assignment() {
        let input = "PGPASSWORD=super-secret-password-123";
        let result = redact_secrets(input);
        assert!(
            result.contains("[REDACTED"),
            "should redact credential assignment, got: {result}"
        );
        assert!(
            !result.contains("super-secret-password-123"),
            "raw credential value must not survive redaction: {result}"
        );
        // capture_only must preserve the variable name so the line stays
        // legible — only the value is scrubbed.
        assert!(
            result.contains("PGPASSWORD="),
            "variable name prefix must be preserved: {result}"
        );
    }

    #[test]
    fn redacts_kek_passphrase_env_line() {
        // Regression: BORG_KEK_PASSPHRASE is the master-KEK secret written to
        // ~/.borg/secrets/kek.env. It contains "KEK" (not "KEY") and the value
        // keyword is "PASSPHRASE", so the pre-fix word list missed it entirely
        // and the passphrase leaked through redaction on every sink.
        let input = "BORG_KEK_PASSPHRASE=Zm9vYmFyMTIzNDU2Nzg5MGFiY2RlZg";
        let result = redact_secrets(input);
        assert!(
            result.contains("[REDACTED"),
            "KEK passphrase env line must be redacted: {result}"
        );
        assert!(
            !result.contains("Zm9vYmFyMTIzNDU2Nzg5MGFiY2RlZg"),
            "raw KEK passphrase must not survive redaction: {result}"
        );
    }

    #[test]
    fn redacts_lowercase_passphrase_assignment() {
        for input in [
            "passphrase=correct-horse-battery-staple",
            r#"passphrase: "correct-horse-battery-staple""#,
        ] {
            let result = redact_secrets(input);
            assert!(
                !result.contains("correct-horse-battery-staple"),
                "passphrase value leaked: {input} -> {result}"
            );
        }
    }

    #[test]
    fn redacts_additional_token_formats() {
        // L5: coverage for credential shapes that were previously missed.
        let cases = [
            ("ASIAIOSFODNN7EXAMPLE", "AWS temporary key"),
            (
                "github_pat_11AAAAAAA0aBcDeFgHiJkLmNoPqRsTuVwXyZ0123",
                "GitHub fine-grained PAT",
            ),
            (
                "AIzaSyA1234567890abcdefghijklmnopqrstuvwx",
                "Google API key",
            ),
            ("sk_live_0123456789abcdefABCDEFghijkl", "Stripe live key"),
            ("xapp-1-A012345678BCDEFGHIJ", "Slack app token"),
        ];
        for (secret, what) in cases {
            let input = format!("value = {secret} end");
            let result = redact_secrets(&input);
            assert!(
                result.contains("[REDACTED"),
                "{what} not redacted: {result}"
            );
            assert!(
                !result.contains(secret),
                "{what} leaked through redaction: {result}"
            );
        }
    }

    #[test]
    fn stripe_test_mode_key_not_over_redacted() {
        // Test-mode Stripe keys are deliberately left alone (documented scope).
        let input = "sk_test_0123456789abcdefABCDEFghijkl";
        assert!(
            !redact_secrets(input).contains("[REDACTED Stripe Key]"),
            "test-mode Stripe key must not match the live-key pattern"
        );
    }

    #[test]
    fn redacts_export_credential() {
        let input = "export DB_SECRET=my_database_secret_key_here";
        let result = redact_secrets(input);
        assert!(
            result.contains("[REDACTED"),
            "should redact exported credential, got: {result}"
        );
        assert!(
            !result.contains("my_database_secret_key_here"),
            "raw exported credential value must not survive redaction: {result}"
        );
    }

    /// Guard against prefilter/pattern-list drift: the RegexSet indices must
    /// line up with `SECRET_PATTERNS` so the prefilter never routes a match
    /// to the wrong pattern or silently skips one. Every representative
    /// sample must still be detected and rewritten with the prefilter on.
    #[test]
    fn prefilter_preserves_detection_of_every_pattern() {
        for sample in SECRET_PATTERNS_FOR_TEST {
            let outcome = redact_with_audit(sample);
            assert!(
                !outcome.hits.is_empty(),
                "sample slipped through the prefilter: {sample}"
            );
            assert_ne!(outcome.text, *sample, "sample not rewritten: {sample}");
        }
    }

    /// A single input carrying several different secret kinds must produce a
    /// hit per pattern in one call — the prefilter selects a set, not just
    /// the first match.
    #[test]
    fn multi_secret_input_hits_multiple_patterns() {
        let input = "aws AKIAIOSFODNN7EXAMPLE and header Authorization: Bearer abcd1234efgh5678 \
                     plus export DB_SECRET=my_database_secret_key_here";
        let outcome = redact_with_audit(input);
        assert!(
            outcome.hits.len() >= 2,
            "expected multiple distinct pattern hits, got {:?}",
            outcome.hits
        );
        assert!(!outcome.text.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(!outcome.text.contains("abcd1234efgh5678"));
        assert!(!outcome.text.contains("my_database_secret_key_here"));
    }
}
