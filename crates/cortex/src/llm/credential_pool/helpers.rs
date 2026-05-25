//! Pure helpers for the credential pool: fingerprinting, URL safety,
//! random IDs, error-message truncation, refresh-deadline arithmetic, and
//! the two hard ceilings (`COOLDOWN_MAX_SECS`, `ERROR_MESSAGE_MAX_LEN`).
//!
//! Lives in its own module because none of these touch `PoolInner` or any
//! other `credential_pool`-internal type; keeping them adjacent to the
//! 1350-line `impl CredentialPool` block forces every reader of that block
//! to scroll past pure helpers to follow the resolver flow.

use rand::RngCore;

/// Hard ceiling for any cooldown duration — defends against absurd provider
/// hints like `retry_after: 999999999` (yes, it happens).
pub(super) const COOLDOWN_MAX_SECS: u64 = 24 * 60 * 60;

/// Truncate persisted error messages to keep `borg auth list` and daemon.log bounded.
pub(super) const ERROR_MESSAGE_MAX_LEN: usize = 500;

/// True when `expires_at_ms` is missing or within 60 seconds of `now_ms`.
/// Missing expiry treated as "needs refresh" so a legacy persisted row with
/// stale state forces one round-trip rather than gambling on a dead token.
pub(super) fn needs_refresh(expires_at_ms: Option<i64>, now_ms: i64) -> bool {
    match expires_at_ms {
        None => true,
        Some(exp) => exp <= now_ms + 60_000,
    }
}

/// 16-hex blake3 prefix used as the stable identifier for an API key.
///
/// Truncating to 16 hex (64 bits) keeps the `borg auth list` ID column
/// readable while still giving us ~2^32 collision headroom across an
/// operator's lifetime — comfortably more than the few-dozen credentials
/// a real deployment ever holds.
pub fn fingerprint(key: &str) -> String {
    let hash = blake3::hash(key.as_bytes());
    let full = hash.to_hex();
    full[..16].to_string()
}

/// Validate that `url` is safe to use as a per-credential `base_url` override.
///
/// Accepts: `https://<any-host>` and `http://<loopback-host>`. Rejects
/// anything else — schemes other than http(s) (file/ftp/gopher), plain
/// `http://` to non-loopback hosts (clear-text bearer over the wire), or
/// strings that don't parse as a URL.
///
/// The threat model: anyone with local-user write access to
/// `~/.qwen/oauth_creds.json` (or a future operator-supplied custom-endpoint
/// file) could otherwise redirect the bearer token to an attacker host on
/// next pool reload. They already have code-exec at that point, but the
/// validation closes the obvious "swap one URL string" vector and surfaces
/// a clear error to the operator instead of silently exfiltrating tokens.
pub fn is_safe_base_url(url: &str) -> bool {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return false,
    };
    match parsed.scheme() {
        "https" => true,
        "http" => parsed
            .host_str()
            .map(|h| {
                h == "localhost"
                    || h == "127.0.0.1"
                    || h == "[::1]"
                    || h == "::1"
                    || h.starts_with("127.")
            })
            .unwrap_or(false),
        _ => false,
    }
}

/// 8-hex random identifier used for new credential rows.
///
/// 4 bytes ≈ 4.3B values — matches the `borg auth list` ID column truncate
/// and gives `resolve_target`'s 4-hex-prefix matcher real headroom.
pub(super) fn random_id() -> String {
    let mut rng = rand::rng();
    let mut buf = [0u8; 4];
    rng.fill_bytes(&mut buf);
    const TBL: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(8);
    for b in buf {
        out.push(TBL[(b >> 4) as usize] as char);
        out.push(TBL[(b & 0xf) as usize] as char);
    }
    out
}

/// Redact secrets out of an upstream error body then clamp to
/// `ERROR_MESSAGE_MAX_LEN` so persisted `last_error_message` doesn't bloat
/// `borg auth list` or shell history.
pub(super) fn truncate_error_msg(s: &str) -> String {
    // Redact first — upstream error bodies occasionally echo the bearer
    // token tail back, and `last_error_message` ends up on operator
    // terminals via `borg auth list` (and into shell history).
    let redacted = crate::security::secrets::redact_secrets(s);
    if redacted.len() <= ERROR_MESSAGE_MAX_LEN {
        return redacted;
    }
    let mut end = ERROR_MESSAGE_MAX_LEN;
    while !redacted.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &redacted[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_safe_base_url_blocks_dangerous_schemes() {
        // https — always allowed.
        assert!(is_safe_base_url("https://api.example.com/v1"));
        // http to loopback — allowed (local Ollama, dev mock servers).
        assert!(is_safe_base_url("http://localhost:8080/v1"));
        assert!(is_safe_base_url("http://127.0.0.1:11434/v1"));
        assert!(is_safe_base_url("http://[::1]/v1"));
        // http to remote — clear-text bearer over the wire, blocked.
        assert!(!is_safe_base_url("http://evil.com/exfil"));
        // file:// and other schemes — always blocked.
        assert!(!is_safe_base_url("file:///etc/passwd"));
        assert!(!is_safe_base_url("ftp://example.com"));
        // Garbage — `url::parse` rejects, we reject.
        assert!(!is_safe_base_url("not a url"));
        assert!(!is_safe_base_url(""));
    }

    #[test]
    fn fingerprint_is_stable_and_short() {
        // Same input -> same fingerprint (deterministic).
        let f1 = fingerprint("sk-test-1234567890");
        let f2 = fingerprint("sk-test-1234567890");
        assert_eq!(f1, f2);
        // 16 hex chars exactly — the `borg auth list` width contract.
        assert_eq!(f1.len(), 16);
        assert!(f1.chars().all(|c| c.is_ascii_hexdigit()));
        // Different inputs collide with vanishingly small probability;
        // assert two near-identical keys differ.
        assert_ne!(fingerprint("sk-a"), fingerprint("sk-b"));
    }

    #[test]
    fn random_id_is_8_lowercase_hex() {
        for _ in 0..32 {
            let id = random_id();
            assert_eq!(id.len(), 8);
            assert!(id
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        }
    }

    #[test]
    fn truncate_error_msg_clamps_long_input() {
        let huge = "x".repeat(ERROR_MESSAGE_MAX_LEN + 1000);
        let out = truncate_error_msg(&huge);
        assert!(out.len() <= ERROR_MESSAGE_MAX_LEN + "…[truncated]".len() + 4);
        assert!(out.ends_with("…[truncated]"));
    }

    #[test]
    fn truncate_error_msg_passes_short_input() {
        let short = "Bad credentials";
        let out = truncate_error_msg(short);
        assert_eq!(out, short);
        assert!(!out.contains("[truncated]"));
    }

    #[test]
    fn needs_refresh_missing_expiry_returns_true() {
        // A persisted row with no expiry is treated as "refresh now" so we
        // don't gamble on a dead token surviving across daemon restarts.
        assert!(needs_refresh(None, 1_700_000_000_000));
    }

    #[test]
    fn needs_refresh_within_60s_window_returns_true() {
        let now = 1_700_000_000_000;
        // Already past.
        assert!(needs_refresh(Some(now - 1), now));
        // Within the 60s safety window.
        assert!(needs_refresh(Some(now + 30_000), now));
        assert!(needs_refresh(Some(now + 60_000), now));
    }

    #[test]
    fn needs_refresh_well_in_future_returns_false() {
        let now = 1_700_000_000_000;
        // 61s in future — just past the safety window.
        assert!(!needs_refresh(Some(now + 60_001), now));
        // 1h in future.
        assert!(!needs_refresh(Some(now + 3_600_000), now));
    }
}
