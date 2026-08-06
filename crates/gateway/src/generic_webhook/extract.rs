//! HMAC verification + JSON dot-path extraction.

use anyhow::{anyhow, Context, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Maximum permitted clock skew between client and server timestamps,
/// in seconds. Same value as Slack's signing-secret window.
pub const REPLAY_WINDOW_SECONDS: i64 = 300;

/// Reasons HMAC verification can fail. Distinct so the route layer
/// can produce specific HTTP status codes (401 vs. 400 vs. 413) and
/// the audit chain captures the failure mode.
#[derive(Debug, PartialEq, Eq)]
pub enum HmacError {
    /// Header was missing entirely (`X-Borg-Signature`/`X-Borg-Timestamp`).
    MissingHeader(&'static str),
    /// Header was present but malformed.
    MalformedHeader(&'static str),
    /// Timestamp older or newer than the replay window allows.
    Replay { skew_seconds: i64 },
    /// HMAC computed locally did not match the presented signature.
    Mismatch,
    /// Configured signing secret was empty — refused to verify.
    EmptySecret,
}

impl std::fmt::Display for HmacError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HmacError::MissingHeader(h) => write!(f, "missing required header `{h}`"),
            HmacError::MalformedHeader(h) => write!(f, "malformed header `{h}`"),
            HmacError::Replay { skew_seconds } => write!(
                f,
                "timestamp outside ±{REPLAY_WINDOW_SECONDS}s replay window (skew {skew_seconds}s)"
            ),
            HmacError::Mismatch => f.write_str("HMAC signature mismatch"),
            HmacError::EmptySecret => f.write_str("signing secret is empty — refusing to verify"),
        }
    }
}

impl std::error::Error for HmacError {}

/// Verify a Borg-signed webhook payload.
///
/// Signature is computed as `HMAC-SHA256(secret, "{ts}.{body}")` and
/// passed in `X-Borg-Signature` as `v1=<hex>`. The timestamp lives in
/// `X-Borg-Timestamp`. Constant-time compare via the `hmac::Mac::verify`
/// API.
pub fn verify_signature(
    secret: &str,
    body: &[u8],
    signature_header: Option<&str>,
    timestamp_header: Option<&str>,
    now_unix: i64,
) -> Result<(), HmacError> {
    // Empty secret is misconfiguration — refuse rather than computing an HMAC
    // under `""` that any caller could forge.
    if secret.is_empty() {
        return Err(HmacError::EmptySecret);
    }
    let signature = signature_header.ok_or(HmacError::MissingHeader("X-Borg-Signature"))?;
    let timestamp = timestamp_header.ok_or(HmacError::MissingHeader("X-Borg-Timestamp"))?;

    let ts: i64 = timestamp
        .parse()
        .map_err(|_| HmacError::MalformedHeader("X-Borg-Timestamp"))?;
    let skew = (now_unix - ts).abs();
    if skew > REPLAY_WINDOW_SECONDS {
        return Err(HmacError::Replay { skew_seconds: skew });
    }

    let hex = signature
        .strip_prefix("v1=")
        .ok_or(HmacError::MalformedHeader("X-Borg-Signature"))?;
    let presented = decode_hex(hex).ok_or(HmacError::MalformedHeader("X-Borg-Signature"))?;

    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| HmacError::MalformedHeader("X-Borg-Signature"))?;
    mac.update(format!("{ts}.").as_bytes());
    mac.update(body);
    mac.verify_slice(&presented)
        .map_err(|_| HmacError::Mismatch)?;
    Ok(())
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

/// Extract a string from `payload` at the given JSON dot-path.
///
/// Path syntax (v1 subset of JSONPath):
/// - `$.foo` — top-level key.
/// - `$.foo.bar` — nested object field.
/// - `$.foo[0]` — array index.
/// - `$` — the whole payload (when it's already a string).
///
/// Filter expressions, recursive descent, and slices are not supported
/// in v1.
pub fn extract_string(payload: &serde_json::Value, path: &str) -> Result<String> {
    let value = navigate(payload, path)?;
    match value {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        serde_json::Value::Bool(b) => Ok(b.to_string()),
        serde_json::Value::Null => Err(anyhow!("path `{path}` resolved to null")),
        _ => Err(anyhow!(
            "path `{path}` resolved to a {} (expected string-coercible)",
            type_name(value)
        )),
    }
}

