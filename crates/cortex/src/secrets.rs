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

/// Redact detected secrets in text, replacing matches with `[REDACTED <label>]`.
pub fn redact_secrets(text: &str) -> String {
    let mut result = text.to_string();
    for pattern in SECRET_PATTERNS.iter() {
        let label = pattern.label;
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
                    let prefix_len = full.len() - value.len();
                    format!("{}[REDACTED {label}]", &full[..prefix_len])
                })
                .into_owned();
        } else {
            result = pattern
                .regex
                .replace_all(&result, format!("[REDACTED {}]", pattern.label))
                .into_owned();
        }
    }
    result
}

/// Check whether text contains any detectable secrets.
pub fn contains_secrets(text: &str) -> bool {
    SECRET_PATTERNS.iter().any(|p| p.regex.is_match(text))
}

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
    }

    #[test]
    fn redacts_anthropic_key() {
        let input = "ANTHROPIC_API_KEY=sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED Anthropic API Key]"));
    }

    #[test]
    fn redacts_jwt() {
        let input = "Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED JWT]"));
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
    }

    #[test]
    fn redacts_underscore_variant_api_keys() {
        let input = "key=sk_ant_api03_aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED Anthropic API Key]"));
    }

    #[test]
    fn redacts_credential_assignment() {
        let input = "PGPASSWORD=super-secret-password-123";
        let result = redact_secrets(input);
        assert!(
            result.contains("[REDACTED"),
            "should redact credential assignment, got: {result}"
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
    }
}
