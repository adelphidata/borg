use regex::Regex;
use std::sync::LazyLock;

struct SecretPattern {
    regex: Regex,
    label: &'static str,
    /// If true, only the first capture group is replaced (preserves leading context).
    capture_only: bool,
}

static SECRET_PATTERNS: LazyLock<Vec<SecretPattern>> = LazyLock::new(|| {
    let patterns: Vec<(&str, &str, bool)> = vec![
        (r"AKIA[0-9A-Z]{16}", "AWS Access Key", false),
        (
            r"(?:ghp|ghs|gho|ghu|ghm)_[A-Za-z0-9_]{36,}",
            "GitHub Token",
            false,
        ),
        (
            r"sk[-_]ant[-_][A-Za-z0-9_-]{32,}",
            "Anthropic API Key",
            false,
        ),
        (
            r"sk[-_]proj[-_][A-Za-z0-9_-]{32,}",
            "OpenAI Project Key",
            false,
        ),
        (r"sk-[A-Za-z0-9]{48,}", "API Key", false),
        (
            r"eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
            "JWT",
            false,
        ),
        (
            r"-----BEGIN (?:RSA |EC |DSA )?PRIVATE KEY-----",
            "Private Key",
            false,
        ),
        (r"xox[bpsar]-[A-Za-z0-9-]{24,}", "Slack Token", false),
        (
            r#"(?:password|passwd|pwd)\s*[=:]\s*"[^"]{8,}""#,
            "Quoted Password",
            false,
        ),
        (
            r"(?:password|passwd|pwd)\s*[=:]\s*(\S{8,64})",
            "Password Assignment",
            true,
        ),
        (
            r"(?:postgres(?:ql)?|mysql|mongodb(?:\+srv)?|redis|amqp|mssql)://[^\s]{8,}",
            "Database Connection String",
            false,
        ),
        // Generic assignment patterns — run last so specific patterns above take priority.
        // capture_only=true: only the captured value (group 1) is replaced, preserving the
        // variable name prefix so the surrounding text remains readable.
        // The replace closure skips values already starting with "[REDACTED" to avoid
        // double-redacting values already handled by an earlier specific pattern
        // (Rust's regex crate does not support lookahead assertions).
        (
            r"(?i)(?:^|[\s;])(?:export\s+)?(?:\w*(?:PASSWORD|SECRET|TOKEN|KEY|APIKEY|API_KEY))\s*=\s*(\S{8,})",
            "Credential Assignment",
            true,
        ),
    ];

    patterns
        .into_iter()
        .filter_map(|(pattern, label, capture_only)| match Regex::new(pattern) {
            Ok(regex) => Some(SecretPattern {
                regex,
                label,
                capture_only,
            }),
            Err(e) => {
                tracing::error!("Failed to compile secret pattern '{label}': {e} — skipping");
                None
            }
        })
        .collect()
});

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
    let mut result = text.to_string();
    let mut hits: Vec<RedactionHit> = Vec::new();
    for pattern in SECRET_PATTERNS.iter() {
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
                    count += 1;
                    let prefix_len = full.len() - value.len();
                    format!("{}[REDACTED {label}]", &full[..prefix_len])
                })
                .into_owned();
        } else {
            result = pattern
                .regex
                .replace_all(&result, |_caps: &regex::Captures| {
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

/// Check whether text contains any detectable secrets.
pub fn contains_secrets(text: &str) -> bool {
    SECRET_PATTERNS.iter().any(|p| p.regex.is_match(text))
}

/// One representative sample per secret pattern. Used by cross-module tests
/// (notably the output-redaction policy) to assert every pattern the
/// detector knows about also gets stripped on the output side, without
/// duplicating the pattern list. The order matches `SECRET_PATTERNS`.
#[cfg(test)]
pub const SECRET_PATTERNS_FOR_TEST: &[&str] = &[
    "AKIAIOSFODNN7EXAMPLE",
    "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijk",
    "sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678",
    "sk-proj-aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678",
    "sk-aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678aBcDeFgHiJkLm",
    "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
    "-----BEGIN RSA PRIVATE KEY-----",
    "xoxb-1234567890-abcdefghijklmnopqrstuvwx",
    r#"password="super-secret-shh""#,
    "password=hunter2trustno1",
    "postgresql://user:hunter2pw@localhost:5432/mydb",
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
}