/// Like [`extract_string`] but returns `None` if the path is missing.
/// Used for optional fields like `thread_id`.
pub fn extract_string_opt(payload: &serde_json::Value, path: &str) -> Option<String> {
    extract_string(payload, path).ok()
}

fn navigate<'a>(payload: &'a serde_json::Value, path: &str) -> Result<&'a serde_json::Value> {
    let trimmed = path.trim();
    if trimmed == "$" || trimmed.is_empty() {
        return Ok(payload);
    }
    let stripped = trimmed
        .strip_prefix("$.")
        .or_else(|| trimmed.strip_prefix('$'))
        .with_context(|| format!("path `{path}` must start with `$`"))?;

    let mut cursor = payload;
    for segment in split_segments(stripped) {
        match segment {
            Segment::Key(k) => {
                cursor = cursor
                    .get(k)
                    .with_context(|| format!("path `{path}` missing key `{k}`"))?;
            }
            Segment::Index(i) => {
                cursor = cursor
                    .get(i)
                    .with_context(|| format!("path `{path}` index {i} out of bounds"))?;
            }
        }
    }
    Ok(cursor)
}

#[derive(Debug, PartialEq, Eq)]
enum Segment<'a> {
    Key(&'a str),
    Index(usize),
}

fn split_segments(s: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    for part in s.split('.') {
        if part.is_empty() {
            continue;
        }
        // Split off trailing `[N]` index suffixes: `users[0]` → key
        // `users`, then index `0`.
        let mut head_end = part.len();
        let mut indices: Vec<usize> = Vec::new();
        while let Some(open) = part[..head_end].rfind('[') {
            let close = part[..head_end].rfind(']');
            match close {
                Some(c) if c == head_end - 1 && c > open => {
                    if let Ok(i) = part[open + 1..c].parse::<usize>() {
                        indices.push(i);
                        head_end = open;
                        continue;
                    }
                    break;
                }
                _ => break,
            }
        }
        let head = &part[..head_end];
        if !head.is_empty() {
            out.push(Segment::Key(head));
        }
        // Indices were appended innermost-first; reverse to outermost-first.
        for i in indices.into_iter().rev() {
            out.push(Segment::Index(i));
        }
    }
    out
}

