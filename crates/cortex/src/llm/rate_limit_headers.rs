//! Provider rate-limit header parsing.
//!
//! Modern LLM providers emit per-bucket headroom on every response — both
//! successful 2xx and 429s. Parsing those headers lets the credential pool
//! skip near-empty keys *before* the next request lands on the same bucket
//! and triggers a hard 429.
//!
//! Two flavors are supported here:
//!
//! - **OpenAI-style** (OpenAI, OpenRouter, Azure, Groq, xAI, Gemini-via-compat,
//!   DeepSeek, OpenAI/Copilot OAuth, user-defined Custom endpoints): the
//!   `x-ratelimit-{limit,remaining,reset}-{requests,tokens}` family.
//!   Reset values arrive either as a Unix-epoch timestamp (OpenRouter) or
//!   as a compound duration string like `6m0s` / `1h30m` (OpenAI).
//! - **Anthropic-style** (Anthropic API + ClaudeCode OAuth): the
//!   `anthropic-ratelimit-{requests,tokens,input-tokens,output-tokens}-{limit,remaining,reset}`
//!   family. Reset values are RFC 3339 timestamps. We treat
//!   `input-tokens` + `output-tokens` as a single combined bucket — whichever
//!   caps first determines the headroom.
//!
//! Local-only providers (Bedrock via AWS SigV4, ClaudeCli subprocess, Ollama)
//! return `None`. They either don't speak HTTP rate-limit headers or run on
//! the operator's machine where rate limits don't apply.
//!
//! Missing or malformed headers degrade gracefully: if `remaining` parses but
//! `reset` doesn't, the bucket is dropped — we'd rather not act on partial
//! state than guess a reset window. A snapshot with neither bucket populated
//! returns `None` so callers can short-circuit.

use chrono::DateTime;
use reqwest::header::HeaderMap;

use crate::llm::provider::Provider;

/// One observation of the provider's rate-limit state for a credential.
///
/// Held in memory only — snapshots expire well before any realistic restart
/// window, so persistence would just mean acting on stale data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitSnapshot {
    /// Wall-clock ms when these headers were captured. Used to detect a
    /// snapshot that's older than its own `reset_at_ms` — at that point the
    /// bucket has already reset and the snapshot is stale.
    pub captured_at_ms: i64,
    /// Per-minute (or per-window) requests bucket, if the provider reported one.
    pub requests: Option<Bucket>,
    /// Tokens bucket. For Anthropic this is `min(input_tokens, output_tokens)`
    /// when both are reported — whichever runs out first is the binding limit.
    pub tokens: Option<Bucket>,
}

/// A single rate-limit bucket. `limit` is total capacity; `remaining` is the
/// headroom after the response we captured these headers from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub limit: u64,
    pub remaining: u64,
    /// Wall-clock ms when this bucket resets.
    pub reset_at_ms: i64,
}

impl Bucket {
    /// Headroom percentage as an integer 0..=100. `limit == 0` returns 100
    /// (treat unknown-limit as "full" — we'd rather miss a rotation than
    /// rotate spuriously off a bogus zero limit).
    pub fn remaining_pct(&self) -> u8 {
        if self.limit == 0 {
            return 100;
        }
        let pct = (self.remaining.saturating_mul(100)) / self.limit;
        pct.min(100) as u8
    }
}

impl RateLimitSnapshot {
    /// True when this snapshot is no longer informative because its bucket
    /// reset has already passed — the provider's headroom is fresh again.
    pub fn is_stale(&self, now_ms: i64) -> bool {
        let req_done = self
            .requests
            .map(|b| b.reset_at_ms <= now_ms)
            .unwrap_or(true);
        let tok_done = self.tokens.map(|b| b.reset_at_ms <= now_ms).unwrap_or(true);
        req_done && tok_done
    }
}

