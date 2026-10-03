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
        // Bearer tokens wherever they appear — a `curl -H "Bearer …"`, a proxy
        // log line, a copied request — not just behind the `Authorization:`
        // header the narrower pattern further down requires.
        //
        // Must stay FIRST: the vendor-prefix patterns below match a *prefix* of
        // a token rather than the whole thing, so running them first on
        // `Bearer sk-<48>+tail` masks the `sk-…` part and leaves `+tail` in the
        // clear.
        //
        // `(?i:)` rather than spelling out casings: the case-insensitive literal
        // does cost the RegexSet prefilter's fast path, but only in isolation —
        // the `(?i)` header and credential-assignment patterns below already
        // defeat it, so narrowing here buys nothing and only loses `BeArEr`.
        (
            r"(?i:\bBearer)[ \t]+([A-Za-z0-9._~+/-]{16,}=*)",
            "Bearer Token",
            true,
            None,
        ),
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
        // GitLab token families. All share a `gl<family>-` literal prefix, so
        // one alternation keeps the RegexSet prefilter's literal-prefix fast
        // path intact and adds a single set member instead of thirteen.
        //
        // Longest prefix first (`glrtr` before `glrt`, `glffct` before `glft`):
        // the regex crate's alternation is leftmost-first, so a shorter branch
        // listed earlier would match and leave a stray character behind.
        //
        // `{20,}` is the real floor — classic `glpat-` bodies are 20 chars —
        // and it keeps short placeholders like `glpat-REPLACEME` unredacted.
        (
            r"(?:glpat|glrtr|glagent|glsoat|glffct|glcbt|glptt|gloas|glimt|glrt|gldt|glft|glwt)-[0-9A-Za-z_-]{20,}",
            "GitLab Token",
            false,
            None,
        ),
        // Legacy GitLab runner registration/auth tokens: a fixed `GR1348941`
        // prefix immediately followed by the token body, no separator.
        (
            r"\bGR1348941[0-9A-Za-z_-]{20,}",
            "GitLab Runner Token",
            false,
            None,
        ),
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
        // Credential in a URL query string or fragment: a pre-signed storage
        // URL (`X-Amz-Signature=`), an OAuth callback (`#access_token=`), an
        // `?api_key=` call. The generic assignment pattern below cannot see
        // these, because its key must follow whitespace or `;`. The name must
        // sit directly after the delimiter, so `?keyboard=` and `&page_token=`
        // do not match. Hyphenated names stay literal. Only the value is
        // masked; host, path and parameter name stay readable.
        (
            r#"(?i)[?&#;](?:access_token|refresh_token|id_token|auth_token|token|api[-_]?key|client_secret|secret|password|signature|sig|x-amz-signature|x-amz-security-token|x-amz-credential)=([^&#;\s"'<>]{8,})"#,
            "URL Query Credential",
            true,
            None,
        ),
        // `user:pass@` userinfo on a non-database scheme. The whole userinfo
        // (user AND password) is masked: a token is often the username with
        // an empty or dummy password (`https://<token>:@proxy`,
        // `https://ghp_…:x-oauth-basic@github.com`). A colon-free `user@host`
        // is left alone. `?` and `#` cannot occur raw in userinfo, and their
        // exclusion keeps `http://host:3000?email=a@b.example` untouched.
        (
            r"(?:https?|wss?|ftp|ssh|git\+https?)://([^/\s:@?#]*:[^/\s@?#]*)@",
            "URL Credential",
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

/// True for characters an attacker can thread through a credential to defeat
/// contiguous regex matching while most render surfaces (terminals, chat
/// clients, log viewers) display the token as if unbroken: all Unicode `Cc`
/// controls (C0 including `\n`/`\r`/`\t`, DEL, C1) plus the zero-width and
/// bidi formatting characters. `\n`/`\r`/`\t` are deliberately included —
/// a newline mid-token is exactly the `ghp_abc\n123…` smuggle shape.
fn is_smuggle_char(c: char) -> bool {
    c.is_control() // Unicode Cc: U+0000..=U+001F, U+007F..=U+009F
        || matches!(c,
            '\u{00AD}' // soft hyphen
            | '\u{034F}' // combining grapheme joiner
            | '\u{180E}' // Mongolian vowel separator
            | '\u{200B}'..='\u{200F}' // zero-width space/joiners, LRM/RLM
            | '\u{202A}'..='\u{202E}' // bidi embedding/override
            | '\u{2060}'..='\u{2064}' // word joiner, invisible operators
            | '\u{FE00}'..='\u{FE0F}' // variation selectors
            | '\u{FEFF}' // zero-width no-break space / BOM
        )
}

/// Copy of `text` with smuggle characters removed. Returns `None` when
/// nothing was stripped (the second pass has no work). The byte-offset map
/// back into `text` is built separately — and only when a pattern actually
/// matched the stripped copy — by [`smuggle_byte_map`]: `\n`/`\t` count as
/// smuggle chars, so ordinary multi-line text reaches this path constantly
/// and must not pay for a per-byte `usize` table it will never use.
fn strip_smuggle_chars(text: &str) -> Option<String> {
    let mut stripped = String::with_capacity(text.len());
    for c in text.chars() {
        if !is_smuggle_char(c) {
            stripped.push(c);
        }
    }
    if stripped.len() == text.len() {
        None
    } else {
        Some(stripped)
    }
}

/// Per-byte index table for the stripped copy of `text`: entry `i` is the
/// byte offset in `text` of stripped-copy byte `i`. Must walk `text` exactly
/// as [`strip_smuggle_chars`] does.
fn smuggle_byte_map(text: &str) -> Vec<usize> {
    let mut map: Vec<usize> = Vec::with_capacity(text.len());
    for (i, c) in text.char_indices() {
        if is_smuggle_char(c) {
            continue;
        }
        for off in 0..c.len_utf8() {
            map.push(i + off);
        }
    }
    map
}

/// True when any pattern matches `text` as-is (contiguous matching only,
/// validators honored). Used by the smuggled-span guard below; must NOT
/// consult the smuggling pass itself.
fn contains_contiguous_secret(text: &str) -> bool {
    matching_pattern_indices(text)
        .into_iter()
        .map(|i| &SECRET_PATTERNS[i])
        .any(|p| match p.validator {
            None => p.regex.is_match(text),
            Some(v) => p.regex.find_iter(text).any(|m| v(m.as_str())),
        })
}

/// Second matching pass for control-char / zero-width smuggled secrets:
/// find pattern matches in the stripped copy of `text` and map them back to
/// original-byte spans. Returns non-overlapping `(start, end, label)` spans
/// in ascending order (earlier patterns win overlaps, mirroring the
/// contiguous pass's pattern priority).
///
/// A span is only accepted when:
/// - it contains at least one stripped character strictly inside the match
///   (a fully contiguous match is the first pass's job — masking it again
///   here would widen the span across neighboring smuggle chars);
/// - the original span consists solely of the matched characters plus
///   smuggle characters, so the mask never swallows adjacent legitimate
///   text;
/// - no contiguous fragment of the span (the runs between smuggle chars)
///   matches a pattern on its own — the contiguous pass already handled
///   those, and re-masking would double-mask / widen.
fn smuggled_secret_spans(text: &str) -> Vec<(usize, usize, &'static str)> {
    let Some(stripped) = strip_smuggle_chars(text) else {
        return Vec::new();
    };
    let matched = matching_pattern_indices(&stripped);
    if matched.is_empty() {
        // The common case (multi-line text, no secret shape): bail before
        // building the per-byte offset map.
        return Vec::new();
    }
    let map = smuggle_byte_map(text);
    let mut spans: Vec<(usize, usize, &'static str)> = Vec::new();
    for pattern in matched.into_iter().map(|i| &SECRET_PATTERNS[i]) {
        for caps in pattern.regex.captures_iter(&stripped) {
            let full = match caps.get(0) {
                Some(m) => m,
                None => continue,
            };
            if pattern.validator.is_some_and(|v| !v(full.as_str())) {
                continue;
            }
            // For capture-only patterns mask just the credential value,
            // matching the contiguous pass's prefix-preserving behavior.
            let target = if pattern.capture_only {
                match caps.get(1) {
                    Some(m) => m,
                    None => continue,
                }
            } else {
                full
            };
            if target.as_str().is_empty() || target.as_str().starts_with("[REDACTED") {
                continue;
            }
            let orig_start = map[target.start()];
            let orig_end = map[target.end() - 1] + 1;
            // Contiguous in the original → the first pass's responsibility.
            if orig_end - orig_start == target.end() - target.start() {
                continue;
            }
            let span = &text[orig_start..orig_end];
            // The mask must cover only token chars + smuggle chars.
            let despanned: String = span.chars().filter(|c| !is_smuggle_char(*c)).collect();
            if despanned != target.as_str() {
                continue;
            }
            // A self-matching contiguous fragment was already masked (or
            // deliberately left, per its validator) by the first pass.
            if span
                .split(is_smuggle_char)
                .any(|frag| !frag.is_empty() && contains_contiguous_secret(frag))
            {
                continue;
            }
            if spans
                .iter()
                .any(|&(s, e, _)| orig_start < e && s < orig_end)
            {
                continue; // earlier pattern already claimed an overlapping span
            }
            spans.push((orig_start, orig_end, pattern.label));
        }
    }
    spans.sort_unstable_by_key(|&(s, e, _)| (s, e));
    spans
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
    // Performance gate for the anti-smuggling second pass: clean text (no
    // control / zero-width chars) never pays for the stripped copy.
    let may_smuggle = text.chars().any(is_smuggle_char);
    if matched.is_empty() && !may_smuggle {
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
                    let Some(value) = caps.get(1) else {
                        return full.to_string();
                    };
                    if value.as_str().starts_with("[REDACTED") {
                        return full.to_string();
                    }
                    if pattern.validator.is_some_and(|v| !v(full)) {
                        return full.to_string();
                    }
                    count += 1;
                    // The value can sit mid-match (`user:pass@`), so keep the
                    // text on both sides of it.
                    let offset = caps.get(0).map_or(0, |m| m.start());
                    format!(
                        "{}[REDACTED {label}]{}",
                        &full[..value.start() - offset],
                        &full[value.end() - offset..]
                    )
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

    // Second pass: secrets whose bodies are split by control / zero-width
    // characters (`sk-abc\x1bdef…`, `ghp_abc\n123…`) defeat the contiguous
    // patterns above. Runs on the pass-one output so contiguous matches are
    // already `[REDACTED …]` markers and can't be re-masked or widened.
    if may_smuggle {
        let spans = smuggled_secret_spans(&result);
        for (start, end, label) in spans.into_iter().rev() {
            result.replace_range(start..end, &format!("[REDACTED {label}]"));
            match hits.iter_mut().find(|h| h.label == label) {
                Some(hit) => hit.count += 1,
                None => hits.push(RedactionHit { label, count: 1 }),
            }
        }
    }
    RedactionOutcome { text: result, hits }
}

/// Check whether text contains any detectable secrets. Honors per-pattern
/// validators so a match `redact_with_audit` would leave unredacted doesn't
/// count as a secret here either.
pub fn contains_secrets(text: &str) -> bool {
    if contains_contiguous_secret(text) {
        return true;
    }
    // Same anti-smuggling second pass as `redact_with_audit`, behind the
    // same gate: a secret split by control / zero-width chars still counts.
    text.chars().any(is_smuggle_char) && !smuggled_secret_spans(text).is_empty()
}

/// One representative sample per secret pattern. Used by cross-module tests
/// (notably the output-redaction policy) to assert every pattern the
/// detector knows about also gets stripped on the output side, without
/// duplicating the pattern list. The order matches `SECRET_PATTERNS`.
#[cfg(test)]
pub const SECRET_PATTERNS_FOR_TEST: &[&str] = &[
    // Opaque (no vendor prefix) so this sample exercises the Bearer pattern
    // itself rather than a narrower one that happens to match first.
    "Bearer AbCdEf0123456789OpaqueToken",
    "AKIAIOSFODNN7EXAMPLE",
    "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijk",
    "github_pat_11AAAAAAA0aBcDeFgHiJkLmNoPqRsTuVwXyZ0123",
    "glpat-ABCDEFGHIJKLMNOPQRSTUV",
    "GR1348941abcdefghijklmnopqrst",
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
    // `Basic`, not `Bearer`: the broad Bearer pattern above consumes any
    // `Bearer` token first, so a bearer sample here would reach that pattern
    // rather than this one. (Today's consumers only assert *that* something was
    // redacted, not which pattern did it, so this keeps the sample honest
    // rather than fixing a live gap.)
    "Authorization: Basic dXNlcjpwYXNzd29yZA==",
    "x-api-key: AbCdEf0123456789TokenValue",
    "API_KEY=AbCdEf0123456789TokenValue",
    "https://app.example/cb?access_token=AbCdEf0123456789Value",
    "https://alice:hunter2pw@git.example/repo",
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

    /// A bearer token must be masked whether or not a header name precedes it —
    /// the `Authorization:`-anchored pattern misses `curl -H "Bearer …"` and
    /// proxy log lines entirely — and in every casing an HTTP scheme uses.
    ///
    /// The label assertion pins the "Bearer pattern must stay FIRST" invariant:
    /// the Authorization Header pattern is also `capture_only` and also keeps
    /// the scheme word, so on the header case "token gone" and "redacted once"
    /// both still hold if the ordering regressed. Only the label distinguishes
    /// which pattern actually won.
    #[test]
    fn redacts_bearer_tokens_with_and_without_a_header_name() {
        const TOKEN: &str = "AbCdEf0123456789OpaqueToken";
        for (label, input) in [
            ("header", format!("Authorization: Bearer {TOKEN}")),
            ("bare", format!("curl -H 'Bearer {TOKEN}' https://api.x")),
            ("lowercase", format!("bearer {TOKEN}")),
            ("uppercase", format!("BEARER {TOKEN}")),
            ("mixed case", format!("BeArEr {TOKEN}")),
        ] {
            let result = redact_secrets(&input);
            assert!(
                !result.contains(TOKEN),
                "{label}: raw token must not survive: {result}"
            );
            assert!(
                result.contains("[REDACTED Bearer Token]"),
                "{label}: the Bearer pattern must be the one that matched: {result}"
            );
            assert_eq!(
                result.matches("[REDACTED").count(),
                1,
                "{label}: must be redacted exactly once: {result}"
            );
        }
    }

    /// The scheme word stays visible so a redacted transcript still shows which
    /// auth mechanism was in play.
    #[test]
    fn bearer_redaction_keeps_the_scheme_word() {
        let result = redact_secrets("Authorization: Bearer abcDEF123456ghiJKL789");
        assert!(
            result.contains("Authorization: Bearer "),
            "scheme word must stay visible for debuggability: {result}"
        );
    }

    /// Ordering regression: the vendor-prefix patterns match a *prefix* of a
    /// token, so if one of them ran before the Bearer pattern it would mask
    /// `sk-…` and leave the base64 tail after it in the clear.
    #[test]
    fn bearer_pattern_masks_whole_token_not_just_its_vendor_prefix() {
        let token = "sk-aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678aBcDeFgHiJkLm+t/ail=";
        let input = format!("Bearer {token}");
        let result = redact_secrets(&input);
        assert!(
            !result.contains("+t/ail="),
            "base64 tail leaked past the vendor-prefix match: {result}"
        );
        assert!(
            !result.contains(token),
            "raw token must not survive: {result}"
        );
    }

    /// No over-blocking: `Bearer` is an ordinary English word, and the token
    /// class must not run past the end of the line to find 16 characters.
    #[test]
    fn bearer_redaction_does_not_touch_prose() {
        let input = "The bearer of this note is\nauthorized to collect the parcel.";
        assert_eq!(redact_secrets(input), input);
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
        // Bare, with no `Bearer` prefix: a JWT leaks just as readily out of a
        // cookie, a query string, or a log line, and this asserts the JWT
        // pattern itself rather than the broader Bearer one that would match
        // first if the prefix were present.
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let input = format!("token={jwt}");
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
            ("glpat-ABCDEFGHIJKLMNOPQRSTUV", "GitLab PAT"),
            ("GR1348941abcdefghijklmnopqrst", "GitLab runner token"),
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

    #[test]
    fn redacts_url_embedded_credentials_and_keeps_the_rest_of_the_url() {
        let signature = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        // (input, secret that must go, label, fragments that must stay)
        let cases: &[(String, &str, &str, &[&str])] = &[
            (
                format!(
                    "https://bucket.s3.amazonaws.com/k?X-Amz-Signature={signature}&X-Amz-Expires=300"
                ),
                signature,
                "URL Query Credential",
                &[
                    "https://bucket.s3.amazonaws.com/k?X-Amz-Signature=",
                    "&X-Amz-Expires=300",
                ],
            ),
            (
                "https://app.example/cb#access_token=ZxCvBnM1234567890qWeRtY&state=xyz".to_string(),
                "ZxCvBnM1234567890qWeRtY",
                "URL Query Credential",
                &["https://app.example/cb#access_token=", "&state=xyz"],
            ),
            (
                "GET https://api.example/v1/items?page=2&api_key=QwErTy0987654321 done".to_string(),
                "QwErTy0987654321",
                "URL Query Credential",
                &["https://api.example/v1/items?page=2&api_key=", " done"],
            ),
            (
                "https://s3.example/o?x-amz-security-token=FwoGZXIvYXdzEJr0123456".to_string(),
                "FwoGZXIvYXdzEJr0123456",
                "URL Query Credential",
                &["https://s3.example/o?x-amz-security-token="],
            ),
            (
                "clone https://alice:hunter2pw@git.example/repo now".to_string(),
                "hunter2pw",
                "URL Credential",
                &["clone https://", "@git.example/repo now"],
            ),
            (
                "wss://svc:s3cr3tvalue@stream.example/socket".to_string(),
                "s3cr3tvalue",
                "URL Credential",
                &["wss://", "@stream.example/socket"],
            ),
        ];
        for (input, secret, label, kept) in cases {
            let outcome = redact_with_audit(input);
            assert!(
                !outcome.text.contains(secret),
                "secret survived in: {}",
                outcome.text
            );
            assert!(
                outcome.text.contains(&format!("[REDACTED {label}]")),
                "expected the {label} pattern to mask {input}, got: {}",
                outcome.text
            );
            for fragment in *kept {
                assert!(
                    outcome.text.contains(fragment),
                    "fragment {fragment:?} must stay readable, got: {}",
                    outcome.text
                );
            }
        }
    }

    #[test]
    fn ordinary_urls_are_not_redacted() {
        let cases = [
            "https://example.com/search?q=rust+tokens&page=2",
            "https://example.com/?keyboard=qwertyuiop",
            "https://user@host/path",
            "https://example.com/list?page_token=abcdefghijkl&sort=signature_desc",
            "https://example.com/?tokenizer=wordpiece1&secretary=jane.doe.smith",
            "http://localhost:3000?email=someone@example.com",
            "https://example.com:8443/users/@alice",
            "https://example.com/docs#installation-guide",
            "see https://example.com:8080/a and mail bob@example.com",
        ];
        for input in cases {
            let outcome = redact_with_audit(input);
            assert_eq!(outcome.text, input, "over-redacted: {input}");
            assert!(outcome.hits.is_empty(), "unexpected hits for {input}");
        }
    }

    #[test]
    fn url_userinfo_with_colon_masks_user_and_password() {
        // A token is often sent as the URL *username* with an empty or dummy
        // password; masking only the password leaked it. Any userinfo with a
        // colon is masked whole; a colon-free `user@host` stays readable.
        let cases: &[(&str, &str)] = &[
            (
                "https://Zq8vT3kP9wLm2xR7nB4cY6fH1dJ5sA0e:@llm-proxy.example/v1",
                "https://[REDACTED URL Credential]@llm-proxy.example/v1",
            ),
            (
                "git push https://ghp_abc123:x-oauth-basic@github.com/o/r",
                "git push https://[REDACTED URL Credential]@github.com/o/r",
            ),
            (
                "clone https://alice:hunter2pw@git.example/repo now",
                "clone https://[REDACTED URL Credential]@git.example/repo now",
            ),
            (
                "ftp://u:pw@files.example/x",
                "ftp://[REDACTED URL Credential]@files.example/x",
            ),
            ("https://user@host/path", "https://user@host/path"),
            (
                "http://host:3000/?email=a@b.example",
                "http://host:3000/?email=a@b.example",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_secrets(input), *expected, "input: {input}");
        }
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

#[cfg(test)]
mod gitlab_pattern_tests {
    use super::{contains_secrets, redact_secrets, redact_with_audit, RedactionHit};

    #[test]
    fn redacts_every_gitlab_token_family() {
        // One row per prefix: a typo'd or omitted branch in the alternation
        // fails its own row rather than hiding behind a sibling.
        let cases: &[(&str, &str)] = &[
            ("glpat-ABCDEFGHIJKLMNOPQRSTUV", "GitLab Token"),
            ("gloas-ABCDEFGHIJKLMNOPQRSTUV", "GitLab Token"),
            ("gldt-ABCDEFGHIJKLMNOPQRSTUV", "GitLab Token"),
            ("glrt-ABCDEFGHIJKLMNOPQRSTUV", "GitLab Token"),
            ("glrtr-ABCDEFGHIJKLMNOPQRSTUV", "GitLab Token"),
            ("glcbt-ABCDEFGHIJKLMNOPQRSTUV", "GitLab Token"),
            ("glptt-ABCDEFGHIJKLMNOPQRSTUV", "GitLab Token"),
            ("glft-ABCDEFGHIJKLMNOPQRSTUV", "GitLab Token"),
            ("glimt-ABCDEFGHIJKLMNOPQRSTUV", "GitLab Token"),
            ("glagent-ABCDEFGHIJKLMNOPQRSTUV", "GitLab Token"),
            ("glsoat-ABCDEFGHIJKLMNOPQRSTUV", "GitLab Token"),
            ("glffct-ABCDEFGHIJKLMNOPQRSTUV", "GitLab Token"),
            ("glwt-ABCDEFGHIJKLMNOPQRSTUV", "GitLab Token"),
            ("GR1348941abcdefghijklmnopqrst", "GitLab Runner Token"),
        ];
        for (token, label) in cases {
            let out = redact_secrets(&format!("token is {token} ok"));
            assert!(
                out.contains(&format!("[REDACTED {label}]")),
                "{token} should be labeled {label}, got: {out}"
            );
            assert!(!out.contains(token), "{token} survived redaction: {out}");
        }
    }

    #[test]
    fn gitlab_lookalikes_are_not_redacted() {
        // Over-match guard. The `gl…-` prefixes are short enough that a sloppy
        // `gl[a-z]+-` pattern would swallow ordinary prose like `global-` on
        // every redaction sink in the process.
        let cases = [
            "glpat-short",
            "global-configuration-identifier-here",
            "glassdoor-1234567890abcdefghij",
            "GROUP1348941abcdefghijklmnopqrst",
            "GR1348941",
        ];
        for input in cases {
            let out = redact_secrets(&format!("value {input} end"));
            assert!(
                out.contains(input),
                "{input:?} must not be redacted, got: {out}"
            );
        }
    }

    #[test]
    fn smuggled_secrets_are_masked_and_clean_neighbors_are_not() {
        // Control-char / zero-width smuggling: a token body split by ESC,
        // NUL, newline, zero-width, or bidi chars must still be masked, while
        // controls between unrelated short strings must not create a mask.
        struct Case {
            name: &'static str,
            input: String,
            masked: bool,
            label: &'static str,
        }
        let cases = [
            Case {
                name: "ESC-split OpenAI-style key",
                input: format!(
                    "key: sk-{}\u{1b}{}",
                    "a1B2c3D4e5F6g7H8i9J0k1L2", "m3N4o5P6q7R8s9T0u1V2w3X4"
                ),
                masked: true,
                label: "API Key",
            },
            Case {
                name: "newline-split GitHub token",
                input: format!(
                    "token ghp_{}\n{} end",
                    "ABCDEFGHIJKLMNOPQR", "abcdefghijklmnopqr"
                ),
                masked: true,
                label: "GitHub Token",
            },
            Case {
                name: "zero-width-space-split Anthropic key",
                input: format!(
                    "sk-ant-api03-{}\u{200b}{}",
                    "aBcDeFgHiJkLmNoP", "qRsTuVwXyZ01234567"
                ),
                masked: true,
                label: "Anthropic API Key",
            },
            Case {
                name: "NUL-and-bidi-split AWS key",
                input: format!("cred AKIA\u{0}IOSF\u{202e}ODNN7EXAMPLE here"),
                masked: true,
                label: "AWS Access Key",
            },
            Case {
                name: "multibyte text around a DEL-split Slack token",
                input: format!("héllo → xoxb-1234567890-abcdef\u{7f}ghijklmnopqrstuvwx ←"),
                masked: true,
                label: "Slack Token",
            },
            Case {
                name: "soft-hyphen-split GitHub token",
                input: format!(
                    "token ghp_{}\u{ad}{} end",
                    "ABCDEFGHIJKLMNOPQR", "abcdefghijklmnopqr"
                ),
                masked: true,
                label: "GitHub Token",
            },
            Case {
                name: "variation-selector-split Anthropic key",
                input: format!(
                    "sk-ant-api03-{}\u{fe0f}{}",
                    "aBcDeFgHiJkLmNoP", "qRsTuVwXyZ01234567"
                ),
                masked: true,
                label: "Anthropic API Key",
            },
            Case {
                name: "combining-grapheme-joiner-and-MVS-split AWS key",
                input: "cred AKIA\u{34f}IOSF\u{180e}ODNN7EXAMPLE here".to_string(),
                masked: true,
                label: "AWS Access Key",
            },
            Case {
                name: "control between unrelated short strings",
                input: "abc\u{0}def and ghp_short\u{1b}bits".to_string(),
                masked: false,
                label: "",
            },
            Case {
                name: "controls but no secret",
                input: "line one\nline two \u{1b}[31mred\u{1b}[0m\ttabbed".to_string(),
                masked: false,
                label: "",
            },
        ];
        for case in &cases {
            let outcome = redact_with_audit(&case.input);
            if case.masked {
                assert!(
                    outcome.text.contains(&format!("[REDACTED {}]", case.label)),
                    "{}: expected [REDACTED {}], got: {:?}",
                    case.name,
                    case.label,
                    outcome.text
                );
                assert_eq!(
                    outcome.hits.iter().map(|h| h.count).sum::<usize>(),
                    1,
                    "{}: expected exactly one hit, got {:?}",
                    case.name,
                    outcome.hits
                );
                assert!(
                    contains_secrets(&case.input),
                    "{}: contains_secrets must agree",
                    case.name
                );
            } else {
                assert_eq!(
                    outcome.text, case.input,
                    "{}: text must pass through unchanged",
                    case.name
                );
                assert!(outcome.hits.is_empty(), "{}: no hits expected", case.name);
                assert!(!contains_secrets(&case.input), "{}", case.name);
            }
        }
    }

    #[test]
    fn smuggled_mask_does_not_swallow_adjacent_clean_text() {
        // The mask span must cover only token chars + smuggle chars — the
        // words on either side of the smuggled token survive verbatim.
        let input = format!(
            "before ghp_{}\u{200b}{} after",
            "ABCDEFGHIJKLMNOPQR", "abcdefghijklmnopqr"
        );
        let out = redact_secrets(&input);
        assert_eq!(out, "before [REDACTED GitHub Token] after", "got: {out}");
    }

    #[test]
    fn contiguous_secret_near_controls_is_masked_exactly_once() {
        // A contiguous secret with control chars elsewhere in the text must
        // be handled by the first pass alone — no second mask, no widening
        // of the span across the neighboring escape sequence.
        let input = format!("\u{1b}[32mok\u{1b}[0m key AKIAIOSFODNN7EXAMPLE done\nnext line");
        let outcome = redact_with_audit(&input);
        assert_eq!(
            outcome.text,
            "\u{1b}[32mok\u{1b}[0m key [REDACTED AWS Access Key] done\nnext line"
        );
        assert_eq!(
            outcome.hits,
            vec![RedactionHit {
                label: "AWS Access Key",
                count: 1
            }]
        );
    }

    #[test]
    fn clean_text_fast_path_returns_text_unchanged() {
        // No controls, no zero-width, no secrets: the gate must keep the
        // original bytes intact (the smuggling pass never runs).
        let input = "plain sentence with numbers 12345 and a url https://example.com/path";
        let outcome = redact_with_audit(input);
        assert_eq!(outcome.text, input);
        assert!(outcome.hits.is_empty());
    }

    #[test]
    fn glrtr_token_is_not_truncated_to_the_glrt_branch() {
        // Encodes the longest-prefix-first ordering decision: leftmost-first
        // alternation would otherwise match `glrt` and leave a stray `r-`.
        let out = redact_secrets("glrtr-ABCDEFGHIJKLMNOPQRSTUV");
        assert_eq!(out, "[REDACTED GitLab Token]", "got: {out}");
    }
}