fn type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_signature(secret: &str, ts: i64, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac");
        mac.update(format!("{ts}.").as_bytes());
        mac.update(body);
        let bytes = mac.finalize().into_bytes();
        let hex = cortex::util::hex::hex_encode(&bytes);
        format!("v1={hex}")
    }

    #[test]
    fn valid_signature_passes_within_window() {
        // Round-trip: a freshly computed HMAC over `{ts}.{body}` must
        // verify when presented within the 5-minute window.
        let secret = "test-secret";
        let body = b"{\"hello\":\"world\"}";
        let now: i64 = 1_700_000_000;
        let sig = make_signature(secret, now, body);
        verify_signature(secret, body, Some(&sig), Some(&now.to_string()), now)
            .expect("valid sig must verify");
    }

    #[test]
    fn wrong_secret_rejected_as_mismatch() {
        // Attacker without the secret cannot forge a signature.
        let now: i64 = 1_700_000_000;
        let body = b"payload";
        let sig = make_signature("real-secret", now, body);
        let err = verify_signature(
            "wrong-secret",
            body,
            Some(&sig),
            Some(&now.to_string()),
            now,
        )
        .expect_err("must reject");
        assert_eq!(err, HmacError::Mismatch);
    }

    #[test]
    fn tampered_body_rejected_as_mismatch() {
        // Even with the right secret + signature, a body modified in
        // flight must fail verification — that's the whole point.
        let secret = "s";
        let now: i64 = 1_700_000_000;
        let sig = make_signature(secret, now, b"original");
        let err = verify_signature(secret, b"tampered", Some(&sig), Some(&now.to_string()), now)
            .expect_err("must reject");
        assert_eq!(err, HmacError::Mismatch);
    }

    #[test]
    fn replay_outside_window_rejected() {
        // A correctly-signed request from 6 minutes ago must be
        // rejected as a replay candidate.
        let secret = "s";
        let body = b"x";
        let signed_at: i64 = 1_700_000_000;
        let now = signed_at + 360;
        let sig = make_signature(secret, signed_at, body);
        let err = verify_signature(secret, body, Some(&sig), Some(&signed_at.to_string()), now)
            .expect_err("must reject replay");
        match err {
            HmacError::Replay { skew_seconds } => assert_eq!(skew_seconds, 360),
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    #[test]
    fn missing_headers_rejected() {
        // The 400-vs-401 distinction — missing headers is malformed
        // input (400), wrong sig is unauthorized (401). The route
        // layer maps both via this enum.
        let now: i64 = 0;
        assert_eq!(
            verify_signature("s", b"", None, Some("0"), now).unwrap_err(),
            HmacError::MissingHeader("X-Borg-Signature")
        );
        assert_eq!(
            verify_signature("s", b"", Some("v1=00"), None, now).unwrap_err(),
            HmacError::MissingHeader("X-Borg-Timestamp")
        );
    }

    #[test]
    fn malformed_signature_format_rejected() {
        // No `v1=` prefix, non-hex characters: malformed, not a
        // mismatch — operators debugging integrations need this
        // distinction.
        let now: i64 = 1_700_000_000;
        let err = verify_signature("s", b"x", Some("not-prefixed"), Some(&now.to_string()), now)
            .unwrap_err();
        assert_eq!(err, HmacError::MalformedHeader("X-Borg-Signature"));

        let err =
            verify_signature("s", b"x", Some("v1=zzzz"), Some(&now.to_string()), now).unwrap_err();
        assert_eq!(err, HmacError::MalformedHeader("X-Borg-Signature"));
    }

    #[test]
    fn empty_secret_refused() {
        // Regression: an empty secret used to mean "verify this request
        // under HMAC(\"\", body)" — any caller who knew the (zero-effort)
        // algorithm could forge a signature. `EmptySecret` is a distinct
        // error so the audit chain captures the cause.
        let now: i64 = 1_700_000_000;
        let sig = make_signature("", now, b"body");
        let err =
            verify_signature("", b"body", Some(&sig), Some(&now.to_string()), now).unwrap_err();
        assert_eq!(err, HmacError::EmptySecret);
    }

    #[test]
    fn extract_string_handles_nested_keys_and_indices() {
        // Realistic upstream payload: GitHub-like webhook with nested
        // `repository.owner.login` and an array of commits. The
        // dot-path subset must handle both forms.
        let payload = serde_json::json!({
            "repository": {"owner": {"login": "octocat"}},
            "commits": [{"id": "abc"}, {"id": "def"}],
            "count": 42,
            "active": true
        });
        assert_eq!(
            extract_string(&payload, "$.repository.owner.login").unwrap(),
            "octocat"
        );
        assert_eq!(extract_string(&payload, "$.commits[0].id").unwrap(), "abc");
        assert_eq!(extract_string(&payload, "$.commits[1].id").unwrap(), "def");
        // Coercion: numbers and booleans stringify so users can use
        // arbitrary fields as `sender_id`.
        assert_eq!(extract_string(&payload, "$.count").unwrap(), "42");
        assert_eq!(extract_string(&payload, "$.active").unwrap(), "true");
    }

    #[test]
    fn extract_string_errors_on_missing_path() {
        // Required-field misses must surface as errors (the route
        // layer translates to 400) rather than empty strings — empty
        // string is the gate's reject-empty signal which would
        // produce the wrong error message.
        let payload = serde_json::json!({"a": {"b": 1}});
        let err = extract_string(&payload, "$.a.missing").unwrap_err();
        assert!(err.to_string().contains("missing key"));
        let err = extract_string(&payload, "$.a.b.c").unwrap_err();
        // "missing key" because b is a number, get("c") returns None.
        assert!(err.to_string().contains("missing key"));
    }

    #[test]
    fn extract_string_opt_returns_none_silently_on_miss() {
        // Optional thread_id: missing path is a normal case, not an
        // error — webhooks can omit the field entirely.
        let payload = serde_json::json!({"a": 1});
        assert_eq!(extract_string_opt(&payload, "$.a"), Some("1".to_string()));
        assert_eq!(extract_string_opt(&payload, "$.missing"), None);
    }

    #[test]
    fn root_path_returns_whole_payload_when_string() {
        // A degenerate but legal config — webhook secret directly
        // sends a JSON string body. `$` selects the root.
        let payload = serde_json::json!("hello");
        assert_eq!(extract_string(&payload, "$").unwrap(), "hello");
    }

    #[test]
    fn array_index_out_of_bounds_errors() {
        let payload = serde_json::json!({"xs": [1, 2]});
        let err = extract_string(&payload, "$.xs[5]").unwrap_err();
        assert!(err.to_string().contains("out of bounds"));
    }
}