/// Parse provider rate-limit headers into a snapshot, or `None` when the
/// provider doesn't speak these headers (Bedrock, ClaudeCli, Ollama) or when
/// no recognizable bucket headers are present.
pub fn parse_rate_limit_headers(
    provider: Provider,
    headers: &HeaderMap,
    now_ms: i64,
) -> Option<RateLimitSnapshot> {
    let (requests, tokens) = match provider {
        // Local / non-HTTP / non-rate-limited providers. Even if a proxy
        // *injects* OpenAI-style headers in front of Bedrock, we don't act
        // on them — those proxies are out of scope and we'd rather not be
        // fooled into rotating an AWS principal because of a header we
        // didn't ask for.
        Provider::Bedrock | Provider::ClaudeCli | Provider::Ollama => (None, None),
        Provider::Anthropic | Provider::ClaudeCodeOAuth => parse_anthropic(headers, now_ms),
        _ => parse_openai_style(headers, now_ms),
    };

    if requests.is_none() && tokens.is_none() {
        return None;
    }
    Some(RateLimitSnapshot {
        captured_at_ms: now_ms,
        requests,
        tokens,
    })
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// Which way a fractional header value is rounded.
#[derive(Clone, Copy)]
enum Rounding {
    /// Toward zero — used for `remaining`, so we never claim headroom the
    /// provider did not grant.
    Down,
    /// Away from zero — used for `limit`, since a larger denominator lowers
    /// [`Bucket::remaining_pct`].
    Up,
}

/// Parse a rate-limit count that may arrive fractional.
///
/// Providers usually send integers, but some report a computed allowance such
/// as `999.5`. A strict integer parse rejected those, and [`build_bucket`]
/// dropped the whole bucket — silently disabling headroom-aware routing for
/// that credential exactly when it mattered. The reset parser in this module
/// already accepted fractional durations, so this closes an inconsistency too.
///
/// Both rounding directions push `remaining_pct` *down*, so a fractional
/// header can only make the pool more cautious, never less. Genuinely
/// unparseable input still yields `None` and drops that bucket alone.
fn parse_count(s: &str, rounding: Rounding) -> Option<u64> {
    let s = s.trim();
    // Integer first: f64 cannot represent every u64 exactly, and a provider
    // reporting a very large token budget deserves the exact value.
    if let Ok(exact) = s.parse::<u64>() {
        return Some(exact);
    }
    let value = s.parse::<f64>().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let rounded = match rounding {
        Rounding::Down => value.floor(),
        Rounding::Up => value.ceil(),
    };
    // Float-to-int casts saturate, so an out-of-range value clamps to u64::MAX
    // rather than wrapping.
    Some(rounded as u64)
}

fn parse_openai_style(headers: &HeaderMap, now_ms: i64) -> (Option<Bucket>, Option<Bucket>) {
    let requests = build_bucket(
        header_str(headers, "x-ratelimit-limit-requests"),
        header_str(headers, "x-ratelimit-remaining-requests"),
        header_str(headers, "x-ratelimit-reset-requests"),
        now_ms,
        parse_openai_reset,
    );
    let tokens = build_bucket(
        header_str(headers, "x-ratelimit-limit-tokens"),
        header_str(headers, "x-ratelimit-remaining-tokens"),
        header_str(headers, "x-ratelimit-reset-tokens"),
        now_ms,
        parse_openai_reset,
    );
    (requests, tokens)
}

fn parse_anthropic(headers: &HeaderMap, now_ms: i64) -> (Option<Bucket>, Option<Bucket>) {
    let requests = build_bucket(
        header_str(headers, "anthropic-ratelimit-requests-limit"),
        header_str(headers, "anthropic-ratelimit-requests-remaining"),
        header_str(headers, "anthropic-ratelimit-requests-reset"),
        now_ms,
        parse_rfc3339_reset,
    );

    // Anthropic ships either a single `tokens` bucket or split
    // `input-tokens` / `output-tokens` buckets (or both during overlap).
    // For pool routing we want one number — the most constrained side
    // is what'll trip first, so take the min across whatever's reported.
    let combined = header_str(headers, "anthropic-ratelimit-tokens-limit").and_then(|lim| {
        let rem = header_str(headers, "anthropic-ratelimit-tokens-remaining")?;
        let rst = header_str(headers, "anthropic-ratelimit-tokens-reset")?;
        build_bucket(Some(lim), Some(rem), Some(rst), now_ms, parse_rfc3339_reset)
    });
    let input = header_str(headers, "anthropic-ratelimit-input-tokens-limit").and_then(|lim| {
        let rem = header_str(headers, "anthropic-ratelimit-input-tokens-remaining")?;
        let rst = header_str(headers, "anthropic-ratelimit-input-tokens-reset")?;
        build_bucket(Some(lim), Some(rem), Some(rst), now_ms, parse_rfc3339_reset)
    });
    let output = header_str(headers, "anthropic-ratelimit-output-tokens-limit").and_then(|lim| {
        let rem = header_str(headers, "anthropic-ratelimit-output-tokens-remaining")?;
        let rst = header_str(headers, "anthropic-ratelimit-output-tokens-reset")?;
        build_bucket(Some(lim), Some(rem), Some(rst), now_ms, parse_rfc3339_reset)
    });

    let tokens = [combined, input, output]
        .into_iter()
        .flatten()
        .reduce(min_remaining_bucket);

    (requests, tokens)
}

/// Take whichever bucket has less remaining headroom (relative to its own
/// limit). The earlier-resetting bucket wins ties because waiting it out
/// is the faster recovery path.
fn min_remaining_bucket(a: Bucket, b: Bucket) -> Bucket {
    let a_pct = a.remaining_pct();
    let b_pct = b.remaining_pct();
    match a_pct.cmp(&b_pct) {
        std::cmp::Ordering::Less => a,
        std::cmp::Ordering::Greater => b,
        std::cmp::Ordering::Equal => {
            if a.reset_at_ms <= b.reset_at_ms {
                a
            } else {
                b
            }
        }
    }
}

fn build_bucket<F>(
    limit: Option<&str>,
    remaining: Option<&str>,
    reset: Option<&str>,
    now_ms: i64,
    parse_reset: F,
) -> Option<Bucket>
where
    F: Fn(&str, i64) -> Option<i64>,
{
    let limit = parse_count(limit?, Rounding::Up)?;
    let remaining = parse_count(remaining?, Rounding::Down)?;
    let reset_at_ms = parse_reset(reset?, now_ms)?;
    Some(Bucket {
        limit,
        remaining,
        reset_at_ms,
    })
}

/// Parse OpenAI's `reset` header. Two shapes seen in the wild:
///
/// 1. Pure integer — Unix epoch seconds (10-digit) or milliseconds (13-digit).
///    OpenRouter uses ms. Treated as absolute wall-clock time.
/// 2. Compound duration suffix string: `1s`, `6m0s`, `1h30m`, `250ms`.
///    OpenAI proper uses this. Treated as offset from `now_ms`.
///
/// Anything else returns `None` rather than guessing.
fn parse_openai_reset(s: &str, now_ms: i64) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Shape 1: pure integer.
    if let Ok(n) = s.parse::<i64>() {
        if n <= 0 {
            // Negative or zero epoch makes no sense; refuse to guess.
            return None;
        }
        // Disambiguate epoch seconds (10-digit, valid through year 2286)
        // from epoch milliseconds (13-digit, valid through year 5138). The
        // split is 1e11 — a value below that is at most ~Mar 2603 in
        // seconds and at most ~Mar 1973 in ms, so anything ≥ 1e11 is
        // unambiguously ms in any realistic timeframe.
        const MS_THRESHOLD: i64 = 100_000_000_000; // 1e11
        if n >= MS_THRESHOLD {
            return Some(n);
        }
        return Some(n.saturating_mul(1000));
    }

    // Shape 2: duration suffix string like "6m0s" or "1h30m15s" or "250ms".
    parse_duration_suffix(s).map(|ms| now_ms.saturating_add(ms))
}

