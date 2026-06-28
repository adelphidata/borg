//! Per-model stream stall-timeout floors for reasoning models.
//!
//! Reasoning models (OpenAI o-series, DeepSeek R1, Qwen QwQ, Nemotron Ultra,
//! Grok reasoning, …) can spend minutes in a hidden thinking phase before
//! emitting their first token. The default per-chunk stream timeout
//! ([`crate::constants::DEFAULT_LLM_STREAM_CHUNK_TIMEOUT_SECS`], 90s) is tuned
//! for ordinary chat models and would idle-kill the socket mid-think — the
//! agent then sees a spurious "No data received" error and an unnecessary
//! fallback fires before the model ever spoke.
//!
//! [`stale_timeout_floor_secs`] returns a minimum per-chunk timeout for known
//! reasoning models. Callers apply it as `max(configured, floor)` so it never
//! *lowers* a threshold and never overrides an explicit, higher user config; a
//! configured value of 0 (no timeout) is left untouched by the caller.

/// Minimum per-chunk stream timeout, in seconds, granted to reasoning models
/// so their pre-first-token thinking phase doesn't trip the stall detector.
const REASONING_FLOOR_SECS: u64 = 300;

/// Family slugs that mark a reasoning model. Matched as a substring of the
/// lowercased model id so provider prefixes (`openai/…`, `fireworks/…`) and
/// aggregator suffixes (`:free`, `-latest`) don't defeat detection.
const REASONING_MARKERS: &[&str] = &[
    "deepseek-r1",
    "deepseek-reasoner",
    "qwq",
    "nemotron",
    "grok-4",
    "reasoning",
    "thinking",
];

/// Return the minimum per-chunk stream timeout (seconds) for `model`, or 0 for
/// models with no special requirement (so a `max()` against it is a no-op).
pub fn stale_timeout_floor_secs(model: &str) -> u64 {
    let m = model.to_ascii_lowercase();

    // Explicit non-reasoning variants opt out (e.g. `grok-4-fast-non-reasoning`).
    // Checked first so the `grok-4` / `reasoning` markers below can't catch it.
    if m.contains("non-reasoning") {
        return 0;
    }

    if is_openai_o_series(&m) || REASONING_MARKERS.iter().any(|k| m.contains(k)) {
        REASONING_FLOOR_SECS
    } else {
        0
    }
}

/// True if `model_lower` names an OpenAI o-series reasoning model (`o1`, `o3`,
/// `o4`, and their `-mini`/`-preview` variants). The `o<digit>` must sit at a
/// token boundary so ids like `gpt-4o` (no `o<digit>`) or a digit embedded in
/// a word don't false-positive.
fn is_openai_o_series(model_lower: &str) -> bool {
    let bytes = model_lower.as_bytes();
    for (i, w) in bytes.windows(2).enumerate() {
        if w[0] == b'o' && matches!(w[1], b'1' | b'3' | b'4') {
            let boundary = i == 0 || matches!(bytes[i - 1], b'/' | b'-' | b':' | b' ' | b'_');
            if boundary {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_models_get_a_floor() {
        for m in [
            "openai/o1-preview",
            "o3-mini",
            "o4-mini",
            "deepseek-r1",
            "deepseek/deepseek-reasoner",
            "qwen/qwq-32b",
            "nvidia/nemotron-3-ultra",
            "x-ai/grok-4",
            "grok-4-fast-reasoning",
        ] {
            assert_eq!(
                stale_timeout_floor_secs(m),
                REASONING_FLOOR_SECS,
                "expected a floor for {m}"
            );
        }
    }

    #[test]
    fn ordinary_models_get_no_floor() {
        for m in [
            "anthropic/claude-sonnet-4-6",
            "gpt-4o",
            "gpt-4o-mini",
            "openai/gpt-5",
            "deepseek-chat",
            "grok-4-fast-non-reasoning",
            "meta-llama/llama-3.3-70b",
        ] {
            assert_eq!(stale_timeout_floor_secs(m), 0, "expected no floor for {m}");
        }
    }

    #[test]
    fn floor_never_lowers_a_higher_config() {
        // Caller contract: max(configured, floor). A 600s user config wins.
        let configured = 600u64;
        let effective = configured.max(stale_timeout_floor_secs("o3-mini"));
        assert_eq!(effective, 600);
        // …but a default 90s config is lifted to the floor.
        let effective = 90u64.max(stale_timeout_floor_secs("o3-mini"));
        assert_eq!(effective, REASONING_FLOOR_SECS);
    }
}
