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
    "pwd",
    "passwd",
    "sshpass",
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
///
/// A value that is a dotted call or index expression (`getpass.getpass()`,
/// `os.environ["X"]`) is source code, not a secret. Shell output reaches the
/// model redacted, so masking it would corrupt the code the model edits.
fn credential_assignment_is_real(full_match: &str) -> bool {
    let Some(eq) = full_match.find('=') else {
        return true; // defensive: pattern guarantees a '='
    };
    if value_is_code_expression(full_match[eq + 1..].trim_start()) {
        return false;
    }
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

/// True when `value` is a dotted identifier chain that is called or indexed
/// and carries no literal that could be a secret: `getpass.getpass()`,
/// `os.environ['NPM_TOKEN']`, `os.getenv("API_KEY")`,
/// `client.auth.fetch_token(scope)`. Each call or index argument must be an
/// identifier (dotted allowed), and the whole value may hold at most one
/// quoted literal, shaped like an env-var name (`[A-Z_][A-Z0-9_]*`). So
/// `os.getenv("K","Hunter2Secret!")` and `base64.b64decode("c2st…")` are
/// still masked. A bare identifier before `(` (`Hunter2(!)`) does not count:
/// passwords carry parentheses, but seldom a `name.name(` shape. A trailing
/// `;` is allowed; anything else after the expression means not code.
fn value_is_code_expression(value: &str) -> bool {
    let value = value.strip_suffix(';').unwrap_or(value);
    let Some(open) = value.find(['(', '[']) else {
        return false;
    };
    let chain = &value[..open];
    if chain.split('.').count() < 2 || !is_dotted_identifier(chain) {
        return false;
    }
    let mut rest = &value[open..];
    let mut literals = 0usize;
    while !rest.is_empty() {
        if let Some(after_dot) = rest.strip_prefix('.') {
            // A method or field after a call: `.strip()`, `.value`.
            let end = after_dot.find(['(', '[', '.']).unwrap_or(after_dot.len());
            if !is_identifier(&after_dot[..end]) {
                return false;
            }
            rest = &after_dot[end..];
            continue;
        }
        let close = match rest.as_bytes()[0] {
            b'(' => ')',
            b'[' => ']',
            _ => return false,
        };
        let Some(end) = rest.find(close) else {
            return false; // unclosed (the match stops at whitespace): mask
        };
        let args = &rest[1..end];
        for arg in args.split(',').map(str::trim) {
            if arg.is_empty() || is_dotted_identifier(arg) {
                continue;
            }
            literals += 1;
            if literals > 1 || !is_env_name_literal(arg) {
                return false;
            }
        }
        rest = &rest[end + 1..];
    }
    true
}

/// `name`: an ASCII identifier (`_` or a letter, then word characters).
fn is_identifier(seg: &str) -> bool {
    seg.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `a.b.c`: one or more identifiers joined by dots.
fn is_dotted_identifier(s: &str) -> bool {
    s.split('.').all(is_identifier)
}

/// `'NAME'` or `"NAME"` where `NAME` is shaped like an env-var name.
fn is_env_name_literal(arg: &str) -> bool {
    let inner = ['\'', '"']
        .iter()
        .find_map(|q| arg.strip_prefix(*q).and_then(|a| a.strip_suffix(*q)));
    let Some(inner) = inner else {
        return false;
    };
    inner
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && inner
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Lowercase words of an identifier, split at non-letters and at camelCase /
/// acronym junctures (`SecretAccessKey` → `secret access key`,
/// `DB_PASSWORD` → `db password`, `APIKey` → `api key`). Digits and
/// punctuation separate words and are dropped.
fn identifier_words(key: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for (i, c) in key.char_indices() {
        if !c.is_ascii_alphabetic() {
            continue;
        }
        let lower = c.to_ascii_lowercase();
        match words.last_mut() {
            Some(word) if !is_word_start(key, i) => word.push(lower),
            _ => words.push(lower.to_string()),
        }
    }
    words
}

/// Whole words that name a credential on their own. Concatenated compounds
/// (`authtoken`, `secretkey`) are spelled out because they read as one word.
/// `auth` is the Docker `config.json` field; `tokens` is deliberately absent
/// so a `max_tokens` usage field is never treated as a credential.
const CREDENTIAL_KEY_WORDS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "secret",
    "secrets",
    "token",
    "auth",
    "authorization",
    "credential",
    "credentials",
    "apikey",
    "apitoken",
    "authtoken",
    "authkey",
    "accesstoken",
    "refreshtoken",
    "sessiontoken",
    "secretkey",
    "accesskey",
    "privatekey",
    "clientsecret",
];

/// Words that, right before `token`, make it a pagination or sync cursor
/// (`nextPageToken`, `page_token`, `continuationToken`, `syncToken`), not a
/// credential. Masking those breaks every paged API listing the agent reads.
const CURSOR_TOKEN_QUALIFIERS: &[&str] = &[
    "page",
    "next",
    "continuation",
    "cursor",
    "sync",
    "resume",
    "resumption",
    "pagination",
];

/// Adjacent word pairs that name a credential (`api_key`, `AccessKey`,
/// `private-key`) where neither word does alone.
const CREDENTIAL_KEY_PAIRS: &[(&str, &str)] =
    &[("api", "key"), ("access", "key"), ("private", "key")];

/// Final words that make a key describe a credential rather than hold one:
/// `token_type`, `auth_uri`, `secret_name`, `password_file`,
/// `token_expires_at`. Their values are metadata, and masking them corrupts
/// ordinary OAuth and cloud-SDK JSON.
const CREDENTIAL_METADATA_WORDS: &[&str] = &[
    "type",
    "types",
    "kind",
    "uri",
    "url",
    "urls",
    "endpoint",
    "host",
    "domain",
    "issuer",
    "audience",
    "scope",
    "scopes",
    "id",
    "ids",
    "name",
    "names",
    "count",
    "length",
    "len",
    "size",
    "limit",
    "path",
    "file",
    "dir",
    "location",
    "ttl",
    "expiry",
    "expires",
    "expiration",
    "exp",
    "at",
    "in",
    "lifetime",
    "hint",
    "policy",
    "mode",
    "method",
    "methods",
    "scheme",
    "provider",
    "header",
    "format",
    "version",
    "enabled",
    "required",
    "label",
    "description",
    "prefix",
    "field",
    "source",
    "strategy",
    "status",
    "style",
    "flow",
    "algorithm",
    "alg",
    "encoding",
];

/// True when a JSON key names a credential at word boundaries: some word
/// (or adjacent pair) is a credential word, and the last word is not a
/// metadata word. `tokenizer`, `author` and `authority` embed a keyword
/// mid-word and are rejected; `SessionToken`, `DB_PASSWORD`, `bot_token`
/// and `SecretAccessKey` are accepted. A `token` right after a cursor
/// qualifier (`nextPageToken`, `syncToken`) does not count.
fn json_key_names_credential(key: &str) -> bool {
    let words = identifier_words(key);
    let Some(last) = words.last() else {
        return false;
    };
    if CREDENTIAL_METADATA_WORDS.contains(&last.as_str()) {
        return false;
    }
    let is_cursor_token = |i: usize| {
        words[i] == "token" && i > 0 && CURSOR_TOKEN_QUALIFIERS.contains(&words[i - 1].as_str())
    };
    words
        .iter()
        .enumerate()
        .any(|(i, w)| CREDENTIAL_KEY_WORDS.contains(&w.as_str()) && !is_cursor_token(i))
        || words.windows(2).any(|pair| {
            CREDENTIAL_KEY_PAIRS
                .iter()
                .any(|(a, b)| pair[0] == *a && pair[1] == *b)
        })
}

/// Post-match validator for the JSON Credential patterns (plain and
/// backslash-escaped quotes). The pattern only guarantees that a keyword
/// sits somewhere in the key; this checks it sits at a word boundary.
fn json_credential_key_is_real(full_match: &str) -> bool {
    let body = full_match.trim_start_matches('\\');
    let Some(body) = body.strip_prefix('"') else {
        return false;
    };
    let key_end = body.find(['"', '\\']).unwrap_or(body.len());
    json_key_names_credential(&body[..key_end])
}

/// The JSON Credential pattern for JSON escaped inside a string value
/// (`"SecretString":"{\"password\":\"…\"}"`). The inner value's own escapes
/// arrive doubled: `\\\"` is an inner `\"`, `\\n` / `\\/` / `\\u00e9` are
/// inner `\n` / `\/` / `é`. A lone `\"` ends the value.
const ESCAPED_JSON_CREDENTIAL_PATTERN: &str = r#"(?i)\\"[\w.-]*(?:passw(?:or)?d|passphrase|secret|token|api[-_]?key|auth|credential|private[-_]?key|access[-_]?key)[\w.-]*\\"\s*:\s*\\"((?:[^"\\]|\\\\\\.|\\\\[^"\\]|\\[^"\\]){8,})\\""#;

/// `ESCAPED_JSON_CREDENTIAL_PATTERN` compiled on its own, so the plain-quote
/// validator can ask whether the escaped pattern will mask an inner field.
static ESCAPED_JSON_CREDENTIAL: LazyLock<Option<Regex>> =
    LazyLock::new(|| match Regex::new(ESCAPED_JSON_CREDENTIAL_PATTERN) {
        Ok(regex) => Some(regex),
        Err(e) => {
            tracing::error!("Failed to compile escaped JSON Credential pattern: {e}");
            None
        }
    });

/// Post-match validator for the plain-quote JSON Credential pattern. The key
/// must name a credential. When the value is an escaped JSON object whose own
/// credential field the escaped-quote pattern masks
/// (`"SecretString":"{\"password\":\"…\"}"`), the outer match is left to it,
/// so the rest of the object stays readable. Any other value, an escaped
/// object without a maskable credential field included, is masked whole.
fn json_plain_credential_is_real(full_match: &str) -> bool {
    if !json_credential_key_is_real(full_match) {
        return false;
    }
    let Some(inner) = ESCAPED_JSON_CREDENTIAL.as_ref() else {
        return true; // the escaped pattern is unavailable: mask the outer value
    };
    let inner_masks_a_field = inner
        .find_iter(full_match)
        .any(|m| json_credential_key_is_real(m.as_str()));
    !inner_masks_a_field
}

/// Post-match validator for the line-start npmrc pattern. The pattern keeps
/// the value to token/base64 characters; this also rejects a `.` before a
/// letter, so code such as `_password = config.secret` stays readable.
fn npmrc_bare_value_is_token(full_match: &str) -> bool {
    let Some(eq) = full_match.find('=') else {
        return false; // defensive: pattern guarantees a '='
    };
    let value = full_match[eq + 1..].trim().as_bytes();
    !value
        .windows(2)
        .any(|w| w[0] == b'.' && w[1].is_ascii_alphabetic())
}

/// Post-match validator for the registry-scoped npmrc pattern: an
/// environment reference (`${NPM_TOKEN}`) is the recommended way to keep the
/// secret out of the file, so it is not a credential itself.
fn npmrc_value_is_literal(full_match: &str) -> bool {
    let Some(eq) = full_match.find('=') else {
        return true; // defensive: pattern guarantees a '='
    };
    let value = full_match[eq + 1..].trim().trim_matches(['"', '\'']);
    !(value.starts_with("${") && value.ends_with('}'))
}

/// Post-match validator for the Discord Token pattern: a real token's first
/// segment is the bot's user id (a 17–20 digit snowflake) in base64url.
fn discord_token_has_user_id(full_match: &str) -> bool {
    use base64::Engine as _;
    let head = full_match.split('.').next().unwrap_or_default();
    match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(head.trim_end_matches('=')) {
        Ok(decoded) => (17..=20).contains(&decoded.len()) && decoded.iter().all(u8::is_ascii_digit),
        Err(_) => false,
    }
}

/// Post-match validator for the CLI Credential Flag pattern. An `=` form
/// (`--token=value`) is always a credential. A space-separated value is one
/// only when it looks like a secret: it carries a digit, `_` or `-`, or mixes
/// case beyond a leading capital. A plain word (`followed`) or a capitalized
/// one (`Authenticate`) is prose or help text.
fn cli_flag_value_is_credential(full_match: &str) -> bool {
    let after_flag = &full_match[2..];
    let name_end = after_flag
        .find(|c: char| c == '=' || c.is_whitespace())
        .unwrap_or(after_flag.len());
    let rest = &after_flag[name_end..];
    if rest.starts_with('=') {
        return true;
    }
    let value = rest.trim_start();
    if value
        .chars()
        .any(|c| c.is_ascii_digit() || c == '_' || c == '-')
    {
        return true;
    }
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let tail = chars.as_str();
    let tail_has_upper = tail.chars().any(|c| c.is_ascii_uppercase());
    let tail_has_lower = tail.chars().any(|c| c.is_ascii_lowercase());
    // `AbCdEf…` and `abcDef…` are mixed; `Authenticate` and `followed`
    // are words; `ABCDEFGH` could be either, so it is masked (fail-secure).
    (tail_has_upper && tail_has_lower) || (first.is_ascii_uppercase() && !tail_has_lower)
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
        // A private key block: PEM (`RSA`/`EC`/`DSA`/`OPENSSH`/`ENCRYPTED`/
        // PKCS#8) and PGP (`PGP PRIVATE KEY BLOCK`). The secret is the body,
        // so the mask runs from BEGIN through the matching END marker. A block
        // cut off before its END (a truncated `cat`, a log line) masks the
        // armor headers and every base64 run of 16+ chars that follows — short
        // enough to cover a 64-col PEM line, long enough that prose after a
        // bare BEGIN line stays readable. `\\[rn]` lets both forms span a key
        // embedded as a JSON string with escaped newlines.
        //
        // Runs right after Bearer: a vendor pattern that matched inside the
        // base64 body first would leave a `[REDACTED …]` marker that ends the
        // truncated-block run early and leaks the rest of the body.
        (
            r"(?s)-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----(?:.*?-----END (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----|(?:(?:\s|\\[rn])*[A-Za-z][A-Za-z-]*:[ \t][^\r\n\\]*)*(?:(?:\s|\\[rn])*[A-Za-z0-9+/=]{16,})*)",
            "Private Key",
            false,
            None,
        ),
        // PuTTY `.ppk` key file: the `Private-Lines: N` count is followed by
        // the private blob, one base64 line each. Only the blob is masked —
        // the public lines, comment and MAC are not secret. Every masked line
        // must be whole (`$`), so the `Private-MAC:` line that follows is
        // never nibbled.
        (
            r"(?mR)Private-Lines:[ \t]*\d+\r?\n((?:[A-Za-z0-9+/=]+\r?\n)*[A-Za-z0-9+/=]+)$",
            "PuTTY Private Key",
            true,
            None,
        ),
        // age identities: a bech32 body (uppercase) behind a fixed prefix.
        (
            r"\bAGE-SECRET-KEY-1[0-9A-Z]{50,}",
            "age Secret Key",
            false,
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
        (r"\bAQ\.[0-9A-Za-z_-]{30,}", "Google API Key", false, None),
        // The long-lived Google OAuth refresh token a calendar sign-in stores.
        (
            r"\b1//0[0-9A-Za-z_-]{40,}",
            "Google OAuth Token",
            false,
            None,
        ),
        (
            r"\bmfa\.[0-9A-Za-z_-]{60,}",
            "Discord MFA Token",
            false,
            None,
        ),
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
        // OpenRouter keys carry a `sk-or-v1-` prefix and a hex body; the
        // hyphens keep them out of the generic `sk-` pattern below.
        // The leading `\b` keeps a word that merely ends in the prefix
        // (`desk-<hex>`, `task-<id>`) out of the short-floor patterns.
        (
            r"\bsk-or-v1-[A-Za-z0-9]{32,}",
            "OpenRouter API Key",
            false,
            None,
        ),
        // Generic `sk-` keys. 32 is the real floor — DeepSeek issues
        // `sk-` + 32 hex — so the old 48-char minimum let those leak bare.
        (r"\bsk-[A-Za-z0-9]{32,}", "API Key", false, None),
        (r"\bgsk_[A-Za-z0-9]{32,}", "Groq API Key", false, None),
        (r"\bxai-[A-Za-z0-9]{32,}", "xAI API Key", false, None),
        // Vendor-prefixed keys the generic `sk-` family cannot reach. `\b`
        // keeps a word that merely ends in the prefix out, and each floor is
        // the vendor's real body length (or close to it) so short placeholders
        // stay readable. `ctx7sk-` needs its own entry: the `sk-` sits after a
        // word char, so the `\bsk-` pattern above never sees it.
        (
            r"\bctx7sk-[A-Za-z0-9_-]{20,}",
            "Context7 API Key",
            false,
            None,
        ),
        (r"\bntn_[A-Za-z0-9]{30,}", "Notion Token", false, None),
        (r"\bfc-[A-Za-z0-9]{24,}", "Firecrawl API Key", false, None),
        (r"\bhf_[A-Za-z0-9]{30,}", "Hugging Face Token", false, None),
        (r"\bnpm_[A-Za-z0-9]{30,}", "npm Token", false, None),
        (r"\bpypi-[A-Za-z0-9_-]{40,}", "PyPI Token", false, None),
        (
            r"\bdo[po]_v1_[A-Za-z0-9]{40,}",
            "DigitalOcean Token",
            false,
            None,
        ),
        (
            r"\bSG\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}",
            "SendGrid API Key",
            false,
            None,
        ),
        (r"\btvly-[A-Za-z0-9_-]{20,}", "Tavily API Key", false, None),
        (
            r"\bpplx-[A-Za-z0-9]{30,}",
            "Perplexity API Key",
            false,
            None,
        ),
        (r"\br8_[A-Za-z0-9]{30,}", "Replicate Token", false, None),
        (r"\bfw_[A-Za-z0-9]{30,}", "Fireworks API Key", false, None),
        (r"\bfpk_[A-Za-z0-9]{30,}", "Fireworks API Key", false, None),
        (
            r"\bbb_live_[A-Za-z0-9_-]{20,}",
            "BrowserBase API Key",
            false,
            None,
        ),
        // Langfuse keys are `pk-lf-`/`sk-lf-` + a hyphenated uuid; the
        // hyphens keep `sk-lf-` out of the generic `sk-` pattern.
        (r"\b[ps]k-lf-[A-Za-z0-9-]{30,}", "Langfuse Key", false, None),
        // Grafana service-account tokens (optional hex checksum suffix) and
        // Grafana Cloud access-policy tokens (base64 body, may be padded).
        (
            r"\bglsa_[A-Za-z0-9]{10,}(?:_[A-Fa-f0-9]{8})?",
            "Grafana Token",
            false,
            None,
        ),
        (
            r"\bglc_[A-Za-z0-9+/_-]{32,}={0,3}",
            "Grafana Token",
            false,
            None,
        ),
        // Google OAuth: a pasted `client_secret.json` carries `GOCSPX-…`;
        // access tokens are `ya29.` plus a dotted url-safe body (an optional
        // one-letter issuer tag first: `c.` service account, `d.` STS).
        (
            r"\bGOCSPX-[A-Za-z0-9_-]{8,}",
            "Google OAuth Secret",
            false,
            None,
        ),
        (
            r"\bya29\.[A-Za-z0-9._-]{20,}",
            "Google OAuth Token",
            false,
            None,
        ),
        // Discord bot tokens: `<base64 user id>.<base64 timestamp>.<hmac>`.
        // The shape alone fits a long dotted CamelCase identifier, so the
        // validator requires the first segment to decode to a real user id.
        // No leading `\b`: a token glued to an escape spelling (`\n`, `%0A`)
        // has a letter on its left and would slip a word boundary.
        (
            r"[MNO][A-Za-z0-9_-]{22,27}\.[A-Za-z0-9_-]{6,7}\.[A-Za-z0-9_-]{27,}",
            "Discord Token",
            false,
            Some(discord_token_has_user_id as fn(&str) -> bool),
        ),
        // Incoming-webhook URLs: the URL itself is the credential, so the
        // secret path segment is masked while host and ids stay readable.
        // `http://` or no scheme at all is matched too — the path carries the
        // same secret. Teams Workflows URLs keep their secret in `sig=`, which the
        // URL Query Credential pattern below already masks.
        (
            r"(?i)(?:https?://)?hooks\.slack(?:-gov)?\.com\.?(?::\d+)?/(?:services|workflows|triggers)/[A-Z0-9]+/[A-Z0-9]+/(?:\d+/)?([A-Za-z0-9]{8,})",
            "Webhook Secret",
            true,
            None,
        ),
        (
            r"(?i)(?:https?://)?(?:canary\.|ptb\.|www\.)?discord(?:app)?\.com\.?(?::\d+)?/api/(?:v\d+/)?webhooks/\d+/([\w-]{8,})",
            "Webhook Secret",
            true,
            None,
        ),
        (
            r"(?i)(?:https?://)?[A-Za-z0-9.-]*(?:webhook\.office\.com\.?(?::\d+)?/webhookb2|outlook\.office\.com\.?(?::\d+)?/webhook)/([\w@/%+=-]{8,})",
            "Webhook Secret",
            true,
            None,
        ),
        (
            r"eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
            "JWT",
            false,
            None,
        ),
        (r"xox[bpsar]-[A-Za-z0-9-]{24,}", "Slack Token", false, None),
        // Slack app-level tokens (`xapp-1-...`) are distinct from the `xox*`
        // bot/user tokens above.
        (r"xapp-[0-9]-[A-Za-z0-9-]{10,}", "Slack Token", false, None),
        // Telegram bot tokens: an 8-10 digit bot id, a colon, then exactly
        // 35 body chars. They sit inside the Bot API URL
        // (`/bot<token>/sendMessage`), so a logged request error carries one
        // glued to `bot`; that literal is the only word-char lead-in allowed.
        // The exact body length plus the trailing `\b` keeps a
        // `<timestamp>:<40-hex sha>` pair out. capture_only so the lead-in
        // character stays in the clear.
        //
        // A colored log line or an escaped transcript also puts a letter on
        // the token's left: an ANSI CSI sequence (`ESC[32m`), its escaped
        // spellings (`\x1b[`, `\u001b[`, `\033[`), a backslash escape (`\n`)
        // or a percent escape (`%0A`). Those lead-ins are allowed too, and
        // stay visible because only the token is captured. Each lead-in may
        // also carry the URL's `bot` prefix (`ESC[32mbot<token>`).
        (
            r"(?:^|[^\w]|\bbot|(?:\x1b\[[0-?]*[ -/]*[@-~]|(?:\\x1[bB]|\\u001[bB]|\\033)\[[0-?]*[ -/]*[@-~]|\\[nrtbfv]|%[0-9A-Fa-f]{2})(?:bot)?)(\d{8,10}:[A-Za-z0-9_-]{35})\b",
            "Telegram Bot Token",
            true,
            None,
        ),
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
        // schemes (Bearer/Basic/Digest/token, plus the Discord-style `Bot`);
        // opaque no-scheme values are left alone to avoid clobbering the
        // scheme word on a second pass.
        (
            r"(?i)authorization\s*:\s*(?:bearer|basic|digest|token|bot)\s+(\S{4,})",
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
        // Other opaque credential headers. `\b` keeps the names from matching
        // inside a longer word; the value is a single token.
        (
            r"(?i)\bx-(?:auth|access|amz-security|vault|session)-token\s*:\s*(\S{8,})",
            "Credential Header",
            true,
            None,
        ),
        // Session cookies. The value is masked to end-of-line because cookie
        // strings contain `; `-separated pairs. Requiring a `name=` shape keeps
        // prose like `Cookie: chocolate chip` untouched.
        (
            r"(?i)\b(?:set-)?cookie\s*:\s*([^\s=;]+=[^\r\n]{4,})",
            "Cookie Header",
            true,
            None,
        ),
        // JSON credential fields (`"api_key": "…"`, `"access_token":"…"`,
        // `"secret":"…"`). The header patterns above need the key to run
        // straight into the colon, so a quoted JSON key never reached them.
        // Any key that carries a credential word counts — `DB_PASSWORD`,
        // `bot_token`, `SessionToken`, `SecretAccessKey`, Docker's `auth` —
        // but the validator demands a word boundary (`tokenizer`, `author`
        // stay readable) and rejects metadata keys (`token_type`,
        // `auth_uri`). See `json_key_names_credential`. Only the quoted value
        // is masked and both quotes survive; a value under 8 chars or a
        // non-string value (`"max_tokens": 1024`) is never touched. A
        // backslash escape (`\"`) inside the value is part of the value.
        (
            r#"(?i)"[\w.-]*(?:passw(?:or)?d|passphrase|secret|token|api[-_]?key|auth|credential|private[-_]?key|access[-_]?key)[\w.-]*"\s*:\s*"((?:[^"\\]|\\.){8,})""#,
            "JSON Credential",
            true,
            Some(json_plain_credential_is_real as fn(&str) -> bool),
        ),
        // The same fields inside JSON that is itself a string value, with
        // escaped quotes: `"SecretString":"{\"password\":\"…\"}"` from a
        // secrets-manager read, or a tool result embedded in a log line. See
        // `ESCAPED_JSON_CREDENTIAL_PATTERN` for the doubled-escape rules.
        (
            ESCAPED_JSON_CREDENTIAL_PATTERN,
            "JSON Credential",
            true,
            Some(json_credential_key_is_real as fn(&str) -> bool),
        ),
        // netrc entries (`~/.netrc`, `_netrc`): whitespace-separated tokens,
        // on one line or spread over several. `login` must come before
        // `password` in the same entry, which keeps prose that only mentions
        // a machine and a password out. Prose such as "machine learning:
        // login once, password reset" is kept out by the entry's shape: the
        // machine value must look like a host (dotted name, IP, IPv6 or
        // `localhost`, optional `:port`) and `default` must open a line.
        // Only the password value is masked.
        (
            r"(?m)(?:^[ \t]*default|\bmachine\s+(?:localhost|[\w-]+(?:\.[\w-]+)+|\[?[0-9A-Fa-f]*:[0-9A-Fa-f:.]*\]?)(?::\d+)?)(?:\s+(?:account|port)\s+\S+)*\s+login\s+\S+(?:\s+(?:account|port)\s+\S+)*\s+password\s+(\S+)",
            "netrc Password",
            true,
            None,
        ),
        // npmrc registry auth (`//registry.example.com/:_authToken=…`,
        // `:_auth=` base64 user:pass, `:_password=`), registry-scoped. An env
        // reference (`${NPM_TOKEN}`) is exempt.
        (
            r"//\S*?:_(?:authToken|auth|password)[ \t]*=[ \t]*(\S+)",
            "npmrc Credential",
            true,
            Some(npmrc_value_is_literal as fn(&str) -> bool),
        ),
        // The unscoped npmrc form, bare at line start. Shell output reaches
        // the model redacted, so code such as `_password = getpass.getpass()`
        // or `_auth == other` must stay readable: the line must be a whole
        // npmrc entry with one `=` and a token/base64 value to end of line,
        // and the validator rejects a `.` before a letter.
        (
            r"(?m)^[ \t]*_(?:authToken|auth|password)[ \t]*=[ \t]*([A-Za-z0-9+/._-][A-Za-z0-9+/=._-]*)[ \t]*\r?$",
            "npmrc Credential",
            true,
            Some(npmrc_bare_value_is_token as fn(&str) -> bool),
        ),
        // CLI flags carrying a credential (`--api-key=…`, `--token …`). The
        // flag must end at the name, so `--token-file path` and
        // `--password-stdin` stay untouched; a value may not start with `-`,
        // so a bare flag followed by another flag is not masked. The
        // validator keeps prose (`--token followed by…`) and help text
        // (`--token  Authenticate…`) in the clear: a space-separated value
        // must look like a credential. See `cli_flag_value_is_credential`.
        (
            r"(?i)--(?:api[-_]?key|token|access[-_]?token|auth[-_]?token|secret|client[-_]?secret|password|passwd)(?:=|\s+)([^\s=-]\S{7,})",
            "CLI Credential Flag",
            true,
            Some(cli_flag_value_is_credential as fn(&str) -> bool),
        ),
        // A percent-encoded URL carrying a credential query parameter, e.g.
        // a redirect target `…?next=https%3A%2F%2Fx%3Ftoken%3Dabc…`. The raw
        // `?token=` pattern below cannot see `%3F`/`%26`/`%23` delimiters.
        // The value stops at an encoded `&` (`%26`) or `#` (`%23`) so the
        // next parameter (`%26page%3D2`) stays readable; any other escape
        // is part of the value.
        (
            r#"(?i)%(?:3F|26|23)(?:access_token|refresh_token|id_token|auth_token|token|api[-_]?key|client_secret|secret|password|signature|sig)%3D((?:[^&#;\s"'<>%]|%[013-9A-Fa-f][0-9A-Fa-f]|%2[0-24-57-9A-Fa-f]){8,})"#,
            "URL Query Credential",
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
        // `_PWD`/`_PASSWD` and `SSHPASS` cover the client env vars that carry a
        // bare password (`MYSQL_PWD`, `SSHPASS` for `sshpass -e`). The
        // underscore is required: the shell's own `PWD`/`OLDPWD` are paths.
        // The post-match validator rejects keys that merely EMBED a keyword
        // (`turkey=`, `mypassword=` prose shapes) — the keyword must sit at a
        // word boundary in the key. See `credential_assignment_is_real`.
        (
            r"(?i)(?:^|[\s;])(?:export\s+)?(?:[\w.]*(?:PASSWORD|PASSPHRASE|SECRET|TOKEN|KEY|APIKEY|API_KEY|_PWD|_PASSWD|SSHPASS))\s*=\s*(\S{8,})",
            "Credential Assignment",
            true,
            Some(credential_assignment_is_real as fn(&str) -> bool),
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

/// Shortest literal `mask_literals` will replace — anything shorter is too
/// likely to collide with ordinary text.
const MIN_LITERAL_SECRET_LEN: usize = 8;

/// Mask every occurrence of each known secret value in `text`. For
/// credentials known by value rather than by shape: a custom provider's
/// random-hex key matches no pattern, yet the provider can still echo it
/// back (`Invalid API key: <key>`). Run it before `redact_secrets`, which
/// still covers everything shaped like a secret.
pub fn mask_literals(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if secret.len() >= MIN_LITERAL_SECRET_LEN && out.contains(secret) {
            out = out.replace(secret, "[REDACTED credential]");
        }
    }
    out
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
    "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEAtestkeybodytestkeybody\n-----END RSA PRIVATE KEY-----",
    "Private-Lines: 1\nAAAAIQCtestprivatekeybodytestprivatekeybody\n",
    "AGE-SECRET-KEY-1QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ",
    "AKIAIOSFODNN7EXAMPLE",
    "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijk",
    "github_pat_11AAAAAAA0aBcDeFgHiJkLmNoPqRsTuVwXyZ0123",
    "glpat-ABCDEFGHIJKLMNOPQRSTUV",
    "GR1348941abcdefghijklmnopqrst",
    "AIzaSyA1234567890abcdefghijklmnopqrstuvwx",
    "sk_live_0123456789abcdefABCDEFghijkl",
    "sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678",
    "sk-proj-aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678",
    "sk-or-v1-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "sk-aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678aBcDeFgHiJkLm",
    "gsk_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789abcdefghijklmnopqrstuvwxyz",
    "xai-aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789abcdefghijklmnopqrstuvwxyz",
    "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
    "xoxb-1234567890-abcdefghijklmnopqrstuvwx",
    "xapp-1-A012345678BCDEFGHIJ",
    "123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsawX",
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
    "x-auth-token: AbCdEf0123456789TokenValue",
    "Cookie: session=AbCdEf0123456789; theme=dark",
    r#"{"api_key": "AbCdEf0123456789TokenValue"}"#,
    r#"{"SecretString":"{\"password\":\"AbCdEf0123456789\"}"}"#,
    "machine api.example.com login bob password AbCdEf0123456789",
    "//registry.example.com/:_authToken=AbCdEf0123456789TokenValue",
    "_authToken=AbCdEf0123456789TokenValue",
    "--api-key=AbCdEf0123456789TokenValue",
    "https://app.example/login?next=https%3A%2F%2Fx.example%2Fcb%3Ftoken%3DAbCdEf0123456789Value",
    "API_KEY=AbCdEf0123456789TokenValue",
    "https://app.example/cb?access_token=AbCdEf0123456789Value",
    "https://alice:hunter2pw@git.example/repo",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_prefixed_keys_are_masked_by_label() {
        // Built at run time so the source never holds a scanner-shaped literal.
        let do_token = format!("dop_v1_{}", "0123456789abcdef".repeat(4));
        let cases = [
            ("ctx7sk-abcdefghijklmnopqrstuvwxyz0123", "Context7 API Key"),
            (
                "ntn_abcdefghijklmnopqrstuvwxyz0123456789ABCD",
                "Notion Token",
            ),
            ("fc-0123456789abcdef0123456789abcdef", "Firecrawl API Key"),
            (
                "hf_abcdefghijklmnopqrstuvwxyz01234567",
                "Hugging Face Token",
            ),
            ("npm_abcdefghijklmnopqrstuvwxyz0123456789", "npm Token"),
            (
                "pypi-AgEIcHlwaS5vcmcCJGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6MDEyMzQ1Njc4",
                "PyPI Token",
            ),
            (do_token.as_str(), "DigitalOcean Token"),
            (
                "SG.abcdefghijklmnopqrstuv.abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJK",
                "SendGrid API Key",
            ),
            ("tvly-abcdefghijklmnopqrstuvwxyz0123", "Tavily API Key"),
            (
                "pplx-abcdefghijklmnopqrstuvwxyz0123456789",
                "Perplexity API Key",
            ),
            ("r8_abcdefghijklmnopqrstuvwxyz01234567", "Replicate Token"),
            ("fw_abcdefghijklmnopqrstuvwxyz01234567", "Fireworks API Key"),
            (
                "fpk_abcdefghijklmnopqrstuvwxyz01234567",
                "Fireworks API Key",
            ),
            ("bb_live_abcdefghijklmnopqrstuvwxyz", "BrowserBase API Key"),
            ("sk-lf-0123abcd-4567-89ef-0123-456789abcdef", "Langfuse Key"),
            ("pk-lf-0123abcd-4567-89ef-0123-456789abcdef", "Langfuse Key"),
        ];
        for (token, label) in cases {
            let out = redact_secrets(&format!("key is {token} ok"));
            assert!(
                out.contains(&format!("[REDACTED {label}]")),
                "{token} should be labeled {label}, got: {out}"
            );
            assert!(!out.contains(token), "{token} survived redaction: {out}");
        }
    }

    #[test]
    fn mask_literals_masks_known_values_and_ignores_short_ones() {
        let key = "9f8e7d6c5b4a39281706f5e4d3c2b1a0";
        let out = mask_literals(
            &format!("Invalid API key: {key} ({key})"),
            &[key, "", "abc"],
        );
        assert_eq!(
            out,
            "Invalid API key: [REDACTED credential] ([REDACTED credential])"
        );
        // A short value would mask ordinary words; it is left alone.
        assert_eq!(mask_literals("abc def", &["abc"]), "abc def");
    }

    #[test]
    fn grafana_and_google_oauth_tokens_are_masked() {
        let sa_body = "abcdefghijklmnopqrstuvwxyzABCDEF0123";
        let cloud_body = "eyJrIjoiYWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXoiLCJuIjoidGVzdCJ9";
        let cases = [
            (format!("glsa_{sa_body}_0123abcd"), "Grafana Token"),
            (format!("glsa_{sa_body}"), "Grafana Token"),
            (format!("glc_{cloud_body}=="), "Grafana Token"),
            (
                format!("GOCSPX-{}", "aB3_dE6-gH9jK2mN5pQ8"),
                "Google OAuth Secret",
            ),
            (
                format!("ya29.{}", "a0AfB_byC9xYz0123456789abcdef"),
                "Google OAuth Token",
            ),
            (
                format!("ya29.c.{}", "c0AZ4bNpq7zP2mX9vLk4nRt8wYb1"),
                "Google OAuth Token",
            ),
        ];
        for (token, label) in cases {
            let out = redact_secrets(&format!("token: {token}\n"));
            assert_eq!(out, format!("token: [REDACTED {label}]\n"), "{token}");
        }
        for benign in [
            "glsa_short",
            "glc_config_loader",
            "myglsa_abcdefghijklmnop",
            "ya29.short",
        ] {
            assert_eq!(
                redact_secrets(benign),
                benign,
                "{benign} must not be masked"
            );
        }
    }

    #[test]
    fn discord_tokens_are_masked_only_when_they_carry_a_user_id() {
        use base64::Engine as _;
        let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("123456789012345678");
        let token = format!("{id}.GhIjKl.{}", "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123");
        for text in [
            format!("connecting with {token}"),
            format!(r#"{{"DISCORD_BOT_TOKEN": "{token}"}}"#),
            format!(r"line one\n{token}"),
        ] {
            let out = redact_secrets(&text);
            assert!(!out.contains(&token), "token survived: {out}");
            assert!(out.contains("[REDACTED Discord Token]"), "{out}");
        }
        // Same shape, but the first segment is not a base64 snowflake.
        let lookalike = "MyVeryLongModuleNameHere.Config.SomeExtremelyLongAttributeName_value";
        assert_eq!(redact_secrets(lookalike), lookalike);
    }

    #[test]
    fn webhook_url_secrets_are_masked_and_hosts_stay_readable() {
        let slack_secret = "AbCdEfGhIjKlMnOpQrStUvWx";
        let discord_secret = "aBcD-eFgH_iJkLmNoPqRsTuVwXyZ0123456789";
        let teams_tail = "0a1b2c3d@4e5f6a7b/IncomingWebhook/9c8d7e6f5a4b/0a1b2c3d";
        let cases = [
            (
                format!("https://hooks.slack.com/services/T0123ABCD/B0123ABCD/{slack_secret}"),
                slack_secret,
                "https://hooks.slack.com/services/T0123ABCD/B0123ABCD/",
            ),
            (
                format!("http://hooks.slack.com/workflows/T01/A02/123456789/{slack_secret}"),
                slack_secret,
                "http://hooks.slack.com/workflows/T01/A02/123456789/",
            ),
            (
                format!("https://discord.com/api/webhooks/123456789012345678/{discord_secret}"),
                discord_secret,
                "https://discord.com/api/webhooks/123456789012345678/",
            ),
            (
                format!("https://contoso.webhook.office.com/webhookb2/{teams_tail}"),
                teams_tail,
                "https://contoso.webhook.office.com/webhookb2/",
            ),
        ];
        for (url, secret, kept) in cases {
            let out = redact_secrets(&format!("posting to {url} now"));
            assert!(!out.contains(secret), "secret survived: {out}");
            assert!(
                out.contains(&format!("{kept}[REDACTED")),
                "host lost: {out}"
            );
        }
        for benign in [
            "https://hooks.slack.com/docs",
            "https://discord.com/api/webhooks",
            "https://example.com/services/T0/B0/notasecret",
        ] {
            assert_eq!(
                redact_secrets(benign),
                benign,
                "{benign} must not be masked"
            );
        }
    }

    #[test]
    fn vendor_prefixes_need_a_word_boundary_and_a_real_body() {
        // A word that merely ends in the prefix, and a short placeholder,
        // both stay readable.
        for text in [
            "traffic-abcdefghijklmnopqrstuvwxyz0123456",
            "fc-REPLACEME",
            "ntn_placeholder",
            "see hf_ docs",
            "my-ctx7sk-short",
        ] {
            assert_eq!(redact_secrets(text), text, "{text} must not be masked");
        }
    }

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
    fn private_key_blocks_are_masked_body_and_all() {
        // (input, body fragments that must not survive, text that must)
        let body = "MIIEpAIBAAKCAQEA0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKL";
        let tail = "QW1hc2tlZHRhaWxsaW5lb2Z0aGVrZXlib2R5";
        let cases: Vec<(String, Vec<&str>, Vec<&str>)> = vec![
            (
                format!("before\n-----BEGIN RSA PRIVATE KEY-----\n{body}\n{tail}\n-----END RSA PRIVATE KEY-----\nafter"),
                vec![body, tail, "END RSA"],
                vec!["before\n", "\nafter"],
            ),
            (
                format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{body}\n-----END OPENSSH PRIVATE KEY-----"),
                vec![body],
                vec![],
            ),
            (
                format!("-----BEGIN ENCRYPTED PRIVATE KEY-----\n{body}\n-----END ENCRYPTED PRIVATE KEY-----"),
                vec![body],
                vec![],
            ),
            (
                format!("-----BEGIN PGP PRIVATE KEY BLOCK-----\nVersion: GnuPG v2\n\n{body}\n=AbCd\n-----END PGP PRIVATE KEY BLOCK-----"),
                vec![body, "GnuPG"],
                vec![],
            ),
            // A key embedded as a JSON string with escaped newlines.
            (
                format!(r#"{{"key": "-----BEGIN PRIVATE KEY-----\n{body}\n-----END PRIVATE KEY-----\n"}}"#),
                vec![body],
                vec![r#"{"key": ""#],
            ),
            // Truncated before END: the armor headers and body still go.
            (
                format!("-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,00FF\n\n{body}\n{tail}"),
                vec![body, tail, "DEK-Info"],
                vec![],
            ),
        ];
        for (input, gone, kept) in cases {
            let out = redact_secrets(&input);
            assert!(out.contains("[REDACTED Private Key]"), "not masked: {out}");
            for g in gone {
                assert!(!out.contains(g), "{g:?} survived: {out}");
            }
            for k in kept {
                assert!(out.contains(k), "{k:?} lost: {out}");
            }
        }
    }

    #[test]
    fn a_bare_private_key_header_leaves_following_prose_alone() {
        let out = redact_secrets(
            "Look for the -----BEGIN RSA PRIVATE KEY----- line, then check the permissions.",
        );
        assert_eq!(
            out,
            "Look for the [REDACTED Private Key] line, then check the permissions."
        );
    }

    #[test]
    fn putty_private_lines_are_masked_without_the_mac() {
        let ppk = "PuTTY-User-Key-File-3: ssh-ed25519\r\nEncryption: none\r\nComment: me@host\r\nPublic-Lines: 2\r\nAAAAC3NzaC1lZDI1NTE5AAAAIPublicPart\r\nMoreOfThePublicKey\r\nPrivate-Lines: 2\r\nAAAAIFirstPrivateLineOfTheKey0123456789\r\nSecondPrivateLine=\r\nPrivate-MAC: 0a1b2c3d\r\n";
        let out = redact_secrets(ppk);
        assert!(!out.contains("FirstPrivateLine"), "{out}");
        assert!(!out.contains("SecondPrivateLine"), "{out}");
        assert!(
            out.contains(
                "Private-Lines: 2\r\n[REDACTED PuTTY Private Key]\r\nPrivate-MAC: 0a1b2c3d"
            ),
            "{out}"
        );
        assert!(out.contains("AAAAC3NzaC1lZDI1NTE5AAAAIPublicPart"), "{out}");
    }

    #[test]
    fn age_secret_keys_are_masked() {
        let key = format!(
            "AGE-SECRET-KEY-1{}",
            "QPZRY9X8GF2TVDW0S3JN54KHCE6MUA7L".repeat(2)
        );
        let out = redact_secrets(&format!("# identity\n{key}\n"));
        assert_eq!(out, "# identity\n[REDACTED age Secret Key]\n");
    }

    #[test]
    fn client_password_env_vars_are_masked_but_shell_paths_are_not() {
        for (input, value) in [
            ("MYSQL_PWD=hunter2trustno1", "hunter2trustno1"),
            ("export SSHPASS=hunter2trustno1", "hunter2trustno1"),
            ("REDIS_PASSWD=hunter2trustno1", "hunter2trustno1"),
            ("mysql_pwd=hunter2trustno1", "hunter2trustno1"),
        ] {
            let out = redact_secrets(input);
            assert!(!out.contains(value), "{input} leaked: {out}");
            assert!(out.contains("=[REDACTED"), "{input} lost its key: {out}");
        }
        let env = "OLDPWD=/home/alice/projects/borg";
        assert_eq!(redact_secrets(env), env);
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

    /// Bare vendor keys that used to leak when they appeared outside a
    /// `KEY=value` assignment or a `Bearer` header.
    #[test]
    fn redacts_bare_vendor_keys_table() {
        let positives: &[(&str, &str)] = &[
            (
                "sk-or-v1-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "OpenRouter API Key",
            ),
            // DeepSeek: `sk-` + 32 hex, below the old 48-char floor.
            ("sk-0123456789abcdef0123456789abcdef", "API Key"),
            (
                "gsk_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789abcdefghijklmnopqrstuvwxyz",
                "Groq API Key",
            ),
            (
                "xai-aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789abcdefghijklmnopqrstuvwxyz",
                "xAI API Key",
            ),
            (
                "123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsawX",
                "Telegram Bot Token",
            ),
            (
                "https://api.telegram.org/bot123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsawX/sendMessage",
                "Telegram Bot Token",
            ),
        ];
        for (secret, label) in positives {
            let input = format!("saw {secret} in the log");
            let out = redact_with_audit(&input);
            assert!(!out.text.contains(secret), "{label} leaked: {}", out.text);
            assert!(
                out.hits.iter().any(|h| h.label == *label),
                "{secret} expected {label}, got {:?}",
                out.hits
            );
        }
        let negatives = [
            "sk-short",
            "sk-0123456789abcdef0123456789abcde", // 31 chars: under the floor
            "gsk_tooshort",
            "xai-tooshort",
            // A word that ends in the prefix is not a key.
            "desk-0123456789abcdef0123456789abcdef01234567",
            "task-0123456789abcdef0123456789abcdef01234567",
            "12:34 meeting at room 56",
            "1234567:ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi", // 7-digit id
            // `<unix timestamp>:<sha1>` from a log line.
            "1700000000:0123456789abcdef0123456789abcdef01234567",
            "123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw", // 34-char body
            "127.0.0.1:8080/health",
            "2026-10-06T12:34:56Z",
        ];
        for input in negatives {
            assert_eq!(redact_secrets(input), input, "false positive on {input}");
        }
    }

    #[test]
    fn redacts_oauth_webhook_and_chat_bot_credentials_table() {
        // (input with `{S}` where the secret sits, the secret in two halves,
        // label, text that must survive). Each secret is joined at runtime so
        // no credential-shaped literal sits in this file.
        let positives: &[(&str, [&str; 2], &str, &str)] = &[
            (
                "client_secret {S}",
                ["GOCSPX-aBcDeFgHiJ", "kLmNoPqRsTuVwXyZ12"],
                "Google OAuth Secret",
                "client_secret",
            ),
            (
                "got {S} back",
                ["ya29.a0AfH6SMBx1234567890", "abcdefghijklmnopqrstuvwxyz-_AB"],
                "Google OAuth Token",
                "back",
            ),
            (
                "refresh {S}",
                ["1//0gAbCdEfGhIjKlMnOpQrSt", "UvWxYz0123456789-_abcdefghij"],
                "Google OAuth Token",
                "refresh",
            ),
            (
                "grafana {S}",
                ["glsa_0123456789abcdefghij", "ABCDEFGHIJ01_9f8e7d6c"],
                "Grafana Token",
                "grafana",
            ),
            (
                "post to https://hooks.slack.com/services/T01ABCDEF/B02GHIJKL/{S} please",
                ["aBcDeFgHiJkL", "mNoPqRsTuVwX"],
                "Webhook Secret",
                "https://hooks.slack.com/services/T01ABCDEF/B02GHIJKL/",
            ),
            (
                "https://discord.com/api/webhooks/123456789012345678/{S}",
                [
                    "abcdefghijklmnopqrstuvwxyzABCDEFGH",
                    "IJKLMNOPQRSTUVWXYZ0123456789_-abcd",
                ],
                "Webhook Secret",
                "https://discord.com/api/webhooks/123456789012345678/",
            ),
            (
                "https://contoso.webhook.office.com/webhookb2/0a1b2c3d-1111-2222-3333-444455556666@7e8f9a0b-aaaa-bbbb-cccc-ddddeeeeffff/IncomingWebhook/{S}/1a2b3c4d",
                ["abcdef0123456789", "abcdef0123456789"],
                "Webhook Secret",
                "https://contoso.webhook.office.com/webhookb2/",
            ),
            // The first dotted segment is base64("123456789012345678").
            (
                "token {S} here",
                [
                    "MTIzNDU2Nzg5MDEyMzQ1Njc4",
                    ".GaBcDe.abcdefghijklmnopqrstuvwxyz0123456789AB",
                ],
                "Discord Token",
                "here",
            ),
        ];
        for (template, halves, label, kept) in positives {
            let secret = halves.concat();
            let input = template.replace("{S}", &secret);
            let out = redact_with_audit(&input);
            assert!(!out.text.contains(&secret), "{label} leaked: {}", out.text);
            assert!(out.text.contains(kept), "{label} over-masked: {}", out.text);
            assert!(
                out.hits.iter().any(|h| h.label == *label),
                "{input} expected {label}, got {:?}",
                out.hits
            );
        }
        // A dotted triple shaped like a bot token whose first segment does
        // not decode to a snowflake id.
        let not_a_snowflake = [
            "abcdefghijklmnopqrstuvwx",
            ".GaBcDe.abcdefghijklmnopqrstuvwxyz0123456789AB",
        ]
        .concat();
        let negatives = [
            "GOCSPX-short",
            "ya29.short",
            "version 1//0 of the spec",
            "glsa_short",
            "https://hooks.slack.com/services/T01ABCDEF/B02GHIJKL/short",
            "https://discord.com/api/webhooks/123456789012345678",
            "https://discord.com/api/channels/123456789012345678/messages",
            // Three dotted runs whose first segment is not a snowflake id.
            "com.example.application_identifier.SomeLongModuleName.AnotherVeryLongTypeNameHere",
            not_a_snowflake.as_str(),
        ];
        for input in negatives {
            assert_eq!(redact_secrets(input), input, "false positive on {input}");
        }
    }

    /// Credentials in structured carriers: JSON fields, CLI flags, opaque
    /// headers, cookies and percent-encoded query strings. Only the value
    /// is masked; the surrounding shape stays readable.
    #[test]
    fn redacts_structured_credential_carriers_table() {
        let cases: &[(&str, &str)] = &[
            (
                r#"{"api_key": "AbCdEf0123456789TokenValue", "model": "gpt"}"#,
                r#"{"api_key": "[REDACTED JSON Credential]", "model": "gpt"}"#,
            ),
            (
                r#"{"access_token":"AbCdEf0123456789TokenValue","token_type":"bearer"}"#,
                r#"{"access_token":"[REDACTED JSON Credential]","token_type":"bearer"}"#,
            ),
            (
                r#"{"secret":"AbCdEf0123456789TokenValue"}"#,
                r#"{"secret":"[REDACTED JSON Credential]"}"#,
            ),
            (
                "borg --api-key=AbCdEf0123456789TokenValue ask",
                "borg --api-key=[REDACTED CLI Credential Flag] ask",
            ),
            (
                "gh auth login --token AbCdEf0123456789TokenValue",
                "gh auth login --token [REDACTED CLI Credential Flag]",
            ),
            // Space-separated values that look like credentials: `_`/`-`
            // carriers and mixed case without a digit.
            (
                "vault login --token hvs_CAESIabcdefgh",
                "vault login --token [REDACTED CLI Credential Flag]",
            ),
            (
                "borg --password correctHorseBattery",
                "borg --password [REDACTED CLI Credential Flag]",
            ),
            // `=` always carries a value, even a plain word.
            (
                "borg --token=hunterhunter",
                "borg --token=[REDACTED CLI Credential Flag]",
            ),
            (
                "u=https%3A%2F%2Fy.example%2F%3Ftoken%3DAbCdEf0123456789Value%26page%3D2",
                "u=https%3A%2F%2Fy.example%2F%3Ftoken%3D[REDACTED URL Query Credential]%26page%3D2",
            ),
            (
                "u=https%3A%2F%2Fy.example%2F%3Ftoken%3DAbCdEf0123456789Value%23section",
                "u=https%3A%2F%2Fy.example%2F%3Ftoken%3D[REDACTED URL Query Credential]%23section",
            ),
            // A value with an escape other than `%26`/`%23` keeps it.
            (
                "u=https%3A%2F%2Fy.example%2F%3Ftoken%3DAbCd%2BEf0123456789Value%26page%3D2",
                "u=https%3A%2F%2Fy.example%2F%3Ftoken%3D[REDACTED URL Query Credential]%26page%3D2",
            ),
            (
                "Cookie: session=AbCdEf0123456789; theme=dark",
                "Cookie: [REDACTED Cookie Header]",
            ),
            (
                "x-auth-token: AbCdEf0123456789TokenValue",
                "x-auth-token: [REDACTED Credential Header]",
            ),
            (
                "X-Access-Token: AbCdEf0123456789TokenValue",
                "X-Access-Token: [REDACTED Credential Header]",
            ),
            (
                "Authorization: Bot MTIzNDU2Nzg5.AbCdEf.0123456789",
                "Authorization: Bot [REDACTED Authorization Header]",
            ),
            (
                "redirect to https://x.example/login?next=https%3A%2F%2Fy.example%2Fcb%3Ftoken%3DAbCdEf0123456789Value",
                "redirect to https://x.example/login?next=https%3A%2F%2Fy.example%2Fcb%3Ftoken%3D[REDACTED URL Query Credential]",
            ),
            (
                "u=https%3A%2F%2Fy.example%2F%3Fpage%3D2%26api_key%3DAbCdEf0123456789Value",
                "u=https%3A%2F%2Fy.example%2F%3Fpage%3D2%26api_key%3D[REDACTED URL Query Credential]",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_secrets(input), *expected, "input: {input}");
        }

        // Ordinary text that shares a keyword with the carriers must survive.
        let negatives = [
            r#"{"token_type": "bearer", "expires_in": 3600}"#,
            r#"{"secret": "short"}"#,
            r#"{"description": "the api_key field is required"}"#,
            "docker login --password-stdin",
            "vault read --token-file /tmp/t",
            "gh auth login --token --with-scopes",
            // Prose and help text after a flag name.
            "the --token followed by the scopes",
            "  --token  Authenticate with a personal token",
            "--password  Password for the registry",
            "Cookie: chocolate chip recipe",
            "x-auth-token-policy: strict",
            "see https://x.example/?page=2&next=%2Fhome",
        ];
        for input in negatives {
            assert_eq!(redact_secrets(input), input, "false positive on {input}");
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

#[cfg(test)]
mod credential_carrier_tests {
    use super::redact_secrets;

    /// JSON fields whose key carries a credential word at a word boundary
    /// are masked, value only, in plain and escaped-quote JSON.
    #[test]
    fn json_credential_keys_are_masked_at_word_boundaries() {
        let cases: &[(&str, &str)] = &[
            // `aws sts get-session-token`: every credential field, not just
            // the key id the AWS shape pattern already knew.
            (
                r#"{"AccessKeyId":"ASIAIOSFODNN7EXAMPLE","SecretAccessKey":"wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY","SessionToken":"IQoJb3JpZ2luX2VjEJr//////////wEaCXVzLWVhc3QtMSJHMEUCIQ"}"#,
                r#"{"AccessKeyId":"[REDACTED AWS Access Key]","SecretAccessKey":"[REDACTED JSON Credential]","SessionToken":"[REDACTED JSON Credential]"}"#,
            ),
            (
                r#"{"DB_PASSWORD":"Zx9vQ2mLp4abc"}"#,
                r#"{"DB_PASSWORD":"[REDACTED JSON Credential]"}"#,
            ),
            (
                r#"{"API_TOKEN": "Zx9vQ2mLp4abcdef"}"#,
                r#"{"API_TOKEN": "[REDACTED JSON Credential]"}"#,
            ),
            (
                r#"{"bot_token":"Zx9vQ2mLp4abcdef","chat":"general"}"#,
                r#"{"bot_token":"[REDACTED JSON Credential]","chat":"general"}"#,
            ),
            (
                r#"{"session_token":"Zx9vQ2mLp4abcdef"}"#,
                r#"{"session_token":"[REDACTED JSON Credential]"}"#,
            ),
            (
                r#"{"clientSecret":"Zx9vQ2mLp4abcdef","x-api-key":"Qw8eR7tY6uI5oP4a"}"#,
                r#"{"clientSecret":"[REDACTED JSON Credential]","x-api-key":"[REDACTED JSON Credential]"}"#,
            ),
            // Concatenated compound and a credential word mid-key.
            (
                r#"{"sessiontoken":"Zx9vQ2mLp4abcdef","password_hash":"Zx9vQ2mLp4abcdef"}"#,
                r#"{"sessiontoken":"[REDACTED JSON Credential]","password_hash":"[REDACTED JSON Credential]"}"#,
            ),
            // Docker `config.json`: the base64 `user:pass` under `auth`.
            (
                r#"{"auths":{"registry.example.com":{"auth":"Ym9iOnNlY3JldHBhc3N3b3Jk"}}}"#,
                r#"{"auths":{"registry.example.com":{"auth":"[REDACTED JSON Credential]"}}}"#,
            ),
            // A secrets-manager read: JSON escaped inside a string value.
            (
                r#"{"SecretString":"{\"password\":\"Zx9vQ2mLp4abc\"}"}"#,
                r#"{"SecretString":"{\"password\":\"[REDACTED JSON Credential]\"}"}"#,
            ),
            (
                r#"log: {\"user\":\"bob\",\"api_key\": \"Zx9vQ2mLp4abcdef\"}"#,
                r#"log: {\"user\":\"bob\",\"api_key\": \"[REDACTED JSON Credential]\"}"#,
            ),
            // OAuth token responses: an id_token is a signed JWT credential,
            // so it is masked with the access/refresh/session tokens.
            (
                r#"{"accessToken":"Zx9vQ2mLp4abcdef","refreshToken":"Qw8eR7tY6uI5oP4a"}"#,
                r#"{"accessToken":"[REDACTED JSON Credential]","refreshToken":"[REDACTED JSON Credential]"}"#,
            ),
            (
                r#"{"idToken":"Zx9vQ2mLp4abcdef","sessionToken":"Qw8eR7tY6uI5oP4a"}"#,
                r#"{"idToken":"[REDACTED JSON Credential]","sessionToken":"[REDACTED JSON Credential]"}"#,
            ),
            (
                r#"{"authorization":"Basic dXNlcjpwYXNzd29yZA=="}"#,
                r#"{"authorization":"[REDACTED JSON Credential]"}"#,
            ),
            // An escaped quote inside the value is part of the value, in
            // plain JSON and in JSON escaped inside a string.
            (
                r#"{"password": "abc\"defghijklmn"}"#,
                r#"{"password": "[REDACTED JSON Credential]"}"#,
            ),
            (
                r#"{\"password\":\"abc\\\"defghijklmn\",\"user\":\"bob\"}"#,
                r#"{\"password\":\"[REDACTED JSON Credential]\",\"user\":\"bob\"}"#,
            ),
            // Other doubled escapes inside the inner value (`\\n`, `\\/`,
            // `\\u00e9`, an inner backslash `\\\\`) stay part of it.
            (
                r#"{"SecretString":"{\"password\":\"line1\\nline2secretvalue\"}"}"#,
                r#"{"SecretString":"{\"password\":\"[REDACTED JSON Credential]\"}"}"#,
            ),
            (
                r#"{"SecretString":"{\"password\":\"https:\\/\\/secretvalue\",\"user\":\"bob\"}"}"#,
                r#"{"SecretString":"{\"password\":\"[REDACTED JSON Credential]\",\"user\":\"bob\"}"}"#,
            ),
            (
                r#"{"SecretString":"{\"password\":\"caf\\u00e9secretvalue\"}"}"#,
                r#"{"SecretString":"{\"password\":\"[REDACTED JSON Credential]\"}"}"#,
            ),
            (
                r#"{"SecretString":"{\"password\":\"abcdefgh\\\\\",\"user\":\"bob\"}"}"#,
                r#"{"SecretString":"{\"password\":\"[REDACTED JSON Credential]\",\"user\":\"bob\"}"}"#,
            ),
            // An escaped object under a credential key: when an inner field
            // names a credential, only that field is masked; when none does
            // (or its value is too short to mask), the outer value is masked
            // whole, so nothing in it reaches the model in the clear.
            (
                r#"{"client_secret":"{\"password\":\"Hunter2Hunter2Hunter2\",\"user\":\"bob\"}"}"#,
                r#"{"client_secret":"{\"password\":\"[REDACTED JSON Credential]\",\"user\":\"bob\"}"}"#,
            ),
            (
                r#"{"client_secret":"{\"k\":\"Hunter2Hunter2Hunter2\"}"}"#,
                r#"{"client_secret":"[REDACTED JSON Credential]"}"#,
            ),
            (
                r#"{"client_secret":"{\"password\":\"short\",\"k\":\"Hunter2Hunter2\"}"}"#,
                r#"{"client_secret":"[REDACTED JSON Credential]"}"#,
            ),
            (
                r#"{"SecretString":"{\"username\":\"bob\",\"host\":\"db.example.com\"}"}"#,
                r#"{"SecretString":"[REDACTED JSON Credential]"}"#,
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_secrets(input), *expected, "input: {input}");
        }

        // A keyword embedded mid-word, a metadata key, a short value and a
        // non-string value all stay readable.
        let negatives = [
            r#"{"tokenizer":"gpt2-tokenizer-fast"}"#,
            r#"{"author":"Robert Smithson"}"#,
            r#"{"authority":"https://login.microsoftonline.com/common"}"#,
            r#"{"passwordless":"enabled-for-everyone"}"#,
            r#"{"oauth":"disabled-entirely"}"#,
            r#"{"max_tokens":1024,"prompt_tokens":"123"}"#,
            r#"{"token_count":"12"}"#,
            r#"{"token_type":"refresh_grant_v2","expires_in":3600}"#,
            r#"{"auth_uri":"https://accounts.google.com/o/oauth2/auth","token_uri":"https://oauth2.googleapis.com/token"}"#,
            r#"{"secretName":"prod-db-credentials","password_file":"/run/secrets/db"}"#,
            r#"{\"tokenizer\":\"gpt2-tokenizer-fast\"}"#,
            r#"{\"password\":\"short\"}"#,
            // Pagination and sync cursors are not credentials.
            r#"{"items":[],"nextPageToken":"CiAKGjBpNDd2Nmp2Zml2cXRwYjBpOXA0dmowMhIB"}"#,
            r#"{"page_token":"eyJvZmZzZXQiOjEwMH0"}"#,
            r#"{"continuationToken":"1-AbCdEf0123456789"}"#,
            r#"{"cursorToken":"Y3Vyc29yOjEyMzQ1Njc4"}"#,
            r#"{"syncToken":"CPDAlvWDx70CEPDAlvWDx70CGAU="}"#,
            r#"{"resumeToken":"8263A1B2C3D4E5F60000"}"#,
            r#"{"NextToken":"AbCdEf0123456789TokenValue"}"#,
            // OAuth discovery metadata names endpoints, not credentials.
            r#"{"authorization_url":"https://idp.example/authorize"}"#,
            r#"{"authorizationEndpoint":"https://idp.example/oauth2/authorize"}"#,
            // An escaped JSON object under a key that names no credential.
            r#"{"Payload":"{\"username\":\"bob\",\"host\":\"db.example.com\"}"}"#,
        ];
        for input in negatives {
            assert_eq!(redact_secrets(input), input, "false positive on {input}");
        }
    }

    /// netrc and npmrc credential lines: only the value is masked.
    #[test]
    fn credential_store_formats_mask_the_value_only() {
        let cases: &[(&str, &str)] = &[
            (
                "machine api.example.com login bob password hunter2secretpass",
                "machine api.example.com login bob password [REDACTED netrc Password]",
            ),
            (
                "machine api.example.com\n  login bob\n  password hunter2secretpass\nmachine b.example login x",
                "machine api.example.com\n  login bob\n  password [REDACTED netrc Password]\nmachine b.example login x",
            ),
            (
                "machine 10.0.0.5 login root password hunter2secretpass",
                "machine 10.0.0.5 login root password [REDACTED netrc Password]",
            ),
            (
                "machine localhost:8080 login dev password hunter2secretpass",
                "machine localhost:8080 login dev password [REDACTED netrc Password]",
            ),
            (
                "default login anonymous password guest2024pw",
                "default login anonymous password [REDACTED netrc Password]",
            ),
            (
                "machine h.example account acct login bob password pw",
                "machine h.example account acct login bob password [REDACTED netrc Password]",
            ),
            (
                // Split so the fake token never sits whole in the source,
                // where forge push protection reads it as a real one.
                concat!(
                    "//registry.example.com/:_authToken=5f3c2a1b-",
                    "9d8e-4f00-a1b2-c3d4e5f6a7b8"
                ),
                "//registry.example.com/:_authToken=[REDACTED npmrc Credential]",
            ),
            (
                "//reg.example:8443/npm/:_auth=Ym9iOnNlY3JldHBhc3M=\nalways-auth=true",
                "//reg.example:8443/npm/:_auth=[REDACTED npmrc Credential]\nalways-auth=true",
            ),
            (
                "//reg/:username=bob\n//reg/:_password=c2VjcmV0",
                "//reg/:username=bob\n//reg/:_password=[REDACTED npmrc Credential]",
            ),
            (
                "registry=https://r.example/\n_auth=Ym9iOnNlY3JldHBhc3M=",
                "registry=https://r.example/\n_auth=[REDACTED npmrc Credential]",
            ),
            (
                "  _authToken = npm_AbCdEf0123456789TokenValue\r\nemail=a@b.example",
                "  _authToken = [REDACTED npmrc Credential]\r\nemail=a@b.example",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_secrets(input), *expected, "input: {input:?}");
        }

        let negatives = [
            // An env reference keeps the secret out of the file.
            "//registry.npmjs.org/:_authToken=${NPM_TOKEN}",
            "registry=https://registry.npmjs.org/",
            "the build machine asked for a password twice",
            "machine learning models never see a password reset link",
            "please login with your password manager",
            // netrc-like prose: the machine value is not a host, and
            // `default` does not open the line.
            "Change the default login page password field",
            "machine learning: login once, password reset after",
            // Code and comparisons with a bare npmrc key name.
            "    _password = getpass.getpass()",
            "_auth == other_value",
            "_auth = settings.registryAuth",
            "_authToken = os.environ['NPM_TOKEN']",
        ];
        for input in negatives {
            assert_eq!(redact_secrets(input), input, "false positive on {input}");
        }
    }

    /// A credential assignment whose value is a dotted call or index
    /// expression is source code and stays readable; a literal value that
    /// merely contains parentheses is still masked.
    #[test]
    fn credential_assignment_skips_code_expression_values() {
        let negatives = [
            "    _password = getpass.getpass()",
            "api_key = os.environ['OPENAI_API_KEY']",
            "API_KEY=os.getenv(\"API_KEY\")",
            "NPM_TOKEN=os.environ[\"NPM_TOKEN\"];",
            "self.token = client.auth.fetch_token(scope)",
            "SECRET_KEY=os.environ.get(\"SECRET_KEY\").strip()",
            "auth_token = self.vault.read(cfg.path)[key]",
        ];
        for input in negatives {
            assert_eq!(redact_secrets(input), input, "false positive on {input}");
        }

        let cases: &[(&str, &str)] = &[
            (
                "DB_PASSWORD=Hunter2(!)xyz",
                "DB_PASSWORD=[REDACTED Credential Assignment]",
            ),
            (
                "API_SECRET=abc.def.Zx9vQ2mLp4",
                "API_SECRET=[REDACTED Credential Assignment]",
            ),
            // A call whose argument is a literal secret, or a default value
            // beside the env-var name, is not plain code.
            (
                "DB_PASSWORD=os.getenv(\"DB_PASSWORD\",\"Hunter2Secret!\")",
                "DB_PASSWORD=[REDACTED Credential Assignment]",
            ),
            (
                "TOKEN=base64.b64decode(\"c2stbGl2ZS1hYmNkZWY=\")",
                "TOKEN=[REDACTED Credential Assignment]",
            ),
            (
                "API_KEY=os.getenv(\"API_KEY\",\"OTHER_KEY\")",
                "API_KEY=[REDACTED Credential Assignment]",
            ),
            (
                "api_token = client.login('Zx9vQ2mLp4abc')",
                "api_token = [REDACTED Credential Assignment]",
            ),
            (
                "SECRET=cfg.get(\"SECRET\")Hunter2tail",
                "SECRET=[REDACTED Credential Assignment]",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_secrets(input), *expected, "input: {input}");
        }
    }

    /// A Telegram bot token after an ANSI color code or an escape spelling
    /// is masked, and the lead-in stays visible.
    #[test]
    fn telegram_token_after_escape_lead_ins_is_masked() {
        let token = ["123456789:", "AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsawX"].concat();
        let lead_ins = [
            "\x1b[32m",
            "\x1b[1;32m",
            "\x1b[32mbot",
            r"\x1b[32m",
            r"\x1b[32mbot",
            r"\nbot",
            r"\u001b[0m",
            r"\033[1m",
            r"\n",
            r"\t",
            "%0A",
            "ok ",
        ];
        for lead in lead_ins {
            let input = format!("{lead}{token}\x1b[0m done");
            let expected = format!("{lead}[REDACTED Telegram Bot Token]\x1b[0m done");
            assert_eq!(redact_secrets(&input), expected, "lead-in {lead:?}");
        }

        // A letter glued to the digits is still not a lead-in, and a
        // `<timestamp>:<40-hex sha>` pair after a color code stays readable.
        let negatives = [
            format!("abc{token}"),
            format!(
                "\x1b[32m1700000000:{}",
                "0123456789abcdef0123456789abcdef01234567"
            ),
        ];
        for input in negatives {
            assert_eq!(redact_secrets(&input), input, "false positive on {input:?}");
        }
    }
}