/// Parse strings like `1s`, `6m0s`, `1h30m15s`, `250ms`, `1.5s`. Returns
/// total milliseconds, or `None` if no recognizable suffix is found.
fn parse_duration_suffix(s: &str) -> Option<i64> {
    let mut total_ms: i64 = 0;
    let mut cursor = s;
    let mut matched_any = false;

    while !cursor.is_empty() {
        // Find run of digits/decimal point.
        let num_end = cursor
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(cursor.len());
        if num_end == 0 {
            return None;
        }
        let (num_str, rest) = cursor.split_at(num_end);
        let n: f64 = num_str.parse().ok()?;
        if n.is_nan() || n.is_infinite() || n < 0.0 {
            return None;
        }

        // The unit can be `ms`, `s`, `m`, `h`, or `d`. `ms` must beat `m`
        // because the prefix is the same.
        let (unit_ms, unit_len) = if rest.starts_with("ms") {
            (1_f64, 2)
        } else if rest.starts_with('s') {
            (1_000_f64, 1)
        } else if rest.starts_with('m') {
            (60_000_f64, 1)
        } else if rest.starts_with('h') {
            (3_600_000_f64, 1)
        } else if rest.starts_with('d') {
            (86_400_000_f64, 1)
        } else {
            return None;
        };

        total_ms = total_ms.saturating_add((n * unit_ms) as i64);
        cursor = &rest[unit_len..];
        matched_any = true;
    }

    if matched_any {
        Some(total_ms)
    } else {
        None
    }
}

/// Parse Anthropic's RFC 3339 reset timestamp into wall-clock ms.
fn parse_rfc3339_reset(s: &str, _now_ms: i64) -> Option<i64> {
    let s = s.trim();
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).expect("header name"),
                HeaderValue::from_str(v).expect("header value"),
            );
        }
        h
    }

    const NOW: i64 = 1_700_000_000_000;

    #[test]
    fn openai_both_buckets_parsed() {
        let h = hm(&[
            ("x-ratelimit-limit-requests", "5000"),
            ("x-ratelimit-remaining-requests", "4998"),
            ("x-ratelimit-reset-requests", "1s"),
            ("x-ratelimit-limit-tokens", "160000"),
            ("x-ratelimit-remaining-tokens", "159500"),
            ("x-ratelimit-reset-tokens", "60s"),
        ]);
        let snap = parse_rate_limit_headers(Provider::OpenAi, &h, NOW).expect("snapshot");
        let req = snap.requests.expect("requests bucket");
        assert_eq!(req.limit, 5000);
        assert_eq!(req.remaining, 4998);
        assert_eq!(req.reset_at_ms, NOW + 1000);
        let tok = snap.tokens.expect("tokens bucket");
        assert_eq!(tok.limit, 160000);
        assert_eq!(tok.remaining, 159500);
        assert_eq!(tok.reset_at_ms, NOW + 60_000);
    }

    #[test]
    fn openai_compound_duration() {
        let h = hm(&[
            ("x-ratelimit-limit-requests", "100"),
            ("x-ratelimit-remaining-requests", "10"),
            ("x-ratelimit-reset-requests", "6m30s"),
        ]);
        let snap = parse_rate_limit_headers(Provider::OpenAi, &h, NOW).expect("snapshot");
        let req = snap.requests.expect("requests bucket");
        assert_eq!(req.reset_at_ms, NOW + 390_000);
    }

    #[test]
    fn openai_millisecond_duration() {
        let h = hm(&[
            ("x-ratelimit-limit-requests", "100"),
            ("x-ratelimit-remaining-requests", "10"),
            ("x-ratelimit-reset-requests", "250ms"),
        ]);
        let snap = parse_rate_limit_headers(Provider::OpenAi, &h, NOW).expect("snapshot");
        assert_eq!(snap.requests.unwrap().reset_at_ms, NOW + 250);
    }

    #[test]
    fn openrouter_epoch_ms_reset() {
        let reset_ms = NOW + 47_500;
        let h = hm(&[
            ("x-ratelimit-limit-requests", "1000"),
            ("x-ratelimit-remaining-requests", "1"),
            ("x-ratelimit-reset-requests", &reset_ms.to_string()),
        ]);
        let snap = parse_rate_limit_headers(Provider::OpenRouter, &h, NOW).expect("snapshot");
        assert_eq!(snap.requests.unwrap().reset_at_ms, reset_ms);
    }

    #[test]
    fn openai_epoch_seconds_reset() {
        let reset_s = NOW / 1000 + 90;
        let h = hm(&[
            ("x-ratelimit-limit-tokens", "100"),
            ("x-ratelimit-remaining-tokens", "5"),
            ("x-ratelimit-reset-tokens", &reset_s.to_string()),
        ]);
        let snap = parse_rate_limit_headers(Provider::OpenAi, &h, NOW).expect("snapshot");
        // Seconds get multiplied by 1000.
        assert_eq!(snap.tokens.unwrap().reset_at_ms, reset_s * 1000);
    }

    #[test]
    fn anthropic_rfc3339_reset() {
        let h = hm(&[
            ("anthropic-ratelimit-requests-limit", "50"),
            ("anthropic-ratelimit-requests-remaining", "49"),
            ("anthropic-ratelimit-requests-reset", "2024-04-29T19:33:36Z"),
            ("anthropic-ratelimit-tokens-limit", "40000"),
            ("anthropic-ratelimit-tokens-remaining", "39500"),
            ("anthropic-ratelimit-tokens-reset", "2024-04-29T19:33:36Z"),
        ]);
        let snap = parse_rate_limit_headers(Provider::Anthropic, &h, NOW).expect("snapshot");
        // 2024-04-29T19:33:36Z = 1714419216 epoch s = 1714419216000 ms
        assert_eq!(snap.requests.unwrap().reset_at_ms, 1714419216000);
        assert_eq!(snap.tokens.unwrap().reset_at_ms, 1714419216000);
    }

    #[test]
    fn anthropic_input_output_tokens_takes_min() {
        // Input bucket more drained (50% remaining) than output (90%) — the
        // combined "tokens" bucket should report input's numbers.
        let h = hm(&[
            ("anthropic-ratelimit-input-tokens-limit", "1000"),
            ("anthropic-ratelimit-input-tokens-remaining", "500"),
            (
                "anthropic-ratelimit-input-tokens-reset",
                "2024-04-29T19:33:36Z",
            ),
            ("anthropic-ratelimit-output-tokens-limit", "2000"),
            ("anthropic-ratelimit-output-tokens-remaining", "1800"),
            (
                "anthropic-ratelimit-output-tokens-reset",
                "2024-04-29T19:34:00Z",
            ),
        ]);
        let snap = parse_rate_limit_headers(Provider::Anthropic, &h, NOW).expect("snapshot");
        let tok = snap.tokens.expect("tokens bucket");
        assert_eq!(tok.limit, 1000);
        assert_eq!(tok.remaining, 500);
    }

    #[test]
    fn claude_code_oauth_parses_anthropic_headers() {
        // Same wire shape as Anthropic.
        let h = hm(&[
            ("anthropic-ratelimit-requests-limit", "10"),
            ("anthropic-ratelimit-requests-remaining", "2"),
            ("anthropic-ratelimit-requests-reset", "2024-04-29T19:33:36Z"),
        ]);
        let snap = parse_rate_limit_headers(Provider::ClaudeCodeOAuth, &h, NOW).expect("snapshot");
        assert_eq!(snap.requests.unwrap().remaining, 2);
    }

    #[test]
    fn no_headers_returns_none() {
        let h = hm(&[]);
        assert!(parse_rate_limit_headers(Provider::OpenAi, &h, NOW).is_none());
        assert!(parse_rate_limit_headers(Provider::Anthropic, &h, NOW).is_none());
    }

    #[test]
    fn malformed_remaining_drops_that_bucket_only() {
        let h = hm(&[
            ("x-ratelimit-limit-requests", "1000"),
            ("x-ratelimit-remaining-requests", "abc"), // garbage
            ("x-ratelimit-reset-requests", "1s"),
            ("x-ratelimit-limit-tokens", "100"),
            ("x-ratelimit-remaining-tokens", "50"),
            ("x-ratelimit-reset-tokens", "2s"),
        ]);
        let snap = parse_rate_limit_headers(Provider::OpenAi, &h, NOW).expect("snapshot");
        assert!(
            snap.requests.is_none(),
            "malformed bucket should be dropped"
        );
        let tok = snap.tokens.expect("tokens bucket should survive");
        assert_eq!(tok.remaining, 50);
    }

    /// A provider reporting a computed allowance (`999.5`) used to drop the
    /// whole bucket, silently disabling headroom-aware routing for that
    /// credential.
    #[test]
    fn fractional_counts_produce_a_bucket() {
        let h = hm(&[
            ("x-ratelimit-limit-requests", "1000.5"),
            ("x-ratelimit-remaining-requests", "12.9"),
            ("x-ratelimit-reset-requests", "30s"),
        ]);
        let snap = parse_rate_limit_headers(Provider::OpenAi, &h, NOW).expect("snapshot");
        let bucket = snap.requests.expect("fractional bucket must survive");
        assert_eq!(
            bucket.remaining, 12,
            "remaining rounds down so we never claim headroom the provider withheld"
        );
        assert_eq!(
            bucket.limit, 1001,
            "limit rounds up so the computed headroom percentage stays cautious"
        );
    }

    /// Anthropic's family goes through the same parser, so the fix must hold
    /// for both header flavors.
    #[test]
    fn fractional_counts_produce_a_bucket_for_anthropic_headers() {
        let h = hm(&[
            ("anthropic-ratelimit-requests-limit", "40.0"),
            ("anthropic-ratelimit-requests-remaining", "0.5"),
            ("anthropic-ratelimit-requests-reset", "2030-01-01T00:00:00Z"),
        ]);
        let snap = parse_rate_limit_headers(Provider::Anthropic, &h, NOW).expect("snapshot");
        let bucket = snap.requests.expect("fractional bucket must survive");
        assert_eq!(bucket.limit, 40);
        assert_eq!(
            bucket.remaining, 0,
            "half a request is not a request — round down to empty"
        );
        assert_eq!(
            bucket.remaining_pct(),
            0,
            "a nearly-empty bucket must read as empty so the pool rotates off it"
        );
    }

    /// Rounding must stay conservative: a fraction can only lower the reported
    /// headroom, never raise it above what an exact reading would give.
    #[test]
    fn fractional_rounding_never_overstates_headroom() {
        let fractional = hm(&[
            ("x-ratelimit-limit-requests", "100.4"),
            ("x-ratelimit-remaining-requests", "50.9"),
            ("x-ratelimit-reset-requests", "1s"),
        ]);
        let exact = hm(&[
            ("x-ratelimit-limit-requests", "100"),
            ("x-ratelimit-remaining-requests", "50"),
            ("x-ratelimit-reset-requests", "1s"),
        ]);
        let fractional_pct = parse_rate_limit_headers(Provider::OpenAi, &fractional, NOW)
            .expect("snapshot")
            .requests
            .expect("bucket")
            .remaining_pct();
        let exact_pct = parse_rate_limit_headers(Provider::OpenAi, &exact, NOW)
            .expect("snapshot")
            .requests
            .expect("bucket")
            .remaining_pct();
        assert!(
            fractional_pct <= exact_pct,
            "fractional {fractional_pct}% must not exceed exact {exact_pct}%"
        );
    }

    /// Values past f64's exact-integer range must survive unchanged; routing
    /// off a silently rounded token budget would be worse than not parsing it.
    #[test]
    fn very_large_integer_counts_stay_exact() {
        let huge = (1u64 << 60) + 1;
        let h = hm(&[
            ("x-ratelimit-limit-tokens", &huge.to_string()),
            ("x-ratelimit-remaining-tokens", &huge.to_string()),
            ("x-ratelimit-reset-tokens", "1s"),
        ]);
        let snap = parse_rate_limit_headers(Provider::OpenAi, &h, NOW).expect("snapshot");
        let bucket = snap.tokens.expect("bucket");
        assert_eq!(bucket.limit, huge);
        assert_eq!(bucket.remaining, huge);
    }

    /// Widening the parser must not start accepting nonsense. Negatives,
    /// non-finite values, and plain garbage still drop their bucket.
    #[test]
    fn non_numeric_and_out_of_domain_counts_still_drop_the_bucket() {
        for bad in ["abc", "-1", "-0.5", "NaN", "inf", "1e", ""] {
            let h = hm(&[
                ("x-ratelimit-limit-requests", "100"),
                ("x-ratelimit-remaining-requests", bad),
                ("x-ratelimit-reset-requests", "1s"),
                ("x-ratelimit-limit-tokens", "100"),
                ("x-ratelimit-remaining-tokens", "50"),
                ("x-ratelimit-reset-tokens", "2s"),
            ]);
            let snap = parse_rate_limit_headers(Provider::OpenAi, &h, NOW).expect("snapshot");
            assert!(
                snap.requests.is_none(),
                "remaining={bad:?} must drop the requests bucket"
            );
            assert!(
                snap.tokens.is_some(),
                "remaining={bad:?} must not disturb the tokens bucket"
            );
        }
    }

    #[test]
    fn unparseable_reset_drops_bucket() {
        let h = hm(&[
            ("x-ratelimit-limit-requests", "100"),
            ("x-ratelimit-remaining-requests", "10"),
            ("x-ratelimit-reset-requests", "tomorrow-ish"),
        ]);
        // Only bucket had a bad reset → no buckets → no snapshot.
        assert!(parse_rate_limit_headers(Provider::OpenAi, &h, NOW).is_none());
    }

    #[test]
    fn bedrock_provider_returns_none_even_with_headers() {
        // Defensive: a proxy in front of Bedrock might inject these.
        // The pool doesn't act on them for non-API-key providers, so we
        // refuse to parse them in the first place.
        let h = hm(&[
            ("x-ratelimit-limit-requests", "100"),
            ("x-ratelimit-remaining-requests", "5"),
            ("x-ratelimit-reset-requests", "60s"),
        ]);
        assert!(parse_rate_limit_headers(Provider::Bedrock, &h, NOW).is_none());
        assert!(parse_rate_limit_headers(Provider::ClaudeCli, &h, NOW).is_none());
        assert!(parse_rate_limit_headers(Provider::Ollama, &h, NOW).is_none());
    }

    #[test]
    fn negative_epoch_rejected() {
        let h = hm(&[
            ("x-ratelimit-limit-requests", "100"),
            ("x-ratelimit-remaining-requests", "10"),
            ("x-ratelimit-reset-requests", "-1"),
        ]);
        assert!(parse_rate_limit_headers(Provider::OpenAi, &h, NOW).is_none());
    }

    #[test]
    fn remaining_pct_zero_limit_is_full() {
        // Defensive: a provider sending limit=0 is broken; we treat it as
        // "full headroom" so we don't rotate spuriously off bogus data.
        let b = Bucket {
            limit: 0,
            remaining: 0,
            reset_at_ms: 0,
        };
        assert_eq!(b.remaining_pct(), 100);
    }

    #[test]
    fn remaining_pct_clamps_to_100() {
        // A buggy provider could report remaining > limit. Don't panic, clamp.
        let b = Bucket {
            limit: 100,
            remaining: 200,
            reset_at_ms: 0,
        };
        assert_eq!(b.remaining_pct(), 100);
    }

    #[test]
    fn is_stale_after_reset() {
        let snap = RateLimitSnapshot {
            captured_at_ms: NOW,
            requests: Some(Bucket {
                limit: 100,
                remaining: 0,
                reset_at_ms: NOW + 60_000,
            }),
            tokens: None,
        };
        assert!(!snap.is_stale(NOW));
        assert!(!snap.is_stale(NOW + 59_999));
        assert!(snap.is_stale(NOW + 60_001));
    }
}
